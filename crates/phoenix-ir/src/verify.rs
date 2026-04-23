//! IR verification — structural invariants after lowering.
//!
//! Checks: one terminator per block, valid branch targets, terminator
//! argument counts match target block params, no operand uses
//! [`VOID_SENTINEL`], value-ids defined in same function, `dyn`-op
//! invariants, and `DynRef` def-site legitimacy.
//!
//! The `dyn`-specific checks live in [`dyn_ops`] so the core structural
//! verifier here stays compact.
//!
//! **Not yet checked:** SSA dominance. Pass values through block
//! parameters across block boundaries (see `lower_try`).

mod dyn_ops;

use crate::block::BlockId;
use crate::instruction::{VOID_SENTINEL, ValueId};
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
        dyn_ops::verify_dyn_ops(module, func, &mut errors);
        dyn_ops::verify_dyn_def_sites(func, &mut errors);
    }
    errors
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
