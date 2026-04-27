//! IR verification — structural invariants after lowering.
//!
//! Checks: one terminator per block, valid branch targets, terminator
//! argument counts match target block params, no operand uses
//! [`VOID_SENTINEL`], value-ids defined in same function, `dyn`-op
//! invariants, `DynRef` def-site legitimacy, and that no
//! [`Op::UnresolvedTraitMethod`](crate::instruction::Op::UnresolvedTraitMethod)
//! or [`Op::UnresolvedDynAlloc`](crate::instruction::Op::UnresolvedDynAlloc)
//! survives into any concrete function post-monomorphization.
//!
//! The `dyn`-specific checks live in [`dyn_ops`] so the core structural
//! verifier here stays compact.
//!
//! **Not yet checked:** SSA dominance. Pass values through block
//! parameters across block boundaries (see `lower_try`).

mod dyn_ops;

use crate::block::BlockId;
use crate::instruction::{Op, VOID_SENTINEL, ValueId};
use crate::module::{IrFunction, IrModule};
use crate::terminator::Terminator;
use std::collections::{HashMap, HashSet};

/// Errors found during verification.
#[derive(Debug)]
pub struct VerifyError {
    /// The function in which the error was found.
    pub function: String,
    /// A human-readable description of the error.
    pub message: String,
}

/// Verify the structural integrity of an IR module.
///
/// Returns a list of errors found.  An empty list means the module is
/// well-formed.
///
/// Generic templates (functions with `is_generic_template = true`) are
/// intentionally skipped: their bodies carry `IrType::TypeVar`
/// annotations that the monomorphization pass consumes to produce
/// specialized copies, and no downstream backend ever reads them.
/// Iterating via [`IrModule::concrete_functions`] is the canonical way
/// to walk verifiable functions.
pub fn verify(module: &IrModule) -> Vec<VerifyError> {
    let mut errors = Vec::new();
    dyn_ops::verify_dyn_vtable_shapes(module, &mut errors);
    for func in module.concrete_functions() {
        verify_function(func, &mut errors);
        verify_value_types_index(func, &mut errors);
        verify_no_unresolved_placeholder_ops(func, &mut errors);
        dyn_ops::verify_dyn_ops(module, func, &mut errors);
        dyn_ops::verify_dyn_def_sites(func, &mut errors);
    }
    errors
}

/// Trait-bounded method calls on type-variable receivers are emitted
/// as [`Op::UnresolvedTraitMethod`] by IR lowering and must be
/// rewritten to a direct `Op::Call` by the monomorphization pass.
/// Similarly, `dyn Trait` coercions on type-variable sources are
/// emitted as [`Op::UnresolvedDynAlloc`] and rewritten to
/// [`Op::DynAlloc`] by the same pass.  Any residual placeholder in a
/// concrete function indicates a mono bug.
fn verify_no_unresolved_placeholder_ops(func: &IrFunction, errors: &mut Vec<VerifyError>) {
    for block in &func.blocks {
        for instr in &block.instructions {
            match &instr.op {
                Op::UnresolvedTraitMethod(method, _, _) => errors.push(VerifyError {
                    function: func.name.clone(),
                    message: format!(
                        "Op::UnresolvedTraitMethod `.{method}` survived \
                         monomorphization — this is an internal compiler bug",
                    ),
                }),
                Op::UnresolvedDynAlloc(trait_name, _) => errors.push(VerifyError {
                    function: func.name.clone(),
                    message: format!(
                        "Op::UnresolvedDynAlloc `@{trait_name}` survived \
                         monomorphization — this is an internal compiler bug",
                    ),
                }),
                _ => {}
            }
        }
    }
}

/// The per-value type index must have one entry per allocated ValueId.
/// Out-of-sync state here causes silent wrong-codegen downstream (e.g.
/// dyn-coercion readings the wrong actual type).
fn verify_value_types_index(func: &IrFunction, errors: &mut Vec<VerifyError>) {
    if func.value_count() as usize != func.value_types_len() {
        errors.push(VerifyError {
            function: func.name.clone(),
            message: format!(
                "value_types length {} out of sync with value_count {} — a pass \
                 allocated a ValueId without recording its type",
                func.value_types_len(),
                func.value_count()
            ),
        });
    }
}

