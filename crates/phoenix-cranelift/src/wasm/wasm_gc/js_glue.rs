//! wasm32-gc `extern js` JS-glue plugin.
//!
//! The [`GcGlue`] backend supplies the wasm32-gc value ABI for the shared glue
//! core ([`super::super::glue`]). It differs from the linear binding exactly
//! where the gc representation does (decisions D/E/G):
//!
//! - **`JsValue` → `externref`, directly.** The host VM owns and traces the
//!   value, so a `JsValue` *is* the host JS value — no handle table, no `put`/`get`.
//! - **`String` via the scratch region.** A gc `$string` is a GC object the host
//!   can't read, so its bytes are copied through a fixed linear-memory buffer via
//!   the exported `phx_extern_str_to_scratch` / `phx_extern_str_from_scratch`
//!   helpers (PR 13). One slot (a `(ref $string)`), not the linear `(ptr, len)`.
//! - **Closures held directly, no pin.** A closure crosses as its managed ref;
//!   the JS wrapper holds it and the host VM traces it, so a host-retained callback
//!   stays alive with no `phx_gc_pin` and is reclaimed when the host drops the
//!   wrapper — no `FinalizationRegistry`, no leak (PR 14).
//!
//! Everything else — the WASI shim, the `instantiate` frame, the thunk/factory
//! structure, the trampoline *names* (decision C) — is the shared core. The two
//! targets do *not* share a glue *file*: each emits its own, and they're not
//! interchangeable (linear's handle table / `phx_string_alloc` vs. gc's scratch
//! helpers). What decision C shares is the *naming* scheme — so the core
//! generator and the callback-factory derivation need no per-backend branch, and
//! both backends' tests assert the same `__cb_*` / `__phoenix_invoke_closure_*`
//! names. A target's single glue file is then correct in either *host
//! environment* (Node or browser), being environment-agnostic.

use super::super::glue::{self, GlueBackend, callback_factory_name};
use super::module_builder::{PRINT_STR_BUF_START, PRINT_STR_MAX_LEN};
use crate::error::CompileError;
use crate::extern_abi::{CallbackSig, ExternSig};
use phoenix_ir::types::IrType;

/// Generate the wasm32-gc JS glue for the program's `extern js` imports.
pub(super) fn generate(externs: &[ExternSig]) -> Result<String, CompileError> {
    glue::generate(externs, &GcGlue)
}

/// The wasm32-gc marshalling plugin.
struct GcGlue;

