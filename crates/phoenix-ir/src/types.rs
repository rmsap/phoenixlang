//! IR-level type representation.
//!
//! [`IrType`] is deliberately simpler than the source-level [`phoenix_sema::types::Type`].
//! Value types (integers, floats, booleans) live in registers or on the stack.
//! Reference types are GC-managed heap pointers.

use std::fmt;

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
}

impl IrType {
    /// Returns `true` if this is a value type that can live in a register.
    pub fn is_value_type(&self) -> bool {
        matches!(
            self,
            IrType::I64 | IrType::F64 | IrType::Bool | IrType::Void
        )
    }

    /// Returns `true` if this is a reference type (GC-managed heap pointer).
    pub fn is_ref_type(&self) -> bool {
        !self.is_value_type()
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
        }
    }
}
