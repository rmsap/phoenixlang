//! Up-front IR validation for the wasm32-linear backend.
//!
//! [`validate`] runs **before** `runtime_discovery::find_runtime_wasm_or_diagnostic`
//! in [`super::compile_wasm_linear`]. That ordering is the whole point:
//! the IR-level rejections below depend only on the IR module, not on
//! the merged runtime, so the *screened* structural faults are rejected
//! with the specific diagnostic regardless of whether the pre-built
//! `phoenix_runtime.wasm` artifact is present. The screened set is:
//! no-main, bad `main` shape, layout-unstable `EnumAlloc`, and the
//! deferred op *families* enumerated below (float arithmetic /
//! comparison, string comparison) — **not** every unsupported op. An
//! unsupported op outside those families isn't screened here; it still
//! reaches the authoritative catch-all in `translate`, so on an
//! artifact-missing host it surfaces as `RuntimeWasmNotFound` rather
//! than the op diagnostic. Widening the artifact-independent set means
//! adding that op family to the screen below (see "Single source of
//! truth").
//!
//! Without this pass even the screened rejections only fired
//! mid-translation — i.e. *after* the runtime merge — so on a host
//! where the runtime artifact is missing (e.g. the release-profile CI
//! job, which doesn't build the wasm runtime) the merge's
//! `RuntimeWasmNotFound` preempted them. The `rejects_*` integration
//! tests in `tests/compile_wasm_linear.rs` pin that the screened
//! layout/op rejections fire before the runtime merge.
//!
//! # Single source of truth
//!
//! The checks here do not re-derive their logic: the `main`
//! existence/shape checks reuse [`ModuleBuilder::validate_main_shape`]
//! (the duplicate-`main` guard stays post-merge — it's an
//! internal-compiler-bug check, not an IR-shape rejection), `EnumAlloc`
//! reuses
//! [`translate::check_enum_alloc_layout_stable`], and the unsupported-op
//! diagnostic reuses [`translate::unsupported_op_error`]. The op screen
//! is a *reject-list* of the deferred op families (float arithmetic /
//! comparison, string comparison) rather than an allow-list of the
//! supported set: an op that gains a lowering is simply removed from the
//! list, and any op this pass doesn't screen still hits the
//! authoritative catch-all in `translate::translate_instruction` (just
//! after the merge, as before). That keeps a newly-added op from being
//! silently false-rejected here.
//!
//! # Error-priority shift
//!
//! Hoisting these rejections also reorders them relative to the
//! *type*-level rejections that still fire mid-translation (e.g. the
//! `IR type F64` rep error from `heap_layout`/`flatten_param_types`).
//! A function carrying both a screened op and an unsupported type now
//! surfaces the *op* diagnostic — `validate` runs first and short-
//! circuits on the first screened op — whereas the old translate-only
//! path could surface whichever the translator reached first (often the
//! type error). Both are valid rejections of invalid IR, so this is a
//! priority change, not a correctness one; it's called out because the
//! `rejects_*` tests deliberately pin the *op* path (`IR op`, not
//! `IR type`) and would otherwise look over-specified.

use std::collections::HashMap;

use phoenix_ir::instruction::Op;
use phoenix_ir::module::IrModule;

use super::heap_layout::{EnumLayout, compute_enum_layout};
use super::module_builder::ModuleBuilder;
use super::translate::{check_enum_alloc_layout_stable, unsupported_op_error};
use crate::error::CompileError;

/// Reject structurally-invalid IR before any runtime artifact is
/// located. See the module docs for why the ordering matters.
pub(super) fn validate(ir_module: &IrModule) -> Result<(), CompileError> {
    // `main` must exist and have the WASI `_start`-compatible shape
    // (no params, returns void). Mirrors the existence/shape checks
    // `ModuleBuilder::declare_phoenix_functions` performs post-merge,
    // hoisted here so they don't depend on the runtime artifact. The
    // `no main` error only matters when no `main` exists, so folding the
    // existence check into the shape loop (rather than a separate
    // `.any` pass) preserves the original error ordering.
    let mut found_main = false;
    for func in ir_module.concrete_functions() {
        if func.name == "main" {
            found_main = true;
            ModuleBuilder::validate_main_shape(func)?;
        }
    }
    if !found_main {
        return Err(CompileError::new("no main function found"));
    }

    // Op-level rejections that only need the IR. Walk exactly the set
    // that `emit_phoenix_bodies` translates (`concrete_functions`) so a
    // deferred op living only in an untranslated template body isn't
    // flagged. `EnumAlloc` layouts are memoized per enum name so a
    // module with many allocs of the same enum doesn't recompute (and
    // re-clone) the layout once per instruction — mirroring the
    // `cached_enum_layout` memoization the translator uses post-merge.
    let mut enum_layout_cache: HashMap<&str, EnumLayout> = HashMap::new();
    for func in ir_module.concrete_functions() {
        for block in &func.blocks {
            for instr in &block.instructions {
                match &instr.op {
                    // Deferred op families (no wasm32-linear lowering
                    // yet — Phase 2.4 PR 3c). SYNC INVARIANT: every op
                    // listed here must lack a `translate_instruction`
                    // arm — listing one that *does* lower would
                    // false-reject it and make the lowering unreachable.
                    // When an op gains a lowering, delete it from this
                    // list. Each family is pinned by a `rejects_*` test
                    // (`rejects_unsupported_ir_op` / `_float_comparison_op`
                    // / `_string_comparison_op`) so dropping a member is
                    // caught. Float arithmetic:
                    Op::FAdd(..)
                    | Op::FSub(..)
                    | Op::FMul(..)
                    | Op::FDiv(..)
                    | Op::FMod(..)
                    | Op::FNeg(..)
                    // Float comparison:
                    | Op::FEq(..)
                    | Op::FNe(..)
                    | Op::FLt(..)
                    | Op::FGt(..)
                    | Op::FLe(..)
                    | Op::FGe(..)
                    // String comparison:
                    | Op::StringEq(..)
                    | Op::StringNe(..)
                    | Op::StringLt(..)
                    | Op::StringGt(..)
                    | Op::StringLe(..)
                    | Op::StringGe(..) => {
                        return Err(unsupported_op_error(&instr.op));
                    }
                    Op::EnumAlloc(name, variant_idx, field_values) => {
                        // `entry().or_insert(..)` can't be used here:
                        // `compute_enum_layout` is fallible and the
                        // `match get { .. None => entry() }` shape is NLL
                        // problem-case-3 (the `get` borrow flows out of
                        // the `Some` arm, so the `entry` reborrow in the
                        // `None` arm conflicts on stable). Populate via a
                        // separate `contains_key` check, then index.
                        if !enum_layout_cache.contains_key(name.as_str()) {
                            let layout = compute_enum_layout(ir_module, name)?;
                            enum_layout_cache.insert(name.as_str(), layout);
                        }
                        let declared_layout = &enum_layout_cache[name.as_str()];
                        check_enum_alloc_layout_stable(
                            declared_layout,
                            name,
                            *variant_idx,
                            field_values.len(),
                        )?;
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}
