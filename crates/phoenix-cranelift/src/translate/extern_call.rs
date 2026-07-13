//! Native (Cranelift) binding of the `extern js` host-FFI boundary.
//!
//! `extern js` is a uniform host-FFI boundary (design-decisions §Phase 2.5
//! decision A0): the generic `Op::ExternCall` is bound per backend. The native
//! binding (decision E) lowers each distinct extern `(module, name)` to a call of
//! a C-ABI symbol `phx_extern_<module>__<name>` (the module half escaped to a C
//! identifier — see [`extern_symbol`]) carrying the native value ABI
//! (`i64`/`f64`/`i8`/string-fat-pointer; `JsValue` → an opaque `i64` host handle).
//! The compiler emits the call and the symbol reference; a linked **host shim**
//! provides the body.
//!
//! **Default when no host shim is linked.** For each distinct called extern the
//! compiler emits a *weak* (`Linkage::Preemptible`) default definition of the
//! symbol whose body calls [`phx_extern_unbound`](phoenix_runtime) — which aborts
//! naming the missing `(module, name)`. A host that links a **strong** definition
//! of `phx_extern_<m>__<n>` overrides the weak default: for the static link this
//! backend drives (`cc app.o shim.o libphoenix_runtime.a`) the override is the
//! plain strong-beats-weak rule resolved at link time; in a dynamically linked
//! image the same override happens via PLT interposition at load time (the native
//! backend builds position-independent code, so a call to a preemptible symbol
//! routes through the PLT). The result: an interop program *links and runs* with
//! no host (the §A0 "clear runtime error, never a silent no-op" — it aborts the
//! instant it actually calls the unbound extern), and links cleanly against a host
//! shim that overrides the defaults.
//!
//! **Platform note.** Weak-symbol override (strong-beats-weak at static link, PLT
//! interposition when dynamic) is the ELF/Mach-O model. On Windows/COFF weak
//! symbols and interposition differ; native interop there is out of scope for this
//! phase (the wasm32 + interpreter bindings cover Windows hosts).

use std::collections::HashMap;

use cranelift_codegen::Context;
use cranelift_codegen::ir::{AbiParam, InstBuilder, MemFlags, Signature, Value, types as cl};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{DataDescription, DataId, Linkage, Module};
use cranelift_object::ObjectModule;

use crate::abi::build_signature;
use crate::context::CompileContext;
use crate::error::CompileError;
use crate::extern_abi::{CallbackSig, ExternSig, callback_sig_codes, callback_sigs_in_externs};
use crate::translate::layout::TypeLayout;
use crate::types::POINTER_TYPE;
use phoenix_ir::instruction::ValueId;

use super::{FuncState, get_val};

/// The C-ABI symbol a Phoenix `extern <module>` function `name` lowers to. A
/// host shim defines this; the compiler emits a weak default. The `__` separator
/// matches the function-name mangling (`TypeName.method` → `TypeName__method`),
/// so `extern js`'s `alert` is `phx_extern_js__alert`.
///
/// The module half is escaped ([`mangle_module`]) so an npm package specifier
/// (`extern js "pkg" { ... }`) — whose `-`/`@`/`/` are not valid in
/// a C identifier — still yields a symbol a host shim can define from plain C:
/// `("left-pad", "leftPad")` → `phx_extern_left_2dpad__leftPad`. The ambient
/// `js` module contains no escapable characters, so Phase 2.5 ambient symbols
/// are byte-identical to before.
pub(crate) fn extern_symbol(module: &str, name: &str) -> String {
    format!("phx_extern_{}__{name}", mangle_module(module))
}

/// Escape a host-module name into C-identifier-safe form: ASCII alphanumerics
/// pass through; every other byte (including `_`) becomes `_xx` (two lowercase
/// hex digits). The encoding is injective, and because an `_` in the output is
/// always followed by two hex digits, the escaped module can never contain `__`
/// or end in `_` — so the `__` separator in [`extern_symbol`] stays unambiguous
/// even against an extern *name* that itself contains `__`.
fn mangle_module(module: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(module.len());
    for b in module.bytes() {
        if b.is_ascii_alphanumeric() {
            out.push(b as char);
        } else {
            out.push('_');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xf) as usize] as char);
        }
    }
    out
}

