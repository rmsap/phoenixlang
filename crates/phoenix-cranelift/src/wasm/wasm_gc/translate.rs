//! Phoenix IR → WASM-GC function-body translation.
//!
//! Op surface as of PR 5 slice 3 (per design-decisions §Phase 2.4
//! decision J):
//!
//! - **Constants:** `Op::ConstI64`, `Op::ConstBool`, `Op::ConstF64`,
//!   `Op::ConstString`.
//! - **`let mut` lowering:** `Op::Alloca` / `Op::Load` / `Op::Store`
//!   (immutable `let` binds its initializer's SSA value directly and
//!   never walks these).
//! - **Arithmetic (Int):** `Op::IAdd`, `Op::ISub`, `Op::IMul`,
//!   `Op::IDiv`, `Op::IMod`, `Op::INeg`.
//! - **Arithmetic (Float):** `Op::FAdd`, `Op::FSub`, `Op::FMul`,
//!   `Op::FDiv`, `Op::FNeg`. WASM `f64.<op>` matches IEEE-754
//!   semantics directly. `Op::FMod` (Float `%`) has no native
//!   `f64.rem`, so it calls the synthesized `phx_fmod` helper (musl
//!   `fmod` port in `float_helpers.rs`, bit-identical to native
//!   Rust's `%`). See §Phase 2.4 decision K.5.
//! - **Comparison (Int → Bool):** `Op::IEq`, `Op::INe`, `Op::ILt`,
//!   `Op::ILe`, `Op::IGt`, `Op::IGe`.
//! - **Comparison (Float → Bool):** `Op::FEq`, `Op::FNe`, `Op::FLt`,
//!   `Op::FLe`, `Op::FGt`, `Op::FGe`.
//! - **Bool ops:** `Op::BoolEq`, `Op::BoolNe`, `Op::BoolNot`.
//! - **Calls:** `Op::Call(fid, [], args)` for direct user-function
//!   calls (recursion included).
//! - **Builtins:** `Op::BuiltinCall("print", Int)` only.
//! - **Structs:** `Op::StructAlloc` / `Op::StructGetField` /
//!   `Op::StructSetField` lowered as `struct.new` / `struct.get` /
//!   `struct.set` against one nominal `(struct …)` declaration per
//!   Phoenix struct (per §Phase 2.4 decision K.1).
//! - **Multi-block control flow:** loop+switch dispatcher with
//!   `Terminator::Return(Some/None)`, `Terminator::Jump`,
//!   `Terminator::Branch`. Block-param locals are single-slot only
//!   today; multi-slot params (`StringRef`, `DynRef`) land later.
//!
//! Strings, enums, floats (incl. print + `%`), and the closure-free
//! `List<T>` / `ListBuilder<T>` surface (§K.7) have landed across the
//! PR 6 slices; closures / maps / dyn (and with closures, the
//! closure-taking list methods) are the remaining matrix-expansion
//! increments.
//!
//! Shadow-stack emission is suppressed entirely on this target — the
//! host VM's GC handles tracing, so `phx_gc_push_frame` / `set_root` /
//! `pop_frame` calls are never emitted (and the runtime that provides
//! them isn't merged into this target's output, per decision I).

use std::collections::HashMap;

use phoenix_ir::block::{BasicBlock, BlockId};
use phoenix_ir::instruction::{Op, ValueId};
use phoenix_ir::module::{IrFunction, IrModule};
use phoenix_ir::terminator::Terminator;
use phoenix_ir::types::IrType;
use wasm_encoder::{BlockType, Function, HeapType, Instruction, RefType, ValType};

use super::closures;
use super::dyn_trait;
use super::lists;
use super::maps;
use super::module_builder::{self, ModuleBuilder};
use super::option_result;
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
/// extended in PR 5 follow-up slices for closure / list / map types.
///
/// `StructRef(name, _)` resolves via `b.require_phx_struct(name)` to a
/// nullable concrete ref `(ref null $struct_idx)`. Nullability lets a
/// freshly-allocated `Op::Alloca` slot for a struct type start in
/// WASM's zero-init (`ref.null`) state; the first `Op::Store` then
/// writes the post-`struct.new` non-nullable reference into it
/// (subtype `(ref $T) <: (ref null $T)`). See §Phase 2.4 decision K.1.
pub(super) fn wasm_valtypes_for(
    ty: &IrType,
    b: &ModuleBuilder,
) -> Result<Vec<ValType>, CompileError> {
    match ty {
        IrType::I64 => Ok(vec![ValType::I64]),
        IrType::F64 => Ok(vec![ValType::F64]),
        IrType::Bool => Ok(vec![ValType::I32]),
        IrType::Void => Ok(Vec::new()),
        IrType::StructRef(name, _) => {
            let idx = b.require_phx_struct(name)?;
            Ok(vec![ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(idx),
            })])
        }
        IrType::StringRef => {
            // Nullable concrete ref. Same nullability rationale as
            // K.1's `StructRef` mapping: `Op::Alloca(StringRef)` slots
            // default to `ref.null` (WASM's zero-init for ref-typed
            // locals), and the first `Op::Store` writes a non-nullable
            // `(ref $string)` from `Op::ConstString` / `Op::StringConcat`
            // into the slot via the `(ref $T) <: (ref null $T)`
            // subtype. See §Phase 2.4 decision K.2.
            let idx = b.require_string_type_idx()?;
            Ok(vec![ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(idx),
            })])
        }
        IrType::EnumRef(name, type_args) => {
            // Enum-typed values flow through the parent (`(sub (struct
            // (field $tag i32)))`) at all SSA boundaries — locals,
            // function params, block params, struct/list/enum fields.
            // `Op::EnumAlloc` produces a `(ref $variant)` which upcasts
            // to `(ref null $parent)` via WASM-GC subtype subsumption.
            // `Op::EnumDiscriminant` reads `$tag` through the parent
            // without a `ref.cast`. `Op::EnumGetField` `ref.cast`s
            // down to the concrete variant before the field load.
            // The `type_args` distinguish concrete instantiations (per
            // K.4 codegen-time monomorphization — `Option<Int>` and
            // `Option<String>` get separate WASM enums).
            // See §Phase 2.4 decision K.4.
            let idx = b.require_enum_parent_idx(name, type_args)?;
            Ok(vec![ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(idx),
            })])
        }
        IrType::ListRef(elem) => {
            // Nullable concrete ref to the K.7 `$list_T` wrapper
            // struct — same nullability rationale as `StructRef` /
            // `StringRef` above.
            let (_, list_idx) = b.require_list_types(elem)?;
            Ok(vec![ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(list_idx),
            })])
        }
        IrType::ListBuilderRef(elem) => {
            let builder_idx = b.require_list_builder_idx(elem)?;
            Ok(vec![ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(builder_idx),
            })])
        }
        IrType::MapRef(k, v) => {
            // Nullable ref to the K.9 `$map_KV` wrapper struct.
            let map_idx = b.require_map_idx(k, v)?;
            Ok(vec![ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(map_idx),
            })])
        }
        IrType::DynRef(trait_name) => {
            // Nullable ref to the K.10 `$dyn_T` fat-pointer struct.
            Ok(vec![dyn_trait::dyn_valtype(b, trait_name)?])
        }
        IrType::ClosureRef {
            param_types,
            return_type,
        } => {
            // Nullable ref to the K.8 signature parent — call sites
            // hold the parent and never see capture layouts.
            let key = (param_types.clone(), (**return_type).clone());
            let (_, parent_idx) = b.require_closure_sig(&key)?;
            Ok(vec![ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(parent_idx),
            })])
        }
        other => Err(CompileError::new(format!(
            "wasm32-gc MVP: IR type `{other:?}` not yet supported \
             (Phase 2.4 PR 5 slices 1-3 + PR 6 slices cover Int / Bool / \
              Float / Void / StructRef / StringRef / EnumRef / ListRef / \
              ListBuilderRef; closures / maps / dyn land in PR 6 \
              follow-up slices)"
        ))),
    }
}

/// Flatten a function-parameter list into WASM `ValType`s, in
/// declaration order. MVP surface is single-slot per parameter, kept in
/// lockstep with [`FuncCtx::new`]'s param binding via [`single_slot`] —
/// otherwise a signature could be built with a multi-slot param that the
/// body translator then rejects. Multi-slot types (`StringRef`,
/// `DynRef`) land in a later slice, which relaxes both sides together.
pub(super) fn flatten_param_types(
    params: &[IrType],
    b: &ModuleBuilder,
) -> Result<Vec<ValType>, CompileError> {
    let mut out = Vec::with_capacity(params.len());
    for ty in params {
        out.push(single_slot(ty, b, "function parameter")?);
    }
    Ok(out)
}

