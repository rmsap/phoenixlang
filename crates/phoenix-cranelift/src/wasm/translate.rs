//! Per-function Phoenix IR → WebAssembly translation.
//!
//! PR 2 scope: enough IR ops to compile `tests/fixtures/hello.phx`
//! (`let x: Int = 42; print(x)`) end-to-end. The minimal op surface is:
//!
//! - [`Op::ConstI64`], [`Op::ConstBool`] — push primitive constants.
//! - [`Op::BuiltinCall`] with name `"print"` and a single primitive
//!   argument — routed to the synthesized `phx_print_*` runtime.
//! - [`Terminator::Return`] with no value — emit the function epilogue.
//!
//! Every other IR op produces a clean [`CompileError`] pointing at PR 3,
//! where the linear-memory `MarkSweepHeap` port comes with full IR
//! coverage (arith, control flow, struct/list/map alloc, etc.).
//!
//! # SSA → WASM-locals mapping
//!
//! WebAssembly's MVP has no SSA — it has typed locals and an operand
//! stack. Each Phoenix `ValueId` that an instruction defines becomes a
//! WASM local of the corresponding [`ValType`]. Phoenix function
//! parameters bind to WASM locals at the same index (WASM auto-declares
//! locals `[0, n_params)` for the parameter slots).
//!
//! # wasm-encoder construction order
//!
//! [`wasm_encoder::Function`] takes its local declarations up front,
//! before any instruction can be pushed. We therefore buffer
//! instructions and the locals list during the IR walk, then finalize
//! into a `Function` at the end. The buffer holds `Instruction<'static>`
//! — all PR-2 ops own their data, so there's no borrow churn.

use std::collections::HashMap;

use phoenix_ir::block::BasicBlock;
use phoenix_ir::instruction::{Op, ValueId};
use phoenix_ir::module::IrFunction;
use phoenix_ir::terminator::Terminator;
use phoenix_ir::types::IrType;
use wasm_encoder::{Function, Instruction, ValType};

use super::module_builder::ModuleBuilder;
use crate::error::CompileError;

/// Map a Phoenix [`IrType`] to the single WASM [`ValType`] used to
/// represent it. Returns an error for types whose representation
/// requires more than one slot (e.g. `StringRef`'s `(ptr, len)`
/// fat pointer) — those will need a multi-slot scheme in PR 3.
///
/// `IrType::Void` is rejected: `Void` is the absence of a value, not
/// a value of any slot type. Callers wanting return-position
/// handling should go through [`wasm_return_valtypes`] instead.
pub(super) fn wasm_valtype_for(ty: &IrType) -> Result<ValType, CompileError> {
    match ty {
        IrType::I64 => Ok(ValType::I64),
        IrType::F64 => Ok(ValType::F64),
        IrType::Bool => Ok(ValType::I32),
        IrType::Void => Err(CompileError::new(
            "wasm32-linear: `Void` has no WASM value representation \
             (internal: callers must route returns through \
             `wasm_return_valtypes`)"
                .to_string(),
        )),
        _ => Err(unsupported(ty, "wasm32-linear value representation")),
    }
}

/// Map a Phoenix function's return [`IrType`] to a vector of WASM
/// [`ValType`]s. `Void` returns map to the empty vector.
pub(super) fn wasm_return_valtypes(ty: &IrType) -> Result<Vec<ValType>, CompileError> {
    match ty {
        IrType::Void => Ok(Vec::new()),
        other => Ok(vec![wasm_valtype_for(other)?]),
    }
}

/// Translate a `wasmparser::ValType` into the corresponding
/// `wasm_encoder::ValType`. Used by the runtime-merge step
/// (`super::runtime_merge`) when re-encoding type-section entries
/// from the pre-compiled `phoenix_runtime.wasm`. Rejects ref types
/// the runtime shouldn't be producing on wasm32-wasip1 today.
pub(super) fn wasm_valtype_from_parser(ty: wasmparser::ValType) -> Result<ValType, CompileError> {
    match ty {
        wasmparser::ValType::I32 => Ok(ValType::I32),
        wasmparser::ValType::I64 => Ok(ValType::I64),
        wasmparser::ValType::F32 => Ok(ValType::F32),
        wasmparser::ValType::F64 => Ok(ValType::F64),
        wasmparser::ValType::V128 => Ok(ValType::V128),
        wasmparser::ValType::Ref(ref_ty) => {
            // Reference types (funcref, externref, the WASM-GC heap
            // types) are not expected from a wasm32-wasip1 cdylib. If
            // a future Rust toolchain emits them (closures backed by
            // ref-types?), the diagnostic points at this site.
            Err(CompileError::new(format!(
                "wasm32-linear: runtime exposes ref-typed value (`{ref_ty:?}`); \
                 not handled by the embed-and-merge step yet"
            )))
        }
    }
}