impl GlueBackend for GcGlue {
    fn target_name(&self) -> &'static str {
        "wasm32-gc"
    }

    fn instantiate_helpers(&self) -> String {
        // The scratch-buffer offset and cap are compile-time constants shared with
        // the gc String-marshalling helpers (`module_builder`), so the glue and the
        // wasm helpers agree on the buffer without a runtime handshake.
        format!(
            r#"  // gc value ABI: a `JsValue` is the host value directly (externref) — no
  // handle table. A `String` is a GC object the host can't read, so its bytes are
  // copied through a fixed linear scratch buffer via the exported helpers
  // (decision E). Closures are held by the JS wrapper and traced by the host VM —
  // no pin, no retention table (decision G).
  let strToScratch = null;   // phx_extern_str_to_scratch
  let strFromScratch = null; // phx_extern_str_from_scratch
  const __STR_BUF = {buf};
  const __STR_MAX = {max};
  // Read a Phoenix `$string`: the helper copies its bytes into the scratch buffer
  // and returns the length; decode them from linear memory. Guard a missing export
  // (the scratch helpers are emitted only when a `String` crosses an extern
  // boundary) so a stale runtime surfaces a clear glue error rather than an opaque
  // `strToScratch is not a function` — mirroring the linear `buildString` guard.
  // A string past the buffer cap is returned *uncopied* by the helper (its true
  // length, which is over `__STR_MAX`); raise the same clear error as the build
  // side rather than decoding the stale buffer.
  const __readGcString = (ref) => {{
    if (typeof strToScratch !== "function")
      throw new Error(
        "phoenix glue: this module did not export `phx_extern_str_to_scratch`; a " +
          "host function taking or returning a String needs it (rebuild with a current runtime)",
      );
    const len = strToScratch(ref);
    if (len > __STR_MAX)
      throw new Error(
        "phoenix glue: a string of " + len + " bytes exceeds the " +
          "wasm32-gc scratch buffer (" + __STR_MAX + " bytes)",
      );
    return readString(__STR_BUF, len);
  }};
  // Build a Phoenix `$string` from a JS string: write the UTF-8 bytes into the
  // scratch buffer and let the helper construct the GC string (bytes copied, never
  // shared — decision F). A string past the buffer cap is a clear glue error
  // rather than a silent overrun (the wasm helper would trap).
  const __buildGcString = (s) => {{
    if (typeof strFromScratch !== "function")
      throw new Error(
        "phoenix glue: this module did not export `phx_extern_str_from_scratch`; a " +
          "host function returning a String needs it (rebuild with a current runtime)",
      );
    const bytes = __encoder.encode(s);
    if (bytes.length > __STR_MAX)
      throw new Error(
        "phoenix glue: a string of " + bytes.length + " bytes exceeds the " +
          "wasm32-gc scratch buffer (" + __STR_MAX + " bytes)",
      );
    new Uint8Array(memory.buffer, __STR_BUF, bytes.length).set(bytes);
    return strFromScratch(bytes.length);
  }};"#,
            buf = PRINT_STR_BUF_START,
            max = PRINT_STR_MAX_LEN,
        )
    }

    fn post_instantiate(&self) -> String {
        // Bind the String-marshalling helpers (exported only when a `String`
        // crosses an extern boundary; `null` otherwise).
        "  strToScratch = instance.exports.phx_extern_str_to_scratch || null;\n  \
         strFromScratch = instance.exports.phx_extern_str_from_scratch || null;"
            .to_string()
    }

    /// Wasm→host marshalling for one extern parameter. Every gc boundary type is a
    /// single slot (a scalar, an `externref`, or one managed ref), so the slot
    /// count is always 1 — unlike linear's 2-slot `String`.
    ///
    /// The authority for that "1" is `module_builder::gc_extern_valtypes`, which
    /// flattens the import signature these `p{slot}` indices walk in lockstep
    /// with: it yields exactly
    /// one `ValType` per marshallable boundary type today. If a future gc boundary
    /// type ever flattened to multiple slots there, the constant `1` returned below
    /// (and the `p{slot}` accumulation in the shared `build_thunk`) would have to
    /// change with it. Linear pins this with a `wasm_valtypes_for` `debug_assert`;
    /// gc can't, because that flattening needs a `ModuleBuilder` the glue plugin
    /// doesn't hold — hence this comment is the coupling's record.
    fn inbound_arg(&self, ty: &IrType, slot: usize) -> Result<(String, usize), String> {
        let expr = match ty {
            IrType::I64 => format!("Number(p{slot})"),
            IrType::F64 => format!("p{slot}"),
            IrType::Bool => format!("(p{slot} !== 0)"),
            // A gc `$string` ref → read its bytes via the scratch helper.
            IrType::StringRef => format!("__readGcString(p{slot})"),
            // `externref` is the host value directly — no handle lookup.
            IrType::JsValue => format!("p{slot}"),
            IrType::ClosureRef {
                param_types,
                return_type,
            } => {
                let sig = CallbackSig {
                    param_types: param_types.clone(),
                    return_type: (**return_type).clone(),
                };
                match callback_factory_name(&sig) {
                    Some(factory) => format!("{factory}(p{slot})"),
                    None => {
                        return Err("callback parameter with a non-marshallable signature \
                             (nested closures are not supported yet)"
                            .to_string());
                    }
                }
            }
            other => return Err(format!("parameter type `{other}` is not marshallable")),
        };
        Ok((expr, 1))
    }

    fn outbound_return(&self, ty: &IrType, call: &str) -> Result<String, String> {
        Ok(match ty {
            IrType::Void => format!("{call};"),
            // Same lenient Int↔Number coercion + non-numeric guard as linear.
            IrType::I64 => format!(
                "const __r = Number({call}); \
                 if (!Number.isFinite(__r)) throw new Error(\"phoenix glue: an `Int`-returning host function returned a non-numeric value\"); \
                 return BigInt(Math.trunc(__r));"
            ),
            IrType::F64 => format!("return {call};"),
            IrType::Bool => format!("return ({call}) ? 1 : 0;"),
            // The host value *is* the `externref` — return it directly.
            IrType::JsValue => format!("return {call};"),
            // Build a gc `$string` from the host's JS string via the scratch
            // helper; guard `null`/`undefined` (a host that returned nothing).
            IrType::StringRef => format!(
                "const __r = {call}; \
                 if (__r == null) throw new Error(\"phoenix glue: a `String`-returning host function returned no value (null or undefined)\"); \
                 return __buildGcString(String(__r));"
            ),
            other => return Err(format!("return type `{other}` is not marshallable")),
        })
    }

    /// Host→wasm marshalling for a callback parameter. A `String` is one slot (a
    /// built `$string` ref), not linear's spread `(ptr, len)`.
    fn callback_arg_to_wasm(&self, ty: &IrType, idx: usize) -> Result<String, CompileError> {
        let a = format!("args[{idx}]");
        Ok(match ty {
            IrType::I64 => format!("__intArg({a})"),
            IrType::F64 => a,
            IrType::Bool => format!("({a}) ? 1 : 0"),
            IrType::StringRef => format!("__buildGcString(String({a}))"),
            // `externref` direct.
            IrType::JsValue => a,
            other => {
                return Err(CompileError::new(format!(
                    "wasm32-gc: callback parameter type `{other}` is not \
                     marshallable (internal compiler bug)"
                )));
            }
        })
    }

    fn callback_wasm_to_host(&self, ty: &IrType) -> Result<String, CompileError> {
        Ok(match ty {
            IrType::I64 => "Number(__r)".to_string(),
            IrType::F64 => "__r".to_string(),
            IrType::Bool => "(__r !== 0)".to_string(),
            // `externref` direct.
            IrType::JsValue => "__r".to_string(),
            IrType::StringRef => "__readGcString(__r)".to_string(),
            other => {
                return Err(CompileError::new(format!(
                    "wasm32-gc: callback return type `{other}` is not \
                     marshallable (internal compiler bug)"
                )));
            }
        })
    }

    fn wrap_callback(&self, inner_body: &str) -> String {
        // No retention table and no pin: the JS wrapper holds the closure managed
        // ref (`ptr`), which the host VM GC traces. The wrapper (and the closure)
        // is reclaimed once the host drops it.
        format!("(...args) => {{ {inner_body} }}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(name: &str, params: Vec<IrType>, return_type: IrType) -> ExternSig {
        ExternSig {
            module: "js".to_string(),
            name: name.to_string(),
            params,
            return_type,
        }
    }

    #[test]
    fn jsvalue_crosses_as_externref_no_handle_table() {
        // gc `JsValue` is the host value directly — the glue never builds a handle
        // table, and the marshalling is the identity in both directions.
        let externs = vec![
            sig("getEl", vec![IrType::StringRef], IrType::JsValue),
            sig("tagOf", vec![IrType::JsValue], IrType::StringRef),
        ];
        let glue = generate(&externs).unwrap();
        assert!(
            !glue.contains("__makeHandles"),
            "gc must not build a handle table"
        );
        assert!(
            !glue.contains("handles."),
            "gc must not reference a handle table"
        );
        // getEl returns a JsValue → returned straight through.
        assert!(glue.contains("return __buildGcString")); // tagOf returns a String
        assert!(glue.contains("host.getEl(__readGcString(p0))"));
        // tagOf takes a JsValue (externref) directly as p0.
        assert!(glue.contains("host.tagOf(p0)"));
    }

    #[test]
    fn string_marshals_via_the_scratch_helpers_one_slot() {
        // A gc `String` is one slot (a managed ref), read/built via the scratch
        // helpers — not linear's 2-slot `(ptr, len)` + `phx_string_alloc`.
        let externs = vec![sig("shout", vec![IrType::StringRef], IrType::StringRef)];
        let glue = generate(&externs).unwrap();
        // One param (p0), not two.
        assert!(glue.contains("shout(p0) {"));
        assert!(
            !glue.contains("phx_string_alloc"),
            "gc strings don't use phx_string_alloc"
        );
        assert!(glue.contains("const len = strToScratch(ref);"));
        assert!(glue.contains("return readString(__STR_BUF, len);"));
        assert!(glue.contains("strFromScratch(bytes.length)"));
        // String-out reads via the scratch helper; String-return builds via it.
        assert!(glue.contains("host.shout(__readGcString(p0))"));
        assert!(glue.contains("return __buildGcString(String(__r));"));
        // Post-instantiate binds the exported helpers.
        assert!(glue.contains("strToScratch = instance.exports.phx_extern_str_to_scratch"));
    }

    #[test]
    fn string_helpers_guard_their_buffer_cap_missing_exports_and_null_returns() {
        // A `String`-in / `String`-out extern exercises every gc String guard:
        // the scratch-buffer overflow cap, the missing-export guards on both
        // helpers, and the null/undefined guard on a String-returning host fn.
        let externs = vec![sig("shout", vec![IrType::StringRef], IrType::StringRef)];
        let glue = generate(&externs).unwrap();
        // The scratch-buffer overflow cap on *both* the build path (`__buildGcString`)
        // and the read path (`__readGcString` checks the length the helper returns
        // uncopied), so an oversized string is a clear glue error in either direction
        // rather than a silent overrun or an opaque wasm trap.
        assert!(
            glue.contains("exceeds the") && glue.contains("wasm32-gc scratch buffer"),
            "a too-long string must be a clear glue error, not a silent overrun"
        );
        assert!(
            glue.contains("const len = strToScratch(ref);")
                && glue.contains("if (len > __STR_MAX)"),
            "the read path must guard the returned length against the buffer cap"
        );
        // Missing-export guards mirroring the linear `phx_string_alloc` guard.
        assert!(
            glue.contains("did not export `phx_extern_str_to_scratch`"),
            "the read helper must guard a missing scratch export"
        );
        assert!(
            glue.contains("did not export `phx_extern_str_from_scratch`"),
            "the build helper must guard a missing scratch export"
        );
        // A String-returning host fn that returns nothing is a clear glue error,
        // not the literal string `"undefined"`.
        assert!(glue.contains("returned no value (null or undefined)"));
    }

    #[test]
    fn scratch_buffer_constants_match_the_wasm_helper_constants() {
        // The gc String guards live on two sides that must agree on the *same* byte
        // boundary: the JS glue (`__readGcString` / `__buildGcString` reject
        // `len > __STR_MAX`) and the wasm `phx_extern_str_to_scratch` helper (returns
        // the length *uncopied* when `len > PRINT_STR_MAX_LEN`, letting the glue raise
        // a clear error instead of trapping). A substring test can't run the boundary,
        // but both sides reference the same Rust constants and both use a strict `>`;
        // pinning that the glue interpolates those *exact* values (not a stale literal
        // that could drift off-by-one from the helper) closes the gap statically.
        let glue = generate(&[sig("shout", vec![IrType::StringRef], IrType::StringRef)]).unwrap();
        assert!(
            glue.contains(&format!("const __STR_BUF = {PRINT_STR_BUF_START};")),
            "glue must interpolate the wasm scratch-buffer offset, not a stale literal"
        );
        assert!(
            glue.contains(&format!("const __STR_MAX = {PRINT_STR_MAX_LEN};")),
            "glue's cap must equal the wasm helper's `PRINT_STR_MAX_LEN` so the \
             trap-free helper return and the glue's `> __STR_MAX` check reject at the \
             same byte boundary"
        );
    }

    #[test]
    fn callbacks_hold_the_ref_with_no_pin() {
        // A `(Int) -> Void` callback: the gc wrapper is a bare arrow holding the
        // closure ref — no `__retainCallback`, no pin, no FinalizationRegistry.
        let externs = vec![sig(
            "eachUpTo",
            vec![
                IrType::I64,
                IrType::ClosureRef {
                    param_types: vec![IrType::I64],
                    return_type: Box::new(IrType::Void),
                },
            ],
            IrType::Void,
        )];
        let glue = generate(&externs).unwrap();
        assert!(glue.contains("function __cb_i_to_v(ptr)"));
        // Bare arrow wrapper, holding `ptr` (the closure ref) — no retention.
        assert!(glue.contains("return (...args) => {"));
        assert!(
            !glue.contains("__retainCallback"),
            "gc callbacks must not retain/pin"
        );
        assert!(!glue.contains("gcPin"), "gc must not pin callbacks");
        assert!(
            !glue.contains("FinalizationRegistry"),
            "gc needs no finalizer"
        );
        // No retention table => no `retainedCallbackCount()` diagnostic either. gc
        // leaves `extra_return_members` at the empty default (the linear-only member),
        // so the returned object is just `{ instance, run }`.
        assert!(
            !glue.contains("retainedCallbackCount"),
            "gc has no retention table, so no retainedCallbackCount() member"
        );
        // Dispatches to the same-named trampoline as linear, with the Int marshalled.
        assert!(
            glue.contains(r#"exports["__phoenix_invoke_closure_i_to_v"](ptr, __intArg(args[0]))"#)
        );
        // The callback-taking extern is a required host binding.
        assert!(glue.contains(r#"for (const __name of ["eachUpTo"])"#));
    }

    #[test]
    fn scalars_marshal_like_linear() {
        let externs = vec![sig(
            "config",
            vec![IrType::I64, IrType::F64, IrType::Bool],
            IrType::Void,
        )];
        let glue = generate(&externs).unwrap();
        assert!(glue.contains("host.config(Number(p0), p1, (p2 !== 0))"));
    }

    #[test]
    fn scalar_returns_marshal_like_linear() {
        // The `outbound_return` Int/Bool/Float arms aren't otherwise pinned at the
        // gc unit layer (the scalars fixture covers them only end-to-end).
        let externs = vec![
            sig("count", vec![], IrType::I64),
            sig("flag", vec![], IrType::Bool),
            sig("ratio", vec![], IrType::F64),
        ];
        let glue = generate(&externs).unwrap();
        // Int return: lenient `Number` coercion + non-finite guard + `BigInt` back
        // to the i64 the import expects.
        assert!(glue.contains("const __r = Number(host.count());"));
        assert!(glue.contains("return BigInt(Math.trunc(__r));"));
        // Bool return: truthy → 1/0; Float return: passed straight through.
        assert!(glue.contains("return (host.flag()) ? 1 : 0;"));
        assert!(glue.contains("return host.ratio();"));
    }

    #[test]
    fn callback_marshals_string_bool_and_float_both_directions() {
        // A `(String, Bool, Float) -> String` callback exercises every
        // `callback_arg_to_wasm` / `callback_wasm_to_host` arm not covered by the
        // `(Int) -> Void` callback test: a String arg built into a `$string`, a
        // Bool arg coerced to 1/0, a Float arg passed through, and a String result
        // read back via the scratch helper.
        let externs = vec![sig(
            "run",
            vec![IrType::ClosureRef {
                param_types: vec![IrType::StringRef, IrType::Bool, IrType::F64],
                return_type: Box::new(IrType::StringRef),
            }],
            IrType::Void,
        )];
        let glue = generate(&externs).unwrap();
        // Host→wasm callback args.
        assert!(glue.contains("__buildGcString(String(args[0]))"));
        assert!(glue.contains("(args[1]) ? 1 : 0"));
        assert!(glue.contains("args[2]")); // Float arg passes through unchanged
        // Wasm→host callback return: the trampoline result read back as a String.
        assert!(glue.contains("return __readGcString(__r);"));
    }

    #[test]
    fn string_param_advances_one_slot_not_two() {
        // gc's `String` is a single slot (a `(ref $string)`), unlike linear's
        // 2-slot `(ptr, len)`. A `String` followed by an `Int` must therefore put
        // the Int at `p1`, not `p2` — pinning that gc `inbound_arg` returns slot
        // count 1 for `String` and the shared `build_thunk` accumulates it. A
        // regression to 2 (e.g. copied from the linear arm) would shift the Int to
        // `p2` and desync from the 1-slot-per-type import section
        // (`gc_extern_valtypes`).
        let externs = vec![sig(
            "tag",
            vec![IrType::StringRef, IrType::I64],
            IrType::Void,
        )];
        let glue = generate(&externs).unwrap();
        // Two flattened wasm params (the string ref, the i64), and the Int marshals
        // from the slot *immediately* after the string's single slot.
        assert!(glue.contains("tag(p0, p1) {"));
        assert!(glue.contains("host.tag(__readGcString(p0), Number(p1))"));
    }

    #[test]
    fn nested_callback_param_emits_throwing_thunk_and_is_not_required() {
        // A callback whose own parameter is itself a closure is not marshallable in
        // this phase, so `weird` falls back to a throwing thunk (the deferral
        // pattern) and needs no host binding — only `alert` is required. Mirrors the
        // linear backend: the gc `inbound_arg` `ClosureRef` arm hits the same
        // `callback_factory_name` -> `None` deferral.
        let nested = IrType::ClosureRef {
            param_types: vec![IrType::ClosureRef {
                param_types: vec![],
                return_type: Box::new(IrType::Void),
            }],
            return_type: Box::new(IrType::Void),
        };
        let externs = vec![
            sig("alert", vec![IrType::StringRef], IrType::Void),
            sig("weird", vec![nested], IrType::Void),
        ];
        let glue = generate(&externs).unwrap();
        assert!(glue.contains("weird(/* ...args */) { throw new Error("));
        assert!(glue.contains("nested closures are not supported yet"));
        assert!(glue.contains(r#"for (const __name of ["alert"])"#));
        // No factory emitted for the unsupported signature.
        assert!(!glue.contains("__cb_"));
    }

    #[test]
    fn shares_the_wasi_shim_and_marker() {
        let glue = generate(&[sig("noop", vec![], IrType::Void)]).unwrap();
        assert!(glue.starts_with(super::super::super::glue::GENERATED_MARKER));
        assert!(glue.contains("wasm32-gc module"));
        assert!(glue.contains("fd_write"));
        assert!(glue.contains("export async function instantiate"));
    }
}
