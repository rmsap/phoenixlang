//! Phoenix IR → WASM-GC function-body translation.
//!
//! Minimal MVP surface (per design-decisions §Phase 2.4 decision J):
//! `Op::ConstI64`, `Op::BuiltinCall("print", Int)`, `Op::Alloca` /
//! `Op::Load` / `Op::Store` (the *mutable* `let mut` lowering uses these;
//! an immutable `let` binds its initializer's SSA value directly, so
//! `hello.phx` never reaches this trio — see `print_int_module` and the
//! `let mut` test in the integration suite), and `Op::Return(None)` —
//! enough for `hello.phx`. The op surface
//! grows incrementally in subsequent PR 5 slices (fibonacci → adds
//! arithmetic and control flow; struct fixture → adds `Op::StructAlloc`
//! / `Op::StructGetField` / `Op::StructSetField` lowered as
//! `struct.new` / `struct.get` / `struct.set`).
//!
//! Shadow-stack emission is suppressed entirely on this target — the
//! host VM's GC handles tracing, so `phx_gc_push_frame` / `set_root` /
//! `pop_frame` calls are never emitted (and the runtime that provides
//! them isn't merged into this target's output, per decision I).

use std::collections::HashMap;

use phoenix_ir::block::BlockId;
use phoenix_ir::instruction::{Op, ValueId};
use phoenix_ir::module::{IrFunction, IrModule};
use phoenix_ir::terminator::Terminator;
use phoenix_ir::types::IrType;
use wasm_encoder::{Function, Instruction, ValType};

use super::module_builder::ModuleBuilder;
use crate::error::CompileError;

/// Does any concrete function call the `print` builtin? The `fd_write`
/// import and the synthesized `phx_print_i64` helper exist only to
/// service `print`, so the module builder skips both when this returns
/// `false` — a non-printing program then carries no dead WASI import and
/// no uncallable helper body.
///
/// Detection is by builtin *name*, not argument type: a `print` whose
/// argument isn't `Int` is rejected later by [`translate_print`], so
/// over-detecting here at worst synthesizes a helper into a module that
/// fails to compile anyway. When the String slice adds `phx_print_str`,
/// this refines into per-helper detection.
pub(super) fn module_calls_print(ir_module: &IrModule) -> bool {
    ir_module.concrete_functions().any(|func| {
        func.blocks.iter().any(|block| {
            block
                .instructions
                .iter()
                .any(|instr| matches!(&instr.op, Op::BuiltinCall(name, _) if name == "print"))
        })
    })
}

/// Flatten a Phoenix `IrType` into the WASM `ValType` slot list used
/// in function signatures and local declarations. MVP surface only —
/// extended in PR 5 follow-up slices for struct / array / managed-ref
/// types.
pub(super) fn wasm_valtypes_for(ty: &IrType) -> Result<Vec<ValType>, CompileError> {
    match ty {
        IrType::I64 => Ok(vec![ValType::I64]),
        IrType::F64 => Ok(vec![ValType::F64]),
        IrType::Bool => Ok(vec![ValType::I32]),
        IrType::Void => Ok(Vec::new()),
        other => Err(CompileError::new(format!(
            "wasm32-gc MVP: IR type `{other:?}` not yet supported \
             (Phase 2.4 PR 5 slice 1 covers Int / Bool / Float / Void; \
              struct / list / string land in later slices)"
        ))),
    }
}

/// Flatten a function-parameter list into WASM `ValType`s, in
/// declaration order. MVP surface is single-slot per parameter, kept in
/// lockstep with [`FuncCtx::new`]'s param binding via [`single_slot`] —
/// otherwise a signature could be built with a multi-slot param that the
/// body translator then rejects. Multi-slot types (`StringRef`,
/// `DynRef`) land in a later slice, which relaxes both sides together.
pub(super) fn flatten_param_types(params: &[IrType]) -> Result<Vec<ValType>, CompileError> {
    let mut out = Vec::with_capacity(params.len());
    for ty in params {
        out.push(single_slot(ty, "function parameter")?);
    }
    Ok(out)
}