/// Declare every called `extern js` function as a weak C-ABI symbol with a
/// default body that aborts via `phx_extern_unbound`, and record its `FuncId` in
/// [`CompileContext::extern_funcs`]. Must run **before** function translation so
/// an `Op::ExternCall` site can resolve its target symbol. A no-op for an empty
/// `externs` slice (a program with no externs), so a non-interop binary's symbol
/// surface is unchanged. The caller collects `externs` once
/// ([`collect_externs`](crate::extern_abi::collect_externs)) and shares it with
/// [`declare_closure_trampolines`].
pub(crate) fn declare_extern_shims(
    ctx: &mut CompileContext,
    externs: &[ExternSig],
) -> Result<(), CompileError> {
    if externs.is_empty() {
        return Ok(());
    }

    // The runtime abort helper, shared by every default shim body:
    // `phx_extern_unbound(module_ptr, module_len, name_ptr, name_len,
    // symbol_ptr, symbol_len) -> !`. All six args are pointer-width (`usize`
    // lengths lower to `i64` on 64-bit). The symbol travels alongside the raw
    // `(module, name)` because only the compiler knows the escaped mangling
    // ([`extern_symbol`]) — the runtime must not re-derive it.
    let mut unbound_sig = Signature::new(ctx.call_conv);
    for _ in 0..6 {
        unbound_sig.params.push(AbiParam::new(POINTER_TYPE));
    }
    let unbound_id =
        ctx.module
            .declare_function("phx_extern_unbound", Linkage::Import, &unbound_sig)?;

    // The `(module, name, symbol)` strings the shim bodies pass to
    // `phx_extern_unbound` are emitted as rodata. Module names repeat across
    // externs (every ambient `extern js` function shares `"js"`), so cache the
    // `DataId` by content to emit each distinct string once.
    let mut rodata_cache: HashMap<String, DataId> = HashMap::new();

    for sig in externs {
        let symbol = extern_symbol(&sig.module, &sig.name);
        let cl_sig = build_signature(&sig.params, &sig.return_type, ctx.call_conv);
        // `Preemptible` emits a *weak* symbol (cranelift-object maps it to a weak
        // binding) that a strong host definition overrides at link time.
        let func_id = ctx
            .module
            .declare_function(&symbol, Linkage::Preemptible, &cl_sig)?;
        define_unbound_shim(
            ctx,
            func_id,
            &cl_sig,
            &sig.module,
            &sig.name,
            &symbol,
            unbound_id,
            &mut rodata_cache,
        )?;
        ctx.extern_funcs
            .entry(sig.module.clone())
            .or_default()
            .insert(sig.name.clone(), func_id);
    }
    Ok(())
}

/// Define the weak default body of one extern symbol: call `phx_extern_unbound`
/// with the `(module, name, symbol)` strings (from rodata), then terminate. The
/// helper never returns (it aborts the process), so the trailing `return` of
/// zeroed values is unreachable — it exists only to give Cranelift a well-typed
/// terminator without depending on a specific trap-code API.
#[allow(clippy::too_many_arguments)]
fn define_unbound_shim(
    ctx: &mut CompileContext,
    func_id: cranelift_module::FuncId,
    cl_sig: &Signature,
    module: &str,
    name: &str,
    symbol: &str,
    unbound_id: cranelift_module::FuncId,
    rodata_cache: &mut HashMap<String, DataId>,
) -> Result<(), CompileError> {
    let mut cl_ctx = Context::new();
    cl_ctx.func.signature = cl_sig.clone();

    let mut fb_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut cl_ctx.func, &mut fb_ctx);

    let block = builder.create_block();
    builder.append_block_params_for_function_params(block);
    builder.switch_to_block(block);
    builder.seal_block(block);

    let (m_ptr, m_len) = emit_rodata_str(&mut ctx.module, &mut builder, module, rodata_cache)?;
    let (n_ptr, n_len) = emit_rodata_str(&mut ctx.module, &mut builder, name, rodata_cache)?;
    let (s_ptr, s_len) = emit_rodata_str(&mut ctx.module, &mut builder, symbol, rodata_cache)?;
    let unbound_ref = ctx.module.declare_func_in_func(unbound_id, builder.func);
    builder
        .ins()
        .call(unbound_ref, &[m_ptr, m_len, n_ptr, n_len, s_ptr, s_len]);

    // Unreachable (the call above diverges), but the block needs a terminator
    // matching the signature's returns. Zeroed values of each return type.
    let rets: Vec<Value> = cl_sig
        .returns
        .iter()
        .map(|abi| zero_value(&mut builder, abi.value_type))
        .collect();
    builder.ins().return_(&rets);
    builder.finalize();

    ctx.module
        .define_function(func_id, &mut cl_ctx)
        .map_err(|e| {
            CompileError::new(format!(
                "failed to define default `extern {module}` shim for `{name}`: {e}"
            ))
        })?;
    Ok(())
}

