//! wasm32-linear `extern js` JS-glue plugin.
//!
//! The [`LinearGlue`] backend supplies the linear value ABI for the shared glue
//! core ([`super::glue`]): `Int`↔Number (exact to 2^53), `Float`↔number,
//! `Bool`↔0/1, `String` read from / built into linear memory (`TextDecoder` /
//! the exported `phx_string_alloc`), `JsValue`↔an opaque `i32` handle into a
//! glue-owned table, and a closure↔its `i32` env pointer wrapped in a *pinned*,
//! `FinalizationRegistry`-released JS callable (design-decisions §Phase 2.5
//! decisions D/E/F/G). The shared core owns the WASI shim, the `instantiate`
//! frame, and the thunk/factory structure; this module owns only the marshalling.
//!
//! **Still deferred:** a closure *returned* from a host to Phoenix, and a *nested*
//! callback (a closure whose own parameter/return is itself a closure). The
//! generator emits a throwing thunk for such a signature so the program still
//! links; only an actual call to that extern fails.

use super::glue::{self, GlueBackend, callback_factory_name};
use super::translate::wasm_valtypes_for;
use crate::error::CompileError;
use crate::extern_abi::{CallbackSig, ExternSig};
use phoenix_ir::types::IrType;

/// Generate the wasm32-linear JS glue for the program's `extern js` imports.
pub(super) fn generate(externs: &[ExternSig]) -> Result<String, CompileError> {
    glue::generate(externs, &LinearGlue)
}

/// The wasm32-linear marshalling plugin. Stateless — all per-call data is in the
/// generated JS; this just decides which expressions to emit.
struct LinearGlue;

