//! IR instructions and SSA value identifiers.
//!
//! Each [`Instruction`] produces at most one SSA value (identified by its
//! [`ValueId`]) and performs an operation described by [`Op`].

use crate::types::IrType;
use phoenix_common::span::Span;
use std::fmt;

/// A unique identifier for an SSA value (a virtual register).
///
/// Each instruction that produces a value assigns a fresh `ValueId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ValueId(pub u32);

/// Sentinel value returned by [`LoweringContext::emit`](crate::lower::LoweringContext)
/// for void-typed operations.  Must never appear as an operand in any
/// instruction or terminator — the verifier checks this.
pub const VOID_SENTINEL: ValueId = ValueId(u32::MAX);

impl fmt::Display for ValueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// A unique identifier for a function within a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FuncId(pub u32);

impl fmt::Display for FuncId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "f{}", self.0)
    }
}

/// An IR instruction that performs an operation and optionally produces
/// an SSA value.
#[derive(Debug, Clone)]
pub struct Instruction {
    /// The SSA value this instruction defines, or `None` for side-effect-only
    /// operations (e.g. `Store`).
    pub result: Option<ValueId>,
    /// The type of the result value.
    pub result_type: IrType,
    /// The operation performed.
    pub op: Op,
    /// Source span for debug info and error messages.
    pub span: Option<Span>,
}

/// The set of IR operations.
#[derive(Debug, Clone)]
pub enum Op {
    // --- Constants ---
    /// Materialize an integer constant.
    ConstI64(i64),
    /// Materialize a float constant.
    ConstF64(f64),
    /// Materialize a boolean constant.
    ConstBool(bool),
    /// Materialize a string constant (allocates on the GC heap).
    ConstString(String),

    // --- Integer arithmetic ---
    /// Integer addition.
    IAdd(ValueId, ValueId),
    /// Integer subtraction.
    ISub(ValueId, ValueId),
    /// Integer multiplication.
    IMul(ValueId, ValueId),
    /// Integer division.
    IDiv(ValueId, ValueId),
    /// Integer modulo.
    IMod(ValueId, ValueId),
    /// Integer negation.
    INeg(ValueId),

    // --- Float arithmetic ---
    /// Float addition.
    FAdd(ValueId, ValueId),
    /// Float subtraction.
    FSub(ValueId, ValueId),
    /// Float multiplication.
    FMul(ValueId, ValueId),
    /// Float division.
    FDiv(ValueId, ValueId),
    /// Float modulo.
    FMod(ValueId, ValueId),
    /// Float negation.
    FNeg(ValueId),

    // --- Integer comparison ---
    /// Integer equality.
    IEq(ValueId, ValueId),
    /// Integer inequality.
    INe(ValueId, ValueId),
    /// Integer less-than.
    ILt(ValueId, ValueId),
    /// Integer greater-than.
    IGt(ValueId, ValueId),
    /// Integer less-than-or-equal.
    ILe(ValueId, ValueId),
    /// Integer greater-than-or-equal.
    IGe(ValueId, ValueId),

    // --- Float comparison ---
    /// Float equality.
    FEq(ValueId, ValueId),
    /// Float inequality.
    FNe(ValueId, ValueId),
    /// Float less-than.
    FLt(ValueId, ValueId),
    /// Float greater-than.
    FGt(ValueId, ValueId),
    /// Float less-than-or-equal.
    FLe(ValueId, ValueId),
    /// Float greater-than-or-equal.
    FGe(ValueId, ValueId),

    // --- String comparison ---
    /// String equality.
    StringEq(ValueId, ValueId),
    /// String inequality.
    StringNe(ValueId, ValueId),
    /// String less-than (lexicographic).
    StringLt(ValueId, ValueId),
    /// String greater-than (lexicographic).
    StringGt(ValueId, ValueId),
    /// String less-than-or-equal.
    StringLe(ValueId, ValueId),
    /// String greater-than-or-equal.
    StringGe(ValueId, ValueId),

    // --- Boolean comparison ---
    /// Boolean equality.
    BoolEq(ValueId, ValueId),
    /// Boolean inequality.
    BoolNe(ValueId, ValueId),

    // --- Logic ---
    /// Boolean negation.
    BoolNot(ValueId),
    // Note: `and`/`or` are lowered to conditional branches for short-circuit
    // evaluation — they are not instructions.

    // --- String operations ---
    /// Concatenate two strings, producing a new `StringRef`.
    StringConcat(ValueId, ValueId),

    // --- Struct operations ---
    /// Allocate a struct on the GC heap with field values in declaration order.
    StructAlloc(String, Vec<ValueId>),
    /// Load a field from a struct by index.
    StructGetField(ValueId, u32),
    /// Store a value into a struct field by index.
    StructSetField(ValueId, u32, ValueId),

    // --- Enum operations ---
    /// Allocate an enum variant on the GC heap.
    /// `EnumAlloc(enum_name, variant_index, field_values)`.
    EnumAlloc(String, u32, Vec<ValueId>),
    /// Extract the discriminant (variant index) from an enum value.
    EnumDiscriminant(ValueId),
    /// Extract a field from an enum variant (after confirming the variant).
    /// `EnumGetField(enum_value, variant_index, field_index)`.
    EnumGetField(ValueId, u32, u32),

    // --- Collection operations ---
    /// Allocate a list on the GC heap from initial elements.
    ListAlloc(Vec<ValueId>),
    /// Allocate a map on the GC heap from key-value pairs.
    MapAlloc(Vec<(ValueId, ValueId)>),

    // --- Closure operations ---
    /// Allocate a closure on the GC heap.
    /// `ClosureAlloc(target_func_id, captured_values)`.
    ClosureAlloc(FuncId, Vec<ValueId>),

    // --- Function calls ---
    /// Direct call to a known function.
    Call(FuncId, Vec<ValueId>),
    /// Indirect call through a closure value.
    CallIndirect(ValueId, Vec<ValueId>),
    /// Call a built-in runtime function by name (e.g. `"print"`, `"String.length"`).
    BuiltinCall(String, Vec<ValueId>),

    // --- Mutable variables (before mem2reg) ---
    /// Allocate a stack slot for a mutable local variable.
    Alloca(IrType),
    /// Load from a stack slot.
    Load(ValueId),
    /// Store to a stack slot.  `Store(slot, value)`.
    Store(ValueId, ValueId),

    // --- Miscellaneous ---
    /// Copy a value (used for block parameter lowering).
    Copy(ValueId),
}