/// Materialize `s` as a rodata constant and return its `(ptr, len)` fat pointer
/// in the current function — the same shape `Op::ConstString` produces. Distinct
/// strings are emitted once and reused via `cache` (keyed by content), so a
/// module name shared by many externs costs one rodata constant, not one per
/// extern.
fn emit_rodata_str(
    module: &mut ObjectModule,
    builder: &mut FunctionBuilder,
    s: &str,
    cache: &mut HashMap<String, DataId>,
) -> Result<(Value, Value), CompileError> {
    let data_id = match cache.get(s) {
        Some(&id) => id,
        None => {
            let id = module.declare_anonymous_data(false, false).map_err(|e| {
                CompileError::new(format!(
                    "failed to declare `extern js` rodata string {s:?}: {e}"
                ))
            })?;
            let mut desc = DataDescription::new();
            desc.define(s.as_bytes().to_vec().into_boxed_slice());
            module.define_data(id, &desc).map_err(|e| {
                CompileError::new(format!(
                    "failed to define `extern js` rodata string {s:?}: {e}"
                ))
            })?;
            cache.insert(s.to_string(), id);
            id
        }
    };
    let gv = module.declare_data_in_func(data_id, builder.func);
    let ptr = builder.ins().global_value(POINTER_TYPE, gv);
    // The length is the 64-bit `usize` lowering — it must match the
    // `POINTER_TYPE` length params of `unbound_sig` (native is 64-bit, so
    // `POINTER_TYPE == I64`; the same assumption the rest of the backend makes).
    let len = builder.ins().iconst(cl::I64, s.len() as i64);
    Ok((ptr, len))
}

/// A zero constant of Cranelift type `ty`, used to fill the unreachable return of
/// a default shim. Covers the float and integer types the native value ABI uses
/// (`f64` for `Float`; `i8`/`i64` for the scalars and pointers); `f32` is handled
/// defensively for completeness even though the native ABI never produces it.
fn zero_value(builder: &mut FunctionBuilder, ty: cl::Type) -> Value {
    if ty == cl::F64 {
        builder.ins().f64const(0.0)
    } else if ty == cl::F32 {
        builder.ins().f32const(0.0)
    } else {
        builder.ins().iconst(ty, 0)
    }
}

/// The exported C-ABI name of the `call_indirect` trampoline for a native
/// callback signature: `phx_invoke_closure_<param-codes>_to_<ret-code>`, derived
/// from the shared [`callback_sig_codes`] (the WASM glue formats the same codes
/// as `__phoenix_invoke_closure_*`). `None` for a non-marshallable signature.
fn native_invoke_closure_name(sig: &CallbackSig) -> Option<String> {
    callback_sig_codes(sig).map(|(params, ret)| format!("phx_invoke_closure_{params}_to_{ret}"))
}

/// Export one `call_indirect` trampoline per distinct callback signature handed
/// to a host (Phase 2.5 decision G — the native binding). A Phoenix closure
/// crosses to the host shim as its `i64` env pointer (a heap pointer; the target
/// function pointer lives at `env[0]`). The shim invokes the closure by calling
/// `phx_invoke_closure_<sig>(env, args…)`, which reloads the function pointer and
/// dispatches — the standalone-function form of [`super::closure_call`]'s
/// indirect call. Retention of a host-held callback is the shim's responsibility,
/// mirroring the linear contract: it must keep the env pointer rooted via
/// `phx_gc_pin` / `phx_gc_unpin` while retained (a synchronous callback is
/// already rooted by the calling frame's shadow stack and needs no pin).
///
/// No-op for a program that hands no closures to a host. Takes the same
/// `externs` slice [`declare_extern_shims`] does (the callback signatures are the
/// `ClosureRef` parameters among them), so the module is scanned once for both.
pub(crate) fn declare_closure_trampolines(
    ctx: &mut CompileContext,
    externs: &[ExternSig],
) -> Result<(), CompileError> {
    for cb in &callback_sigs_in_externs(externs) {
        let name = native_invoke_closure_name(cb).ok_or_else(|| {
            CompileError::new(
                "a callback signature reached native trampoline declaration without \
                 a marshalling name (internal compiler bug — \
                 `callback_sigs_in_externs` should only yield marshallable \
                 signatures)"
                    .to_string(),
            )
        })?;
        let sig = closure_trampoline_signature(ctx, cb);
        let func_id = ctx.module.declare_function(&name, Linkage::Export, &sig)?;
        define_closure_trampoline(ctx, func_id, &sig)?;
    }
    Ok(())
}