impl GlueBackend for LinearGlue {
    fn target_name(&self) -> &'static str {
        "wasm32-linear"
    }

    fn instantiate_helpers(&self) -> String {
        LINEAR_HELPERS.to_string()
    }

    fn post_instantiate(&self) -> String {
        // `phx_string_alloc` builds a GC string from host bytes; the GC pin/unpin
        // hooks root a host-retained callback. A module that uses neither leaves
        // them `null` (the `|| null` guard) — they're exported only when needed.
        "  stringAlloc = instance.exports.phx_string_alloc || null;\n  \
         gcPin = instance.exports.phx_gc_pin || null;\n  \
         gcUnpin = instance.exports.phx_gc_unpin || null;"
            .to_string()
    }

    fn extra_return_members(&self) -> String {
        // Diagnostic introspection (not part of the stable embedding API): the
        // number of host-retained Phoenix callbacks still tracked — one map entry
        // per pinned closure, including any whose wrapper was collected but whose
        // finalizer has not yet run. Drops to zero once every retained callback has
        // been released. Exposed so tests can observe reclamation.
        "    retainedCallbackCount() {\n      return __callbacks.size;\n    },".to_string()
    }

    /// Inbound (wasm → host) marshalling for one extern parameter. The slot count
    /// comes from [`wasm_valtypes_for`] — the same function that flattened this
    /// type into the import signature — so the `p{n}` indices can't desync.
    fn inbound_arg(&self, ty: &IrType, slot: usize) -> Result<(String, usize), String> {
        let expr = match ty {
            // Inbound `Int` is unguarded by design: a wasm-side i64 above 2**53
            // loses precision silently here, matching the documented Int↔Number
            // contract. (The outbound arm guards only against a non-numeric host
            // return, not precision.)
            IrType::I64 => format!("Number(p{slot})"),
            IrType::F64 => format!("p{slot}"),
            IrType::Bool => format!("(p{slot} !== 0)"),
            IrType::StringRef => {
                // The `(ptr, len)` layout hardcodes 2 slots; pin it to the shared
                // flattening so a future change to `wasm_valtypes_for` trips here
                // rather than silently desyncing the `p{slot+1}` index.
                debug_assert_eq!(
                    wasm_valtypes_for(ty).map(|v| v.len()).unwrap_or(0),
                    2,
                    "StringRef must flatten to exactly 2 wasm slots (ptr, len)"
                );
                format!("readString(p{slot}, p{})", slot + 1)
            }
            IrType::JsValue => {
                debug_assert_eq!(
                    wasm_valtypes_for(ty).map(|v| v.len()).unwrap_or(0),
                    1,
                    "JsValue must flatten to exactly 1 wasm slot (an i32 handle)"
                );
                format!("handles.get(p{slot})")
            }
            IrType::ClosureRef {
                param_types,
                return_type,
            } => {
                // A closure crosses as its single `i32` env pointer. Wrap it
                // through the signature's factory, which retains + pins it and
                // dispatches to the exported trampoline. A signature the linear
                // glue can't marshal (a nested closure) has no factory name — fall
                // back to the throwing-thunk deferral.
                let sig = CallbackSig {
                    param_types: param_types.clone(),
                    return_type: (**return_type).clone(),
                };
                debug_assert_eq!(
                    wasm_valtypes_for(ty).map(|v| v.len()).unwrap_or(0),
                    1,
                    "a closure must flatten to exactly 1 wasm slot (its env pointer)"
                );
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
        // Slot count from the shared flattening — computed after the match so an
        // unmarshallable parameter reports the tailored reason above.
        let slots = wasm_valtypes_for(ty).map_err(|e| e.message)?.len();
        Ok((expr, slots))
    }

    fn outbound_return(&self, ty: &IrType, call: &str) -> Result<String, String> {
        Ok(match ty {
            IrType::Void => format!("{call};"),
            // Coerce through `Number` first so a host that returns a `BigInt`
            // doesn't make `Math.trunc` throw; precision above 2**53 is lost either
            // way, but the call succeeds. Guard a non-numeric result so
            // `BigInt(NaN)` doesn't throw an opaque `RangeError`.
            IrType::I64 => format!(
                "const __r = Number({call}); \
                 if (!Number.isFinite(__r)) throw new Error(\"phoenix glue: an `Int`-returning host function returned a non-numeric value\"); \
                 return BigInt(Math.trunc(__r));"
            ),
            IrType::F64 => format!("return {call};"),
            IrType::Bool => format!("return ({call}) ? 1 : 0;"),
            IrType::JsValue => format!("return handles.put({call});"),
            // String return: the host returns a JS string; the glue builds a
            // GC-managed Phoenix string in linear memory and returns its
            // `(ptr, len)` fat pointer. Guard `null`/`undefined` so the bug
            // surfaces as a clear glue error, not the literal string `"undefined"`.
            IrType::StringRef => format!(
                "const __r = {call}; \
                 if (__r == null) throw new Error(\"phoenix glue: a `String`-returning host function returned no value (null or undefined)\"); \
                 return buildString(String(__r));"
            ),
            other => return Err(format!("return type `{other}` is not marshallable")),
        })
    }

    /// Host→wasm marshalling for a callback parameter. A `String` spreads to the
    /// trampoline's `(ptr, len)` pair (two wasm args from one host argument).
    fn callback_arg_to_wasm(&self, ty: &IrType, idx: usize) -> Result<String, CompileError> {
        let a = format!("args[{idx}]");
        Ok(match ty {
            // `__intArg` applies the same lenient Int↔Number coercion the
            // extern-return arm uses, with the non-finite guard.
            IrType::I64 => format!("__intArg({a})"),
            IrType::F64 => a,
            IrType::Bool => format!("({a}) ? 1 : 0"),
            IrType::StringRef => format!("...buildString(String({a}))"),
            // Each invocation interns a fresh handle; `handles` has no free path
            // yet, so a callback fired in a loop accumulates one handle per call.
            // Tracked in known-issues.md.
            IrType::JsValue => format!("handles.put({a})"),
            other => {
                return Err(CompileError::new(format!(
                    "wasm32-linear: callback parameter type `{other}` is not \
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
            IrType::JsValue => "handles.get(__r)".to_string(),
            // Multi-value wasm result surfaces as a JS array `[ptr, len]`.
            IrType::StringRef => "readString(__r[0], __r[1])".to_string(),
            other => {
                return Err(CompileError::new(format!(
                    "wasm32-linear: callback return type `{other}` is not \
                     marshallable (internal compiler bug)"
                )));
            }
        })
    }

    fn wrap_callback(&self, inner_body: &str) -> String {
        format!("__retainCallback(ptr, (args) => {{ {inner_body} }})")
    }
}

/// The wasm32-linear instantiate-body helpers: the JsValue handle table, the
/// `phx_string_alloc`-backed string builder, the callback Int-arg coercion, and
/// the pinned/`FinalizationRegistry`-released callback retention table. Injected
/// at `/*__INSTANTIATE_HELPERS__*/` (2-space body indentation).
const LINEAR_HELPERS: &str = r#"  // Host-owned JsValue handle table (design-decisions §Phase 2.5 decision D):
  // maps an i32 handle to/from a real JS object. Handle 0 is the null sentinel —
  // `null`/`undefined` map to 0 (and `get(0)` yields `undefined`), so a wasm-side
  // `== 0` null check matches a host that returns nothing. Handles are never
  // reclaimed: each non-null `put` grows the table by one slot.
  function __makeHandles() {
    const objs = [undefined];
    return {
      put(obj) {
        if (obj === null || obj === undefined) return 0;
        const h = objs.length;
        objs.push(obj);
        return h;
      },
      get(h) { return objs[h]; },
    };
  }
  const handles = __makeHandles();
  // Set after instantiation: the exported `phx_string_alloc` (builds a GC string
  // from host bytes for a String-returning extern) and the GC pin/unpin hooks
  // (root a host-retained callback). `null` when the module exports neither.
  let stringAlloc = null;
  let gcPin = null;
  let gcUnpin = null;
  // Build a Phoenix `String` (a `(ptr, len)` fat pointer) in linear memory from a
  // JS string: allocate `phx_string_alloc(byteLen)` (returns the writable payload
  // pointer — the GC header sits just before it) and copy the UTF-8 bytes in. The
  // bytes are *copied*, never shared (decision F). The result is rooted by the
  // calling Phoenix function the instant the extern call returns.
  const buildString = (s) => {
    if (typeof stringAlloc !== "function") {
      throw new Error(
        "phoenix glue: this module did not export `phx_string_alloc`; a host " +
          "function returning a String needs it (rebuild with a current runtime)",
      );
    }
    const bytes = __encoder.encode(s);
    const ptr = stringAlloc(bytes.length);
    new Uint8Array(memory.buffer, ptr, bytes.length).set(bytes);
    return [ptr, bytes.length];
  };
  // Callback retention (design-decisions §Phase 2.5 decision G). A Phoenix closure
  // handed to a host crosses as its `i32` env pointer. `__retainCallback` wraps it
  // in a JS callable (invoking the relevant trampoline), pins the closure so a
  // host-retained callback survives a GC after the extern returns, and tracks it
  // so the *same* env pointer yields the *same* callable (stable identity, one pin
  // per live closure). `__releaseCallback` unpins it — explicitly via the
  // wrapper's `release()` or by the `FinalizationRegistry` when the wrapper is
  // collected. A callback the host never releases stays pinned for the program's
  // life (the documented, linear-only leak).
  //
  // The map holds the wrapper *weakly* (`WeakRef`): a strong reference would keep
  // the wrapper reachable forever, so the `FinalizationRegistry` could never fire
  // and the dominant reclaim path (the host simply drops the callback) would leak.
  // Each wrapper gets a fresh identity object `token = { ptr }` used as the
  // finalizer's held value, the unregister token, and the staleness key — guarding
  // env-pointer recycling after release (unregister the still-armed finalizer) and
  // re-handing a still-pinned closure whose finalizer hasn't run (reuse the pin).
  const __hasWeakRef = typeof WeakRef !== "undefined";
  const __callbacks = new Map(); // env pointer -> { ref, token }
  const __finalizers =
    typeof FinalizationRegistry !== "undefined"
      ? new FinalizationRegistry((token) => __releaseCallback(token))
      : null;
  function __releaseCallback(token) {
    const entry = __callbacks.get(token.ptr);
    // Ignore a finalizer superseded by a fresh retain of the same still-pinned
    // pointer: the newer entry (different token) owns the pin now.
    if (!entry || entry.token !== token) return;
    __callbacks.delete(token.ptr);
    if (__finalizers) __finalizers.unregister(token);
    if (typeof gcUnpin === "function") gcUnpin(token.ptr);
  }
  function __retainCallback(ptr, invoke) {
    if (ptr === 0) return null; // a null Phoenix closure crosses as 0
    const entry = __callbacks.get(ptr);
    // A live wrapper for this pointer => reuse it (stable identity, one pin). A
    // stale entry (wrapper collected, finalizer pending) falls through to a fresh
    // wrapper that reuses the still-held pin.
    const alive = entry && (__hasWeakRef ? entry.ref.deref() : entry.ref);
    if (alive) return alive;
    const fn = (...args) => invoke(args);
    const token = { ptr };
    fn.release = () => __releaseCallback(token);
    // Pin only on a genuinely fresh pointer; a stale entry is still pinned.
    if (!entry && typeof gcPin === "function") gcPin(ptr);
    __callbacks.set(ptr, { ref: __hasWeakRef ? new WeakRef(fn) : fn, token });
    if (__finalizers) __finalizers.register(fn, token, token);
    return fn;
  }"#;

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
    fn generates_thunks_and_wasi_shim() {
        let externs = vec![
            sig("alert", vec![IrType::StringRef], IrType::Void),
            sig("getLength", vec![IrType::StringRef], IrType::I64),
        ];
        let glue = generate(&externs).unwrap();
        // Shared scaffolding.
        assert!(glue.contains("export async function instantiate"));
        assert!(glue.contains("fd_write"));
        assert!(glue.contains("proc_exit"));
        // The `js` import namespace with a thunk per extern.
        assert!(glue.contains("\"js\": {"));
        assert!(glue.contains("alert(p0, p1)"));
        assert!(glue.contains("host.alert(readString(p0, p1))"));
        assert!(glue.contains("getLength(p0, p1)"));
        assert!(glue.contains("const __r = Number(host.getLength(readString(p0, p1)));"));
        assert!(glue.contains("return BigInt(Math.trunc(__r));"));
        assert!(glue.contains(r#"for (const __name of ["alert", "getLength"])"#));
    }

    #[test]
    fn no_externs_yields_an_empty_required_host_list() {
        let glue = generate(&[]).unwrap();
        assert!(glue.contains("for (const __name of [])"));
        assert!(!glue.contains("__REQUIRED_HOST__"));
    }

    #[test]
    fn jsvalue_marshals_through_handle_table() {
        let externs = vec![
            sig("getEl", vec![IrType::StringRef], IrType::JsValue),
            sig("tagOf", vec![IrType::JsValue], IrType::I64),
        ];
        let glue = generate(&externs).unwrap();
        assert!(glue.contains("return handles.put(host.getEl(readString(p0, p1)));"));
        assert!(glue.contains("host.tagOf(handles.get(p0))"));
    }

    #[test]
    fn scalar_arg_marshalling() {
        let externs = vec![sig(
            "config",
            vec![IrType::I64, IrType::F64, IrType::Bool],
            IrType::Void,
        )];
        let glue = generate(&externs).unwrap();
        assert!(glue.contains("host.config(Number(p0), p1, (p2 !== 0))"));
    }

    #[test]
    fn slot_index_advances_past_a_multi_slot_param() {
        let externs = vec![sig(
            "tag",
            vec![IrType::StringRef, IrType::I64],
            IrType::Void,
        )];
        let glue = generate(&externs).unwrap();
        assert!(glue.contains("tag(p0, p1, p2)"));
        assert!(glue.contains("host.tag(readString(p0, p1), Number(p2))"));
    }

    #[test]
    fn string_return_builds_a_gc_string_via_phx_string_alloc() {
        let externs = vec![sig("greet", vec![IrType::StringRef], IrType::StringRef)];
        let glue = generate(&externs).unwrap();
        assert!(
            glue.contains("const __r = host.greet(readString(p0, p1));")
                && glue.contains("return buildString(String(__r));"),
            "string-return thunk should build a GC string from the host result"
        );
        assert!(glue.contains("returned no value (null or undefined)"));
        assert!(glue.contains("stringAlloc(bytes.length)"));
        assert!(glue.contains(r#"for (const __name of ["greet"])"#));
    }

    #[test]
    fn callback_params_wrap_through_a_factory_and_are_required() {
        let externs = vec![
            sig(
                "onTick",
                vec![IrType::ClosureRef {
                    param_types: vec![],
                    return_type: Box::new(IrType::Void),
                }],
                IrType::Void,
            ),
            sig(
                "withValue",
                vec![IrType::ClosureRef {
                    param_types: vec![IrType::I64],
                    return_type: Box::new(IrType::Void),
                }],
                IrType::Void,
            ),
        ];
        let glue = generate(&externs).unwrap();
        assert!(glue.contains("function __cb__to_v(ptr)"));
        assert!(glue.contains("function __cb_i_to_v(ptr)"));
        assert!(glue.contains("host.onTick(__cb__to_v(p0))"));
        assert!(glue.contains("host.withValue(__cb_i_to_v(p0))"));
        assert!(
            glue.contains(r#"exports["__phoenix_invoke_closure_i_to_v"](ptr, __intArg(args[0]))"#)
        );
        assert!(glue.contains(r#"exports["__phoenix_invoke_closure__to_v"](ptr)"#));
        assert!(glue.contains("__retainCallback(ptr, (args) =>"));
        assert!(glue.contains(r#"for (const __name of ["onTick", "withValue"])"#));
    }

    #[test]
    fn callback_with_return_and_string_arg_marshals_both_directions() {
        let externs = vec![sig(
            "keep",
            vec![IrType::ClosureRef {
                param_types: vec![IrType::StringRef],
                return_type: Box::new(IrType::Bool),
            }],
            IrType::Void,
        )];
        let glue = generate(&externs).unwrap();
        assert!(glue.contains("function __cb_s_to_b(ptr)"));
        assert!(glue.contains("...buildString(String(args[0]))"));
        assert!(glue.contains("const __r = ") && glue.contains("return (__r !== 0);"));
        assert!(glue.contains("host.keep(__cb_s_to_b(p0))"));
    }

    #[test]
    fn retention_holds_wrappers_weakly_and_disarms_the_finalizer_on_release() {
        let glue = generate(&[]).unwrap();
        assert!(
            glue.contains("new WeakRef(fn)"),
            "the wrapper must be stored weakly so it can be collected and finalized"
        );
        assert!(
            glue.contains("__finalizers.register(fn, token, token)"),
            "the wrapper must be registered with a per-wrapper token"
        );
        assert!(
            glue.contains("if (__finalizers) __finalizers.unregister(token);"),
            "an explicit release must unregister the still-armed finalizer"
        );
        assert!(
            glue.contains("if (!entry && typeof gcPin === \"function\") gcPin(ptr);"),
            "a fresh pin must be taken only when there is no existing entry"
        );
    }

    #[test]
    fn callback_marshals_jsvalue_through_the_handle_table_both_directions() {
        let externs = vec![sig(
            "map",
            vec![IrType::ClosureRef {
                param_types: vec![IrType::JsValue],
                return_type: Box::new(IrType::JsValue),
            }],
            IrType::Void,
        )];
        let glue = generate(&externs).unwrap();
        assert!(glue.contains("function __cb_j_to_j(ptr)"));
        assert!(
            glue.contains(
                r#"exports["__phoenix_invoke_closure_j_to_j"](ptr, handles.put(args[0]))"#
            )
        );
        assert!(glue.contains("const __r = ") && glue.contains("return handles.get(__r);"));
        assert!(glue.contains("host.map(__cb_j_to_j(p0))"));
    }

    #[test]
    fn nested_callback_param_emits_throwing_thunk_and_is_not_required() {
        let nested = IrType::ClosureRef {
            param_types: vec![IrType::ClosureRef {
                param_types: vec![],
                return_type: Box::new(IrType::Void),
            }],
            return_type: Box::new(IrType::Void),
        };
        let externs = vec![
            sig("weird", vec![nested], IrType::Void),
            sig("alert", vec![IrType::StringRef], IrType::Void),
        ];
        let glue = generate(&externs).unwrap();
        assert!(glue.contains("weird(/* ...args */) { throw new Error("));
        assert!(glue.contains("nested closures are not supported yet"));
        assert!(glue.contains(r#"for (const __name of ["alert"])"#));
        assert!(!glue.contains("__cb_"));
    }
}