/// Map a Phoenix function's return `IrType` to its WASM `ValType`s.
/// `Void` → empty (the function pushes no operand-stack values).
pub(super) fn wasm_return_valtypes(
    ty: &IrType,
    b: &ModuleBuilder,
) -> Result<Vec<ValType>, CompileError> {
    wasm_valtypes_for(ty, b)
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
pub(super) struct FuncCtx {
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
    /// The current function's `$site_F` struct index when it is a
    /// closure function (a `ClosureAlloc` target) — the statically
    /// known `ref.cast` target for `Op::ClosureLoadCapture`. `None`
    /// for ordinary functions. See §Phase 2.4 K.8.
    closure_site: Option<u32>,
    /// Per-block destination locals for non-entry blocks, in param
    /// declaration order. `Jump` / `Branch` lowering reads this to find
    /// where to copy arg values when control transfers to the block.
    /// Entry-block params are NOT listed here — they bind to function-
    /// parameter locals at [`Self::new`] time. Single-slot only for the
    /// MVP; multi-slot params will need a `Vec<Vec<u32>>` (one slot run
    /// per param) when `StringRef` / `DynRef` block params land.
    block_param_locals: HashMap<BlockId, Vec<u32>>,
}

/// Codegen-side metadata shared between the multi-block dispatcher
/// and the terminator translator. `depth_to_loop` is the WASM label
/// depth from the current emission point to the outer `(loop $L)` so
/// `Jump` / `Branch` can issue `br <depth>` to re-enter the dispatch.
/// `dispatch_local` holds the "next block ID" the `br_table` reads.
#[derive(Debug, Clone, Copy)]
struct DispatcherContext {
    depth_to_loop: u32,
    dispatch_local: u32,
}

impl FuncCtx {
    fn new(func: &IrFunction, b: &ModuleBuilder) -> Result<Self, CompileError> {
        let mut bindings = HashMap::new();
        let mut binding_types = HashMap::new();
        let mut next_local: u32 = 0;
        let closure_site = b.closure_site_idx_if_set(func.id);
        // Entry block's params bind to function-parameter locals.
        if let Some(entry) = func.blocks.first() {
            for (idx, (vid, ty)) in entry.params.iter().enumerate() {
                // A closure function's env parameter (param 0) is the
                // abstract `(ref null struct)` per K.8's `$fn_SIG` —
                // not the `(ref null $clo_SIG)` the `ClosureRef`
                // mapping would yield. The body casts it down at each
                // `ClosureLoadCapture`.
                if idx == 0 && closure_site.is_some() {
                    bindings.insert(*vid, next_local);
                    binding_types.insert(*vid, super::closures::env_valtype());
                    next_local += 1;
                    continue;
                }
                let slots = wasm_valtypes_for(ty, b)?;
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
            closure_site,
            block_param_locals: HashMap::new(),
        })
    }

    /// Allocate the `i32` "next block ID" dispatch local. Stable
    /// position relative to function params guarantees an `i32.const
    /// 0` init isn't needed — WASM zero-initializes all locals, and
    /// `BlockId(0)` (the entry block) is the desired initial dispatch
    /// target.
    fn allocate_dispatch_local(&mut self) -> u32 {
        let idx = self.next_local;
        self.push_local_decl(ValType::I32);
        self.next_local += 1;
        idx
    }

    /// Register `local` as one block-param destination for the given
    /// non-entry block. `Jump` / `Branch` lowering reads these (in
    /// declaration order) to find where to copy arg values when control
    /// transfers to the block. Single-slot only for the MVP; a future
    /// multi-slot expansion stores one `Vec<u32>` slot run per param.
    fn register_block_param(&mut self, block: BlockId, local: u32) {
        self.block_param_locals
            .entry(block)
            .or_default()
            .push(local);
    }

    /// Return the block's parameter destination locals (in declaration
    /// order), or an empty slice if none have been registered. Empty
    /// for the entry block (whose params alias function-parameter
    /// locals).
    fn block_param_locals_of(&self, block: BlockId) -> &[u32] {
        self.block_param_locals
            .get(&block)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub(super) fn allocate_local(&mut self, vid: ValueId, wasm_ty: ValType) -> u32 {
        let idx = self.next_local;
        self.push_local_decl(wasm_ty);
        self.next_local += 1;
        self.bindings.insert(vid, idx);
        self.binding_types.insert(vid, wasm_ty);
        idx
    }

    /// Allocate an anonymous scratch local not tied to any `ValueId`.
    /// Used by multi-step lowerings (the K.7 list copy loops, builder
    /// growth) that revisit intermediate values. WASM zero-initializes
    /// locals **once at function entry**, not per use — a lowering
    /// that re-executes inside a loop must explicitly initialize its
    /// scratch before reading it. Scratches are never pooled: every
    /// call declares a fresh local, so each list-op *instruction* in a
    /// function body adds its own — harmless for validity, but a
    /// candidate for reuse-by-type if body size ever matters.
    pub(super) fn scratch_local(&mut self, wasm_ty: ValType) -> u32 {
        let idx = self.next_local;
        self.push_local_decl(wasm_ty);
        self.next_local += 1;
        idx
    }

    fn push_local_decl(&mut self, wasm_ty: ValType) {
        match self.pending_locals.last_mut() {
            Some((count, last_ty)) if *last_ty == wasm_ty => *count += 1,
            _ => self.pending_locals.push((1, wasm_ty)),
        }
    }

    pub(super) fn emit(&mut self, instr: Instruction<'static>) {
        self.instructions.push(instr);
    }

    pub(super) fn binding_of(&self, vid: ValueId) -> Result<u32, CompileError> {
        self.bindings.get(&vid).copied().ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: unbound `{vid:?}` reached the translator \
                 (internal compiler bug)"
            ))
        })
    }

    /// The WASM `ValType` a binding was allocated with. Used to
    /// dispatch type-sensitive lowering (e.g. `print`) on the operand's
    /// actual representation.
    pub(super) fn binding_type_of(&self, vid: ValueId) -> Result<ValType, CompileError> {
        self.binding_types.get(&vid).copied().ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: binding `{vid:?}` has no recorded WASM type \
                 (internal compiler bug)"
            ))
        })
    }

    /// The current function's closure `$site_F` index, when inside a
    /// closure body. See [`Self::closure_site`] (the field doc).
    pub(super) fn closure_site(&self) -> Option<u32> {
        self.closure_site
    }

    fn into_function(self) -> Function {
        let mut func = Function::new(self.pending_locals);
        for instr in self.instructions {
            func.instruction(&instr);
        }
        func
    }
}

/// Translate one Phoenix function into a WASM `Function` body.
/// Single-block functions emit straight-line bodies; multi-block
/// functions route through the loop+switch dispatcher (mirroring the
/// wasm32-linear backend's shape per §Phase 2.4 decision G).
pub(super) fn translate_function(
    b: &mut ModuleBuilder,
    ir_module: &IrModule,
    func: &IrFunction,
) -> Result<Function, CompileError> {
    if func.blocks.is_empty() {
        return Err(CompileError::new(format!(
            "wasm32-gc: function `{}` has no blocks (internal compiler bug)",
            func.name
        )));
    }
    let mut ctx = FuncCtx::new(func, b)?;
    if func.blocks.len() == 1 {
        let block = &func.blocks[0];
        for instr in &block.instructions {
            translate_instruction(&mut ctx, b, ir_module, instr)?;
        }
        translate_terminator(&mut ctx, &block.terminator, &func.return_type, None)?;
    } else {
        translate_multi_block(&mut ctx, b, ir_module, func)?;
    }
    // Every WASM function body needs an explicit `end`.
    ctx.emit(Instruction::End);
    Ok(ctx.into_function())
}

/// Emit the loop+switch dispatcher for a multi-block function.
///
/// Structure (for 3 blocks, bb_0..bb_2):
///
/// ```text
/// loop $L                              ;; outermost
///   block $bb_2                        ;; depth N-1
///     block $bb_1                      ;; depth N-2
///       block $bb_0                    ;; innermost
///         local.get $dispatch
///         br_table 0 1 2 0             ;; default = bb_0 (unreachable)
///       end                            ;; close $bb_0
///       ;; bb_0 body + terminator
///     end                              ;; close $bb_1
///     ;; bb_1 body + terminator
///   end                                ;; close $bb_2
///   ;; bb_2 body + terminator
/// end                                  ;; close $L
/// unreachable
/// ```
///
/// Each block's terminator (`Jump` / `Branch`) writes the next
/// `BlockId` into the dispatch local and `br $L`s back to the
/// dispatcher; `Return` exits the function and skips the loop
/// re-entry. The `unreachable` sentinel after the loop satisfies
/// wasm-encoder's "function body must terminate every path" rule —
/// in a well-formed program every block's terminator either re-enters
/// the loop or returns, so the bytecode beyond `end $L` is genuinely
/// dead.
fn translate_multi_block(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    ir_module: &IrModule,
    func: &IrFunction,
) -> Result<(), CompileError> {
    let n_blocks = func.blocks.len();
    debug_assert!(n_blocks > 1, "translate_multi_block called with <= 1 block");

    // Validate the `func.blocks[i].id == BlockId(i)` invariant up front,
    // before any bytecode is emitted. The `br_table` dispatches by array
    // position while `Jump` / `Branch` set the dispatch local to a
    // `BlockId` value, so a mismatch would route control to the wrong
    // block; checking it as a precondition keeps the emission loop below
    // free of the concern.
    for (block_idx, block) in func.blocks.iter().enumerate() {
        require_block_id_matches_index(block, block_idx)?;
    }

    // Allocate the dispatch local first so its index is stable
    // before any block-param locals get assigned. Zero-initialized,
    // which matches the entry block (`BlockId(0)`) — no init needed.
    let dispatch_local = ctx.allocate_dispatch_local();

    // Allocate locals for non-entry block params. Entry-block params
    // alias function-parameter locals (already bound in
    // `FuncCtx::new`). Records are keyed by the block's own `id` (not
    // its array position), so `Jump` / `Branch` lookups — which use
    // the target `BlockId` — stay correct independent of array order.
    for block in func.blocks.iter().skip(1) {
        for (vid, ty) in &block.params {
            let wasm_ty = single_slot(ty, b, "non-entry block param")?;
            let local = ctx.allocate_local(*vid, wasm_ty);
            ctx.register_block_param(block.id, local);
        }
    }

    // Open the outer loop, then N labeled blocks (deepest-first so
    // bb_0 ends up at depth 0).
    ctx.emit(Instruction::Loop(BlockType::Empty));
    for _ in 0..n_blocks {
        ctx.emit(Instruction::Block(BlockType::Empty));
    }
    // br_table identity vector: $bb_i sits at depth i (innermost
    // first), default = 0 (= $bb_0).
    ctx.emit(Instruction::LocalGet(dispatch_local));
    let table_targets: Vec<u32> = (0..n_blocks as u32).collect();
    ctx.emit(Instruction::BrTable(
        std::borrow::Cow::Owned(table_targets),
        0,
    ));

    // Emit each block's body + terminator. Between consecutive
    // bodies, close the corresponding labeled block (so the next
    // br target's label index naturally decreases).
    for (block_idx, block) in func.blocks.iter().enumerate() {
        ctx.emit(Instruction::End); // close the labeled block whose body follows
        let depth_to_loop = (n_blocks - 1 - block_idx) as u32;
        let dispatcher = Some(DispatcherContext {
            depth_to_loop,
            dispatch_local,
        });
        for instr in &block.instructions {
            translate_instruction(ctx, b, ir_module, instr)?;
        }
        translate_terminator(ctx, &block.terminator, &func.return_type, dispatcher)?;
    }
    // Close the outer loop. Every block ends with a terminator
    // (`return` or `br $L`), so falling off here is impossible — emit
    // `unreachable` to satisfy wasm-encoder's path-terminator rule.
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::Unreachable);
    Ok(())
}