fn verify_function(func: &IrFunction, errors: &mut Vec<VerifyError>) {
    if func.blocks.is_empty() {
        // Functions with no blocks are stubs (body not yet lowered).
        return;
    }

    let valid_blocks: HashSet<BlockId> = func.blocks.iter().map(|b| b.id).collect();

    // Map from BlockId to its parameter count for argument-count checks.
    let block_param_counts: HashMap<BlockId, usize> =
        func.blocks.iter().map(|b| (b.id, b.params.len())).collect();

    // Collect all defined ValueIds: block parameters + instruction results.
    let mut defined: HashSet<ValueId> = HashSet::new();
    for block in &func.blocks {
        for (val, _) in &block.params {
            defined.insert(*val);
        }
        for inst in &block.instructions {
            if let Some(val) = inst.result {
                defined.insert(val);
            }
        }
    }

    for block in &func.blocks {
        // Check that the terminator is not `None`.
        if matches!(block.terminator, Terminator::None) {
            errors.push(VerifyError {
                function: func.name.clone(),
                message: format!("block {} has no terminator", block.id),
            });
        }

        // Check that all branch targets are valid.
        for target in terminator_targets(&block.terminator) {
            if !valid_blocks.contains(&target) {
                errors.push(VerifyError {
                    function: func.name.clone(),
                    message: format!("block {} references invalid target {}", block.id, target),
                });
            }
        }

        // Check terminator argument counts match target block parameter counts.
        for (target, arg_count) in terminator_target_args(&block.terminator) {
            if let Some(&param_count) = block_param_counts.get(&target)
                && arg_count != param_count
            {
                errors.push(VerifyError {
                    function: func.name.clone(),
                    message: format!(
                        "block {} jumps to {} with {} args, but {} expects {} params",
                        block.id, target, arg_count, target, param_count
                    ),
                });
            }
        }

        // Check instruction operands: no VOID_SENTINEL, all defined.
        for inst in &block.instructions {
            for val in inst.op.operands() {
                if val == VOID_SENTINEL {
                    errors.push(VerifyError {
                        function: func.name.clone(),
                        message: format!(
                            "block {} instruction uses VOID_SENTINEL as operand",
                            block.id
                        ),
                    });
                } else if !defined.contains(&val) {
                    errors.push(VerifyError {
                        function: func.name.clone(),
                        message: format!(
                            "block {} instruction uses undefined value {}",
                            block.id, val
                        ),
                    });
                }
            }
        }

        // Check terminator operands: no VOID_SENTINEL, all defined.
        for val in terminator_operands(&block.terminator) {
            if val == VOID_SENTINEL {
                errors.push(VerifyError {
                    function: func.name.clone(),
                    message: format!(
                        "block {} terminator uses VOID_SENTINEL as operand",
                        block.id
                    ),
                });
            } else if !defined.contains(&val) {
                errors.push(VerifyError {
                    function: func.name.clone(),
                    message: format!("block {} terminator uses undefined value {}", block.id, val),
                });
            }
        }
    }
}

/// Extract all block targets from a terminator.
fn terminator_targets(term: &Terminator) -> Vec<BlockId> {
    match term {
        Terminator::Jump { target, .. } => vec![*target],
        Terminator::Branch {
            true_block,
            false_block,
            ..
        } => vec![*true_block, *false_block],
        Terminator::Switch { cases, default, .. } => {
            let mut targets: Vec<BlockId> = cases.iter().map(|(_, b, _)| *b).collect();
            targets.push(*default);
            targets
        }
        Terminator::Return(_) | Terminator::Unreachable | Terminator::None => Vec::new(),
    }
}

/// Extract `(target_block, argument_count)` pairs from a terminator.
fn terminator_target_args(term: &Terminator) -> Vec<(BlockId, usize)> {
    match term {
        Terminator::Jump { target, args } => vec![(*target, args.len())],
        Terminator::Branch {
            true_block,
            true_args,
            false_block,
            false_args,
            ..
        } => vec![
            (*true_block, true_args.len()),
            (*false_block, false_args.len()),
        ],
        Terminator::Switch {
            cases,
            default,
            default_args,
            ..
        } => {
            let mut pairs: Vec<(BlockId, usize)> =
                cases.iter().map(|(_, b, a)| (*b, a.len())).collect();
            pairs.push((*default, default_args.len()));
            pairs
        }
        Terminator::Return(_) | Terminator::Unreachable | Terminator::None => Vec::new(),
    }
}

/// Extract all [`ValueId`] operands from a terminator (conditions, args,
/// return values).
fn terminator_operands(term: &Terminator) -> Vec<ValueId> {
    match term {
        Terminator::Jump { args, .. } => args.clone(),
        Terminator::Branch {
            condition,
            true_args,
            false_args,
            ..
        } => {
            let mut ops = vec![*condition];
            ops.extend(true_args);
            ops.extend(false_args);
            ops
        }
        Terminator::Switch {
            value,
            cases,
            default_args,
            ..
        } => {
            let mut ops = vec![*value];
            for (_, _, args) in cases {
                ops.extend(args);
            }
            ops.extend(default_args);
            ops
        }
        Terminator::Return(Some(v)) => vec![*v],
        Terminator::Return(None) | Terminator::Unreachable | Terminator::None => Vec::new(),
    }
}

