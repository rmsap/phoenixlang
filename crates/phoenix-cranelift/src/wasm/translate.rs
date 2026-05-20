//! Per-function Phoenix IR → WebAssembly translation.
//!
//! # Op surface (PR 3b)
//!
//! - **Constants:** [`Op::ConstI64`], [`Op::ConstBool`].
//! - **Integer arithmetic:** [`Op::IAdd`] / [`Op::ISub`] / [`Op::IMul`] /
//!   [`Op::IDiv`] / [`Op::IMod`] / [`Op::INeg`]. Wrap-on-overflow for
//!   `+`/`-`/`*`/unary-`-` (matches the `Int` spec); `IDiv`/`IMod` trap
//!   on divide-by-zero and signed overflow, matching native's panic.
//! - **Integer & bool comparisons:** [`Op::IEq`] / [`Op::INe`] /
//!   [`Op::ILt`] / [`Op::IGt`] / [`Op::ILe`] / [`Op::IGe`],
//!   [`Op::BoolEq`] / [`Op::BoolNe`] / [`Op::BoolNot`].
//! - **Direct calls:** [`Op::Call`] to a Phoenix user function (resolved
//!   via [`ModuleBuilder::require_phx_user_func`]). Mutual recursion is
//!   supported — every concrete function is registered before any body
//!   is emitted.
//! - **Built-ins:** [`Op::BuiltinCall`] with name `"print"` routes to the
//!   matching `phx_print_*` runtime export, dispatching on the Phoenix
//!   `IrType` (`Int` / `Bool` today; `String` returns a PR 3c diagnostic).
//! - **Control flow:** every [`Terminator`] except `Switch` and `None`
//!   (which the IR verifier rejects). Multi-block functions use the
//!   loop+switch dispatcher described in
//!   [`docs/design-decisions.md`](../../../../docs/design-decisions.md)
//!   §Phase 2.4 decision G; single-block functions skip the dispatcher.
//! - **Multi-slot values:** [`IrType::StringRef`] is two WASM slots
//!   (`(i32 ptr, i32 len)`). Function parameters and returns flatten via
//!   [`wasm_valtypes_for`]; SSA bindings carry a `Vec<u32>` of locals;
//!   non-entry block params are restricted to single-slot today
//!   (deferred to PR 3c alongside the rest of the heap-aware surface).
//!
//! # Deferred to PR 3c
//!
//! [`Op::ConstString`] (data-section vs `__heap_base` collision), every
//! sret-returning runtime call (`toString`, string concat, list/map
//! methods), struct / enum / list / map / closure allocation, the
//! shadow-stack root-emission pass, `defer` exit-path emission, and
//! multi-slot non-entry block params. Each rejection in this file cites
//! PR 3c so a regression in the deferred-error wording is visible.
//!
//! # SSA → WASM-locals mapping
//!
//! WebAssembly's MVP has no SSA — it has typed locals and an operand
//! stack. Each Phoenix `ValueId` that an instruction defines maps to
//! one or more WASM locals (single-slot for most types; two slots for
//! `StringRef`). Phoenix function parameters bind to the auto-declared
//! WASM parameter locals; the entry block's params alias them. The
//! loop+switch dispatcher (multi-block functions) allocates one i32
//! local for the "next block ID" dispatch value, then a fresh local
//! per non-entry block parameter.
//!
//! # wasm-encoder construction order
//!
//! [`wasm_encoder::Function`] takes its local declarations up front,
//! before any instruction can be pushed. We therefore buffer
//! instructions and the locals list during the IR walk, then finalize
//! into a `Function` at the end. The buffer holds `Instruction<'static>`
//! — every op landed here owns its data, so there's no borrow churn.

use std::collections::HashMap;

use phoenix_ir::block::{BasicBlock, BlockId};
use phoenix_ir::instruction::{Op, ValueId};
use phoenix_ir::module::IrFunction;
use phoenix_ir::terminator::Terminator;
use phoenix_ir::types::IrType;
use wasm_encoder::{BlockType, Function, Instruction, ValType};

use super::module_builder::ModuleBuilder;
use crate::error::CompileError;

/// Map a Phoenix [`IrType`] to the single WASM [`ValType`] used to
/// represent it. Returns an error for types whose representation
/// requires more than one slot (e.g. `StringRef`'s `(ptr, len)`
/// fat pointer) — those callers go through [`wasm_valtypes_for`].
///
/// `IrType::Void` is rejected: `Void` is the absence of a value, not
/// a value of any slot type. Return-position handling routes through
/// [`wasm_return_valtypes`] instead.
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
        _ => Err(unsupported(ty, "single-slot wasm32-linear value rep")),
    }
}

/// Multi-slot version of [`wasm_valtype_for`]. Each Phoenix [`IrType`]
/// maps to a fixed number of WASM value-stack slots — most are
/// single-slot (1), `StringRef` is two (`i32 ptr`, `i32 len`), `Void`
/// is zero. PR 3c will extend this for `List` / `Map` / `Closure`
/// references (single `i32` each — pointers into the GC heap).
///
/// Used for both function-signature flattening
/// ([`super::module_builder::ModuleBuilder::declare_phoenix_functions`])
/// and per-SSA-value local allocation (each `Vec<ValType>` entry gets
/// its own WASM local).
pub(super) fn wasm_valtypes_for(ty: &IrType) -> Result<Vec<ValType>, CompileError> {
    match ty {
        IrType::I64 => Ok(vec![ValType::I64]),
        IrType::F64 => Ok(vec![ValType::F64]),
        IrType::Bool => Ok(vec![ValType::I32]),
        IrType::Void => Ok(Vec::new()),
        IrType::StringRef => Ok(vec![ValType::I32, ValType::I32]),
        _ => Err(unsupported(ty, "wasm32-linear value representation")),
    }
}

/// Map a Phoenix function's return [`IrType`] to a vector of WASM
/// [`ValType`]s. `Void` returns map to the empty vector; `StringRef`
/// returns to `[I32, I32]` (multi-value return for the fat pointer).
pub(super) fn wasm_return_valtypes(ty: &IrType) -> Result<Vec<ValType>, CompileError> {
    wasm_valtypes_for(ty)
}

