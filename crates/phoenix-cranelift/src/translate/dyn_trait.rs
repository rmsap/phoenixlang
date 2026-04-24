//! Codegen for `dyn Trait` trait-object operations.
//!
//! ABI (also in `docs/design-decisions.md`):
//! - `DynAlloc` produces a two-slot `(data_ptr, vtable_ptr)` pair inline.
//! - `DynCall` loads `vtable[slot * 8]` and does an indirect call with
//!   `data_ptr` prepended as `self`.
//!
//! Vtables are rodata, one per `(concrete_type, trait)` pair, 8-byte
//! aligned; entries are in trait-declaration order.

use cranelift_codegen::ir::{AbiParam, InstBuilder, MemFlags, SigRef, Value};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::FunctionBuilder;
use cranelift_module::{DataDescription, DataId, Linkage, Module};

use crate::abi::build_signature;
use crate::context::CompileContext;
use crate::error::CompileError;
use crate::types::POINTER_TYPE;
use phoenix_ir::instruction::{Op, ValueId};
use phoenix_ir::module::IrModule;

use super::FuncState;
use super::layout::SLOT_SIZE;

/// Dispatch for `Op::DynAlloc` and `Op::DynCall`.
pub(super) fn translate_dyn_op(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    op: &Op,
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    match op {
        Op::DynAlloc(trait_name, concrete_type, value) => {
            let data_ptr = super::get_val1(state, *value)?;
            let vtable_id = get_or_emit_vtable(ctx, ir_module, concrete_type, trait_name)?;
            let gv = ctx.module.declare_data_in_func(vtable_id, builder.func);
            let vtable_ptr = builder.ins().global_value(POINTER_TYPE, gv);
            Ok(vec![data_ptr, vtable_ptr])
        }
        Op::UnresolvedDynAlloc(trait_name, _) => Err(CompileError::new(format!(
            "internal error: unresolved dyn-alloc coercion into `@{trait_name}` \
             reached Cranelift codegen — monomorphization was expected to \
             rewrite it to a concrete Op::DynAlloc"
        ))),
        Op::DynCall(trait_name, method_idx, receiver, args) => {
            let site = DynCallSite {
                trait_name,
                method_idx: *method_idx as usize,
                receiver: *receiver,
                args,
            };
            translate_dyn_call(builder, ctx, ir_module, state, &site)
        }
        _ => unreachable!("translate_dyn_op dispatched on non-dyn op"),
    }
}

/// Operand bundle for one `Op::DynCall` translation site.
struct DynCallSite<'a> {
    trait_name: &'a str,
    method_idx: usize,
    receiver: ValueId,
    args: &'a [ValueId],
}

/// Load `vtable[method_idx * SLOT_SIZE]` and emit an indirect call with
/// `data_ptr` prepended as `self`.
fn translate_dyn_call(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    state: &FuncState,
    site: &DynCallSite<'_>,
) -> Result<Vec<Value>, CompileError> {
    let recv_slots = super::get_val(state, site.receiver)?;
    if recv_slots.len() != 2 {
        return Err(CompileError::new(format!(
            "DynCall receiver value {:?} expected 2 slots, got {}",
            site.receiver,
            recv_slots.len()
        )));
    }
    let data_ptr = recv_slots[0];
    let vtable_ptr = recv_slots[1];

    // Vtable offset is `method_idx * SLOT_SIZE`. Cranelift's `load` takes
    // `i32`; assert the multiplication stays in range. Realistically
    // unreachable (would require ~268M trait methods) but cheap insurance
    // against a future regression that lets `method_idx` come from
    // unsanitized input.
    debug_assert!(
        site.method_idx <= (i32::MAX as usize) / SLOT_SIZE,
        "DynCall vtable offset overflow: method_idx={} would exceed i32 range \
         when multiplied by SLOT_SIZE={}",
        site.method_idx,
        SLOT_SIZE,
    );
    let offset = (site.method_idx * SLOT_SIZE) as i32;
    let fn_ptr = builder
        .ins()
        .load(POINTER_TYPE, MemFlags::trusted(), vtable_ptr, offset);

    let sig_ref = get_or_build_call_sig(ctx, ir_module, site.trait_name, site.method_idx, builder)?;

    let mut call_args = vec![data_ptr];
    for arg_vid in site.args {
        let vs = super::get_val(state, *arg_vid)?;
        call_args.extend_from_slice(&vs);
    }
    let inst = builder.ins().call_indirect(sig_ref, fn_ptr, &call_args);
    Ok(builder.inst_results(inst).to_vec())
}

