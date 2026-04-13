//! Terminators for IR basic blocks.
//!
//! A [`Terminator`] ends every basic block and determines control flow
//! to successor blocks.

use crate::block::BlockId;
use crate::instruction::ValueId;

/// The terminator of a basic block.  Determines control flow out of the block.
#[derive(Debug, Clone)]
pub enum Terminator {
    /// Unconditional jump to a target block, passing arguments that bind
    /// to the target's block parameters.
    Jump {
        /// The target block.
        target: BlockId,
        /// Values passed as block parameter arguments.
        args: Vec<ValueId>,
    },
    /// Conditional branch on a boolean value.
    Branch {
        /// The boolean condition to test.
        condition: ValueId,
        /// Block to jump to when the condition is true.
        true_block: BlockId,
        /// Arguments for the true block's parameters.
        true_args: Vec<ValueId>,
        /// Block to jump to when the condition is false.
        false_block: BlockId,
        /// Arguments for the false block's parameters.
        false_args: Vec<ValueId>,
    },
    /// Multi-way branch on an integer discriminant (for match on enums).
    ///
    /// Reserved for future optimization — the lowering pass currently emits
    /// chained [`Branch`] instructions for enum matching.  A later IR
    /// optimization pass can coalesce those chains into a single `Switch`.
    Switch {
        /// The integer discriminant value to dispatch on.
        value: ValueId,
        /// `(discriminant, target_block, args)` cases.
        cases: Vec<(u32, BlockId, Vec<ValueId>)>,
        /// The fallthrough block (for wildcard/binding patterns).
        default: BlockId,
        /// Arguments for the default block's parameters.
        default_args: Vec<ValueId>,
    },
    /// Return from the function with an optional value.
    Return(Option<ValueId>),
    /// Unreachable code (e.g. after an exhaustive match).
    Unreachable,
    /// Placeholder for blocks under construction.  Must be replaced before
    /// the function is complete.
    None,
}
