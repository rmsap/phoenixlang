//! IR-level type representation.
//!
//! [`IrType`] is deliberately simpler than the source-level [`phoenix_sema::types::Type`].
//! Value types (integers, floats, booleans) live in registers or on the stack.
//! Reference types are GC-managed heap pointers.

use std::fmt;

/// Sentinel name used in [`IrType::StructRef`] to represent an unresolved
/// generic type parameter.  The concrete type is determined at each use site
/// via type inference or monomorphization.
pub const GENERIC_PLACEHOLDER: &str = "__generic";

/// The name of the built-in `Option` enum in the IR.
pub const OPTION_ENUM: &str = "Option";
/// The name of the built-in `Result` enum in the IR.
pub const RESULT_ENUM: &str = "Result";

/// A type in the IR.
///
/// Value types (`I64`, `F64`, `Bool`) are passed by copy and live in
/// registers.  Reference types (`StringRef`, `StructRef`, …) are
/// pointers to GC-managed heap objects.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IrType {
    /// Signed 64-bit integer.  Maps to Cranelift `i64`.
    I64,
    /// IEEE 754 double-precision float.  Maps to Cranelift `f64`.
    F64,
    /// Boolean.  Maps to Cranelift `i8` (0 or 1).
    Bool,
    /// The unit type (zero-sized).  Functions returning `Void` produce no value.
    Void,

    // --- Reference types (GC-managed pointers) ---
    /// Heap-allocated, immutable UTF-8 string.
    StringRef,
    /// Heap-allocated struct instance.  The [`String`] is the struct name,
    /// used for layout lookup.
    StructRef(String),
    /// Heap-allocated enum value (tagged union).  The [`String`] is the enum
    /// name, used for variant layout lookup.
    EnumRef(String),
    /// Heap-allocated `List<T>`.
    ListRef(Box<IrType>),
    /// Heap-allocated `Map<K, V>`.
    MapRef(Box<IrType>, Box<IrType>),
    /// Heap-allocated closure object (code pointer + captured environment).
    ClosureRef {
        /// Parameter types of the closure.
        param_types: Vec<IrType>,
        /// Return type of the closure.
        return_type: Box<IrType>,
    },

    /// An unresolved generic type parameter by name (e.g., `T`, `U`).
    /// Present only in pre-monomorphization IR for generic function
    /// templates. The monomorphization pass substitutes each `TypeVar(name)`
    /// with a concrete `IrType` when specializing a template. Post-mono, no
    /// function body should contain `TypeVar`; downstream consumers
    /// (interpreter, Cranelift backend) treat it as `unreachable!()`.
    ///
    /// Distinct from [`GENERIC_PLACEHOLDER`], which is a nameless sentinel
    /// used by built-in enum layouts (Option/Result/List/Map) whose concrete
    /// types are resolved at use sites via inference strategies rather than
    /// by monomorphization.
    TypeVar(String),
}

impl IrType {
    /// Returns `true` if this is a value type that can live in a register.
    ///
    /// # Panics
    ///
    /// Panics on [`IrType::TypeVar`]: unresolved type parameters have no
    /// representation, and must be substituted (by monomorphization) or
    /// erased (to [`GENERIC_PLACEHOLDER`]) before any classification
    /// question is meaningful. See [`IrType::is_type_var`] to test for
    /// the variant without triggering the panic.
    pub fn is_value_type(&self) -> bool {
        match self {
            IrType::I64 | IrType::F64 | IrType::Bool | IrType::Void => true,
            IrType::StringRef
            | IrType::StructRef(_)
            | IrType::EnumRef(_)
            | IrType::ListRef(_)
            | IrType::MapRef(_, _)
            | IrType::ClosureRef { .. } => false,
            IrType::TypeVar(name) => unreachable!(
                "IrType::is_value_type on TypeVar({name}) — monomorphization \
                 or erasure must eliminate type variables before classification"
            ),
        }
    }

    /// Returns `true` if this is a reference type (GC-managed heap pointer).
    ///
    /// # Panics
    ///
    /// See [`IrType::is_value_type`].
    pub fn is_ref_type(&self) -> bool {
        !self.is_value_type()
    }

    /// Returns `true` if this type is an unresolved generic parameter.
    /// Unlike [`IrType::is_value_type`] / [`IrType::is_ref_type`], this
    /// never panics on `TypeVar`.
    pub fn is_type_var(&self) -> bool {
        matches!(self, IrType::TypeVar(_))
    }

    /// Returns `true` if this type is the [`GENERIC_PLACEHOLDER`] sentinel,
    /// representing an unresolved generic type parameter.
    pub fn is_generic_placeholder(&self) -> bool {
        matches!(self, IrType::StructRef(n) if n == GENERIC_PLACEHOLDER)
    }

    /// Recursively replace every [`IrType::TypeVar`] occurrence in `self`
    /// with `StructRef(GENERIC_PLACEHOLDER)`. Used by post-monomorphization
    /// cleanup (for orphan type variables that escaped sema inference,
    /// e.g. empty list literals without annotations) and by built-in
    /// enum-layout registration (where the concrete payload type is
    /// resolved at use sites via inference strategies in the Cranelift
    /// backend, not by monomorphization).
    pub fn erase_type_vars(&self) -> IrType {
        match self {
            IrType::TypeVar(_) => IrType::StructRef(GENERIC_PLACEHOLDER.to_string()),
            IrType::ListRef(inner) => IrType::ListRef(Box::new(inner.erase_type_vars())),
            IrType::MapRef(k, v) => {
                IrType::MapRef(Box::new(k.erase_type_vars()), Box::new(v.erase_type_vars()))
            }
            IrType::ClosureRef {
                param_types,
                return_type,
            } => IrType::ClosureRef {
                param_types: param_types.iter().map(IrType::erase_type_vars).collect(),
                return_type: Box::new(return_type.erase_type_vars()),
            },
            IrType::I64
            | IrType::F64
            | IrType::Bool
            | IrType::Void
            | IrType::StringRef
            | IrType::StructRef(_)
            | IrType::EnumRef(_) => self.clone(),
        }
    }
}

impl fmt::Display for IrType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IrType::I64 => write!(f, "i64"),
            IrType::F64 => write!(f, "f64"),
            IrType::Bool => write!(f, "bool"),
            IrType::Void => write!(f, "void"),
            IrType::StringRef => write!(f, "string"),
            IrType::StructRef(name) => write!(f, "struct.{name}"),
            IrType::EnumRef(name) => write!(f, "enum.{name}"),
            IrType::ListRef(elem) => write!(f, "list<{elem}>"),
            IrType::MapRef(k, v) => write!(f, "map<{k}, {v}>"),
            IrType::ClosureRef {
                param_types,
                return_type,
            } => {
                write!(f, "closure(")?;
                for (i, p) in param_types.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{p}")?;
                }
                write!(f, ") -> {return_type}")
            }
            IrType::TypeVar(name) => write!(f, "{name}"),
        }
    }
}