fn translate_instruction(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    // Consulted by `Op::StructAlloc` arity-checks against
    // `IrModule::struct_layouts`; future slices will read it for
    // `Op::ClosureAlloc` capture layouts, enum variant layouts, etc.
    ir_module: &IrModule,
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
        Op::ConstF64(v) => {
            let vid = expect_result(instr, "Op::ConstF64")?;
            ctx.emit(Instruction::F64Const(wasm_encoder::Ieee64::from(*v)));
            let local = ctx.allocate_local(vid, ValType::F64);
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
            let wasm_ty = single_slot(ty, b, "Op::Alloca")?;
            ctx.allocate_local(vid, wasm_ty);
            // Initial value is zero/undefined; first Store sets it.
            Ok(())
        }
        Op::Load(slot_vid) => {
            let vid = expect_result(instr, "Op::Load")?;
            let slot_local = ctx.binding_of(*slot_vid)?;
            let wasm_ty = single_slot(&instr.result_type, b, "Op::Load")?;
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
        // Integer arithmetic. Phoenix `Int` is signed i64; for `IDiv`
        // and `IMod` we emit `i64.div_s` / `i64.rem_s`. Per the WASM
        // spec their trap behavior differs: `div_s` traps on a zero
        // divisor *and* on the signed-overflow case `i64::MIN / -1`,
        // while `rem_s` traps *only* on a zero divisor — `i64::MIN %
        // -1` does not trap and is defined to yield `0`. This mirrors
        // the wasm32-linear backend's lowering (see its `IDiv`/`IMod`
        // comment) and §2.2 decision *Numeric error semantics*.
        Op::IAdd(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, Instruction::I64Add),
        Op::ISub(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, Instruction::I64Sub),
        Op::IMul(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, Instruction::I64Mul),
        Op::IDiv(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, Instruction::I64DivS),
        Op::IMod(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, Instruction::I64RemS),
        Op::INeg(a) => {
            let vid = expect_result(instr, "Op::INeg")?;
            let a_local = ctx.binding_of(*a)?;
            // Two's-complement negation: 0 - x.
            ctx.emit(Instruction::I64Const(0));
            ctx.emit(Instruction::LocalGet(a_local));
            ctx.emit(Instruction::I64Sub);
            let local = ctx.allocate_local(vid, ValType::I64);
            ctx.emit(Instruction::LocalSet(local));
            Ok(())
        }
        // Integer comparisons. Result is `Bool` (i32 0/1).
        Op::IEq(a, b_) => emit_cmp(ctx, instr, *a, *b_, Instruction::I64Eq),
        Op::INe(a, b_) => emit_cmp(ctx, instr, *a, *b_, Instruction::I64Ne),
        Op::ILt(a, b_) => emit_cmp(ctx, instr, *a, *b_, Instruction::I64LtS),
        Op::ILe(a, b_) => emit_cmp(ctx, instr, *a, *b_, Instruction::I64LeS),
        Op::IGt(a, b_) => emit_cmp(ctx, instr, *a, *b_, Instruction::I64GtS),
        Op::IGe(a, b_) => emit_cmp(ctx, instr, *a, *b_, Instruction::I64GeS),
        // Bool comparison. `Bool` is i32 0/1, so equality emits
        // `i32.eq` / `i32.ne`. (`BoolNot` is below the Float block.)
        Op::BoolEq(a, b_) => emit_cmp(ctx, instr, *a, *b_, Instruction::I32Eq),
        Op::BoolNe(a, b_) => emit_cmp(ctx, instr, *a, *b_, Instruction::I32Ne),
        // Float arithmetic. Phoenix `Float` is IEEE-754 f64; WASM
        // `f64.<op>` matches the semantics directly (no trap on divide-
        // by-zero — `f64.div` yields `inf` / `-inf` / `NaN` per the
        // spec, matching native Rust's `f64 / f64`). See §Phase 2.4
        // decision K.5.
        Op::FAdd(a, b_) => emit_f64_binop(ctx, instr, *a, *b_, Instruction::F64Add),
        Op::FSub(a, b_) => emit_f64_binop(ctx, instr, *a, *b_, Instruction::F64Sub),
        Op::FMul(a, b_) => emit_f64_binop(ctx, instr, *a, *b_, Instruction::F64Mul),
        Op::FDiv(a, b_) => emit_f64_binop(ctx, instr, *a, *b_, Instruction::F64Div),
        // Float `%` is the one arithmetic op without a one-instruction
        // lowering: WASM has no `f64.rem`, so it calls the synthesized
        // `phx_fmod` helper (musl `fmod` port — sign-of-dividend
        // truncated remainder, bit-identical to native Rust's
        // `f64 % f64`). See §Phase 2.4 decision K.5.
        Op::FMod(a, b_) => {
            let fmod_idx = b.require_fmod_idx()?;
            emit_f64_binop(ctx, instr, *a, *b_, Instruction::Call(fmod_idx))
        }
        Op::FNeg(a) => {
            // `f64.neg` is its own instruction — unlike i64 (which has
            // no unary negate and uses 0 - x), WASM provides `f64.neg`
            // directly. Flips the sign bit without changing the
            // mantissa or exponent, so `f64.neg(NaN)` is still NaN.
            let vid = expect_result(instr, "Op::FNeg")?;
            let a_local = ctx.binding_of(*a)?;
            ctx.emit(Instruction::LocalGet(a_local));
            ctx.emit(Instruction::F64Neg);
            let local = ctx.allocate_local(vid, ValType::F64);
            ctx.emit(Instruction::LocalSet(local));
            Ok(())
        }
        // Float comparisons. WASM `f64.<cmp>` returns i32 0/1 directly
        // — exactly Phoenix's `Bool` representation. NaN comparisons
        // follow IEEE-754: every ordered op returns 0 when either
        // operand is NaN; `f64.eq(NaN, NaN)` returns 0; `f64.ne(NaN, _)`
        // returns 1. Matches native Rust f64 ordering.
        Op::FEq(a, b_) => emit_cmp(ctx, instr, *a, *b_, Instruction::F64Eq),
        Op::FNe(a, b_) => emit_cmp(ctx, instr, *a, *b_, Instruction::F64Ne),
        Op::FLt(a, b_) => emit_cmp(ctx, instr, *a, *b_, Instruction::F64Lt),
        Op::FLe(a, b_) => emit_cmp(ctx, instr, *a, *b_, Instruction::F64Le),
        Op::FGt(a, b_) => emit_cmp(ctx, instr, *a, *b_, Instruction::F64Gt),
        Op::FGe(a, b_) => emit_cmp(ctx, instr, *a, *b_, Instruction::F64Ge),
        // Bool `not`. `Bool` is i32 0/1; `i32.eqz` flips it (1 → 0,
        // 0 → 1).
        Op::BoolNot(a) => {
            let vid = expect_result(instr, "Op::BoolNot")?;
            let a_local = ctx.binding_of(*a)?;
            ctx.emit(Instruction::LocalGet(a_local));
            ctx.emit(Instruction::I32Eqz);
            let local = ctx.allocate_local(vid, ValType::I32);
            ctx.emit(Instruction::LocalSet(local));
            Ok(())
        }
        // Direct user-function call. After monomorphization,
        // `type_args` is always empty.
        Op::Call(func_id, type_args, args) => {
            debug_assert!(
                type_args.is_empty(),
                "wasm32-gc: `Op::Call({func_id:?})` reached codegen with \
                 unresolved type args {type_args:?}"
            );
            let target_idx = b.require_phx_user_func(*func_id)?;
            for arg in args {
                let local = ctx.binding_of(*arg)?;
                ctx.emit(Instruction::LocalGet(local));
            }
            ctx.emit(Instruction::Call(target_idx));
            bind_call_result(ctx, b, instr, &format!("Op::Call({func_id:?})"))
        }
        Op::BuiltinCall(name, args) => translate_builtin_call(ctx, b, name, args, instr),
        // Struct ops — see §Phase 2.4 decision K.1. Each Phoenix struct
        // has one nominal WASM-GC struct type (reserved then defined at
        // module-build time by `reserve_phoenix_structs` /
        // `define_phoenix_structs`); the receiver value's
        // binding `ValType` is `(ref null $struct_idx)`, from which
        // get/set extract the struct's WASM type index without a
        // parallel `ValueId → struct_name` map.
        Op::StructAlloc(name, vals) => translate_struct_alloc(ctx, b, ir_module, name, vals, instr),
        Op::StructGetField(obj, field_idx) => translate_struct_get(ctx, b, *obj, *field_idx, instr),
        // Enum ops — see §Phase 2.4 decision K.4. Each Phoenix enum
        // has a parent type holding `$tag` and one final variant
        // subtype per variant. EnumAlloc builds the concrete variant
        // (which upcasts to the parent automatically); Discriminant
        // reads `$tag` via the parent without a cast; GetField
        // `ref.cast`s down to the concrete variant before the field
        // load.
        Op::EnumAlloc(name, variant_idx, vals) => {
            translate_enum_alloc(ctx, b, ir_module, name, *variant_idx, vals, instr)
        }
        Op::EnumDiscriminant(value) => translate_enum_discriminant(ctx, b, *value, instr),
        Op::EnumGetField(obj, variant_idx, field_idx) => {
            translate_enum_get_field(ctx, b, ir_module, *obj, *variant_idx, *field_idx, instr)
        }
        // String ops — see §Phase 2.4 decision K.2. `$string` is a
        // 3-field struct over a mutable `$bytes` array; ConstString
        // emits a passive data segment + `array.new_data` + `struct.new`,
        // concat / equality dispatch to synthesized helpers, length
        // lowers inline as `struct.get $string $len`.
        Op::ConstString(s) => {
            let vid = expect_result(instr, "Op::ConstString")?;
            let string_idx = b.require_string_type_idx()?;
            let bytes_idx = b.require_bytes_type_idx()?;
            let bytes = s.as_bytes();
            let seg_idx = b.reserve_string_data(bytes);
            // `array.new_data` allocates a fresh `(ref $bytes)` of
            // length `bytes.len()` and copies `bytes.len()` bytes from
            // segment `seg_idx` starting at offset 0. The bytes live in
            // both the data segment (read-only at runtime) and the
            // heap array; this duplication is the WASM-GC convention
            // for literal materialization — there's no "borrow from
            // data segment" pattern for managed-ref arrays.
            ctx.emit(Instruction::I32Const(0)); // data segment offset
            ctx.emit(Instruction::I32Const(bytes.len() as i32)); // size
            ctx.emit(Instruction::ArrayNewData {
                array_type_index: bytes_idx,
                array_data_index: seg_idx,
            });
            // Wrap with $offset = 0, $len = bytes.len(). Newly
            // constructed strings start at offset 0 of their own
            // freshly-allocated byte array.
            ctx.emit(Instruction::I32Const(0));
            ctx.emit(Instruction::I32Const(bytes.len() as i32));
            ctx.emit(Instruction::StructNew(string_idx));
            let wasm_ty = ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(string_idx),
            });
            let local = ctx.allocate_local(vid, wasm_ty);
            ctx.emit(Instruction::LocalSet(local));
            Ok(())
        }
        Op::StringConcat(a, b_vid) => {
            let vid = expect_result(instr, "Op::StringConcat")?;
            let string_idx = b.require_string_type_idx()?;
            let concat_idx = b.require_str_concat_idx()?;
            let a_local = ctx.binding_of(*a)?;
            let b_local = ctx.binding_of(*b_vid)?;
            ctx.emit(Instruction::LocalGet(a_local));
            ctx.emit(Instruction::LocalGet(b_local));
            ctx.emit(Instruction::Call(concat_idx));
            let wasm_ty = ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(string_idx),
            });
            let local = ctx.allocate_local(vid, wasm_ty);
            ctx.emit(Instruction::LocalSet(local));
            Ok(())
        }
        Op::StringEq(a, b_vid) => {
            let vid = expect_result(instr, "Op::StringEq")?;
            let eq_idx = b.require_str_eq_idx()?;
            let a_local = ctx.binding_of(*a)?;
            let b_local = ctx.binding_of(*b_vid)?;
            ctx.emit(Instruction::LocalGet(a_local));
            ctx.emit(Instruction::LocalGet(b_local));
            ctx.emit(Instruction::Call(eq_idx));
            let local = ctx.allocate_local(vid, ValType::I32);
            ctx.emit(Instruction::LocalSet(local));
            Ok(())
        }
        Op::StringLt(a, b_vid) => {
            translate_str_lex_op(ctx, b, instr, *a, *b_vid, Instruction::I32LtS)
        }
        Op::StringLe(a, b_vid) => {
            translate_str_lex_op(ctx, b, instr, *a, *b_vid, Instruction::I32LeS)
        }
        Op::StringGt(a, b_vid) => {
            translate_str_lex_op(ctx, b, instr, *a, *b_vid, Instruction::I32GtS)
        }
        Op::StringGe(a, b_vid) => {
            translate_str_lex_op(ctx, b, instr, *a, *b_vid, Instruction::I32GeS)
        }
        Op::StringNe(a, b_vid) => {
            let vid = expect_result(instr, "Op::StringNe")?;
            let eq_idx = b.require_str_eq_idx()?;
            let a_local = ctx.binding_of(*a)?;
            let b_local = ctx.binding_of(*b_vid)?;
            ctx.emit(Instruction::LocalGet(a_local));
            ctx.emit(Instruction::LocalGet(b_local));
            ctx.emit(Instruction::Call(eq_idx));
            // NE = !EQ — flip the helper's 0/1 result with i32.eqz
            // (0 → 1, 1 → 0).
            ctx.emit(Instruction::I32Eqz);
            let local = ctx.allocate_local(vid, ValType::I32);
            ctx.emit(Instruction::LocalSet(local));
            Ok(())
        }
        Op::StructSetField(obj, field_idx, val) => {
            translate_struct_set(ctx, b, *obj, *field_idx, *val)
        }
        Op::ListAlloc(elems) => lists::translate_list_alloc(ctx, b, elems, instr),
        Op::MapAlloc(pairs) => maps::translate_map_alloc(ctx, b, pairs, instr),
        // dyn-trait ops — §Phase 2.4 decision K.10.
        Op::DynAlloc(trait_name, concrete, value) => {
            dyn_trait::translate_dyn_alloc(ctx, b, trait_name, concrete, *value, instr)
        }
        Op::DynCall(trait_name, slot, receiver, args) => {
            dyn_trait::translate_dyn_call(ctx, b, trait_name, *slot, *receiver, args, instr)
        }
        // Closure ops — §Phase 2.4 decision K.8: per-signature subtype
        // hierarchy over typed function references (`call_ref`).
        Op::ClosureAlloc(target, captures) => {
            closures::translate_closure_alloc(ctx, b, *target, captures, instr)
        }
        Op::CallIndirect(closure, args) => {
            closures::translate_call_indirect(ctx, b, *closure, args, instr)
        }
        Op::ClosureLoadCapture(env, capture_idx) => {
            closures::translate_closure_load_capture(ctx, b, *env, *capture_idx, instr)
        }
        other => Err(CompileError::new(format!(
            "wasm32-gc MVP: IR op `{other:?}` not yet supported \
             (Phase 2.4 PR 5 slices 1-3 cover arithmetic / control \
              flow / direct calls / struct ops — closures / \
              maps / dyn land in PR 6 follow-up slices)"
        ))),
    }
}

