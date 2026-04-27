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
///
/// Re-exported from [`phoenix_common::ids`] so sema and IR share a
/// single id space. Sema's registration pass allocates ids in AST
/// order; IR lowering adopts them for user-declared functions and
/// appends synthesized ones (closures, generic specializations) past
/// the end.
pub use phoenix_common::ids::FuncId;

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
    ///
    /// The middle `Vec<IrType>` carries concrete generic type arguments
    /// in the callee's declared type-parameter order. It is empty for
    /// calls to non-generic functions and for calls emitted after the
    /// monomorphization pass (which rewrites generic calls to point at
    /// specialized `FuncId`s and clears the type-args).
    Call(FuncId, Vec<IrType>, Vec<ValueId>),
    /// Indirect call through a closure value.
    CallIndirect(ValueId, Vec<ValueId>),
    /// Call a built-in runtime function by name (e.g. `"print"`, `"String.length"`).
    BuiltinCall(String, Vec<ValueId>),
    /// Placeholder for a trait-bounded method call whose receiver is a
    /// type variable at lowering time.
    ///
    /// Emitted by `lower_method_call` when the receiver's static type
    /// is `Type::TypeVar(T)` with `<T: Trait>` — the concrete impl is
    /// not known yet, so the call cannot be materialized as an
    /// `Op::Call`.  The monomorphization pass rewrites it into a direct
    /// `Op::Call` after substituting `T` with each concrete type.
    ///
    /// Fields: `(method_name, method_type_args, args)`.  `args[0]` is
    /// the receiver; `args[1..]` are the user-visible arguments.
    /// `method_type_args` carries concrete type arguments bound to the
    /// method's own type parameters (`function greet<U>(self, x: U)`)
    /// — empty for the common zero-generic-method case.
    ///
    /// Invariant: no [`Op::UnresolvedTraitMethod`] may survive the
    /// monomorphization pass — the verifier enforces this for every
    /// non-template function.
    UnresolvedTraitMethod(String, Vec<IrType>, Vec<ValueId>),

    // --- Trait object operations ---
    /// `DynAlloc(trait_name, concrete_type, value)` — materialize a
    /// `dyn Trait` fat pointer from a concrete receiver. Result type is
    /// `IrType::DynRef(trait_name)`. The verifier requires a registered
    /// vtable for `(concrete_type, trait_name)`.
    DynAlloc(String, String, ValueId),
    /// Placeholder `dyn Trait` coercion whose source value is typed by a
    /// generic type parameter at lowering time.
    ///
    /// Fields: `(trait_name, value)`. Result type is
    /// `IrType::DynRef(trait_name)`.
    ///
    /// # Emission
    ///
    /// Emitted by `coerce_to_expected` when a value with IR type
    /// `IrType::TypeVar(T)` flows into a `dyn Trait` slot inside a
    /// generic function body (e.g. `let d: dyn Drawable = x` where
    /// `<T: Drawable>`). The concrete type is unknown until
    /// monomorphization specializes the containing function, so vtable
    /// registration has to be deferred.
    ///
    /// # Resolution
    ///
    /// Function-monomorphization's Pass B rewrites this into a concrete
    /// `Op::DynAlloc` after substituting `T` with each concrete type
    /// and registers the corresponding `(concrete, trait)` vtable on
    /// the module. See
    /// `phoenix-ir/src/monomorphize/placeholder_resolution.rs::resolve_unresolved_dyn_allocs`.
    ///
    /// # Invariant
    ///
    /// No [`Op::UnresolvedDynAlloc`] may survive the monomorphization
    /// pass — the verifier enforces this for every non-template
    /// function.
    UnresolvedDynAlloc(String, ValueId),
    /// `DynCall(trait_name, method_idx, receiver, args)` — indirect
    /// dispatch through a trait-object vtable. `method_idx` is the slot
    /// index in the trait's declared method order (pinned by
    /// `TraitInfo::methods`; also the order in `IrModule::dyn_vtables`).
    /// The verifier requires that `receiver: IrType::DynRef(trait_name)`
    /// and that the slot index is in range.
    DynCall(String, u32, ValueId, Vec<ValueId>),

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

impl Op {
    /// Every [`ValueId`] this op reads as an operand, in a stable order.
    /// Used by the verifier and by any pass that needs to walk SSA
    /// use-sites. Adding a new `Op` variant must add a match arm here
    /// (enforced by exhaustive match — omitting an arm is a compile
    /// error).
    pub fn operands(&self) -> Vec<ValueId> {
        match self {
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
            Op::EnumGetField(v, _, _) => vec![*v],

            // Collection ops.
            Op::ListAlloc(vals) => vals.clone(),
            Op::MapAlloc(pairs) => pairs.iter().flat_map(|(k, v)| [*k, *v]).collect(),

            // Closure ops.
            Op::ClosureAlloc(_, vals) => vals.clone(),

            // Call ops.
            Op::Call(_, _, args) => args.clone(),
            Op::CallIndirect(callee, args) => {
                let mut ops = vec![*callee];
                ops.extend(args);
                ops
            }
            Op::BuiltinCall(_, args) => args.clone(),
            Op::UnresolvedTraitMethod(_, _, args) => args.clone(),

            // Trait object ops.
            Op::DynAlloc(_, _, value) => vec![*value],
            Op::UnresolvedDynAlloc(_, value) => vec![*value],
            Op::DynCall(_, _, receiver, args) => {
                let mut ops = vec![*receiver];
                ops.extend_from_slice(args);
                ops
            }

            // Alloca — no value operands.
            Op::Alloca(_) => Vec::new(),
        }
    }
}