/// Translate a concrete Phoenix function body into a complete WASM
/// [`Function`] (locals + body instructions).
pub(super) fn translate_function(
    b: &ModuleBuilder,
    func: &IrFunction,
) -> Result<Function, CompileError> {
    if func.blocks.is_empty() {
        return Err(CompileError::new(format!(
            "wasm32-linear: function `{}` has no blocks",
            func.name
        )));
    }
    if func.blocks.len() > 1 {
        return Err(CompileError::new(format!(
            "wasm32-linear: multi-block control flow in `{}` not yet supported \
             (Phase 2.4 PR 3 — see docs/design-decisions.md §Phase 2.4)",
            func.name
        )));
    }

    let mut ctx = FuncTranslateCtx::new(func)?;
    translate_block(&mut ctx, b, &func.blocks[0])?;
    Ok(ctx.into_function())
}

/// Codegen-side metadata recorded for every Phoenix `ValueId` bound
/// during translation: the WASM local slot it occupies, and the
/// original Phoenix [`IrType`] (kept around so the print-builtin
/// translator can dispatch on the IR type even when distinct IR types
/// collapse to the same WASM [`ValType`] — e.g. `Bool` and a future
/// string pointer both occupy `ValType::I32`).
struct ValueBinding {
    local: u32,
    ir_type: IrType,
}

/// Per-function translation state. Buffers instructions until
/// finalization so `wasm_encoder::Function::new` can be called with
/// the complete locals list.
struct FuncTranslateCtx {
    /// Instruction buffer. Replayed in [`Self::into_function`].
    instructions: Vec<Instruction<'static>>,
    /// Locals declared by the body, in declaration order, in the
    /// WASM run-length-encoded form `wasm_encoder::Function::new`
    /// expects: each entry is `(count, ValType)` and consecutive
    /// allocations of the same `ValType` merge into the last entry.
    /// Index `n_params + i` (where `i` is the count of locals declared
    /// before this point) is the WASM local index.
    pending_locals: Vec<(u32, ValType)>,
    /// Phoenix `ValueId` → ([`ValueBinding`]) for both parameter
    /// locals (assigned at function entry) and instruction-result
    /// locals (assigned as ops are visited).
    bindings: HashMap<ValueId, ValueBinding>,
    /// Next WASM local index to assign for an instruction-result
    /// value. Initialized past the parameter locals.
    next_local: u32,
}

impl FuncTranslateCtx {
    fn new(func: &IrFunction) -> Result<Self, CompileError> {
        let mut bindings: HashMap<ValueId, ValueBinding> = HashMap::new();
        let n_params = func.param_types.len() as u32;

        // Bind entry-block params (if any) to their parameter local
        // slots. The IR puts params on `blocks[0].params` as
        // `(ValueId, IrType)` pairs; index in that vec matches the
        // WASM parameter local index.
        //
        // Codegen assumes `entry.params.len() == func.param_types.len()`
        // — the WASM function signature is computed from
        // `param_types` while every reference site indexes into
        // `entry.params`. A mismatch would silently shift local
        // indices and emit invalid WASM. The verifier enforces this
        // upstream; this debug_assert converts a verifier regression
        // into a localized panic instead of an opaque
        // wasmparser-validation failure.
        if let Some(entry) = func.blocks.first() {
            debug_assert_eq!(
                entry.params.len(),
                func.param_types.len(),
                "wasm32-linear: entry-block param count ({}) does not match \
                 function param_types arity ({}) in `{}`",
                entry.params.len(),
                func.param_types.len(),
                func.name,
            );
            for (i, (vid, ty)) in entry.params.iter().enumerate() {
                // Validate the param fits in a single WASM `ValType`
                // slot at entry-block binding time so the error points
                // at the function's signature rather than at the first
                // instruction that uses the param. Cross-check that
                // both representation calls (here and the one in
                // `declare_phoenix_functions`) agree — they do today
                // by construction, but a future refactor that split
                // the two paths could silently shift parameter local
                // indices, and a debug assertion turns that into a
                // localized panic at codegen time rather than an
                // opaque wasmparser-validation failure.
                let entry_param_valtype = wasm_valtype_for(ty)?;
                let sig_param_valtype = wasm_valtype_for(&func.param_types[i])?;
                debug_assert_eq!(
                    entry_param_valtype, sig_param_valtype,
                    "wasm32-linear: entry-block param {i} ValType ({entry_param_valtype:?}) \
                     disagrees with function signature ValType ({sig_param_valtype:?}) in `{}`",
                    func.name,
                );
                bindings.insert(
                    *vid,
                    ValueBinding {
                        local: i as u32,
                        ir_type: ty.clone(),
                    },
                );
            }
        }

        Ok(Self {
            instructions: Vec::new(),
            pending_locals: Vec::new(),
            bindings,
            next_local: n_params,
        })
    }