/// Lower `Op::StructAlloc(name, vals)` to `struct.new $name`, arity-checking
/// the field-value count against `IrModule::struct_layouts` first. The result
/// is bound as the nullable `(ref null $struct_idx)` — `struct.new` yields the
/// non-nullable `(ref $struct_idx)`, but the subtype `(ref $T) <: (ref null
/// $T)` lets it settle (without a coercion) into a local typed to compose with
/// null-defaulting `Op::Alloca` slots. See §Phase 2.4 decision K.1.
fn translate_struct_alloc(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    ir_module: &IrModule,
    name: &str,
    vals: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "Op::StructAlloc")?;
    let struct_idx = b.require_phx_struct(name)?;
    let layout = require_struct_layout(ir_module, name)?;
    if vals.len() != layout.len() {
        return Err(CompileError::new(format!(
            "wasm32-gc: `Op::StructAlloc({name:?})` has {} field \
             values but the struct layout declares {} fields \
             (IR verifier should have caught this)",
            vals.len(),
            layout.len(),
        )));
    }
    for v in vals {
        let local = ctx.binding_of(*v)?;
        ctx.emit(Instruction::LocalGet(local));
    }
    ctx.emit(Instruction::StructNew(struct_idx));
    let wasm_ty = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(struct_idx),
    });
    let local = ctx.allocate_local(vid, wasm_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// Lower `Op::StructGetField(obj, field_idx)` to `struct.get`. The
/// struct-type index comes from the receiver binding's `ValType` (see
/// [`struct_idx_of_binding`]) and the field index is bounds-checked first.
/// The field's declared type is *not* checked against `instr.result_type`
/// here — that agreement is trusted from sema and gated by the IR verifier;
/// a mismatch would surface only as a `wasmparser` type error.
fn translate_struct_get(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    obj: ValueId,
    field_idx: u32,
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "Op::StructGetField")?;
    let obj_local = ctx.binding_of(obj)?;
    let struct_idx = struct_idx_of_binding(ctx, obj, "Op::StructGetField receiver")?;
    check_field_index(b, struct_idx, field_idx, "Op::StructGetField")?;
    ctx.emit(Instruction::LocalGet(obj_local));
    // `struct.get` accepts nullable refs and traps on null — matching
    // Phoenix's "no null structs" invariant: a null value can only appear
    // in a slot that was Alloca'd but never Stored, which sema prohibits.
    // No explicit null-check or ref.cast needed.
    ctx.emit(Instruction::StructGet {
        struct_type_index: struct_idx,
        field_index: field_idx,
    });
    let wasm_ty = single_slot(&instr.result_type, b, "Op::StructGetField result")?;
    let local = ctx.allocate_local(vid, wasm_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// Lower `Op::StructSetField(obj, field_idx, val)` to `struct.set`, the
/// write sibling of [`translate_struct_get`] (same index recovery and
/// bounds check; `val`'s type agreement with the field is likewise trusted
/// from sema). The op has no result; a future IR change attaching one would
/// need the verifier as the gate, not this translator.
fn translate_struct_set(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    obj: ValueId,
    field_idx: u32,
    val: ValueId,
) -> Result<(), CompileError> {
    let obj_local = ctx.binding_of(obj)?;
    let val_local = ctx.binding_of(val)?;
    let struct_idx = struct_idx_of_binding(ctx, obj, "Op::StructSetField receiver")?;
    check_field_index(b, struct_idx, field_idx, "Op::StructSetField")?;
    ctx.emit(Instruction::LocalGet(obj_local));
    ctx.emit(Instruction::LocalGet(val_local));
    ctx.emit(Instruction::StructSet {
        struct_type_index: struct_idx,
        field_index: field_idx,
    });
    Ok(())
}

/// Lower `Op::EnumAlloc(name, variant_idx, fields)` to `struct.new`
/// against the variant's WASM-GC subtype. Pushes the discriminant
/// constant followed by each field value, then `struct.new`. The
/// result is bound at the WASM type of the **parent**, not the
/// variant — every Phoenix SSA enum value flows through the parent
/// type at locals / params / block params (`(ref null $enum_parent)`),
/// and the concrete variant `(ref $enum_Var)` upcasts to it via
/// subtype subsumption. See §Phase 2.4 decision K.4.
fn translate_enum_alloc(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    ir_module: &IrModule,
    name: &str,
    variant_idx: u32,
    vals: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "Op::EnumAlloc")?;
    // The IR carries just the template name on `Op::EnumAlloc`, but
    // the instruction's `result_type` is `EnumRef(name, type_args)` —
    // we read the concrete instantiation from there and look up the
    // monomorphized WASM enum.
    let type_args = match &instr.result_type {
        IrType::EnumRef(rname, args) if rname == name => args.clone(),
        other => {
            return Err(CompileError::new(format!(
                "wasm32-gc: `Op::EnumAlloc({name:?})` has `result_type` \
                 `{other:?}`, expected `EnumRef({name:?}, _)` (internal \
                 compiler bug — IR verifier should have caught this)"
            )));
        }
    };
    let parent_idx = b.require_enum_parent_idx(name, &type_args)?;
    let variant_struct_idx = b.require_enum_variant_idx(name, &type_args, variant_idx)?;
    // Arity check against the IR enum layout — IR verifier should
    // have caught a mismatch, but a wrong arity here would emit a
    // structurally invalid `struct.new` that wasmparser would only
    // reject deep in binary decoding.
    let expected = b
        .enum_variant_field_count(ir_module, name, variant_idx)
        .ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: `Op::EnumAlloc({name:?}, {variant_idx})` references \
                 an unknown variant (IR verifier should have caught this)"
            ))
        })?;
    if vals.len() != expected as usize {
        return Err(CompileError::new(format!(
            "wasm32-gc: `Op::EnumAlloc({name:?}, {variant_idx})` has {} field \
             values but the variant declares {expected} fields (IR verifier \
             should have caught this)",
            vals.len(),
        )));
    }
    // Push discriminant first (slot 0 of the variant struct, which
    // matches the parent's `$tag`), then payload fields in IR order.
    ctx.emit(Instruction::I32Const(variant_idx as i32));
    for v in vals {
        let local = ctx.binding_of(*v)?;
        ctx.emit(Instruction::LocalGet(local));
    }
    ctx.emit(Instruction::StructNew(variant_struct_idx));
    // Bind the result at the PARENT type. This way the same
    // `ValueId` can be passed to any enum-typed slot (function param,
    // block param, struct field, another enum variant field) without
    // a type mismatch, since all those slots are typed at the parent.
    let wasm_ty = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(parent_idx),
    });
    let local = ctx.allocate_local(vid, wasm_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// Lower `Op::EnumDiscriminant(value)` to `struct.get $parent 0` +
/// `i64.extend_i32_u`. The discriminant lives at slot 0 of every
/// variant struct (inherited from the parent), so reading through the
/// parent type is well-typed — no `ref.cast` needed. Phoenix `Int` is
/// i64, so the i32 tag widens unsigned. See §Phase 2.4 decision K.4.
fn translate_enum_discriminant(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    value: ValueId,
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "Op::EnumDiscriminant")?;
    let value_local = ctx.binding_of(value)?;
    let parent_idx = enum_parent_idx_of_binding(ctx, value, "Op::EnumDiscriminant receiver")?;
    // Confirm the binding's concrete type index is actually a recorded
    // enum parent before reading `$tag` through it. Symmetric with the
    // `enum_by_parent_idx` check `Op::EnumGetField` already performs:
    // `enum_parent_idx_of_binding` accepts any `(ref $concrete)` binding
    // (a struct ref looks identical), so without this guard a
    // mis-typed receiver would emit a `struct.get <idx> 0` that reads
    // some struct's first field as a discriminant instead of erroring.
    if b.enum_by_parent_idx(parent_idx).is_none() {
        return Err(CompileError::new(format!(
            "wasm32-gc: `Op::EnumDiscriminant` receiver `ValueId({value:?})` \
             is bound to type index {parent_idx}, which is not a recorded \
             enum parent — the receiver must be an enum value (internal \
             compiler bug)"
        )));
    }
    ctx.emit(Instruction::LocalGet(value_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: parent_idx,
        field_index: 0,
    });
    ctx.emit(Instruction::I64ExtendI32U);
    let local = ctx.allocate_local(vid, ValType::I64);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// Lower `Op::EnumGetField(obj, variant_idx, field_idx)` to a
/// `ref.cast` followed by `struct.get`. The IR field index is
/// shifted by 1 in the variant struct because slot 0 of the variant
/// is the inherited `$tag`. See §Phase 2.4 decision K.4.
fn translate_enum_get_field(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    ir_module: &IrModule,
    obj: ValueId,
    variant_idx: u32,
    field_idx: u32,
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "Op::EnumGetField")?;
    let obj_local = ctx.binding_of(obj)?;
    let parent_idx = enum_parent_idx_of_binding(ctx, obj, "Op::EnumGetField receiver")?;
    // Recover the enum's Phoenix name from the parent index so we can
    // look up the variant's struct index — the receiver's binding
    // carries the parent type only, not the variant, by design (so
    // SSA enum values are uniformly typed).
    let ((enum_name, _type_args), variant_indices) =
        b.enum_by_parent_idx(parent_idx).ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: `Op::EnumGetField` receiver `ValueId({obj:?})` is \
                 bound to enum-parent type index {parent_idx} that has no \
                 recorded enum (internal compiler bug)"
            ))
        })?;
    let enum_name = enum_name.clone();
    if (variant_idx as usize) >= variant_indices.len() {
        return Err(CompileError::new(format!(
            "wasm32-gc: `Op::EnumGetField({enum_name:?}, {variant_idx})` is out \
             of range — the enum has {} variants (IR verifier should have \
             caught this)",
            variant_indices.len()
        )));
    }
    let variant_struct_idx = variant_indices[variant_idx as usize];
    // Bounds-check the field index against the IR variant layout
    // (excluding the inherited `$tag` at slot 0).
    let expected_fields = b
        .enum_variant_field_count(ir_module, &enum_name, variant_idx)
        .ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: `Op::EnumGetField({enum_name:?}, {variant_idx})` \
                 references an unknown variant (internal compiler bug)"
            ))
        })?;
    if field_idx >= expected_fields {
        return Err(CompileError::new(format!(
            "wasm32-gc: `Op::EnumGetField({enum_name:?}, {variant_idx}, \
             {field_idx})` is out of range — the variant declares {expected_fields} \
             fields (IR verifier should have caught this)"
        )));
    }
    ctx.emit(Instruction::LocalGet(obj_local));
    // ref.cast narrows from (ref null $parent) to (ref $variant).
    // wasmtime treats this as an inline check against the runtime
    // type tag — cheap.
    ctx.emit(Instruction::RefCastNonNull(HeapType::Concrete(
        variant_struct_idx,
    )));
    ctx.emit(Instruction::StructGet {
        struct_type_index: variant_struct_idx,
        // +1 because slot 0 is the inherited `$tag`.
        field_index: field_idx + 1,
    });
    let wasm_ty = single_slot(&instr.result_type, b, "Op::EnumGetField result")?;
    let local = ctx.allocate_local(vid, wasm_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// Look up the WASM enum-parent-type-section index a binding's
/// `IrType::EnumRef` resolved to. Symmetric with
/// [`struct_idx_of_binding`]: enum values are bound at their parent
/// type, so the binding's `ValType` carries
/// `HeapType::Concrete(parent_idx)` directly.
pub(super) fn enum_parent_idx_of_binding(
    ctx: &FuncCtx,
    vid: ValueId,
    label: &str,
) -> Result<u32, CompileError> {
    let ty = ctx.binding_type_of(vid)?;
    match ty {
        ValType::Ref(RefType {
            heap_type: HeapType::Concrete(idx),
            ..
        }) => Ok(idx),
        other => Err(CompileError::new(format!(
            "wasm32-gc: {label} `ValueId({vid:?})` is bound to WASM type \
             `{other:?}`, not a concrete `(ref $enum_parent)` — enum ops \
             require an EnumRef-typed receiver (internal compiler bug)"
        ))),
    }
}

/// Look up the WASM-struct-type-section index a binding's
/// `IrType::StructRef` resolved to. Reads the binding's recorded
/// `ValType` (a `(ref null $idx)` for any StructRef value) and
/// extracts the index. Errors clearly if the binding isn't a concrete
/// ref — that's an internal compiler bug, since `Op::StructGetField` /
/// `Op::StructSetField` are only emitted by IR lowering against
/// StructRef-typed receivers.
fn struct_idx_of_binding(ctx: &FuncCtx, vid: ValueId, label: &str) -> Result<u32, CompileError> {
    let ty = ctx.binding_type_of(vid)?;
    match ty {
        ValType::Ref(RefType {
            heap_type: HeapType::Concrete(idx),
            ..
        }) => Ok(idx),
        other => Err(CompileError::new(format!(
            "wasm32-gc: {label} `{vid:?}` is bound to WASM type \
             `{other:?}`, not a concrete `(ref $struct_idx)` — \
             struct ops require a StructRef-typed receiver (internal \
             compiler bug)"
        ))),
    }
}

/// Bounds-check an IR field index against the receiver struct's
/// declared field count before emitting a `struct.get` / `struct.set`.
/// An out-of-range index would otherwise produce a module that only
/// `wasmparser` rejects, deep in binary decoding — this surfaces the
/// (internal-bug) mismatch with a precise diagnostic, mirroring the
/// arity check on `Op::StructAlloc`.
fn check_field_index(
    b: &ModuleBuilder,
    struct_idx: u32,
    field_idx: u32,
    label: &str,
) -> Result<(), CompileError> {
    let field_count = b.struct_field_count(struct_idx).ok_or_else(|| {
        CompileError::new(format!(
            "wasm32-gc: {label} references WASM struct type {struct_idx}, \
             which `reserve_phoenix_structs` recorded no field count for \
             (internal compiler bug)"
        ))
    })?;
    if field_idx >= field_count {
        return Err(CompileError::new(format!(
            "wasm32-gc: {label} field index {field_idx} is out of range for \
             WASM struct type {struct_idx}, which declares {field_count} \
             field(s) (IR verifier should have caught this)"
        )));
    }
    Ok(())
}

/// Resolve a struct layout by name, erroring with a clear diagnostic
/// (rather than panicking) if the name doesn't appear in
/// `IrModule::struct_layouts`. Used to arity-check `Op::StructAlloc`
/// before emitting the (silently wrong) WASM bytecode that an arity
/// mismatch would produce.
fn require_struct_layout<'a>(
    ir_module: &'a IrModule,
    name: &str,
) -> Result<&'a [(String, IrType)], CompileError> {
    ir_module
        .struct_layouts
        .get(name)
        .map(Vec::as_slice)
        .ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: struct `{name}` is referenced by an `Op::StructAlloc` \
                 but missing from `IrModule::struct_layouts` (internal compiler bug)"
            ))
        })
}