/// Flatten a list of Phoenix parameter types into a WASM signature's
/// flattened param-list. Each multi-slot Phoenix type (currently only
/// `StringRef`) expands to multiple WASM `ValType`s in declaration
/// order — `(ptr, len)` for strings — so an `extern "C"` Rust fn with
/// a `PhxFatPtr` param sees `(i32, i32)` in WASM, matching what
/// `phoenix-runtime`'s compiled `phx_str_*` exports declare.
pub(super) fn flatten_param_types(params: &[IrType]) -> Result<Vec<ValType>, CompileError> {
    let mut out = Vec::with_capacity(params.len());
    for ty in params {
        out.extend(wasm_valtypes_for(ty)?);
    }
    Ok(out)
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
///
/// Single-block functions are emitted directly — the function body is
/// the block's instruction stream followed by its terminator. This
/// keeps PR 3a's hello.phx bytecode shape unchanged.
///
/// Multi-block functions are emitted via a loop+switch dispatcher
/// ([decision G](`docs/design-decisions.md`)): each basic block lives
/// inside a labeled WASM `block` nested inside an outer `loop`, and a
/// `br_table` at the top of the loop reads a "next block ID" local
/// and branches to the matching labeled block. Block-param SSA values
/// get fresh locals at the dispatcher's entry; `Jump` / `Branch`
/// terminators copy their args into those locals before re-entering
/// the dispatch.
pub(super) fn translate_function(
    b: &mut ModuleBuilder,
    func: &IrFunction,
) -> Result<Function, CompileError> {
    if func.blocks.is_empty() {
        return Err(CompileError::new(format!(
            "wasm32-linear: function `{}` has no blocks",
            func.name
        )));
    }

    let mut ctx = FuncTranslateCtx::new(func)?;
    if func.blocks.len() == 1 {
        translate_block(&mut ctx, b, &func.blocks[0], None)?;
    } else {
        translate_multi_block(&mut ctx, b, func)?;
    }
    // Every WASM function body must terminate with an `end` opcode
    // (0x0B) — `wasm_encoder::Function` requires it regardless of
    // reachability. Emitting it here as a single fixed point keeps
    // terminator translators from each having to think about
    // function-level closing.
    ctx.emit(Instruction::End);
    Ok(ctx.into_function())
}

/// Emit the loop+switch dispatcher for a multi-block function.
///
/// Structure (for 3 blocks, bb_0..bb_2):
///
/// ```text
/// loop $L                              ;; depth N+1 inside body
///   block $bb_2                        ;; depth N
///     block $bb_1                      ;; depth N-1
///       block $bb_0                    ;; depth 0
///         local.get $dispatch
///         br_table 0 1 2 0             ;; default = bb_0 (unreachable)
///       end                            ;; close $bb_0
///       ;; bb_0 body+terminator (br $L or return)
///     end                              ;; close $bb_1
///     ;; bb_1 body+terminator
///   end                                ;; close $bb_2
///   ;; bb_2 body+terminator
/// end                                  ;; close $L
/// unreachable
/// ```
///
/// `br_table`'s targets are *label indices* relative to the current
/// nesting depth. At the dispatcher's `br_table` site, $bb_i has
/// depth `(n_blocks - 1 - i)`, so the table is filled with that
/// formula and `0` (= $bb_0) as the unreachable default.
///
/// Each block's body terminator emits the appropriate WASM control
/// transfer: `Return` → `return` (function-level exit, ignores
/// nesting); `Jump` / `Branch` → set the dispatch local + `br <depth
/// of $L from here>`. The depth of $L from inside bb_i's body is
/// `(n_blocks - 1 - i)`.
fn translate_multi_block(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    func: &IrFunction,
) -> Result<(), CompileError> {
    let n_blocks = func.blocks.len();
    debug_assert!(n_blocks > 1, "translate_multi_block called with <= 1 block");

    // Allocate the dispatch local *first* so its index is stable
    // before any block-param locals get assigned. Initial value of an
    // i32 local is 0, which matches the entry-block ID (`BlockId(0)`)
    // — no explicit init needed.
    let dispatch_local = ctx.allocate_dispatch_local();

    // Allocate locals for non-entry block params (the entry block's
    // params are the function parameters, already bound in
    // `FuncTranslateCtx::new`). Each non-entry block-param gets its
    // own WASM local; `Jump` / `Branch` terminators copy their args
    // into these locals before dispatching.
    //
    // The dispatcher relies on `func.blocks[i].id == BlockId(i)` —
    // both for the br_table identity-vector (target = block index =
    // BlockId) and for the `Jump`/`Branch` terminators that write the
    // target BlockId into the dispatch local. `IrFunction::create_block`
    // (phoenix-ir/src/module.rs) appends in BlockId order, so this
    // holds by construction today; the assert catches a future IR
    // refactor that reorders or deletes blocks before that change
    // ships an opaque wasmparser-validation error.
    for (block_idx, block) in func.blocks.iter().enumerate().skip(1) {
        let block_id = BlockId(block_idx as u32);
        debug_assert_eq!(
            block.id, block_id,
            "wasm32-linear: block at array index {block_idx} has id {:?}, \
             expected {:?} — the loop+switch dispatcher assumes \
             `func.blocks[i].id == BlockId(i)` (internal compiler bug — \
             IR builder invariant violated)",
            block.id, block_id,
        );
        for (vid, ty) in &block.params {
            let wasm_ty = wasm_valtype_for(ty)?;
            let local = ctx.allocate_local(*vid, wasm_ty, ty.clone());
            ctx.register_block_param(block_id, local);
        }
    }

    // Open the outer loop.
    ctx.emit(Instruction::Loop(BlockType::Empty));

    // Open one labeled block per Phoenix basic block, nested deepest-
    // first (bb_0 innermost). The `n_blocks` `End` markers later close
    // these in reverse — see body emission below.
    for _ in 0..n_blocks {
        ctx.emit(Instruction::Block(BlockType::Empty));
    }

    // Emit the dispatch table at the innermost point. The br_table
    // targets are *label depths* relative to the dispatcher site;
    // $bb_i sits at depth `i` (innermost-first: the last block opened
    // is $bb_0 at depth 0, the first opened is $bb_(N-1) at depth
    // N-1), so the table is the identity vector `[0, 1, ..., N-1]`.
    // The default target (consulted when the index is out of range)
    // is `0` (= $bb_0) — unreachable in a well-formed program but
    // required by the br_table opcode.
    ctx.emit(Instruction::LocalGet(dispatch_local));
    let table_targets: Vec<u32> = (0..n_blocks as u32).collect();
    ctx.emit(Instruction::BrTable(
        std::borrow::Cow::Owned(table_targets),
        0,
    ));

    // Emit each block's body+terminator. Between every pair of
    // bodies, close the corresponding labeled block (so the next br
    // target's label index naturally decreases).
    for (block_idx, block) in func.blocks.iter().enumerate() {
        // Re-assert the BlockId-vs-index invariant at the body-emission
        // site too: a future change that walked `func.blocks` in a
        // different order (or that filtered/deduped blocks between the
        // two loops) would otherwise emit bodies in br_table-disagreeing
        // order. Cheap insurance against a subtle miscompile.
        debug_assert_eq!(
            block.id,
            BlockId(block_idx as u32),
            "wasm32-linear: block at array index {block_idx} has id {:?} \
             at body-emission time — dispatcher index ordering invariant \
             violated (internal compiler bug)",
            block.id,
        );
        // Close the labeled block whose body we're about to emit. For
        // bb_0 this closes the innermost `(block $bb_0)`; for bb_N-1
        // this closes the outermost.
        ctx.emit(Instruction::End);
        // Emit bb_i body. Terminator handling needs to know the depth
        // of $L from this position so a `br $L` translates to the
        // right label index.
        let depth_to_loop = (n_blocks - 1 - block_idx) as u32;
        translate_block(
            ctx,
            b,
            block,
            Some(DispatcherContext {
                depth_to_loop,
                dispatch_local,
            }),
        )?;
    }

    // Close the outer loop and emit an unreachable sentinel — every
    // block ends with a terminator (`return` or `br $L`), so falling
    // off the loop is impossible. Emitting `unreachable` here keeps
    // wasmparser happy without us having to declare the loop's
    // signature.
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::Unreachable);
    Ok(())
}