    /// Allocate a fresh WASM local of `wasm_ty` for the value `vid`
    /// (recording its originating Phoenix [`IrType`] for later
    /// print-dispatch). Returns the WASM local index. Run-length-
    /// encodes consecutive same-type allocations so
    /// [`Self::into_function`] hands `wasm_encoder` the compressed
    /// locals representation directly.
    fn allocate_local(&mut self, vid: ValueId, wasm_ty: ValType, ir_ty: IrType) -> u32 {
        let idx = self.next_local;
        match self.pending_locals.last_mut() {
            Some((count, last_ty)) if *last_ty == wasm_ty => *count += 1,
            _ => self.pending_locals.push((1, wasm_ty)),
        }
        self.next_local += 1;
        self.bindings.insert(
            vid,
            ValueBinding {
                local: idx,
                ir_type: ir_ty,
            },
        );
        idx
    }

    /// Push an instruction onto the buffered body.
    fn emit(&mut self, instr: Instruction<'static>) {
        self.instructions.push(instr);
    }

    /// Look up the binding for a Phoenix `ValueId`. Errors indicate a
    /// use-before-definition, which is an IR bug — the verifier should
    /// catch this before codegen.
    fn binding_of(&self, vid: ValueId) -> Result<&ValueBinding, CompileError> {
        self.bindings.get(&vid).ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-linear: ValueId {vid:?} used before definition \
                 (internal compiler bug — IR verifier should have caught this)"
            ))
        })
    }

    /// Finalize: produce a `wasm_encoder::Function` with the
    /// accumulated locals and instruction stream. The locals list is
    /// already in the `(count, ValType)` RLE shape `wasm_encoder`
    /// expects — built up incrementally by [`Self::allocate_local`].
    fn into_function(self) -> Function {
        let mut f = Function::new(self.pending_locals);
        for instr in &self.instructions {
            f.instruction(instr);
        }
        f
    }
}

/// Translate a single basic block. PR 2 only visits block 0.
fn translate_block(
    ctx: &mut FuncTranslateCtx,
    b: &ModuleBuilder,
    block: &BasicBlock,
) -> Result<(), CompileError> {
    for instr in &block.instructions {
        translate_instruction(ctx, b, instr)?;
    }
    translate_terminator(ctx, b, &block.terminator)?;
    Ok(())
}

/// Pull the SSA result binding off an instruction. Every value-producing
/// op needs one; absence means the IR verifier let through an op that
/// would leave its result stranded on the WASM operand stack and fail
/// validation. PR 3 will add more value-producing ops with the same
/// shape — having one helper means the diagnostic stays consistent.
fn expect_result(
    instr: &phoenix_ir::instruction::Instruction,
    op_name: &str,
) -> Result<ValueId, CompileError> {
    instr.result.ok_or_else(|| {
        CompileError::new(format!(
            "wasm32-linear: `{op_name}` without a result binding would leave \
             a value stranded on the operand stack and fail validation \
             (internal compiler bug — IR verifier should have caught this)"
        ))
    })
}