/// Map a Phoenix function's return `IrType` to its WASM `ValType`s.
/// `Void` → empty (the function pushes no operand-stack values).
pub(super) fn wasm_return_valtypes(ty: &IrType) -> Result<Vec<ValType>, CompileError> {
    wasm_valtypes_for(ty)
}

/// Per-function translation state. Tracks which WASM local index
/// each `ValueId` is bound to and the next free local index.
///
/// Binding strategy: **every** produced SSA value gets its own WASM
/// local, materialized immediately with a `local.set` after the
/// instruction that computes it, and re-read with `local.get` at each
/// use (e.g. `Op::Load` allocates a fresh local and copies the slot into
/// it rather than leaving the value on the operand stack). This emits
/// more locals and redundant copies than a stack-aware scheme would —
/// `hello.phx` alone produces several throwaway locals — but it is a
/// deliberate MVP simplicity choice: a flat `ValueId → local` map sidesteps
/// operand-stack lifetime tracking entirely. Revisit if local pressure or
/// code size becomes a concern in a later slice.
struct FuncCtx {
    /// Buffered body instructions. WASM `Function` wants the whole
    /// body up front; we accumulate here and hand off at the end.
    instructions: Vec<Instruction<'static>>,
    /// Per-binding local indices. Single-slot scalar types occupy
    /// one entry; multi-slot types will land in a later slice.
    bindings: HashMap<ValueId, u32>,
    /// WASM `ValType` each binding was allocated with. Kept alongside
    /// [`Self::bindings`] so call lowering can dispatch on the actual
    /// operand type (e.g. `print` routing `Int`/`i64` to the
    /// `phx_print_i64` helper) rather than blindly assuming one.
    binding_types: HashMap<ValueId, ValType>,
    /// Pending local declarations beyond the function parameters,
    /// in WASM's run-length-encoded `(count, type)` form.
    pending_locals: Vec<(u32, ValType)>,
    /// Next free WASM local index (past the function-param locals).
    next_local: u32,
}

impl FuncCtx {
    fn new(func: &IrFunction) -> Result<Self, CompileError> {
        let mut bindings = HashMap::new();
        let mut binding_types = HashMap::new();
        let mut next_local: u32 = 0;
        // Entry block's params bind to function-parameter locals.
        if let Some(entry) = func.blocks.first() {
            for (vid, ty) in &entry.params {
                let slots = wasm_valtypes_for(ty)?;
                // MVP surface is single-slot only; multi-slot types
                // (StringRef, DynRef) land in later slices.
                if slots.len() != 1 {
                    return Err(CompileError::new(format!(
                        "wasm32-gc MVP: function `{}` parameter type `{ty:?}` \
                         flattens to {} slots — only single-slot params are \
                         supported in PR 5 slice 1 (multi-slot lands in a \
                         later slice)",
                        func.name,
                        slots.len()
                    )));
                }
                bindings.insert(*vid, next_local);
                binding_types.insert(*vid, slots[0]);
                next_local += 1;
            }
        }
        Ok(Self {
            instructions: Vec::new(),
            bindings,
            binding_types,
            pending_locals: Vec::new(),
            next_local,
        })
    }

    fn allocate_local(&mut self, vid: ValueId, wasm_ty: ValType) -> u32 {
        let idx = self.next_local;
        self.push_local_decl(wasm_ty);
        self.next_local += 1;
        self.bindings.insert(vid, idx);
        self.binding_types.insert(vid, wasm_ty);
        idx
    }

    fn push_local_decl(&mut self, wasm_ty: ValType) {
        match self.pending_locals.last_mut() {
            Some((count, last_ty)) if *last_ty == wasm_ty => *count += 1,
            _ => self.pending_locals.push((1, wasm_ty)),
        }
    }

    fn emit(&mut self, instr: Instruction<'static>) {
        self.instructions.push(instr);
    }