/// Emit (or look up in the cache) the rodata vtable for a
/// `(concrete_type, trait_name)` pair and return its `DataId`.
///
/// The vtable is a flat array of function pointers, one per trait method,
/// in trait-declaration order.  Method-to-FuncId mapping comes from
/// [`phoenix_ir::module::IrModule::dyn_vtables`] (populated by IR lowering
/// in [`phoenix_ir::lower::LoweringContext::register_dyn_vtable`]).
fn get_or_emit_vtable(
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    concrete_type: &str,
    trait_name: &str,
) -> Result<DataId, CompileError> {
    // Cache key: two owned Strings.  A borrowed-key lookup would dodge the
    // allocation on the hit path, but (concrete, trait) pairs are created
    // a handful of times per function — this hasn't shown up as hot.
    // Revisit with `Borrow` or a `(Rc<str>, Rc<str>)` key if it does.
    let key = (concrete_type.to_string(), trait_name.to_string());
    if let Some(id) = ctx.dyn_vtable_cache.get(&key) {
        return Ok(*id);
    }

    let entries = ir_module.dyn_vtables.get(&key).ok_or_else(|| {
        CompileError::new(format!(
            "vtable for `{concrete_type}` as `dyn {trait_name}` not found in \
             IrModule::dyn_vtables — IR lowering must register the vtable \
             before DynAlloc reaches the backend."
        ))
    })?;

    let vtable_len_bytes = entries.len() * SLOT_SIZE;
    let mut data_desc = DataDescription::new();
    data_desc.define(vec![0u8; vtable_len_bytes].into_boxed_slice());
    // Pointer-sized loads with MemFlags::trusted() require pointer alignment
    // on the rodata segment — object-file default alignment is not enough.
    data_desc.set_align(SLOT_SIZE as u64);

    for (i, (_method_name, phx_fid)) in entries.iter().enumerate() {
        let cl_fid = ctx.func_ids.get(phx_fid).copied().ok_or_else(|| {
            CompileError::new(format!(
                "vtable for `{concrete_type}` as `dyn {trait_name}`: Phoenix \
                 FuncId {phx_fid:?} missing from Cranelift func_ids map"
            ))
        })?;
        let func_ref = ctx.module.declare_func_in_data(cl_fid, &mut data_desc);
        data_desc.write_function_addr((i * SLOT_SIZE) as u32, func_ref);
    }

    let symbol_name = format!("phx_vtable_{concrete_type}__{trait_name}");
    let data_id = ctx
        .module
        .declare_data(&symbol_name, Linkage::Local, false, false)
        .map_err(CompileError::from_display)?;
    ctx.module
        .define_data(data_id, &data_desc)
        .map_err(CompileError::from_display)?;
    ctx.dyn_vtable_cache.insert(key, data_id);
    Ok(data_id)
}

/// Import a Cranelift `SigRef` for `(trait_name, method_idx)`, caching
/// the `Signature` module-wide.  `SigRef` itself is per-function and
/// cannot be cached at module scope; what we deduplicate is the cost of
/// *constructing* the `Signature` (param-type lowering, ABI normalization).
///
/// **Unavoidable clone.** `FunctionBuilder::import_signature` consumes
/// the `Signature` by value, so each call to this helper hands one fresh
/// copy to Cranelift while the cache keeps the canonical one.  Profile
/// before optimizing: trait dispatch is not a compile-time hot path, and
/// `Signature` is small (a `Vec<AbiParam>` and a `CallConv`).
///
/// Prepends a pointer-typed receiver slot — the `data_ptr` half of the
/// `dyn` fat pointer — to the trait method's declared params, because
/// trait-method signatures from `IrModule::traits` exclude `self`
/// whereas the Cranelift call pushes `data_ptr` as the first arg.
fn get_or_build_call_sig(
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    trait_name: &str,
    method_idx: usize,
    builder: &mut FunctionBuilder,
) -> Result<SigRef, CompileError> {
    let key = (trait_name.to_string(), method_idx);
    if !ctx.dyn_call_sig_cache.contains_key(&key) {
        let (params, ret) = ir_module
            .trait_method_signature(trait_name, method_idx)
            .ok_or_else(|| {
                CompileError::new(format!(
                    "no IR trait metadata for dyn {trait_name} slot {method_idx} — \
                     trait is missing or non-object-safe"
                ))
            })?;
        let sig = build_dyn_call_signature(params, ret, ctx.call_conv);
        ctx.dyn_call_sig_cache.insert(key.clone(), sig);
    }
    Ok(builder
        .func
        .import_signature(ctx.dyn_call_sig_cache[&key].clone()))
}

/// Build a Cranelift [`cranelift_codegen::ir::Signature`] for a `dyn`
/// method dispatch: a pointer-typed receiver slot, then the trait
/// method's IR-lowered params, then the return type.
///
/// **Cross-backend ABI contract.** Every `Op::DynCall` dispatch must
/// prepend the concrete receiver as the first argument, matching the
/// target function's `self: StructRef/EnumRef(ConcreteType)` parameter
/// at index 0. The IR interpreter does the same in
/// `phoenix-ir-interp/src/interpreter.rs::interpret_dyn_call`, and the
/// IR-level trait signature is read from the same
/// [`IrModule::trait_method_signature`] entry point. If this convention
/// ever diverges (one backend prepends, the other not — or prepends a
/// different type), every vtable call silently miscompiles with no
/// verifier signal, because the trait-method `FuncId` is shared. Pin
/// both sites together when changing.
fn build_dyn_call_signature(
    params: &[phoenix_ir::types::IrType],
    ret: &phoenix_ir::types::IrType,
    call_conv: CallConv,
) -> cranelift_codegen::ir::Signature {
    let mut sig = build_signature(params, ret, call_conv);
    sig.params.insert(0, AbiParam::new(POINTER_TYPE));
    sig
}

#[cfg(test)]
mod invariants {
    use super::{POINTER_TYPE, SLOT_SIZE};

    /// `SLOT_SIZE` is what rodata vtables align to and what `DynCall` uses to
    /// compute per-method offsets — it must equal `POINTER_TYPE.bytes()` or
    /// vtable loads under-align (or over-align harmlessly but wastefully).
    /// Tracked as a test rather than a `const` assertion because
    /// `cranelift_codegen::ir::Type::bytes()` is not `const fn`.
    #[test]
    fn slot_size_matches_pointer_type() {
        assert_eq!(
            POINTER_TYPE.bytes() as usize,
            SLOT_SIZE,
            "POINTER_TYPE ({} bytes) and SLOT_SIZE ({}) disagree — dyn vtable \
             alignment / offset arithmetic would silently drift",
            POINTER_TYPE.bytes(),
            SLOT_SIZE
        );
    }
}
