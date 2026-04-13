//! Top-level IR module and function definitions.

use crate::block::{BasicBlock, BlockId};
use crate::instruction::{FuncId, Instruction, Op, ValueId};
use crate::terminator::Terminator;
use crate::types::IrType;
use phoenix_common::span::Span;
use std::collections::HashMap;

/// The top-level IR container for a compilation unit.
///
/// Contains all functions (including methods lowered as standalone functions),
/// struct/enum layout metadata, and name-to-ID lookup tables.
#[derive(Debug, Clone)]
pub struct IrModule {
    /// All functions in the module.
    pub functions: Vec<IrFunction>,
    /// Struct layout info: name → ordered `(field_name, field_type)` pairs.
    pub struct_layouts: HashMap<String, Vec<(String, IrType)>>,
    /// Enum layout info: name → variant list, each variant has
    /// `(variant_name, field_types)`.
    pub enum_layouts: HashMap<String, Vec<(String, Vec<IrType>)>>,
    /// Function name → [`FuncId`] mapping for call resolution.
    pub function_index: HashMap<String, FuncId>,
    /// Method dispatch table: `(type_name, method_name)` → [`FuncId`].
    pub method_index: HashMap<(String, String), FuncId>,
}

impl IrModule {
    /// Creates an empty module.
    pub fn new() -> Self {
        Self {
            functions: Vec::new(),
            struct_layouts: HashMap::new(),
            enum_layouts: HashMap::new(),
            function_index: HashMap::new(),
            method_index: HashMap::new(),
        }
    }
}

impl Default for IrModule {
    fn default() -> Self {
        Self::new()
    }
}

/// An IR function.  Methods are lowered as functions with an explicit
/// `self` parameter as the first argument.
#[derive(Debug, Clone)]
pub struct IrFunction {
    /// The unique identifier of this function within the module.
    pub id: FuncId,
    /// The fully qualified name.  Methods are named `"TypeName.method_name"`.
    pub name: String,
    /// Parameter types (including explicit `self` for methods).
    pub param_types: Vec<IrType>,
    /// Parameter names, parallel to `param_types`.
    pub param_names: Vec<String>,
    /// Return type.
    pub return_type: IrType,
    /// The basic blocks, in order.  `blocks[0]` is always the entry block.
    pub blocks: Vec<BasicBlock>,
    /// Counter for fresh [`ValueId`] allocation.
    next_value_id: u32,
    /// Counter for fresh [`BlockId`] allocation.
    next_block_id: u32,
    /// Source span of the original function declaration (for debug info).
    pub span: Option<Span>,
}

impl IrFunction {
    /// Creates a new function with an empty body.
    pub fn new(
        id: FuncId,
        name: String,
        param_types: Vec<IrType>,
        param_names: Vec<String>,
        return_type: IrType,
        span: Option<Span>,
    ) -> Self {
        Self {
            id,
            name,
            param_types,
            param_names,
            return_type,
            blocks: Vec::new(),
            next_value_id: 0,
            next_block_id: 0,
            span,
        }
    }

    /// Allocates a fresh [`ValueId`].
    pub fn fresh_value(&mut self) -> ValueId {
        let id = ValueId(self.next_value_id);
        self.next_value_id += 1;
        id
    }

    /// Creates a new basic block and returns its [`BlockId`].
    /// The block is appended to `self.blocks` with an empty body and
    /// a [`Terminator::None`] placeholder.
    pub fn create_block(&mut self) -> BlockId {
        let id = BlockId(self.next_block_id);
        self.next_block_id += 1;
        self.blocks.push(BasicBlock {
            id,
            params: Vec::new(),
            instructions: Vec::new(),
            terminator: Terminator::None,
        });
        id
    }

    /// Returns a mutable reference to the block with the given ID.
    ///
    /// # Panics
    ///
    /// Panics if the block ID does not correspond to a block in this function.
    pub fn block_mut(&mut self, id: BlockId) -> &mut BasicBlock {
        &mut self.blocks[id.0 as usize]
    }

    /// Returns a reference to the block with the given ID.
    ///
    /// # Panics
    ///
    /// Panics if the block ID does not correspond to a block in this function.
    pub fn block(&self, id: BlockId) -> &BasicBlock {
        &self.blocks[id.0 as usize]
    }

    /// Appends an instruction to the specified block and returns its result
    /// [`ValueId`] (if the instruction produces a value).
    pub fn emit(
        &mut self,
        block: BlockId,
        op: Op,
        result_type: IrType,
        span: Option<Span>,
    ) -> Option<ValueId> {
        let result = if result_type != IrType::Void {
            Some(self.fresh_value())
        } else {
            None
        };
        let inst = Instruction {
            result,
            result_type,
            op,
            span,
        };
        self.block_mut(block).instructions.push(inst);
        result
    }

    /// Appends an instruction that always produces a value.
    ///
    /// # Panics
    ///
    /// Panics if `result_type` is `Void`.
    pub fn emit_value(
        &mut self,
        block: BlockId,
        op: Op,
        result_type: IrType,
        span: Option<Span>,
    ) -> ValueId {
        assert!(
            result_type != IrType::Void,
            "emit_value called with Void type"
        );
        self.emit(block, op, result_type, span)
            .expect("non-void instruction must produce a value")
    }

    /// Sets the terminator for the specified block.
    pub fn set_terminator(&mut self, block: BlockId, term: Terminator) {
        self.block_mut(block).terminator = term;
    }

    /// Adds a block parameter and returns its [`ValueId`].
    pub fn add_block_param(&mut self, block: BlockId, ty: IrType) -> ValueId {
        let id = self.fresh_value();
        self.block_mut(block).params.push((id, ty));
        id
    }
}