/// Codegen-side metadata recorded for every Phoenix `ValueId` bound
/// during translation: the WASM local slot(s) it occupies, and the
/// original Phoenix [`IrType`].
///
/// Most Phoenix types are single-slot (one WASM local each) — `Int`,
/// `Float`, `Bool`. `StringRef` is two-slot: `locals[0]` holds the
/// `i32` data pointer and `locals[1]` holds the `i32` byte length.
/// PR 3c's `List` / `Map` / `Closure` references will each be
/// single-slot pointers into the GC heap. The slot count for a given
/// type is fixed by [`wasm_valtypes_for`].
///
/// `ir_type` is retained so dispatchers like the print-builtin
/// translator can route on the *Phoenix* type even when several IR
/// types collapse to the same WASM `ValType` (e.g. `Bool` and a
/// raw `i32` heap-pointer both occupy `ValType::I32`).
struct ValueBinding {
    locals: Vec<u32>,
    ir_type: IrType,
}

impl ValueBinding {
    /// Convenience accessor for the single-slot case. Panics on a
    /// multi-slot binding — callers that handle multi-slot values
    /// (`print(str)`, returning `String`, etc.) go through `locals`
    /// directly via `emit_load_all` / `emit_store_result`.
    ///
    /// Asserts in release as well as debug: a silent `locals[0]` read
    /// on a `StringRef` binding would forward the `ptr` slot only and
    /// miscompile (the `len` slot would still occupy a WASM local with
    /// no codegen referencing it, and the operand stack would be off
    /// by one for the next instruction). Catching this at codegen time
    /// is better than a far-removed wasmparser validation error.
    fn single_local(&self) -> u32 {
        assert_eq!(
            self.locals.len(),
            1,
            "single_local called on a multi-slot binding for IR type {:?} \
             (internal compiler bug — caller should route through `locals` \
             via emit_load_all / emit_store_result)",
            self.ir_type
        );
        self.locals[0]
    }
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
    /// Per-block param WASM locals, indexed by [`BlockId`]. The entry-
    /// block params bind to function-parameter locals and are NOT
    /// listed here (`BlockId(0)` resolves to function params via
    /// `func.param_types`). Non-entry blocks' params get fresh locals
    /// allocated in [`translate_multi_block`]; `Jump` / `Branch`
    /// terminators look them up here when copying args before
    /// dispatching.
    block_param_locals: HashMap<BlockId, Vec<u32>>,
    /// Next WASM local index to assign for an instruction-result
    /// value. Initialized past the parameter locals.
    next_local: u32,
}

/// Dispatcher context shared by [`translate_block`] and the
/// terminator translator when the function uses loop+switch dispatch.
/// `Some` for multi-block functions, `None` for single-block (which
/// can `return` directly without touching the dispatch local).
#[derive(Debug, Clone, Copy)]
struct DispatcherContext {
    /// Label depth from a block's body to the outer `(loop $L)`.
    /// Used by `Jump` / `Branch` terminators to compute the operand
    /// to `br <depth>` that re-enters the dispatch.
    depth_to_loop: u32,
    /// WASM local holding the "next block ID" dispatch value.
    /// `Jump` / `Branch` write here before branching to `$L`.
    dispatch_local: u32,
}