    fn binding_of(&self, vid: ValueId) -> Result<u32, CompileError> {
        self.bindings.get(&vid).copied().ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: unbound `ValueId({vid:?})` reached the translator \
                 (internal compiler bug)"
            ))
        })
    }

    /// The WASM `ValType` a binding was allocated with. Used to
    /// dispatch type-sensitive lowering (e.g. `print`) on the operand's
    /// actual representation.
    fn binding_type_of(&self, vid: ValueId) -> Result<ValType, CompileError> {
        self.binding_types.get(&vid).copied().ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: binding `ValueId({vid:?})` has no recorded WASM type \
                 (internal compiler bug)"
            ))
        })
    }

    fn into_function(self) -> Function {
        let mut func = Function::new(self.pending_locals);
        for instr in self.instructions {
            func.instruction(&instr);
        }
        func
    }
}

/// Translate one Phoenix function into a WASM `Function` body. MVP
/// surface assumes a single block (no Jump / Branch terminators yet).
pub(super) fn translate_function(
    b: &mut ModuleBuilder,
    _ir_module: &IrModule,
    func: &IrFunction,
) -> Result<Function, CompileError> {
    if func.blocks.is_empty() {
        return Err(CompileError::new(format!(
            "wasm32-gc: function `{}` has no blocks (internal compiler bug)",
            func.name
        )));
    }
    if func.blocks.len() > 1 {
        return Err(CompileError::new(format!(
            "wasm32-gc MVP: function `{}` has {} blocks — multi-block \
             control flow lands in a later slice (PR 5 slice 2)",
            func.name,
            func.blocks.len()
        )));
    }
    let mut ctx = FuncCtx::new(func)?;
    let block = &func.blocks[0];
    for instr in &block.instructions {
        translate_instruction(&mut ctx, b, instr)?;
    }
    translate_terminator(&block.terminator, &func.return_type, BlockId(0))?;
    // Every WASM function body needs an explicit `end`.
    ctx.emit(Instruction::End);
    Ok(ctx.into_function())
}

fn translate_instruction(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    match &instr.op {
        Op::ConstI64(n) => {
            let vid = expect_result(instr, "Op::ConstI64")?;
            ctx.emit(Instruction::I64Const(*n));
            let local = ctx.allocate_local(vid, ValType::I64);
            ctx.emit(Instruction::LocalSet(local));
            Ok(())
        }
        Op::ConstBool(v) => {
            let vid = expect_result(instr, "Op::ConstBool")?;
            ctx.emit(Instruction::I32Const(if *v { 1 } else { 0 }));
            let local = ctx.allocate_local(vid, ValType::I32);
            ctx.emit(Instruction::LocalSet(local));
            Ok(())
        }
        // A *mutable* `let mut x: T = expr` lowers via `Op::Alloca(T) +
        // Op::Store(slot, expr)`, and each read of `x` emits `Op::Load(slot)`
        // (see `phoenix-ir` `lower_stmt`: immutable `let` instead binds the
        // initializer's SSA value directly, so it never reaches this trio).
        // The Alloca's "slot" is a single WASM local of T's wasm type;
        // Store writes into it, Load reads it back.
        Op::Alloca(ty) => {
            let vid = expect_result(instr, "Op::Alloca")?;
            let wasm_ty = single_slot(ty, "Op::Alloca")?;
            ctx.allocate_local(vid, wasm_ty);
            // Initial value is zero/undefined; first Store sets it.
            Ok(())
        }
        Op::Load(slot_vid) => {
            let vid = expect_result(instr, "Op::Load")?;
            let slot_local = ctx.binding_of(*slot_vid)?;
            let wasm_ty = single_slot(&instr.result_type, "Op::Load")?;
            ctx.emit(Instruction::LocalGet(slot_local));
            let local = ctx.allocate_local(vid, wasm_ty);
            ctx.emit(Instruction::LocalSet(local));
            Ok(())
        }
        Op::Store(slot_vid, value_vid) => {
            let slot_local = ctx.binding_of(*slot_vid)?;
            let value_local = ctx.binding_of(*value_vid)?;
            ctx.emit(Instruction::LocalGet(value_local));
            ctx.emit(Instruction::LocalSet(slot_local));
            Ok(())
        }
        Op::BuiltinCall(name, args) => translate_builtin_call(ctx, b, name, args, instr),
        other => Err(CompileError::new(format!(
            "wasm32-gc MVP: IR op `{other:?}` not yet supported \
             (Phase 2.4 PR 5 slice 1 covers `Op::ConstI64`, `Op::Alloca` / \
              `Op::Load` / `Op::Store`, `Op::BuiltinCall(\"print\", Int)`, \
              and `Op::Return(None)` — enough for hello.phx)"
        ))),
    }
}

