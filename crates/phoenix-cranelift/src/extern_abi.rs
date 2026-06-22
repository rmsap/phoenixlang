//! Backend-neutral analysis of the `extern js` host-FFI boundary.
//!
//! `extern js` is a *uniform* host-FFI boundary (design-decisions §Phase 2.5
//! decision A0): the generic `Op::ExternCall` host-call node is shared, and each
//! backend binds it to its host differently. The pieces here are pure IR
//! analysis — collecting the distinct called externs and the closure (callback)
//! signatures that cross the boundary, plus the stable per-type marshalling codes
//! — and are consumed by *both* WASM bindings (the import section + JS glue,
//! `super::wasm`) and the native binding (the C-ABI host shim, `super::translate`).
//! Keeping them here means the two backends derive the same extern table and the
//! same callback-signature set from the same code, so the bindings can't drift.
//!
//! `super::wasm::translate` re-exports these for its existing call sites; the
//! native backend imports them directly.

use std::collections::HashSet;

use phoenix_ir::instruction::Op;
use phoenix_ir::types::IrType;

use crate::error::CompileError;

/// The IR-level signature of a *called* `extern js` function, derived from its
/// call site. Consumed by every backend's binding — the WASM import/glue
/// generators flatten it to a WASM signature, the native binding flattens it to
/// a C-ABI signature — so the call and its host binding can never drift.
pub(crate) struct ExternSig {
    /// The host module (`"js"` today).
    pub(crate) module: String,
    /// The host function name.
    pub(crate) name: String,
    /// Parameter IR types, in call order.
    pub(crate) params: Vec<IrType>,
    /// Return IR type (`Void` for a no-result extern).
    pub(crate) return_type: IrType,
}

/// Collect the distinct *called* `extern js` functions from every concrete
/// function, deriving each one's signature from its first `Op::ExternCall` site.
///
/// Only externs actually *called* appear: a declared-but-uncalled extern emits
/// no `Op::ExternCall`. The signature is taken from the first call site; sema
/// coerces every argument to the declared parameter type and rejects
/// non-marshallable types, so every site of a given extern agrees and matches
/// the declaration. The IR carries no extern declaration table — only
/// `Op::ExternCall` sites — so call-site derivation is the only signal here.
pub(crate) fn collect_externs(
    ir_module: &phoenix_ir::module::IrModule,
) -> Result<Vec<ExternSig>, CompileError> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut out = Vec::new();
    for func in ir_module.concrete_functions() {
        for block in &func.blocks {
            for instr in &block.instructions {
                let Op::ExternCall(module, name, args) = &instr.op else {
                    continue;
                };
                if !seen.insert((module.clone(), name.clone())) {
                    continue;
                }
                let mut params = Vec::with_capacity(args.len());
                for arg in args {
                    let ty = func.instruction_result_type(*arg).ok_or_else(|| {
                        CompileError::new(format!(
                            "`extern js` call `{module}.{name}` argument {arg:?} has no \
                             recorded IR type (internal compiler bug)"
                        ))
                    })?;
                    params.push(ty.clone());
                }
                out.push(ExternSig {
                    module: module.clone(),
                    name: name.clone(),
                    params,
                    return_type: instr.result_type.clone(),
                });
            }
        }
    }
    Ok(out)
}

/// A distinct closure (callback) signature that crosses the `extern js`
/// boundary as a parameter — i.e. a Phoenix closure handed to a host function.
/// Each backend exports one `call_indirect` trampoline per distinct signature
/// that its host binding invokes to call the closure; the trampoline names are
/// derived from the same [`callback_sig_codes`], so a host binding can't name a
/// trampoline the backend didn't emit.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CallbackSig {
    /// The closure's own parameter types (the values the host passes when it
    /// invokes the callback). Marshalled host→Phoenix at the boundary.
    pub(crate) param_types: Vec<IrType>,
    /// The closure's return type (the value the callback hands back to the
    /// host). Marshalled Phoenix→host. `Void` for a no-result callback.
    pub(crate) return_type: IrType,
}

/// The single-character marshalling code for a type that can cross the `extern
/// js` boundary, or `None` for a type the callback bindings do not marshal
/// (today: a *nested* closure parameter/return, or any non-marshallable type
/// sema should already have rejected at the extern signature). The codes are
/// stable — they appear in generated trampoline symbol names — so a new
/// marshallable type must append a fresh code, never reuse one.
fn marshall_code(ty: &IrType) -> Option<char> {
    match ty {
        IrType::I64 => Some('i'),
        IrType::F64 => Some('f'),
        IrType::Bool => Some('b'),
        IrType::StringRef => Some('s'),
        IrType::JsValue => Some('j'),
        IrType::Void => Some('v'),
        _ => None,
    }
}