/// Emit a binary i64 → i64 op. Loads both operands, applies the
/// supplied WASM instruction, stores the result into a fresh i64
/// local bound to `instr.result`.
fn emit_i64_binop(
    ctx: &mut FuncCtx,
    instr: &phoenix_ir::instruction::Instruction,
    a: ValueId,
    b: ValueId,
    op: Instruction<'static>,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "i64 binop")?;
    let a_local = ctx.binding_of(a)?;
    let b_local = ctx.binding_of(b)?;
    ctx.emit(Instruction::LocalGet(a_local));
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(op);
    let local = ctx.allocate_local(vid, ValType::I64);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// Emit a binary comparison whose result is Phoenix `Bool` (WASM i32
/// 0/1). The operand WASM type is irrelevant here — i64, f64, and i32
/// comparison instructions all consume their operands off the stack
/// and push the same i32 0/1 — so this single helper serves every
/// comparison family: `IEq`…`IGe` (`i64.<cmp>`), `FEq`…`FGe`
/// (`f64.<cmp>`), and `BoolEq`/`BoolNe` (`i32.<cmp>`). The caller picks
/// the WASM instruction; only the i32 result type is fixed here.
fn emit_cmp(
    ctx: &mut FuncCtx,
    instr: &phoenix_ir::instruction::Instruction,
    a: ValueId,
    b: ValueId,
    op: Instruction<'static>,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "cmp")?;
    let a_local = ctx.binding_of(a)?;
    let b_local = ctx.binding_of(b)?;
    ctx.emit(Instruction::LocalGet(a_local));
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(op);
    let local = ctx.allocate_local(vid, ValType::I32);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// Emit a binary f64 → f64 op. Same shape as [`emit_i64_binop`] but
/// for Float arithmetic — `FAdd` / `FSub` / `FMul` / `FDiv` route
/// through here. `op` need not be a pure opcode: `FMod` passes an
/// `Instruction::Call` to the `(f64, f64) → f64` `phx_fmod` helper,
/// which consumes the same two-operand stack shape.
fn emit_f64_binop(
    ctx: &mut FuncCtx,
    instr: &phoenix_ir::instruction::Instruction,
    a: ValueId,
    b: ValueId,
    op: Instruction<'static>,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "f64 binop")?;
    let a_local = ctx.binding_of(a)?;
    let b_local = ctx.binding_of(b)?;
    ctx.emit(Instruction::LocalGet(a_local));
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(op);
    let local = ctx.allocate_local(vid, ValType::F64);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

fn translate_builtin_call(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    name: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    match name {
        "print" => translate_print(ctx, b, args),
        "String.length" => translate_string_length(ctx, b, args, instr),
        "String.substring" => translate_string_substring(ctx, b, args, instr),
        "toString" => translate_to_string(ctx, b, args, instr),
        // Option/Result method builtins (`Option.map`, `Result.unwrap`,
        // …) — lowered in terms of the K.4 enum representation + K.8
        // closure calls. Dispatched by template prefix so the
        // per-method routing lives in `option_result.rs`.
        _ if name.starts_with("Option.") || name.starts_with("Result.") => {
            let (enum_name, method) = name
                .split_once('.')
                .expect("name starts with `Option.`/`Result.`");
            option_result::translate_builtin(ctx, b, enum_name, method, args, instr)
        }
        "List.length" => lists::translate_list_length(ctx, args, instr),
        "List.get" => lists::translate_list_get(ctx, b, args, instr),
        "List.push" => lists::translate_list_push(ctx, b, args, instr),
        "List.contains" => lists::translate_list_contains(ctx, b, args, instr),
        "List.take" => lists::translate_list_take_drop(ctx, b, args, instr, lists::ListSlice::Take),
        "List.drop" => lists::translate_list_take_drop(ctx, b, args, instr, lists::ListSlice::Drop),
        // Closure-taking List methods (§K.8 follow-up): each walks the
        // receiver's array calling a user closure per element.
        "List.map" => lists::translate_list_map(ctx, b, args, instr),
        "List.filter" => lists::translate_list_filter(ctx, b, args, instr),
        "List.reduce" => lists::translate_list_reduce(ctx, b, args, instr),
        "List.flatMap" => lists::translate_list_flat_map(ctx, b, args, instr),
        "List.sortBy" => lists::translate_list_sort_by(ctx, b, args, instr),
        // Map methods (§K.9): ordered association over parallel arrays.
        "Map.length" => maps::translate_map_length(ctx, b, args, instr),
        "Map.get" => maps::translate_map_get(ctx, b, args, instr),
        "Map.contains" => maps::translate_map_contains(ctx, b, args, instr),
        "Map.set" => maps::translate_map_set(ctx, b, args, instr),
        "Map.remove" => maps::translate_map_remove(ctx, b, args, instr),
        "Map.keys" => maps::translate_map_keys_or_values(ctx, b, args, instr, true),
        "Map.values" => maps::translate_map_keys_or_values(ctx, b, args, instr, false),
        "ListBuilder.alloc" => lists::translate_list_builder_alloc(ctx, b, instr),
        "ListBuilder.push" => lists::translate_list_builder_push(ctx, b, args),
        "ListBuilder.freeze" => lists::translate_list_builder_freeze(ctx, b, args, instr),
        other => Err(CompileError::new(format!(
            "wasm32-gc MVP: builtin `{other}` not yet supported \
             (PR 5 slice 1 covers `print(Int)`; PR 6 slices add \
              `print(String)` / `print(Bool)` / `print(Float)`, the \
              `String.*` surface, and the closure-free `List.*` / \
              `ListBuilder.*` surface; closure-taking list methods land \
              after the closure slice — §Phase 2.4 K.7)"
        ))),
    }
}

/// `String.substring(s, start, end) -> String` — dispatch to the
/// synthesized `phx_str_substring` helper. The helper walks code-point
/// boundaries and clamps in-line (see [`super::string_helpers::synthesize_str_substring`]).
fn translate_string_substring(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "BuiltinCall(\"String.substring\")")?;
    if args.len() != 3 {
        return Err(CompileError::new(format!(
            "wasm32-gc: `BuiltinCall(\"String.substring\")` requires 3 args \
             (string, start, end), got {} (internal compiler bug — IR \
             verifier should have caught this)",
            args.len()
        )));
    }
    let string_idx = b.require_string_type_idx()?;
    let substring_idx = b.require_str_substring_idx()?;
    for arg in args {
        let local = ctx.binding_of(*arg)?;
        ctx.emit(Instruction::LocalGet(local));
    }
    ctx.emit(Instruction::Call(substring_idx));
    let wasm_ty = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(string_idx),
    });
    let local = ctx.allocate_local(vid, wasm_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `String.length(s) -> Int` — calls the `phx_str_length` helper,
/// which walks code-point starts to return the char count (matching
/// Phoenix's char-indexed semantics and the runtime's
/// `s.chars().count()`). See the K.2 correction note for why this is a
/// helper rather than a `struct.get $len`.
fn translate_string_length(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "BuiltinCall(\"String.length\")")?;
    if args.len() != 1 {
        return Err(CompileError::new(format!(
            "wasm32-gc: `BuiltinCall(\"String.length\")` requires 1 arg \
             (the string), got {} (internal compiler bug — IR verifier \
             should have caught this)",
            args.len()
        )));
    }
    let length_idx = b.require_str_length_idx()?;
    let recv_local = ctx.binding_of(args[0])?;
    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::Call(length_idx));
    let local = ctx.allocate_local(vid, ValType::I64);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `toString(value) -> String` — dispatch on the operand's WASM
/// binding type to the matching `phx_tostring_*` constructor
/// (`i64` → decimal digits, `f64` → the shared ryu formatter,
/// `i32`-carried Bool → `"true"` / `"false"` literals), with
/// `toString(String)` lowered as a plain local copy (source-level
/// identity). Same ValType-keyed dispatch shape as `translate_print`.
fn translate_to_string(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "BuiltinCall(\"toString\")")?;
    if args.len() != 1 {
        return Err(CompileError::new(format!(
            "wasm32-gc: `toString` builtin takes exactly one argument; got {} \
             (IR verifier should have caught this)",
            args.len()
        )));
    }
    let arg_local = ctx.binding_of(args[0])?;
    let string_idx = b.require_string_type_idx()?;
    let string_ty = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(string_idx),
    });
    let helper_idx = match ctx.binding_type_of(args[0])? {
        ValType::I64 => b.require_tostring_i64_idx()?,
        ValType::F64 => b.require_tostring_f64_idx()?,
        // Bare i32 is Phoenix `Bool` — the same representation-keyed
        // assumption `translate_print` documents.
        ValType::I32 => b.require_tostring_bool_idx()?,
        ValType::Ref(RefType {
            heap_type: HeapType::Concrete(idx),
            ..
        }) if idx == string_idx => {
            // Identity: copy the existing `$string` ref into the
            // result binding.
            ctx.emit(Instruction::LocalGet(arg_local));
            let local = ctx.allocate_local(vid, string_ty);
            ctx.emit(Instruction::LocalSet(local));
            return Ok(());
        }
        other => {
            return Err(CompileError::new(format!(
                "wasm32-gc: `toString` argument lowered to `{other:?}`, which \
                 has no toString mapping yet (Int / Float / Bool / String are \
                 supported — matching the wasm32-linear surface; other types \
                 land with their own slices)"
            )));
        }
    };
    ctx.emit(Instruction::LocalGet(arg_local));
    ctx.emit(Instruction::Call(helper_idx));
    let local = ctx.allocate_local(vid, string_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `print(value)` — dispatch on the value's WASM binding type to the
/// matching synthesized helper (or inline emission for `Bool`).
/// Supports `Int`, `String`, `Bool`, and `Float`.
///
/// Dispatch shape: `i64` → `phx_print_i64` helper. `(ref null $string)`
/// → `phx_print_str` helper. `i32` carrying the Phoenix `Bool` lowers
/// inline (no helper) via an if/else that stages the iovec at one of
/// two pre-populated linear-memory regions. `f64` → `phx_print_f64`
/// helper (the shared `phx_ryu_format_f64` formatter + newline +
/// `fd_write`). See §Phase 2.4 decisions K.2 (string helper), K.3
/// (Bool inline shape), and K.6 (Float).
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
    let arg_local = ctx.binding_of(arg_vid)?;
    let arg_ty = ctx.binding_type_of(arg_vid)?;
    if arg_ty == ValType::I64 {
        let print_i64_idx = b.require_print_i64_idx()?;
        ctx.emit(Instruction::LocalGet(arg_local));
        ctx.emit(Instruction::Call(print_i64_idx));
        return Ok(());
    }
    if let ValType::Ref(RefType {
        heap_type: HeapType::Concrete(idx),
        ..
    }) = arg_ty
        && Some(idx) == b.string_type_idx_if_set()
    {
        let print_str_idx = b.require_print_str_idx()?;
        ctx.emit(Instruction::LocalGet(arg_local));
        ctx.emit(Instruction::Call(print_str_idx));
        return Ok(());
    }
    if arg_ty == ValType::I32 {
        // Treat `i32` as `Bool` — Phoenix's only `i32`-valued operand
        // type today is `Bool`. (Bool comparisons, BoolNot, etc. all
        // produce i32; struct/list refs are concrete refs and were
        // matched above.)
        emit_print_bool_inline(ctx, b, arg_local)?;
        return Ok(());
    }
    if arg_ty == ValType::F64 {
        let print_f64_idx = b.require_print_f64_idx()?;
        ctx.emit(Instruction::LocalGet(arg_local));
        ctx.emit(Instruction::Call(print_f64_idx));
        return Ok(());
    }
    Err(CompileError::new(format!(
        "wasm32-gc: `print(...)` supports `Int`, `String`, `Bool`, and \
         `Float` arguments (got a value lowered to WASM `{arg_ty:?}`)"
    )))
}

