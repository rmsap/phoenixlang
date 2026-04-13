//! Basic blocks for the Phoenix IR.
//!
//! A [`BasicBlock`] is a straight-line sequence of instructions ending with
//! a [`Terminator`] that transfers control to another block.

use crate::instruction::{Instruction, ValueId};
use crate::terminator::Terminator;
use crate::types::IrType;
use std::fmt;

/// A unique identifier for a basic block within a function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(pub u32);

impl fmt::Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bb{}", self.0)
    }
}

/// A basic block: a straight-line sequence of instructions ending with
/// a terminator that transfers control to another block.
#[derive(Debug, Clone)]
pub struct BasicBlock {
    /// The unique identifier of this block within its function.
    pub id: BlockId,
    /// Block parameters (SSA phi-node replacements).  When a predecessor
    /// branches to this block, it passes values that bind to these parameters.
    pub params: Vec<(ValueId, IrType)>,
    /// The instructions in this block, in order.
    pub instructions: Vec<Instruction>,
    /// The terminator that ends this block.  Every block must have exactly one.
    pub terminator: Terminator,
}