impl FuncTranslateCtx {
    fn new(func: &IrFunction) -> Result<Self, CompileError> {
        let mut bindings: HashMap<ValueId, ValueBinding> = HashMap::new();

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
        // WASM auto-declares one local per `ValType` in the function's
        // flat parameter list. Phoenix params expand to 1+ slots each
        // via `wasm_valtypes_for` — a `StringRef` param occupies two
        // consecutive WASM-local indices, etc. Walk the entry-block
        // params side-by-side with the function's `param_types` to
        // bind each Phoenix `ValueId` to the right slot range.
        let mut next_wasm_local: u32 = 0;
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
                // Cross-check that the entry-block param type matches
                // the function-signature param type. They agree today
                // by construction but a future refactor could silently
                // shift slot indices; the assertion catches that
                // before wasmparser does.
                let entry_slots = wasm_valtypes_for(ty)?;
                let sig_slots = wasm_valtypes_for(&func.param_types[i])?;
                debug_assert_eq!(
                    entry_slots, sig_slots,
                    "wasm32-linear: entry-block param {i} valtypes ({entry_slots:?}) \
                     disagree with function signature ({sig_slots:?}) in `{}`",
                    func.name,
                );
                let n_slots = entry_slots.len() as u32;
                let locals: Vec<u32> = (next_wasm_local..next_wasm_local + n_slots).collect();
                next_wasm_local += n_slots;
                bindings.insert(
                    *vid,
                    ValueBinding {
                        locals,
                        ir_type: ty.clone(),
                    },
                );
            }
        }
        // `next_wasm_local` now equals the flattened WASM parameter
        // count — also the index where instruction-result locals
        // start. The function signature was built from the same
        // flatten, so this matches the WASM ABI exactly.

        Ok(Self {
            instructions: Vec::new(),
            pending_locals: Vec::new(),
            bindings,
            block_param_locals: HashMap::new(),
            next_local: next_wasm_local,
        })
    }

    /// Allocate the `i32` "next block ID" local used by the loop+switch
    /// dispatcher. Returns its WASM local index. Must be called before
    /// any block-param locals so the dispatch local sits at a stable
    /// position relative to function params (one local past the last
    /// function param).
    fn allocate_dispatch_local(&mut self) -> u32 {
        let idx = self.next_local;
        self.push_local_decl(ValType::I32);
        self.next_local += 1;
        idx
    }

    /// Record `local` as the WASM local that holds block-param `vid`
    /// for the given target `block`. Used by [`translate_multi_block`]
    /// when reserving locals for non-entry blocks' params.
    fn register_block_param(&mut self, block: BlockId, local: u32) {
        self.block_param_locals
            .entry(block)
            .or_default()
            .push(local);
    }

    /// Look up the WASM locals for a block's params, in declaration
    /// order. Empty if the block has no params (or is the entry
    /// block, whose params bind to function-parameter locals).
    fn block_param_locals_of(&self, block: BlockId) -> &[u32] {
        self.block_param_locals
            .get(&block)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Allocate a fresh single-slot WASM local of `wasm_ty` for the
    /// value `vid` (recording its originating Phoenix [`IrType`] for
    /// later type-based dispatch). Returns the WASM local index.
    /// Run-length-encodes consecutive same-type allocations so
    /// [`Self::into_function`] hands `wasm_encoder` the compressed
    /// locals representation directly.
    ///
    /// Multi-slot bindings (currently only `StringRef`) go through
    /// [`Self::allocate_locals_for_ir_type`] which allocates the
    /// matching number of consecutive slots.
    fn allocate_local(&mut self, vid: ValueId, wasm_ty: ValType, ir_ty: IrType) -> u32 {
        let idx = self.next_local;
        self.push_local_decl(wasm_ty);
        self.next_local += 1;
        self.bindings.insert(
            vid,
            ValueBinding {
                locals: vec![idx],
                ir_type: ir_ty,
            },
        );
        idx
    }

    /// Allocate the right number of consecutive WASM locals for the
    /// Phoenix [`IrType`] backing `vid`. Returns the locals' indices
    /// in declaration order (matching [`wasm_valtypes_for`]'s slot
    /// ordering — for `StringRef` that's `[ptr_local, len_local]`).
    fn allocate_locals_for_ir_type(
        &mut self,
        vid: ValueId,
        ir_ty: IrType,
    ) -> Result<Vec<u32>, CompileError> {
        let slots = wasm_valtypes_for(&ir_ty)?;
        let locals: Vec<u32> = (0..slots.len() as u32)
            .map(|offset| self.next_local + offset)
            .collect();
        for vt in &slots {
            self.push_local_decl(*vt);
            self.next_local += 1;
        }
        self.bindings.insert(
            vid,
            ValueBinding {
                locals: locals.clone(),
                ir_type: ir_ty,
            },
        );
        Ok(locals)
    }

    /// Allocate a fresh single-slot WASM local of `wasm_ty` that is
    /// *not* bound to any Phoenix `ValueId` — used as scratch by
    /// codegen sequences that need a temporary (e.g. *sret* calls
    /// holding the result-area pointer between the `i32.sub` and the
    /// `i32.load`s). Returns the WASM local index. Does not appear in
    /// `bindings` so future `binding_of` lookups won't find it; that's
    /// intentional — temps are private to the emission sequence that
    /// created them.
    fn allocate_temp_local(&mut self, wasm_ty: ValType) -> u32 {
        let idx = self.next_local;
        self.push_local_decl(wasm_ty);
        self.next_local += 1;
        idx
    }

    /// Append one local declaration of type `wasm_ty` to the RLE
    /// `pending_locals` list, merging into the trailing run when the
    /// types match.
    fn push_local_decl(&mut self, wasm_ty: ValType) {
        match self.pending_locals.last_mut() {
            Some((count, last_ty)) if *last_ty == wasm_ty => *count += 1,
            _ => self.pending_locals.push((1, wasm_ty)),
        }
    }

    /// Push an instruction onto the buffered body.
    fn emit(&mut self, instr: Instruction<'static>) {
        self.instructions.push(instr);
    }

    /// Emit a sequence of `local.get` for every WASM local backing
    /// the Phoenix `vid`, in declaration order. For single-slot
    /// values this is just one `local.get`; for `StringRef` it's
    /// `(local.get ptr_local) (local.get len_local)` so the operand
    /// stack ends up `[..., ptr, len]` — matching the call-arg order
    /// `phoenix-runtime`'s `extern "C" fn phx_print_str(ptr, len)`
    /// declares.
    fn emit_load_all(&mut self, vid: ValueId) -> Result<(), CompileError> {
        let locals = self.binding_of(vid)?.locals.clone();
        for local in locals {
            self.emit(Instruction::LocalGet(local));
        }
        Ok(())
    }

    /// Allocate the locals for a `vid` of the given Phoenix
    /// [`IrType`] and emit `local.set` instructions to pop the call's
    /// return value(s) off the operand stack into them. The stack
    /// effect is "pop N values, store in declaration order": multi-
    /// value returns push the first result *deepest* (so `local.set`
    /// runs in reverse declaration order, popping from the top each
    /// time).
    ///
    /// For a `StringRef` return, the runtime's compiled function
    /// pushes `[ptr, len]` onto the stack at the call's exit; we
    /// `local.set $len_local` first (popping `len`), then
    /// `local.set $ptr_local` (popping `ptr`).
    fn emit_store_result(&mut self, vid: ValueId, ir_type: IrType) -> Result<(), CompileError> {
        let locals = self.allocate_locals_for_ir_type(vid, ir_type)?;
        // Reverse: top-of-stack pops first, which is the last slot in
        // declaration order.
        for local in locals.iter().rev() {
            self.emit(Instruction::LocalSet(*local));
        }
        Ok(())
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

/// Translate a single basic block: every instruction, then the
/// terminator. `dispatcher` is `None` for single-block functions and
/// `Some` for multi-block — the terminator translator needs it to
/// route `Jump` / `Branch` through the dispatcher.
fn translate_block(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    block: &BasicBlock,
    dispatcher: Option<DispatcherContext>,
) -> Result<(), CompileError> {
    for instr in &block.instructions {
        translate_instruction(ctx, b, instr)?;
    }
    translate_terminator(ctx, b, &block.terminator, dispatcher)?;
    Ok(())
}

/// Pull the SSA result binding off an instruction. Every value-producing
/// op needs one; absence means the IR verifier let through an op that
/// would leave its result stranded on the WASM operand stack and fail
/// validation. Centralizing the diagnostic keeps the phrasing
/// consistent across the (still-growing) set of value-producing ops.
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
    b: &mut ModuleBuilder,
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
        Op::ConstString(s) => {
            // Decision H (string-literal materialization): place the
            // bytes in a user data segment at low offsets (below the
            // runtime's stack region), then push the segment's
            // `(offset, len)` directly as a 2-slot `StringRef` fat
            // pointer. The runtime's `phx_print_str` / `phx_str_concat`
            // / etc. treat their fat-pointer args as borrowed slices,
            // so a data-section pointer composes uniformly with heap
            // pointers produced by runtime ops — no shadow-stack
            // rooting needed for literals (they live in the data
            // section forever).
            //
            // Bounded stack-collision risk: the runtime's stack grows
            // down from offset 1048576 and for the current fixture
            // set stays comfortably above the user-data region. The
            // codegen-time tripwire is `reserve_user_data`'s upper
            // bound (`USER_DATA_LIMIT = STACK_REGION_BASE -
            // STACK_SAFETY_MARGIN`); a measured stack high-water-
            // mark check is on the table for a Phase 2.5 follow-up
            // if deeper-recursion programs surface a collision.
            let vid = expect_result(instr, "Op::ConstString")?;
            let (offset, len) = b.reserve_user_data(s.as_bytes())?;
            ctx.emit(Instruction::I32Const(offset as i32));
            ctx.emit(Instruction::I32Const(len as i32));
            ctx.emit_store_result(vid, IrType::StringRef)?;
        }
        // Integer arithmetic. Phoenix maps `Int` → `i64`; every op
        // here produces an `i64` result.
        //
        // `IAdd` / `ISub` / `IMul` / `INeg` lower to WASM ops that
        // wrap silently on overflow, matching Phoenix's spec ("`Int`
        // is wrapping on overflow") and Rust's release-mode two's-
        // complement semantics on native.
        //
        // `IDiv` / `IMod` lower to `i64.div_s` / `i64.rem_s`. Per the
        // WASM spec, `div_s` traps on both division-by-zero and
        // signed-overflow (`i64::MIN / -1`); `rem_s` traps only on
        // zero. Native Rust panics on the same two cases. So the
        // trap behavior matches end-to-end, but the wrap rationale
        // above does *not* apply to these two — flag this if a
        // future op-coverage pass moves Div/Mod to a wrapping form.
        Op::IAdd(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, "IAdd", Instruction::I64Add)?,
        Op::ISub(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, "ISub", Instruction::I64Sub)?,
        Op::IMul(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, "IMul", Instruction::I64Mul)?,
        Op::IDiv(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, "IDiv", Instruction::I64DivS)?,
        Op::IMod(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, "IMod", Instruction::I64RemS)?,
        Op::INeg(a) => {
            // WASM MVP has no `i64.neg`; the canonical lowering is
            // `0 - a` via i64.sub. Two's-complement wrap on
            // `i64::MIN` matches native (per `docs/design-decisions.md`
            // §Numeric error semantics — `Int` negation wraps).
            let vid = expect_result(instr, "Op::INeg")?;
            let a_local = ctx.binding_of(*a)?.single_local();
            ctx.emit(Instruction::I64Const(0));
            ctx.emit(Instruction::LocalGet(a_local));
            ctx.emit(Instruction::I64Sub);
            let result_local = ctx.allocate_local(vid, ValType::I64, IrType::I64);
            ctx.emit(Instruction::LocalSet(result_local));
        }
        // Integer comparisons → produce a `Bool` (WASM `i32` 0/1).
        Op::IEq(a, b_) => emit_i64_cmp(ctx, instr, *a, *b_, "IEq", Instruction::I64Eq)?,
        Op::INe(a, b_) => emit_i64_cmp(ctx, instr, *a, *b_, "INe", Instruction::I64Ne)?,
        Op::ILt(a, b_) => emit_i64_cmp(ctx, instr, *a, *b_, "ILt", Instruction::I64LtS)?,
        Op::IGt(a, b_) => emit_i64_cmp(ctx, instr, *a, *b_, "IGt", Instruction::I64GtS)?,
        Op::ILe(a, b_) => emit_i64_cmp(ctx, instr, *a, *b_, "ILe", Instruction::I64LeS)?,
        Op::IGe(a, b_) => emit_i64_cmp(ctx, instr, *a, *b_, "IGe", Instruction::I64GeS)?,
        // Bool ops.
        Op::BoolEq(a, b_) => emit_i32_cmp(ctx, instr, *a, *b_, "BoolEq", Instruction::I32Eq)?,
        Op::BoolNe(a, b_) => emit_i32_cmp(ctx, instr, *a, *b_, "BoolNe", Instruction::I32Ne)?,
        Op::BoolNot(a) => {
            let vid = expect_result(instr, "Op::BoolNot")?;
            let a_local = ctx.binding_of(*a)?.single_local();
            ctx.emit(Instruction::LocalGet(a_local));
            ctx.emit(Instruction::I32Eqz); // `eqz` = "is zero" = logical NOT for 0/1 bool
            let result_local = ctx.allocate_local(vid, ValType::I32, IrType::Bool);
            ctx.emit(Instruction::LocalSet(result_local));
        }
        // Direct function call to a Phoenix-user function.
        // `Op::Call(func_id, type_args, args)` — type_args are erased
        // post-monomorphization (the IR carries them for now but every
        // call into a concrete function has none left to resolve).
        Op::Call(func_id, type_args, args) => {
            // Catch a sema/IR regression that lets an unmonomorphized
            // call reach codegen. Without this, the call would silently
            // resolve to the template's `FuncId` and likely miscompile
            // (or hit `require_phx_user_func`'s missing-id error, which
            // is less specific). Debug-only because monomorphization is
            // a hard precondition the IR verifier enforces.
            debug_assert!(
                type_args.is_empty(),
                "wasm32-linear: `Op::Call({func_id:?})` reached codegen with \
                 {} unresolved type args — monomorphization should have erased \
                 them (internal compiler bug)",
                type_args.len(),
            );
            let target_idx = b.require_phx_user_func(*func_id)?;
            // Load each argument's slots onto the operand stack in
            // declaration order. Multi-slot args (`StringRef`) expand
            // to multiple `local.get`s; `emit_load_all` handles the
            // count and ordering.
            for arg in args {
                ctx.emit_load_all(*arg)?;
            }
            ctx.emit(Instruction::Call(target_idx));
            // Bind the result, if any. Two mismatches are both
            // internal-compiler-bug shapes the IR verifier should
            // reject — surface them here rather than letting them
            // silently corrupt the operand stack:
            //   * `result: Some` + `result_type: Void`: would emit an
            //     empty `LocalSet` chain (`emit_store_result` allocates
            //     zero locals, pops zero values — no-op — but the
            //     `vid` binding is then unusable).
            //   * `result: None` + `result_type: !Void`: the call
            //     pushes return slots no one consumes; the next
            //     instruction sees a stack-type mismatch that
            //     wasmparser reports far from the actual cause.
            match (instr.result, &instr.result_type) {
                (Some(_), IrType::Void) => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `Op::Call({func_id:?})` has a result \
                         binding but a Void return type (internal compiler bug)"
                    )));
                }
                (Some(vid), ty) => {
                    ctx.emit_store_result(vid, ty.clone())?;
                }
                (None, IrType::Void) => {}
                (None, ty) => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `Op::Call({func_id:?})` returns `{ty:?}` \
                         but has no result binding — the call's return slots \
                         would be stranded on the operand stack (internal \
                         compiler bug — IR verifier should bind every \
                         non-Void call result)"
                    )));
                }
            }
        }
        Op::BuiltinCall(name, args) => translate_builtin_call(ctx, b, name, args, instr)?,
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: IR op `{other:?}` not yet supported \
                 (Phase 2.4 PR 3b/3c — see docs/design-decisions.md §Phase 2.4 \
                 for the linear-memory port's full op coverage)"
            )));
        }
    }
    Ok(())
}