#[cfg(test)]
mod unresolved_placeholder_op_tests {
    //! Negative verifier tests for [`Op::UnresolvedTraitMethod`] and
    //! [`Op::UnresolvedDynAlloc`]: both placeholders must not survive
    //! the monomorphization pass inside a concrete (non-template)
    //! function.  Positive cases — where the placeholders legitimately
    //! appear in a template body — are covered by the backend-crate
    //! integration suite.

    use crate::instruction::{FuncId, Op};
    use crate::module::{IrFunction, IrModule};
    use crate::terminator::Terminator;
    use crate::types::IrType;
    use crate::verify::verify;

    #[test]
    fn unresolved_trait_method_in_concrete_function_is_flagged() {
        let mut module = IrModule::new();
        let mut func = IrFunction::new(
            FuncId(0),
            "concrete".into(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        let entry = func.create_block();
        let recv = func.add_block_param(entry, IrType::StructRef("Concrete".into(), Vec::new()));
        func.emit(
            entry,
            Op::UnresolvedTraitMethod("greet".into(), Vec::new(), vec![recv]),
            IrType::Void,
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(func);

        let errors = verify(&module);
        assert!(
            errors.iter().any(
                |e| e.message.contains("UnresolvedTraitMethod") && e.message.contains(".greet")
            ),
            "expected unresolved-trait-method error, got: {errors:?}"
        );
    }

    #[test]
    fn unresolved_dyn_alloc_in_concrete_function_is_flagged() {
        let mut module = IrModule::new();
        let mut func = IrFunction::new(
            FuncId(0),
            "concrete".into(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        let entry = func.create_block();
        let src = func.add_block_param(entry, IrType::StructRef("Circle".into(), Vec::new()));
        func.emit(
            entry,
            Op::UnresolvedDynAlloc("Drawable".into(), src),
            IrType::DynRef("Drawable".into()),
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(func);

        let errors = verify(&module);
        assert!(
            errors.iter().any(
                |e| e.message.contains("UnresolvedDynAlloc") && e.message.contains("@Drawable")
            ),
            "expected unresolved-dyn-alloc error, got: {errors:?}"
        );
    }

    #[test]
    fn unresolved_trait_method_in_template_is_not_flagged() {
        let mut module = IrModule::new();
        let mut func = IrFunction::new(
            FuncId(0),
            "tmpl".into(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        // Templates are skipped by `concrete_functions()`, so the
        // placeholder is allowed here.
        func.is_generic_template = true;
        let entry = func.create_block();
        let recv = func.add_block_param(entry, IrType::TypeVar("T".into()));
        func.emit(
            entry,
            Op::UnresolvedTraitMethod("greet".into(), Vec::new(), vec![recv]),
            IrType::Void,
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(func);

        let errors = verify(&module);
        assert!(
            !errors
                .iter()
                .any(|e| e.message.contains("UnresolvedTraitMethod")),
            "placeholder in a template must not be flagged, got: {errors:?}"
        );
    }

    #[test]
    fn unresolved_dyn_alloc_in_template_is_not_flagged() {
        let mut module = IrModule::new();
        let mut func = IrFunction::new(
            FuncId(0),
            "tmpl".into(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        func.is_generic_template = true;
        let entry = func.create_block();
        let src = func.add_block_param(entry, IrType::TypeVar("T".into()));
        func.emit(
            entry,
            Op::UnresolvedDynAlloc("Drawable".into(), src),
            IrType::DynRef("Drawable".into()),
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(func);

        let errors = verify(&module);
        assert!(
            !errors
                .iter()
                .any(|e| e.message.contains("UnresolvedDynAlloc")),
            "placeholder in a template must not be flagged, got: {errors:?}"
        );
    }
}

#[cfg(test)]
mod structural_verifier_tests {
    //! Negative tests for the core structural invariants in
    //! [`verify_function`] and [`verify_value_types_index`]. Companion
    //! to the four `unresolved_placeholder_op_tests` above and the
    //! `dyn`-op tests in [`super::dyn_ops`]. Each test constructs a
    //! minimal IR module that violates one specific invariant and
    //! asserts the verifier rejects it with a recognisable message.

    use crate::block::BlockId;
    use crate::instruction::{FuncId, Op, VOID_SENTINEL, ValueId};
    use crate::module::{IrFunction, IrModule};
    use crate::terminator::Terminator;
    use crate::types::IrType;
    use crate::verify::verify;

    fn func_returning(ret: IrType) -> IrFunction {
        IrFunction::new(FuncId(0), "f".into(), Vec::new(), Vec::new(), ret, None)
    }

    fn empty_func() -> IrFunction {
        func_returning(IrType::Void)
    }

    #[test]
    fn terminator_arg_count_mismatch_is_flagged() {
        let mut module = IrModule::new();
        let mut func = empty_func();
        let entry = func.create_block();
        let target = func.create_block();
        // Target expects one Int param.
        func.add_block_param(target, IrType::I64);
        func.set_terminator(target, Terminator::Return(None));
        // Entry jumps to target with zero args.
        func.set_terminator(
            entry,
            Terminator::Jump {
                target,
                args: Vec::new(),
            },
        );
        module.functions.push(func);

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("0 args") && e.message.contains("expects 1 params")),
            "expected arg-count mismatch, got: {errors:?}"
        );
    }

    #[test]
    fn instruction_operand_void_sentinel_is_flagged() {
        let mut module = IrModule::new();
        let mut func = empty_func();
        let entry = func.create_block();
        let lhs = func.add_block_param(entry, IrType::I64);
        // RHS is VOID_SENTINEL — the verifier must catch this.
        func.emit(entry, Op::IAdd(lhs, VOID_SENTINEL), IrType::I64, None);
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(func);

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("instruction uses VOID_SENTINEL")),
            "expected VOID_SENTINEL operand error, got: {errors:?}"
        );
    }

    #[test]
    fn instruction_operand_undefined_is_flagged() {
        let mut module = IrModule::new();
        let mut func = empty_func();
        let entry = func.create_block();
        let lhs = func.add_block_param(entry, IrType::I64);
        // RHS is ValueId(99) — never allocated.
        func.emit(entry, Op::IAdd(lhs, ValueId(99)), IrType::I64, None);
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(func);

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("instruction uses undefined value")),
            "expected undefined-operand error, got: {errors:?}"
        );
    }

    #[test]
    fn terminator_operand_void_sentinel_is_flagged() {
        let mut module = IrModule::new();
        let mut func = func_returning(IrType::I64);
        let entry = func.create_block();
        // Return VOID_SENTINEL from a function declared to return I64.
        func.set_terminator(entry, Terminator::Return(Some(VOID_SENTINEL)));
        module.functions.push(func);

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("terminator uses VOID_SENTINEL")),
            "expected VOID_SENTINEL terminator-operand error, got: {errors:?}"
        );
    }

    #[test]
    fn terminator_operand_undefined_is_flagged() {
        let mut module = IrModule::new();
        let mut func = func_returning(IrType::I64);
        let entry = func.create_block();
        // Branch on a value that was never allocated.
        let other = func.create_block();
        func.set_terminator(other, Terminator::Return(None));
        func.set_terminator(
            entry,
            Terminator::Branch {
                condition: ValueId(99),
                true_block: other,
                true_args: Vec::new(),
                false_block: other,
                false_args: Vec::new(),
            },
        );
        module.functions.push(func);

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("terminator uses undefined value")),
            "expected undefined terminator-operand error, got: {errors:?}"
        );
    }

    #[test]
    fn value_types_index_out_of_sync_is_flagged() {
        let mut module = IrModule::new();
        let mut func = empty_func();
        let entry = func.create_block();
        func.set_terminator(entry, Terminator::Return(None));
        // Bump next_value_id without pushing a slot into value_types,
        // simulating a buggy pass that allocated a ValueId but skipped
        // the type index.
        func.debug_desync_value_types();
        module.functions.push(func);

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("value_types length")
                    && e.message.contains("out of sync")),
            "expected value_types desync error, got: {errors:?}"
        );
    }

    #[test]
    fn block_with_no_terminator_is_flagged() {
        let mut module = IrModule::new();
        let mut func = empty_func();
        // create_block leaves Terminator::None in place; never call
        // set_terminator on it.
        func.create_block();
        module.functions.push(func);

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("has no terminator")),
            "expected missing-terminator error, got: {errors:?}"
        );
    }

    #[test]
    fn terminator_references_invalid_target_is_flagged() {
        let mut module = IrModule::new();
        let mut func = empty_func();
        let entry = func.create_block();
        // Jump to BlockId(99), which was never created.
        func.set_terminator(
            entry,
            Terminator::Jump {
                target: BlockId(99),
                args: Vec::new(),
            },
        );
        module.functions.push(func);

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("references invalid target")),
            "expected invalid-target error, got: {errors:?}"
        );
    }

    #[test]
    fn well_formed_function_passes_verification() {
        // Sanity check: a minimal well-formed function verifies clean.
        // Catches regressions where a verifier change starts flagging
        // legitimate IR.
        let mut module = IrModule::new();
        let mut func = empty_func();
        let entry = func.create_block();
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(func);

        let errors = verify(&module);
        assert!(errors.is_empty(), "expected clean verify, got: {errors:?}");
    }
}
