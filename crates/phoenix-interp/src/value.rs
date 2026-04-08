use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::rc::Rc;

/// A runtime value in the Phoenix interpreter.
///
/// Every expression in Phoenix evaluates to a `Value`. The interpreter uses
/// this enum to represent all possible runtime types, including primitives,
/// composite structs, and enum variants.
#[derive(Debug, Clone)]
pub enum Value {
    /// A 64-bit signed integer value.
    Int(i64),
    /// A 64-bit floating-point value.
    Float(f64),
    /// A heap-allocated string value.
    String(String),
    /// A boolean value (`true` or `false`).
    Bool(bool),
    /// The unit value, returned by functions with no explicit return value.
    Void,
    /// A struct instance, carrying its type name and a map of field names
    /// to their values.
    Struct(String, BTreeMap<String, Value>),
    /// An enum variant instance, carrying the enum type name, the variant
    /// name, and the positional field values.
    EnumVariant(String, String, Vec<Value>),
    /// A list of values, representing the built-in `List<T>` collection type.
    List(Vec<Value>),
    /// A map of key-value pairs, representing the built-in `Map<K, V>` type.
    /// Uses `Vec<(Value, Value)>` because `Value` does not implement `Hash`.
    Map(Vec<(Value, Value)>),
    /// A closure (first-class function value).
    ///
    /// Carries the parameter names, the AST body to execute, and a map of
    /// captured variables.  Each captured variable is a shared
    /// `Rc<RefCell<Value>>` cell, so mutations inside the closure are
    /// visible in the enclosing scope and vice versa.
    Closure {
        params: Vec<String>,
        body: phoenix_parser::ast::Block,
        captures: HashMap<String, Rc<RefCell<Value>>>,
    },
}

impl Value {
    /// Returns the human-readable type name for this value.
    ///
    /// For primitive types this returns a fixed string (`"Int"`, `"Float"`,
    /// etc.). For structs and enum variants it returns the declared type name.
    pub fn type_name(&self) -> &str {
        match self {
            Value::Int(_) => "Int",
            Value::Float(_) => "Float",
            Value::String(_) => "String",
            Value::Bool(_) => "Bool",
            Value::Void => "Void",
            Value::Struct(name, _) => name,
            Value::EnumVariant(enum_name, _, _) => enum_name,
            Value::List(_) => "List",
            Value::Map(_) => "Map",
            Value::Closure { .. } => "<function>",
        }
    }

    /// Returns whether this value is considered "truthy" in a boolean context.
    ///
    /// Only `Bool(false)` is falsy; all other values (including `Void`) are
    /// truthy.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            _ => true,
        }
    }
}

/// Writes a comma-separated list of items using the provided formatter for each.
fn write_comma_separated(
    f: &mut fmt::Formatter<'_>,
    items: impl IntoIterator<Item = impl fmt::Display>,
) -> fmt::Result {
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        write!(f, "{}", item)?;
    }
    Ok(())
}

/// Formats a [`Value`] for user-facing display output.
///
/// Primitives are printed in their natural form. Structs are displayed as
/// `Name(field: value, ...)` and enum variants as `Variant(value, ...)`.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{}", n),
            Value::Float(n) => write!(f, "{}", n),
            Value::String(s) => write!(f, "{}", s),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Void => write!(f, "void"),
            Value::Struct(name, fields) => {
                write!(f, "{}(", name)?;
                write_comma_separated(f, fields.iter().map(|(k, v)| format!("{}: {}", k, v)))?;
                write!(f, ")")
            }
            Value::EnumVariant(_, variant, fields) => {
                write!(f, "{}", variant)?;
                if !fields.is_empty() {
                    write!(f, "(")?;
                    write_comma_separated(f, fields)?;
                    write!(f, ")")?;
                }
                Ok(())
            }
            Value::List(elements) => {
                write!(f, "[")?;
                write_comma_separated(f, elements)?;
                write!(f, "]")
            }
            Value::Map(entries) => {
                write!(f, "{{")?;
                write_comma_separated(f, entries.iter().map(|(k, v)| format!("{}: {}", k, v)))?;
                write!(f, "}}")
            }
            Value::Closure { .. } => write!(f, "<function>"),
        }
    }
}

/// Compares two [`Value`]s for equality.
///
/// Values of different types are never equal. Struct comparison is not
/// supported; only primitives and enum variants can be compared.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Void, Value::Void) => true,
            (Value::Struct(n1, f1), Value::Struct(n2, f2)) => n1 == n2 && f1 == f2,
            (Value::EnumVariant(e1, v1, f1), Value::EnumVariant(e2, v2, f2)) => {
                e1 == e2 && v1 == v2 && f1 == f2
            }
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => a == b,
            (Value::Closure { .. }, Value::Closure { .. }) => false, // closures are never equal
            _ => false,
        }
    }
}

/// Provides ordering for [`Value`]s of the same numeric or string type.
///
/// Only `Int`-`Int`, `Float`-`Float`, and `String`-`String` comparisons
/// are supported; all other combinations return `None`.
impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a.partial_cmp(b),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::String(a), Value::String(b)) => a.partial_cmp(b),
            _ => None,
        }
    }
}