/// Emit an i64 binary arith op. Loads both operands, applies the WASM
/// instruction, stores the result into a fresh i64 local. Shared by
/// IAdd / ISub / IMul / IDiv / IMod.
fn emit_i64_binop(
    ctx: &mut FuncTranslateCtx,
    instr: &phoenix_ir::instruction::Instruction,
    a: ValueId,
    b: ValueId,
    op_name: &str,
    wasm_op: Instruction<'static>,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, &format!("Op::{op_name}"))?;
    let a_local = ctx.binding_of(a)?.single_local();
    let b_local = ctx.binding_of(b)?.single_local();
    ctx.emit(Instruction::LocalGet(a_local));
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(wasm_op);
    let result_local = ctx.allocate_local(vid, ValType::I64, IrType::I64);
    ctx.emit(Instruction::LocalSet(result_local));
    Ok(())
}

/// Emit an i64 comparison. Same shape as [`emit_i64_binop`] but the
/// result is `Bool` (WASM `i32`).
fn emit_i64_cmp(
    ctx: &mut FuncTranslateCtx,
    instr: &phoenix_ir::instruction::Instruction,
    a: ValueId,
    b: ValueId,
    op_name: &str,
    wasm_op: Instruction<'static>,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, &format!("Op::{op_name}"))?;
    let a_local = ctx.binding_of(a)?.single_local();
    let b_local = ctx.binding_of(b)?.single_local();
    ctx.emit(Instruction::LocalGet(a_local));
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(wasm_op);
    let result_local = ctx.allocate_local(vid, ValType::I32, IrType::Bool);
    ctx.emit(Instruction::LocalSet(result_local));
    Ok(())
}