/// `true` iff the callback bindings can marshal this signature end-to-end —
/// every parameter and the return type has a [`marshall_code`], and no parameter
/// is `Void` (a `Void` parameter is meaningless). A *nested* closure (a callback
/// whose own parameter/return is itself a closure) is **not** supported in this
/// phase: it has no code, so the predicate rejects it and the owning extern falls
/// back to the deferral path (a throwing WASM thunk / no native trampoline).
/// Shared by the trampoline emitters and the host bindings so they agree on
/// exactly which callbacks get a real trampoline.
///
/// Defined as "[`callback_sig_codes`] succeeds": a signature is glue-supported
/// exactly when codes can be derived for it, so the two share one source of truth
/// rather than re-deriving the same param/return predicate independently.
pub(crate) fn callback_sig_is_glue_supported(sig: &CallbackSig) -> bool {
    callback_sig_codes(sig).is_some()
}

/// The `(parameter codes, return code)` of a callback signature, or `None` if it
/// is not marshallable ([`callback_sig_is_glue_supported`] is false). Each
/// backend formats its own trampoline symbol name from these (the WASM glue uses
/// `__phoenix_invoke_closure_<p>_to_<r>`, the native shim `phx_invoke_closure_<p>_to_<r>`),
/// so coincident signatures across a module collapse to one trampoline and the
/// emitter and host binding always agree on the name.
pub(crate) fn callback_sig_codes(sig: &CallbackSig) -> Option<(String, char)> {
    let mut params = String::with_capacity(sig.param_types.len());
    for ty in &sig.param_types {
        match marshall_code(ty) {
            // A `Void` parameter (code 'v') is meaningless, and a `None` code is
            // a non-marshallable (nested-closure) parameter — both reject.
            Some('v') | None => return None,
            Some(c) => params.push(c),
        }
    }
    let ret = marshall_code(&sig.return_type)?;
    Some((params, ret))
}

/// The exported name of the WASM `call_indirect`/`call_ref` trampoline for a
/// callback signature: `__phoenix_invoke_closure_<param-codes>_to_<ret-code>`
/// (e.g. every `(Int) -> Void` callback routes through
/// `__phoenix_invoke_closure_i_to_v`). Shared by **both** WASM backends — the
/// linear binding (which `call_indirect`s through the closure's env pointer) and
/// the wasm32-gc binding (which `call_ref`s the closure's funcref) — so a single
/// generated JS glue references the same name on either target. The native shim
/// uses a distinct `phx_invoke_closure_*` name (a C symbol, not a JS export).
/// `None` for a non-marshallable signature.
pub(crate) fn wasm_closure_trampoline_name(sig: &CallbackSig) -> Option<String> {
    callback_sig_codes(sig)
        .map(|(params, ret)| format!("__phoenix_invoke_closure_{params}_to_{ret}"))
}

/// The distinct, marshallable callback signatures among a set of already-
/// collected externs — every `ClosureRef`-typed parameter, deduped by structural
/// signature and filtered to [`callback_sig_is_glue_supported`], in first-seen
/// order. Shared by [`collect_callback_signatures`] (which feeds a backend's
/// trampoline emitter) and the WASM glue (which builds one factory per
/// signature), so the two derive the same set in the same order.
pub(crate) fn callback_sigs_in_externs(externs: &[ExternSig]) -> Vec<CallbackSig> {
    let mut out: Vec<CallbackSig> = Vec::new();
    for ext in externs {
        for param in &ext.params {
            let IrType::ClosureRef {
                param_types,
                return_type,
            } = param
            else {
                continue;
            };
            let sig = CallbackSig {
                param_types: param_types.clone(),
                return_type: (**return_type).clone(),
            };
            if callback_sig_is_glue_supported(&sig) && !out.contains(&sig) {
                out.push(sig);
            }
        }
    }
    out
}

/// Collect the distinct, marshallable closure signatures that cross the `extern
/// js` boundary as call arguments across the whole module — the convenience
/// composition of [`collect_externs`] + [`callback_sigs_in_externs`].
pub(crate) fn collect_callback_signatures(
    ir_module: &phoenix_ir::module::IrModule,
) -> Result<Vec<CallbackSig>, CompileError> {
    Ok(callback_sigs_in_externs(&collect_externs(ir_module)?))
}