/// The trampoline signature `(env: ptr, user-params…) -> ret` — the closure's
/// own signature with an `i64` env pointer prepended. Identical to the type the
/// in-program indirect call uses, so the `call_indirect` target type matches the
/// closure functions in memory.
fn closure_trampoline_signature(ctx: &CompileContext, cb: &CallbackSig) -> Signature {
    let mut sig = Signature::new(ctx.call_conv);
    sig.params.push(AbiParam::new(POINTER_TYPE)); // env pointer
    for pt in &cb.param_types {
        for &clt in TypeLayout::of(pt).cl_types() {
            sig.params.push(AbiParam::new(clt));
        }
    }
    for &clt in TypeLayout::of(&cb.return_type).cl_types() {
        sig.returns.push(AbiParam::new(clt));
    }
    sig
}

/// Define a callback trampoline body: load the closure's function pointer from
/// `env[0]` and `call_indirect` through it with `(env, user-args…)`, returning
/// the result. Mirrors [`super::closure_call`]'s env-pointer ABI.
fn define_closure_trampoline(
    ctx: &mut CompileContext,
    func_id: cranelift_module::FuncId,
    sig: &Signature,
) -> Result<(), CompileError> {
    let mut cl_ctx = Context::new();
    cl_ctx.func.signature = sig.clone();

    let mut fb_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut cl_ctx.func, &mut fb_ctx);

    let block = builder.create_block();
    builder.append_block_params_for_function_params(block);
    builder.switch_to_block(block);
    builder.seal_block(block);

    // Block params are exactly the call_indirect args: `(env, user-args…)`. The
    // env pointer (param 0) is both the first call argument and where the target
    // function pointer is stored (`env[0]`).
    let call_args = builder.block_params(block).to_vec();
    let env = call_args[0];
    let fn_ptr = builder.ins().load(POINTER_TYPE, MemFlags::new(), env, 0);
    let sig_ref = builder.import_signature(sig.clone());
    let call = builder.ins().call_indirect(sig_ref, fn_ptr, &call_args);
    let results = builder.inst_results(call).to_vec();
    builder.ins().return_(&results);
    builder.finalize();

    ctx.module
        .define_function(func_id, &mut cl_ctx)
        .map_err(|e| CompileError::new(format!("failed to define callback trampoline: {e}")))?;
    Ok(())
}

/// Lower an `Op::ExternCall` to a call of its weak/overridable C-ABI symbol. The
/// `FuncId` (and thus the signature) was fixed by [`declare_extern_shims`]; this
/// just forwards the marshalled argument values and returns the results, exactly
/// like a direct `Op::Call`.
pub(super) fn translate_extern_call(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    module: &str,
    name: &str,
    args: &[ValueId],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let func_id = *ctx
        .extern_funcs
        .get(module)
        .and_then(|by_name| by_name.get(name))
        .ok_or_else(|| {
            CompileError::new(format!(
                "internal error: `extern js` call `{module}.{name}` reached native \
                 codegen with no declared shim — `declare_extern_shims` must run \
                 before function translation"
            ))
        })?;
    let func_ref = ctx.module.declare_func_in_func(func_id, builder.func);
    let mut cl_args = Vec::new();
    for arg in args {
        cl_args.extend(get_val(state, *arg)?);
    }
    let call = builder.ins().call(func_ref, &cl_args);
    Ok(builder.inst_results(call).to_vec())
}

#[cfg(test)]
mod tests {
    use super::extern_symbol;

    #[test]
    fn ambient_symbols_are_unchanged() {
        // The Phase 2.5 host-shim contract: hosts already define these exact
        // symbols, so the npm-module escaping must leave them byte-identical.
        assert_eq!(extern_symbol("js", "alert"), "phx_extern_js__alert");
        assert_eq!(
            extern_symbol("js", "get_length"),
            "phx_extern_js__get_length"
        );
    }

    #[test]
    fn npm_specifiers_escape_to_c_identifiers() {
        assert_eq!(
            extern_symbol("left-pad", "leftPad"),
            "phx_extern_left_2dpad__leftPad"
        );
        assert_eq!(
            extern_symbol("@scope/pkg", "f"),
            "phx_extern__40scope_2fpkg__f"
        );
    }

    #[test]
    fn escaping_keeps_distinct_pairs_distinct() {
        // The classic `__`-separator ambiguity: without escaping, ("a__b", "c")
        // and ("a", "b__c") would mangle identically. Escaping `_` in the module
        // keeps the first `__` an unambiguous separator.
        assert_ne!(extern_symbol("a__b", "c"), extern_symbol("a", "b__c"));
        // And a raw module that happens to spell an escaped form stays distinct.
        assert_ne!(
            extern_symbol("left-pad", "f"),
            extern_symbol("left_2dpad", "f")
        );
    }
}