/// Emit an i32 (bool) comparison.
fn emit_i32_cmp(
    ctx: &mut FuncTranslateCtx,
    instr: &phoenix_ir::instruction::Instruction,
    a: ValueId,
    b: ValueId,
    op_name: &str,
    wasm_op: Instruction<'static>,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, &format!("Op::{op_name}"))?;
    let a_local = ctx.binding_of(a)?.single_local();
    let b_local = ctx.binding_of(b)?.single_local();
    ctx.emit(Instruction::LocalGet(a_local));
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(wasm_op);
    let result_local = ctx.allocate_local(vid, ValType::I32, IrType::Bool);
    ctx.emit(Instruction::LocalSet(result_local));
    Ok(())
}

/// Translate a `BuiltinCall(name, args)`. PR 3b covers `print` for
/// `Int` / `Bool` / `String`, plus `toString(Int)`. Other builtins
/// (`toString(Float)` / `toString(Bool)`, string method calls,
/// list/map methods) defer to PR 3c.
fn translate_builtin_call(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    name: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    match name {
        "print" => translate_print_builtin(ctx, b, args),
        "toString" => translate_to_string_builtin(ctx, b, args, instr),
        other => Err(CompileError::new(format!(
            "wasm32-linear: builtin `{other}` not yet supported \
             (Phase 2.4 PR 3c — see docs/design-decisions.md §Phase 2.4)"
        ))),
    }
}

/// Translate `print(value)` — dispatch on the value's Phoenix
/// [`IrType`] to the matching `phx_print_*` runtime export.
fn translate_print_builtin(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
) -> Result<(), CompileError> {
    let arg = *args.first().ok_or_else(|| {
        CompileError::new(
            "wasm32-linear: `print` builtin called with zero arguments — \
             IR verifier should have caught this"
                .to_string(),
        )
    })?;
    let arg_ir_ty = ctx.binding_of(arg)?.ir_type.clone();
    match arg_ir_ty {
        IrType::I64 => {
            let idx = b.require_phx_func("phx_print_i64")?;
            ctx.emit_load_all(arg)?;
            ctx.emit(Instruction::Call(idx));
        }
        IrType::Bool => {
            let idx = b.require_phx_func("phx_print_bool")?;
            ctx.emit_load_all(arg)?;
            ctx.emit(Instruction::Call(idx));
        }
        IrType::StringRef => {
            // `phx_print_str(ptr: i32, len: i32) -> ()` — push the
            // fat pointer's two slots in declaration order. Works
            // uniformly for `Op::ConstString` data-section pointers
            // (decision H) and heap pointers produced by runtime ops
            // (`phx_str_concat`, `phx_i64_to_str`, …) because the
            // runtime treats the fat pointer as a borrowed slice.
            let idx = b.require_phx_func("phx_print_str")?;
            ctx.emit_load_all(arg)?;
            ctx.emit(Instruction::Call(idx));
        }
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: `print` on argument of IR type `{other:?}` \
                 not yet supported (Phase 2.4 PR 3c — see docs/design-decisions.md §Phase 2.4)"
            )));
        }
    }
    Ok(())
}

