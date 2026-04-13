//! IR verification.
//!
//! Validates structural invariants of the IR after lowering:
//! - Every basic block has exactly one terminator.
//! - All branch targets reference valid blocks.
//! - All terminator argument counts match target block parameter counts.
//! - No instruction or terminator operand references [`VOID_SENTINEL`].
//! - Every [`ValueId`] used as an operand is defined in the same function.
//!
//! Run the verifier after every lowering pass in debug builds to catch
//! bugs early.

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
pub fn verify(module: &IrModule) -> Vec<VerifyError> {
    let mut errors = Vec::new();

    for func in &module.functions {
        verify_function(func, &mut errors);
    }

    errors
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
            for val in op_operands(&inst.op) {
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

/// Extract all [`ValueId`] operands from an instruction's [`Op`].
fn op_operands(op: &Op) -> Vec<ValueId> {
    match op {
        // Constants — no operands.
        Op::ConstI64(_) | Op::ConstF64(_) | Op::ConstBool(_) | Op::ConstString(_) => Vec::new(),

        // Binary ops.
        Op::IAdd(a, b)
        | Op::ISub(a, b)
        | Op::IMul(a, b)
        | Op::IDiv(a, b)
        | Op::IMod(a, b)
        | Op::FAdd(a, b)
        | Op::FSub(a, b)
        | Op::FMul(a, b)
        | Op::FDiv(a, b)
        | Op::FMod(a, b)
        | Op::IEq(a, b)
        | Op::INe(a, b)
        | Op::ILt(a, b)
        | Op::IGt(a, b)
        | Op::ILe(a, b)
        | Op::IGe(a, b)
        | Op::FEq(a, b)
        | Op::FNe(a, b)
        | Op::FLt(a, b)
        | Op::FGt(a, b)
        | Op::FLe(a, b)
        | Op::FGe(a, b)
        | Op::StringEq(a, b)
        | Op::StringNe(a, b)
        | Op::StringLt(a, b)
        | Op::StringGt(a, b)
        | Op::StringLe(a, b)
        | Op::StringGe(a, b)
        | Op::BoolEq(a, b)
        | Op::BoolNe(a, b)
        | Op::StringConcat(a, b)
        | Op::Store(a, b) => vec![*a, *b],

        // Unary ops.
        Op::INeg(a)
        | Op::FNeg(a)
        | Op::BoolNot(a)
        | Op::Load(a)
        | Op::Copy(a)
        | Op::EnumDiscriminant(a) => vec![*a],

        // Struct ops.
        Op::StructAlloc(_, vals) => vals.clone(),
        Op::StructGetField(v, _) => vec![*v],
        Op::StructSetField(obj, _, val) => vec![*obj, *val],

        // Enum ops.
        Op::EnumAlloc(_, _, vals) => vals.clone(),
        Op::EnumGetField(v, _) => vec![*v],

        // Collection ops.
        Op::ListAlloc(vals) => vals.clone(),
        Op::MapAlloc(pairs) => pairs.iter().flat_map(|(k, v)| [*k, *v]).collect(),

        // Closure ops.
        Op::ClosureAlloc(_, vals) => vals.clone(),

        // Call ops.
        Op::Call(_, args) => args.clone(),
        Op::CallIndirect(callee, args) => {
            let mut ops = vec![*callee];
            ops.extend(args);
            ops
        }
        Op::BuiltinCall(_, args) => args.clone(),

        // Alloca — no value operands.
        Op::Alloca(_) => Vec::new(),
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