/// Inline emission for `print(Bool)`. Stages an iovec at
/// `IOVEC_OFFSET` pointing at either the pre-populated `"true\n"` or
/// `"false\n"` region, then calls `fd_write(1, IOVEC_OFFSET, 1,
/// NWRITTEN_OFFSET); drop`. Five instructions per arm of the if/else.
/// No helper call; the two data segments are emitted at module-build
/// time by `ModuleBuilder::declare_bool_data`.
fn emit_print_bool_inline(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    bool_local: u32,
) -> Result<(), CompileError> {
    // The two `"true\n"` / `"false\n"` segments this lowering reads from
    // are emitted by `declare_bool_data`, gated on `scan_helper_needs`
    // having flagged a `print(Bool)` site. Refuse to stage iovecs at
    // their offsets unless that ran — otherwise a scan/translate
    // divergence would emit a valid module that prints uninitialized
    // memory.
    b.require_bool_data()?;
    let fd_write_idx = b.require_fd_write_idx()?;
    let i32_memarg = wasm_encoder::MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    };
    // iovec.iov_ptr = (cond ? BOOL_TRUE_OFFSET : BOOL_FALSE_OFFSET)
    ctx.emit(Instruction::I32Const(module_builder::IOVEC_OFFSET as i32));
    ctx.emit(Instruction::I32Const(
        module_builder::BOOL_TRUE_OFFSET as i32,
    ));
    ctx.emit(Instruction::I32Const(
        module_builder::BOOL_FALSE_OFFSET as i32,
    ));
    ctx.emit(Instruction::LocalGet(bool_local));
    ctx.emit(Instruction::Select);
    ctx.emit(Instruction::I32Store(i32_memarg));
    // iovec.iov_len = (cond ? len("true\n") : len("false\n"))
    ctx.emit(Instruction::I32Const(
        module_builder::IOVEC_OFFSET as i32 + 4,
    ));
    ctx.emit(Instruction::I32Const(
        module_builder::BOOL_TRUE_BYTES.len() as i32
    ));
    ctx.emit(Instruction::I32Const(
        module_builder::BOOL_FALSE_BYTES.len() as i32,
    ));
    ctx.emit(Instruction::LocalGet(bool_local));
    ctx.emit(Instruction::Select);
    ctx.emit(Instruction::I32Store(i32_memarg));
    // fd_write(1, IOVEC_OFFSET, 1, NWRITTEN_OFFSET); drop
    ctx.emit(Instruction::I32Const(1)); // stdout
    ctx.emit(Instruction::I32Const(module_builder::IOVEC_OFFSET as i32));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Const(
        module_builder::NWRITTEN_OFFSET as i32,
    ));
    ctx.emit(Instruction::Call(fd_write_idx));
    ctx.emit(Instruction::Drop);
    Ok(())
}