/// Translate `toString(value)` — convert a primitive Phoenix value to
/// a heap-allocated `String` via the runtime's `phx_*_to_str`
/// family. These runtime functions are declared `extern "C" fn(val) ->
/// PhxFatPtr` in Rust source; on wasm32-wasip1 the C ABI lowers a
/// 2-i32-field struct return via an implicit *sret* (struct-return)
/// pointer as the first argument. The caller (us) is responsible for
/// reserving 8 bytes of stack space, passing its pointer, and reading
/// back the `(ptr, len)` fat pointer the callee wrote there.
///
/// `toString(String)` is the identity — no runtime call needed; the
/// arg's two slots are copied straight into the result locals so the
/// rest of the translator can treat `toString` uniformly without
/// the caller having to know whether the source operand was already a
/// String.
///
/// Stack-pointer management uses the merged runtime's `__stack_pointer`
/// global (accessible via [`ModuleBuilder::require_stack_pointer_global`]).
/// We save the original SP into a local, subtract the frame size (16
/// bytes — 8 for `PhxFatPtr` plus 8 of headroom in case the struct
/// grows; only 4-byte alignment is actually required), invoke the
/// callee, load the result, then restore SP from the saved local.
/// Restoring from a saved copy (rather than `current_SP + 16`) is
/// robust against any future ABI quirk where a callee fails to
/// restore SP on its return path: even if SP is wrong on return, we
/// put the caller's frame back exactly. Per Decision H, the resulting
/// heap pointer is a GC-tracked value; future shadow-stack root
/// emission (the rest of PR 3c) will root it between the call and the
/// next allocation site.
fn translate_to_string_builtin(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    // Validate IR shape first — bail before doing any plumbing work
    // (argument-type inspection, runtime-fn lookup, SP-global lookup)
    // so an IR malformation surfaces as a clean diagnostic instead of
    // accidentally leaving partial state in the builder.
    let vid = expect_result(instr, "Op::BuiltinCall(\"toString\")")?;
    // Hard arity check rather than debug_assert + `args.first()`: in
    // release builds with `args.len() > 1`, the silent-truncation
    // shape (debug_assert no-ops, `args[0]` is used) would silently
    // drop the extra args. The IR verifier should prevent this, but
    // a one-line guard keeps debug/release behavior identical on the
    // arity edge.
    if args.len() != 1 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `toString` builtin takes exactly one argument; \
             got {} (IR verifier should have caught this)",
            args.len(),
        )));
    }
    let arg = args[0];
    // Resolve dispatch by *reference* — `IrType` carries owned
    // `String`/`Vec` payloads on its reference variants, so cloning
    // for the dispatch table is wasted work. The borrow released at
    // the end of this match lets the mutating builder calls below
    // run unobstructed.
    let runtime_fn_name = match &ctx.binding_of(arg)?.ir_type {
        // `toString(String)` is the source-level identity: alias
        // `vid` to the arg's existing binding (same locals, same
        // IrType). No runtime call, no SP plumbing, no new locals,
        // no `local.get`/`local.set` copies — and no shadow-stack
        // rooting needed (the source binding is already rooted by
        // its defining op). Future reads of `vid` resolve via
        // `binding_of` to the same locals the arg already owns.
        IrType::StringRef => {
            debug_assert_ne!(
                vid, arg,
                "Op::BuiltinCall(toString) result must differ from its arg \
                 (single-assignment IR invariant)"
            );
            let aliased = ValueBinding {
                locals: ctx.binding_of(arg)?.locals.clone(),
                ir_type: IrType::StringRef,
            };
            debug_assert_eq!(aliased.locals.len(), 2, "StringRef arg must be 2 slots");
            ctx.bindings.insert(vid, aliased);
            return Ok(());
        }
        IrType::I64 => "phx_i64_to_str",
        IrType::F64 => "phx_f64_to_str",
        IrType::Bool => "phx_bool_to_str",
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: `toString` on argument of IR type `{other:?}` \
                 not yet supported (only `Int` / `Float` / `Bool` / `String` \
                 lower today)"
            )));
        }
    };
    let runtime_idx = b.require_phx_func(runtime_fn_name)?;
    let sp_global = b.require_stack_pointer_global()?;

    // Allocate two consecutive i32 locals for the (ptr, len) result.
    let result_locals = ctx.allocate_locals_for_ir_type(vid, IrType::StringRef)?;
    debug_assert_eq!(result_locals.len(), 2, "StringRef must be 2 slots");
    let result_ptr_local = result_locals[0];
    let result_len_local = result_locals[1];

    // `saved_sp` — the caller's SP value at entry. Restored verbatim
    // on the way out. `sret_ptr` — pointer to the reserved result
    // area inside the new frame. `PhxFatPtr` is exactly 8 bytes on
    // wasm32 (two `i32` fields, pinned by the const-block assertions
    // in `phoenix-runtime/src/lib.rs`), but we reserve 16 to match
    // wasm-ld's 16-byte stack alignment for the wasm32-wasip1 C ABI.
    // Keeping every sret frame to a 16-byte multiple lets nested
    // sret calls (and any future shadow-stack push that interleaves
    // with one) compose without re-deriving alignment at each site.
    //
    // The reservation must be a multiple of 4 at minimum: every
    // mutating site on `__stack_pointer` subtracts a multiple of 4
    // so the sret area inherits ≥ 4-byte alignment — matching the
    // `align: 2` hint on the loads below. A future change that
    // picked a non-multiple-of-4 amount would break that invariant;
    // the const-assert catches it at codegen time. (16-byte ABI
    // alignment is the stronger invariant; 4-byte is the minimum
    // the loads below require, asserted explicitly so a future
    // tightening of the constant doesn't accidentally relax the
    // load-alignment guarantee.)
    const SRET_FRAME_BYTES: i32 = 16;
    const _: () = assert!(
        SRET_FRAME_BYTES % 4 == 0,
        "SRET_FRAME_BYTES must keep `__stack_pointer` 4-byte aligned \
         to match the `align: 2` hint on the PhxFatPtr loads below"
    );
    let saved_sp_local = ctx.allocate_temp_local(ValType::I32);
    let sret_ptr_local = ctx.allocate_temp_local(ValType::I32);

    // saved_sp = SP
    ctx.emit(Instruction::GlobalGet(sp_global));
    ctx.emit(Instruction::LocalSet(saved_sp_local));

    // SP = saved_sp - SRET_FRAME_BYTES; sret_ptr = SP
    ctx.emit(Instruction::LocalGet(saved_sp_local));
    ctx.emit(Instruction::I32Const(SRET_FRAME_BYTES));
    ctx.emit(Instruction::I32Sub);
    ctx.emit(Instruction::LocalTee(sret_ptr_local));
    ctx.emit(Instruction::GlobalSet(sp_global));

    // Call: runtime_fn(sret = sret_ptr, val)
    ctx.emit(Instruction::LocalGet(sret_ptr_local));
    ctx.emit_load_all(arg)?;
    ctx.emit(Instruction::Call(runtime_idx));

    // Load PhxFatPtr { ptr, len } from sret_ptr.
    // Layout: ptr at offset 0, len at offset 4. `PhxFatPtr` in
    // `phoenix-runtime` is `#[repr(C)]`; compile-time assertions in
    // that crate pin the offsets so these stay valid if the struct
    // ever changes.
    //
    // `align: 2` (4-byte hint) matches what `SRET_FRAME_BYTES`'s
    // multiple-of-4 invariant guarantees: SP starts at the runtime
    // image's `__stack_pointer` init value (1_048_576 = 1 MiB, which
    // is 16-aligned — see decision H in design-decisions.md) and
    // every mutating site here subtracts a multiple of 4, so
    // `sret_ptr` is always 4-byte aligned for i32 reads. This relies
    // on the runtime never landing SP at a non-4-aligned value
    // between our save/restore brackets; the wasm32-wasip1 runtime
    // satisfies that today. A future codegen change that breaks
    // the SP alignment invariant would trap on engines that enforce
    // alignment hints — which is the right behavior, since misaligned
    // access through `align: 2` would be a real correctness bug, not
    // just a perf miss.
    ctx.emit(Instruction::LocalGet(sret_ptr_local));
    ctx.emit(Instruction::I32Load(wasm_encoder::MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    }));
    ctx.emit(Instruction::LocalSet(result_ptr_local));
    ctx.emit(Instruction::LocalGet(sret_ptr_local));
    ctx.emit(Instruction::I32Load(wasm_encoder::MemArg {
        offset: 4,
        align: 2,
        memory_index: 0,
    }));
    ctx.emit(Instruction::LocalSet(result_len_local));

    // Restore SP from the saved copy rather than `current_SP + 16`:
    // if the callee mismanaged SP (against ABI), we still put the
    // caller's frame back exactly where it was.
    ctx.emit(Instruction::LocalGet(saved_sp_local));
    ctx.emit(Instruction::GlobalSet(sp_global));

    Ok(())
}