/// Translate a single IR instruction.
fn translate_instruction(
    ctx: &mut FuncTranslateCtx,
    b: &ModuleBuilder,
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    match &instr.op {
        Op::ConstI64(n) => {
            let vid = expect_result(instr, "Op::ConstI64")?;
            ctx.emit(Instruction::I64Const(*n));
            let local = ctx.allocate_local(vid, ValType::I64, IrType::I64);
            ctx.emit(Instruction::LocalSet(local));
        }
        Op::ConstBool(v) => {
            let vid = expect_result(instr, "Op::ConstBool")?;
            ctx.emit(Instruction::I32Const(if *v { 1 } else { 0 }));
            let local = ctx.allocate_local(vid, ValType::I32, IrType::Bool);
            ctx.emit(Instruction::LocalSet(local));
        }
        Op::BuiltinCall(name, args) => translate_builtin_call(ctx, b, name, args)?,
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: IR op `{other:?}` not yet supported \
                 (Phase 2.4 PR 3 — see docs/design-decisions.md §Phase 2.4 \
                 for the linear-memory port's full op coverage)"
            )));
        }
    }
    Ok(())
}

/// Translate a `BuiltinCall(name, args)`. PR 2 covers `"print"` on
/// `i64` and `bool`; everything else defers to PR 3.
fn translate_builtin_call(
    ctx: &mut FuncTranslateCtx,
    b: &ModuleBuilder,
    name: &str,
    args: &[ValueId],
) -> Result<(), CompileError> {
    if name != "print" {
        return Err(CompileError::new(format!(
            "wasm32-linear: builtin `{name}` not yet supported \
             (Phase 2.4 PR 3+)"
        )));
    }
    let arg = *args.first().ok_or_else(|| {
        CompileError::new(
            "wasm32-linear: `print` builtin called with zero arguments — \
             IR verifier should have caught this"
                .to_string(),
        )
    })?;
    let binding = ctx.binding_of(arg)?;
    let arg_local = binding.local;
    // Dispatch off the original Phoenix IR type rather than the WASM
    // ValType. `bool` and string-pointer-like values both reduce to
    // `ValType::I32` once PR 3 lands `Op::ConstString`; switching on
    // `IrType` keeps us from silently routing strings to
    // `phx_print_bool`.
    let arg_ir_ty = binding.ir_type.clone();
    match arg_ir_ty {
        IrType::I64 => {
            let idx = b.require_phx_func("phx_print_i64")?;
            ctx.emit(Instruction::LocalGet(arg_local));
            ctx.emit(Instruction::Call(idx));
        }
        IrType::Bool => {
            let idx = b.require_phx_func("phx_print_bool")?;
            ctx.emit(Instruction::LocalGet(arg_local));
            ctx.emit(Instruction::Call(idx));
        }
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: `print` on argument of IR type `{other:?}` \
                 not yet supported (Phase 2.4 PR 3b — see docs/design-decisions.md §Phase 2.4)"
            )));
        }
    }
    Ok(())
}

/// Translate a basic-block terminator. PR 2 covers only the
/// no-value-return path.
fn translate_terminator(
    ctx: &mut FuncTranslateCtx,
    _b: &ModuleBuilder,
    term: &Terminator,
) -> Result<(), CompileError> {
    match term {
        Terminator::Return(None) => {
            ctx.emit(Instruction::End);
            Ok(())
        }
        Terminator::Return(Some(_)) => Err(CompileError::new(
            "wasm32-linear: value-returning functions not yet supported \
             (Phase 2.4 PR 3 — see docs/design-decisions.md §Phase 2.4)"
                .to_string(),
        )),
        // Unreachable while `translate_function` enforces the
        // single-block restriction (the only terminator a 1-block
        // function can carry is `Return`); kept for PR 3 when
        // multi-block control flow lifts that restriction.
        other => Err(CompileError::new(format!(
            "wasm32-linear: terminator `{other:?}` not yet supported \
             (Phase 2.4 PR 3)"
        ))),
    }
}

fn unsupported(ty: &IrType, where_: &str) -> CompileError {
    CompileError::new(format!(
        "wasm32-linear: IR type `{ty:?}` not yet supported in {where_} \
         (Phase 2.4 PR 3 — see docs/design-decisions.md §Phase 2.4)"
    ))
}