/// Lower one of `Op::StringLt` / `Le` / `Gt` / `Ge`: call
/// `phx_str_cmp(a, b)`, push `i32.const 0`, and apply the supplied
/// signed-i32 comparison. The helper returns negative / zero /
/// positive, so the four comparisons against zero pick out the
/// matching strict-or-loose ordering. See §Phase 2.4 decision K.3.
fn translate_str_lex_op(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    instr: &phoenix_ir::instruction::Instruction,
    a: ValueId,
    b_vid: ValueId,
    cmp_op: Instruction<'static>,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "lex str cmp")?;
    let cmp_idx = b.require_str_cmp_idx()?;
    let a_local = ctx.binding_of(a)?;
    let b_local = ctx.binding_of(b_vid)?;
    ctx.emit(Instruction::LocalGet(a_local));
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(Instruction::Call(cmp_idx));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(cmp_op);
    let local = ctx.allocate_local(vid, ValType::I32);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// Translate a basic-block terminator. `dispatcher` is `Some` for
/// multi-block functions and `None` for single-block (which can fall
/// through to the function-level `end` for void return, or emit a
/// plain `return` for value-returning).
fn translate_terminator(
    ctx: &mut FuncCtx,
    term: &Terminator,
    _return_type: &IrType,
    dispatcher: Option<DispatcherContext>,
) -> Result<(), CompileError> {
    match term {
        Terminator::Return(None) => {
            // Always emit an explicit `return`. For single-block void
            // functions the function-level closing `end` would cover
            // it, but in a multi-block function an inner block's
            // `Return(None)` is *not* the last byte before that
            // closing `end` (there are sibling blocks afterwards),
            // and falling through to them would mis-execute the
            // dispatch fall-through. Emit unconditionally so both
            // shapes are correct.
            ctx.emit(Instruction::Return);
            Ok(())
        }
        Terminator::Return(Some(v)) => {
            let local = ctx.binding_of(*v)?;
            ctx.emit(Instruction::LocalGet(local));
            ctx.emit(Instruction::Return);
            Ok(())
        }
        Terminator::Jump { target, args } => {
            let d = require_dispatcher(dispatcher)?;
            emit_block_param_copies(ctx, *target, args)?;
            ctx.emit(Instruction::I32Const(target.0 as i32));
            ctx.emit(Instruction::LocalSet(d.dispatch_local));
            ctx.emit(Instruction::Br(d.depth_to_loop));
            Ok(())
        }
        Terminator::Branch {
            condition,
            true_block,
            true_args,
            false_block,
            false_args,
        } => {
            let d = require_dispatcher(dispatcher)?;
            let cond_local = ctx.binding_of(*condition)?;
            ctx.emit(Instruction::LocalGet(cond_local));
            ctx.emit(Instruction::If(BlockType::Empty));
            // Then-branch: copy args to true_block's params + set
            // dispatch local.
            emit_block_param_copies(ctx, *true_block, true_args)?;
            ctx.emit(Instruction::I32Const(true_block.0 as i32));
            ctx.emit(Instruction::LocalSet(d.dispatch_local));
            ctx.emit(Instruction::Else);
            emit_block_param_copies(ctx, *false_block, false_args)?;
            ctx.emit(Instruction::I32Const(false_block.0 as i32));
            ctx.emit(Instruction::LocalSet(d.dispatch_local));
            ctx.emit(Instruction::End); // close if/else
            // Both arms set the dispatch local; one `br $L` after the
            // if/else closes covers both paths. WASM's `If`/`End`
            // doesn't increase the visible label depth past the
            // closing `End`, so the dispatcher's recorded
            // `depth_to_loop` stays correct.
            ctx.emit(Instruction::Br(d.depth_to_loop));
            Ok(())
        }
        Terminator::Unreachable => {
            ctx.emit(Instruction::Unreachable);
            Ok(())
        }
        Terminator::Switch { .. } => Err(CompileError::new(
            "wasm32-gc: `Switch` terminator not yet emitted by IR lowering \
             — if it becomes reachable, extend the wasm32-gc terminator \
             translator alongside the IR change"
                .to_string(),
        )),
        Terminator::None => Err(CompileError::new(
            "wasm32-gc: encountered `Terminator::None` (placeholder for blocks \
             under construction) — IR verifier should have caught this"
                .to_string(),
        )),
    }
}

/// Copy each arg's local into the target block's matching param
/// local. The block-param locals were allocated up-front in
/// [`translate_multi_block`]'s pre-pass; this just emits the copy.
///
/// The copy is performed parallel-copy-safe: every source is pushed
/// onto the operand stack *before* any destination is written, then the
/// destinations are popped in reverse so dst_i receives src_i. Reading
/// all sources before writing any destination means a jump whose args
/// permute the target's params (e.g. a back edge that swaps two
/// loop-carried values) lowers correctly — no source is clobbered by an
/// earlier destination write. Today's fixtures don't exercise the swap
/// case (join blocks take zero or one param), but the pattern costs
/// nothing extra and removes the hazard for future loop lowerings.
///
/// LIMITATION: a `Jump` / `Branch` back to the **entry block**
/// (`BlockId(0)`) with args is not handled. Entry-block params alias
/// function-parameter locals and are deliberately *not* registered in
/// `block_param_locals` (see [`FuncCtx::new`] and the pre-pass in
/// [`translate_multi_block`]), so such a jump would fall into the
/// arity-mismatch arm below and report "target has 0 params" — a
/// misleading message for what is really "the dispatcher can't re-enter
/// the entry block." No current lowering emits a back-edge to the entry
/// (recursion calls the function afresh rather than jumping); loop
/// lowering, which could, lands in a later slice and must register the
/// entry's param destinations (or route loops through a dedicated header
/// block) before relying on this path.
fn emit_block_param_copies(
    ctx: &mut FuncCtx,
    target: BlockId,
    args: &[ValueId],
) -> Result<(), CompileError> {
    let dest_locals: Vec<u32> = ctx.block_param_locals_of(target).to_vec();
    if dest_locals.len() != args.len() {
        return Err(CompileError::new(format!(
            "wasm32-gc: jump to {target:?} has {} args but the target has \
             {} params (IR verifier should have caught this)",
            args.len(),
            dest_locals.len()
        )));
    }
    // Push every source value first...
    for arg in args {
        let src_local = ctx.binding_of(*arg)?;
        ctx.emit(Instruction::LocalGet(src_local));
    }
    // ...then drain into destinations in reverse (stack is LIFO, so the
    // last source pushed pops into the last destination).
    for dest_local in dest_locals.iter().rev() {
        ctx.emit(Instruction::LocalSet(*dest_local));
    }
    Ok(())
}

/// Enforce the dispatcher's `func.blocks[i].id == BlockId(i)`
/// invariant. The `br_table` dispatches by array position while
/// `Jump`/`Branch` set the dispatch local to a `BlockId` value, so a
/// mismatch would route control to the wrong block. Returns an error
/// (rather than a `debug_assert`) so release builds fail loudly
/// instead of emitting silently-wrong bytecode.
fn require_block_id_matches_index(
    block: &BasicBlock,
    block_idx: usize,
) -> Result<(), CompileError> {
    let expected = BlockId(block_idx as u32);
    if block.id != expected {
        return Err(CompileError::new(format!(
            "wasm32-gc: block at array index {block_idx} has id {:?}, expected \
             {expected:?} (dispatcher requires `func.blocks[i].id == BlockId(i)`)",
            block.id
        )));
    }
    Ok(())
}

fn require_dispatcher(
    dispatcher: Option<DispatcherContext>,
) -> Result<DispatcherContext, CompileError> {
    dispatcher.ok_or_else(|| {
        CompileError::new(
            "wasm32-gc: `Jump` / `Branch` reached in a single-block function \
             — the IR builder should never emit cross-block control flow \
             in a function with one block (internal compiler bug)"
                .to_string(),
        )
    })
}

pub(super) fn expect_result(
    instr: &phoenix_ir::instruction::Instruction,
    op_label: &str,
) -> Result<ValueId, CompileError> {
    instr.result.ok_or_else(|| {
        CompileError::new(format!(
            "wasm32-gc: `{op_label}` has no result binding (internal compiler bug)"
        ))
    })
}

pub(super) fn single_slot(
    ty: &IrType,
    b: &ModuleBuilder,
    label: &str,
) -> Result<ValType, CompileError> {
    let slots = wasm_valtypes_for(ty, b)?;
    if slots.len() != 1 {
        return Err(CompileError::new(format!(
            "wasm32-gc MVP: `{label}` expected a single-slot type, got \
             `{ty:?}` ({} slots)",
            slots.len()
        )));
    }
    Ok(slots[0])
}

/// Bind the result of a `call` / `call_ref` whose single result value
/// (if any) is already on the operand stack, following the WASM calling
/// convention's four `(result binding, return type)` cases. Shared by
/// `Op::Call` and `Op::DynCall` (the cast-and-`call` trampoline path) so
/// the two stay in lockstep — a change to one return shape can't drift
/// from the other. `op_label` names the op for the
/// internal-compiler-bug diagnostics.
pub(super) fn bind_call_result(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    instr: &phoenix_ir::instruction::Instruction,
    op_label: &str,
) -> Result<(), CompileError> {
    match (instr.result, &instr.result_type) {
        (Some(_), IrType::Void) => Err(CompileError::new(format!(
            "wasm32-gc: `{op_label}` has a result binding but a Void return \
             type (internal compiler bug)"
        ))),
        (Some(vid), ty) => {
            let wasm_ty = single_slot(ty, b, op_label)?;
            let local = ctx.allocate_local(vid, wasm_ty);
            ctx.emit(Instruction::LocalSet(local));
            Ok(())
        }
        (None, IrType::Void) => Ok(()),
        (None, ty) => Err(CompileError::new(format!(
            "wasm32-gc: `{op_label}` returns `{ty:?}` but has no result \
             binding (internal compiler bug)"
        ))),
    }
}