/// Translate a basic-block terminator.
///
/// - `Return(None)` / `Return(Some(v))`: emit a WASM `return` (always
///   exits the function regardless of nesting).
/// - `Jump { target, args }` / `Branch { ... }`: copy args to the
///   target block's param locals, set the dispatch local to the target
///   block's ID, then `br <depth_to_loop>` to re-enter the dispatcher.
/// - `Unreachable`: emit WASM `unreachable` (traps at runtime).
///
/// In single-block functions, `dispatcher` is `None` and only `Return`
/// / `Unreachable` are reachable; the others would mean a sema/IR bug.
fn translate_terminator(
    ctx: &mut FuncTranslateCtx,
    _b: &mut ModuleBuilder,
    term: &Terminator,
    dispatcher: Option<DispatcherContext>,
) -> Result<(), CompileError> {
    match term {
        Terminator::Return(None) => {
            // Bare `return` — no operand. WASM `return` exits the
            // function and ignores any nesting; matches Phoenix's
            // "Return ignores enclosing block scopes" semantics.
            ctx.emit(Instruction::Return);
            // After `return`, WASM is in unreachable code; we don't
            // need an explicit `unreachable` because every code path
            // ends in a terminator anyway.
            Ok(())
        }
        Terminator::Return(Some(v)) => {
            // Multi-slot returns (`StringRef`) push their slots in
            // declaration order, then `return` exits with all
            // operand-stack values matching the function's return
            // type (WASM multi-value return).
            ctx.emit_load_all(*v)?;
            ctx.emit(Instruction::Return);
            Ok(())
        }
        Terminator::Jump { target, args } => {
            let dispatcher = require_dispatcher(dispatcher)?;
            emit_block_param_copies(ctx, *target, args)?;
            ctx.emit(Instruction::I32Const(target.0 as i32));
            ctx.emit(Instruction::LocalSet(dispatcher.dispatch_local));
            ctx.emit(Instruction::Br(dispatcher.depth_to_loop));
            Ok(())
        }
        Terminator::Branch {
            condition,
            true_block,
            true_args,
            false_block,
            false_args,
        } => {
            let dispatcher = require_dispatcher(dispatcher)?;
            let cond_local = ctx.binding_of(*condition)?.single_local();
            ctx.emit(Instruction::LocalGet(cond_local));
            ctx.emit(Instruction::If(BlockType::Empty));
            // Then-branch: jump to `true_block`.
            emit_block_param_copies(ctx, *true_block, true_args)?;
            ctx.emit(Instruction::I32Const(true_block.0 as i32));
            ctx.emit(Instruction::LocalSet(dispatcher.dispatch_local));
            ctx.emit(Instruction::Else);
            // Else-branch: jump to `false_block`.
            emit_block_param_copies(ctx, *false_block, false_args)?;
            ctx.emit(Instruction::I32Const(false_block.0 as i32));
            ctx.emit(Instruction::LocalSet(dispatcher.dispatch_local));
            ctx.emit(Instruction::End);
            // `If`/`Else`/`End` is `+1` to the WASM nesting depth
            // within the block being emitted; both arms set
            // `dispatch_local` then fall through, and we follow up
            // with the `br` that re-enters the loop. The br depth is
            // measured from *here* (post-`End`), not from inside the
            // If — and since the If/Else/End block has been closed,
            // the depth is the same as the dispatcher's
            // `depth_to_loop`.
            ctx.emit(Instruction::Br(dispatcher.depth_to_loop));
            Ok(())
        }
        Terminator::Unreachable => {
            ctx.emit(Instruction::Unreachable);
            Ok(())
        }
        Terminator::Switch { .. } => Err(CompileError::new(
            "wasm32-linear: `Switch` terminator not yet emitted by the IR \
             lowering pass; if it becomes reachable, extend the wasm32-linear \
             terminator translator alongside the IR change",
        )),
        Terminator::None => Err(CompileError::new(
            "wasm32-linear: encountered `Terminator::None` (a placeholder for \
             blocks under construction). The IR verifier should reject any \
             function reaching codegen with such a terminator.",
        )),
    }
}

/// Require a dispatcher context — used by terminators that branch
/// back into the dispatch. The error path indicates a single-block
/// function carrying a `Jump` / `Branch` terminator, which is a
/// sema / IR bug.
fn require_dispatcher(
    dispatcher: Option<DispatcherContext>,
) -> Result<DispatcherContext, CompileError> {
    dispatcher.ok_or_else(|| {
        CompileError::new(
            "wasm32-linear: single-block function carries a `Jump` / `Branch` \
             terminator (internal compiler bug — sema / IR should reject a \
             single-block function with non-`Return` control flow)",
        )
    })
}

/// Copy each Jump/Branch arg into the corresponding block-param local.
/// The IR verifier guarantees `args.len() == target.params.len()`; a
/// mismatch indicates an IR-verifier regression.
///
/// Implemented as a parallel copy: push all source values onto the
/// operand stack first, then pop them into the destinations in
/// reverse order. Why: a back-edge that passes a block's own params
/// in shuffled order (e.g. `jump header(b, a)` where `a, b` are
/// `header`'s params) overlaps the source and destination local
/// sets. A naive `get src; set dest;` per-pair would clobber the
/// remaining sources before they're read — the classic parallel-copy
/// problem. Doing all reads before any writes makes the copy atomic
/// w.r.t. the local-set state and costs only the stack slots that
/// already exist.
///
/// **PR 3b testing status:** the shuffled / overlapping-args path
/// above is *not exercised by any source-level fixture in PR 3b*.
/// Phoenix lowers mutable loop state through `Op::Alloca` rather than
/// block-param threading, so a `Jump { args: [b, a] }` shape can't be
/// expressed in source today. `if_as_expression_runs_under_wasmtime`
/// covers the non-overlapping single-arg case. The overlapping case
/// is correctness-by-construction here until PR 3c lifts loop state
/// onto block params; at that point an end-to-end fixture with a
/// `while`-shaped loop carrying ≥2 block params in shuffled order
/// will pin this path against regression.
fn emit_block_param_copies(
    ctx: &mut FuncTranslateCtx,
    target: BlockId,
    args: &[ValueId],
) -> Result<(), CompileError> {
    let param_locals = ctx.block_param_locals_of(target).to_vec();
    if param_locals.len() != args.len() {
        return Err(CompileError::new(format!(
            "wasm32-linear: jump to {target:?} has {} args but the target \
             has {} params (internal compiler bug — IR verifier should have \
             caught this)",
            args.len(),
            param_locals.len(),
        )));
    }
    for arg in args {
        let src_local = ctx.binding_of(*arg)?.single_local();
        ctx.emit(Instruction::LocalGet(src_local));
    }
    for dest_local in param_locals.iter().rev() {
        ctx.emit(Instruction::LocalSet(*dest_local));
    }
    Ok(())
}

fn unsupported(ty: &IrType, where_: &str) -> CompileError {
    CompileError::new(format!(
        "wasm32-linear: IR type `{ty:?}` not yet supported in {where_} \
         (Phase 2.4 PR 3 — see docs/design-decisions.md §Phase 2.4)"
    ))
}