fn translate_builtin_call(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    name: &str,
    args: &[ValueId],
    _instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    match name {
        "print" => translate_print(ctx, b, args),
        other => Err(CompileError::new(format!(
            "wasm32-gc MVP: builtin `{other}` not yet supported \
             (PR 5 slice 1 covers `print(Int)` only)"
        ))),
    }
}

/// `print(value)` — dispatch on the value's Phoenix `IrType` to the
/// matching synthesized helper. MVP supports `Int` only; other types
/// land in subsequent slices.
fn translate_print(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
) -> Result<(), CompileError> {
    let arg_vid = *args.first().ok_or_else(|| {
        CompileError::new(
            "wasm32-gc: `print(...)` requires 1 argument (internal compiler bug)".to_string(),
        )
    })?;
    // Dispatch on the binding's WASM `ValType`, which is unambiguous
    // because every supported scalar (`Int`, `Bool`, `Float`) occupies
    // a distinct `ValType`. Slice 1 only synthesizes `phx_print_i64`,
    // so anything other than `i64` (e.g. a `Bool`, which sema's
    // unconstrained `print` does accept) must error here rather than
    // silently emit a `Call` against a mismatched signature — that
    // would produce a structurally invalid module.
    let arg_local = ctx.binding_of(arg_vid)?;
    let arg_ty = ctx.binding_type_of(arg_vid)?;
    if arg_ty != ValType::I64 {
        return Err(CompileError::new(format!(
            "wasm32-gc MVP: `print(...)` supports `Int` arguments only in PR 5 \
             slice 1 (got a value lowered to WASM `{arg_ty:?}`); `Bool` / `Float` \
             / `String` printing lands in later slices"
        )));
    }
    let print_i64_idx = b.require_print_i64_idx()?;
    ctx.emit(Instruction::LocalGet(arg_local));
    ctx.emit(Instruction::Call(print_i64_idx));
    Ok(())
}

// No `FuncCtx` parameter yet: slice 1's only accepted terminator is
// `Return(None)`, which emits nothing (the implicit return at the
// function's `end` covers it). Slice 2 re-threads `ctx` here when
// value-returning / branch terminators need to emit instructions.
fn translate_terminator(
    term: &Terminator,
    _return_type: &IrType,
    _block_id: BlockId,
) -> Result<(), CompileError> {
    match term {
        Terminator::Return(None) => {
            // A void function returns implicitly when control reaches the
            // closing `end` that `translate_function` appends, so no
            // explicit `return` is emitted — it would only add dead
            // bytecode before `end`. This is sound for slice 1 because the
            // body is a single block whose terminator *is* the last thing
            // before `end`. Slice 2's multi-block control flow revisits
            // this: a `Return(None)` that is not the final block will need
            // an explicit `return`.
            Ok(())
        }
        Terminator::Return(Some(_v)) => Err(CompileError::new(
            "wasm32-gc MVP: value-returning `Return` lands in PR 5 slice 2 \
             (fibonacci needs it; hello.phx is Void-returning)"
                .to_string(),
        )),
        other => Err(CompileError::new(format!(
            "wasm32-gc MVP: terminator `{other:?}` not yet supported \
             (PR 5 slice 1 is single-block; multi-block lands in slice 2)"
        ))),
    }
}

fn expect_result(
    instr: &phoenix_ir::instruction::Instruction,
    op_label: &str,
) -> Result<ValueId, CompileError> {
    instr.result.ok_or_else(|| {
        CompileError::new(format!(
            "wasm32-gc: `{op_label}` has no result binding (internal compiler bug)"
        ))
    })
}

fn single_slot(ty: &IrType, label: &str) -> Result<ValType, CompileError> {
    let slots = wasm_valtypes_for(ty)?;
    if slots.len() != 1 {
        return Err(CompileError::new(format!(
            "wasm32-gc MVP: `{label}` expected a single-slot type, got \
             `{ty:?}` ({} slots)",
            slots.len()
        )));
    }
    Ok(slots[0])
}
