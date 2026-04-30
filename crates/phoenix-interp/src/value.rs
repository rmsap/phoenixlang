use phoenix_common::module_path::bare_name;
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
        /// Parameter names declared in the closure signature.
        params: Vec<String>,
        /// The AST block that forms the closure body.
        body: phoenix_parser::ast::Block,
        /// Variables captured from the enclosing scope (by shared reference).
        captures: HashMap<String, Rc<RefCell<Value>>>,
    },
}

impl Value {
    /// Returns the human-readable type name for this value.
    ///
    /// For primitive types this returns a fixed string (`"Int"`, `"Float"`,
    /// etc.). For structs and enum variants it returns the source-level
    /// declared name (with any module prefix stripped). Use
    /// [`Self::type_key`] when you need the canonical lookup key.
    pub fn type_name(&self) -> &str {
        match self {
            Value::Int(_) => "Int",
            Value::Float(_) => "Float",
            Value::String(_) => "String",
            Value::Bool(_) => "Bool",
            Value::Void => "Void",
            Value::Struct(name, _) => bare_name(name),
            Value::EnumVariant(enum_name, _, _) => bare_name(enum_name),
            Value::List(_) => "List",
            Value::Map(_) => "Map",
            Value::Closure { .. } => "<function>",
        }
    }

    /// Returns the canonical type key for dispatch (qualified for
    /// user-defined types declared in non-entry modules; bare for
    /// builtins and entry-module types). Same as [`Self::type_name`]
    /// for builtins; differs only when a user struct/enum value
    /// carries a `module::Name` key.
    ///
    /// Invariant: `type_name(v)` is the bare-name suffix of
    /// `type_key(v)` (i.e. `bare_name(type_key()) == type_name()`).
    /// Use `type_key` for symbol-table lookup and `type_name` for
    /// any user-facing rendering — never the other way around.
    pub fn type_key(&self) -> &str {
        match self {
            Value::Struct(name, _) => name,
            Value::EnumVariant(enum_name, _, _) => enum_name,
            _ => self.type_name(),
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
                // Strip the module prefix on the qualified key so user
                // output shows the source-level name (`User`) rather
                // than the canonical key (`models::User`).
                write!(f, "{}(", bare_name(name))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the documented invariant on [`Value::type_key`] /
    /// [`Value::type_name`]: `bare_name(type_key()) == type_name()`.
    /// The methods table is keyed by `type_key`; user-facing
    /// diagnostics render via `type_name`. A regression that broke
    /// the invariant would surface as a confusing dispatch failure
    /// — the error message names a type that doesn't match the table
    /// key the dispatcher actually probed.
    ///
    /// Covers the qualified user, bare user, and builtin shapes, so a
    /// future change that adds a `Value` variant has to extend this
    /// table to stay green.
    #[test]
    fn type_key_round_trips_through_bare_name_to_type_name() {
        let cases: Vec<Value> = vec![
            // Qualified user struct — the cross-module case Phase 2.6
            // introduced; this is the load-bearing one.
            Value::Struct("models::User".to_string(), BTreeMap::new()),
            // Bare user struct (entry-module declaration; module_qualify
            // is identity on entry, so the key is bare).
            Value::Struct("Point".to_string(), BTreeMap::new()),
            // Qualified user enum variant.
            Value::EnumVariant(
                "shapes::Outcome".to_string(),
                "Win".to_string(),
                vec![Value::Int(1)],
            ),
            // Bare user enum variant.
            Value::EnumVariant("Color".to_string(), "Red".to_string(), vec![]),
            // Builtins: `type_key` delegates to `type_name`, and the
            // names contain no `::`, so `bare_name` is a no-op.
            Value::Int(0),
            Value::Float(0.0),
            Value::String(String::new()),
            Value::Bool(false),
            Value::Void,
            Value::List(vec![]),
            Value::Map(vec![]),
        ];

        for v in &cases {
            assert_eq!(
                bare_name(v.type_key()),
                v.type_name(),
                "type_key/type_name invariant broken for {:?}: \
                 type_key = {:?}, bare_name(type_key) = {:?}, type_name = {:?}",
                v,
                v.type_key(),
                bare_name(v.type_key()),
                v.type_name(),
            );
        }
    }

    /// Direct pin: a qualified `Value::Struct` exposes the qualified
    /// key via `type_key` (so the methods table — keyed under
    /// `models::User` post-Phase-2.6 — resolves) but the bare name
    /// via `type_name` (so user-facing rendering shows `User`).
    /// Pre-Phase-2.6 these two were the same; this asserts the
    /// post-2.6 split holds rather than collapsing to either side.
    #[test]
    fn qualified_struct_type_key_and_type_name_split() {
        let v = Value::Struct("models::User".to_string(), BTreeMap::new());
        assert_eq!(v.type_key(), "models::User");
        assert_eq!(v.type_name(), "User");
    }

    /// The same split for enum variants.
    #[test]
    fn qualified_enum_variant_type_key_and_type_name_split() {
        let v = Value::EnumVariant(
            "shapes::Outcome".to_string(),
            "Win".to_string(),
            vec![Value::Int(7)],
        );
        assert_eq!(v.type_key(), "shapes::Outcome");
        assert_eq!(v.type_name(), "Outcome");
    }
}
