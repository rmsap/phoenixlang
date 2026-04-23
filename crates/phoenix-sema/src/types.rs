/// The types in the Phoenix type system.
///
/// This enum represents every type the compiler currently understands.
/// Built-in primitive types (`Int`, `Float`, `String`, `Bool`, `Void`) are
/// represented as distinct variants, while user-defined or unresolved types
/// are captured by [`Type::Named`].  The special [`Type::Error`] variant is
/// used as a sentinel to suppress cascading diagnostics after a type error
/// has already been reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    /// A signed integer type (e.g. `42`).
    Int,
    /// A floating-point type (e.g. `3.14`).
    Float,
    /// A UTF-8 string type (e.g. `"hello"`).
    String,
    /// A boolean type (`true` or `false`).
    Bool,
    /// The unit type, used for functions that do not return a value.
    Void,
    /// A named type that hasn't been resolved yet or is user-defined.
    Named(std::string::String),
    /// A function type representing closures and first-class function values.
    ///
    /// The first element contains the parameter types and the second contains
    /// the return type.  For example, `(Int, Int) -> Bool` is represented as
    /// `Function(vec![Int, Int], Box::new(Bool))`.
    Function(Vec<Type>, Box<Type>),
    /// A type variable (generic parameter) such as `T` or `U`, used within
    /// generic function/struct/enum bodies.
    TypeVar(std::string::String),
    /// A generic type application such as `Option<Int>` or `Pair<Int, String>`.
    ///
    /// The first element is the base type name and the second is the list of
    /// concrete type arguments supplied at the use site.
    Generic(std::string::String, Vec<Type>),
    /// A trait-object type (`dyn TraitName`).  Concrete values are carried
    /// behind a `(data_ptr, vtable_ptr)` pair at the IR/runtime level.
    /// The inner string is the trait name.  See `docs/design-decisions.md`
    /// for the "why explicit `dyn`" rationale — bare `TraitName` as a type
    /// remains an error; `dyn TraitName` is the runtime-dispatch opt-in.
    Dyn(std::string::String),
    /// Error type — used when type checking fails to avoid cascading errors.
    Error,
}

impl Type {
    /// Resolve a type-name string to a [`Type`].
    ///
    /// The five built-in type names (`"Int"`, `"Float"`, `"String"`, `"Bool"`,
    /// `"Void"`) are matched **case-sensitively** and mapped to the
    /// corresponding variant.  Any other name is wrapped in
    /// [`Type::Named`].
    pub fn from_name(name: &str) -> Type {
        match name {
            "Int" => Type::Int,
            "Float" => Type::Float,
            "String" => Type::String,
            "Bool" => Type::Bool,
            "Void" => Type::Void,
            other => Type::Named(other.to_string()),
        }
    }

    /// Returns `true` if the type is a numeric type (`Int` or `Float`).
    ///
    /// This is used during type checking to validate operands of arithmetic
    /// and comparison operators.
    #[must_use]
    pub fn is_numeric(&self) -> bool {
        matches!(self, Type::Int | Type::Float)
    }

    /// Returns `true` if this type contains any unresolved type variables.
    ///
    /// The check is recursive: a `Generic` or `Function` type reports `true`
    /// if any of its nested types contain a [`Type::TypeVar`].
    pub fn has_type_vars(&self) -> bool {
        match self {
            Type::TypeVar(_) => true,
            Type::Generic(_, args) => args.iter().any(|a| a.has_type_vars()),
            Type::Function(params, ret) => {
                params.iter().any(|p| p.has_type_vars()) || ret.has_type_vars()
            }
            _ => false,
        }
    }

    /// Returns `true` if this is the [`Type::Error`] sentinel.
    ///
    /// When a sub-expression has already been flagged as erroneous the
    /// checker uses this predicate to skip further diagnostics and avoid
    /// cascading error messages.
    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self, Type::Error)
    }

    /// Returns `true` if this type is a type variable.
    #[must_use]
    pub fn is_type_var(&self) -> bool {
        matches!(self, Type::TypeVar(_))
    }
}

/// Creates a `List<T>` type with the given element type.
///
/// This is a convenience constructor for the built-in generic `List` type,
/// equivalent to `Type::Generic("List".to_string(), vec![element_type])`.
pub fn list_of(element_type: Type) -> Type {
    Type::Generic("List".to_string(), vec![element_type])
}

/// Shorthand for constructing `Map<K, V>`.
pub fn map_of(key_type: Type, value_type: Type) -> Type {
    Type::Generic("Map".to_string(), vec![key_type, value_type])
}

/// Formats the type for user-facing messages.
///
/// Built-in types are displayed by their canonical name (e.g. `"Int"`,
/// `"Float"`).  [`Type::Named`] prints the stored name verbatim, and
/// [`Type::Error`] is rendered as `"<error>"`.
impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Int => write!(f, "Int"),
            Type::Float => write!(f, "Float"),
            Type::String => write!(f, "String"),
            Type::Bool => write!(f, "Bool"),
            Type::Void => write!(f, "Void"),
            Type::Named(name) => write!(f, "{}", name),
            Type::Function(params, ret) => {
                write!(f, "(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", p)?;
                }
                write!(f, ") -> {}", ret)
            }
            Type::TypeVar(name) => write!(f, "{}", name),
            Type::Generic(name, args) => {
                write!(f, "{}<", name)?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", a)?;
                }
                write!(f, ">")
            }
            Type::Dyn(name) => write!(f, "dyn {}", name),
            Type::Error => write!(f, "<error>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_name_builtin_types() {
        assert_eq!(Type::from_name("Int"), Type::Int);
        assert_eq!(Type::from_name("Float"), Type::Float);
        assert_eq!(Type::from_name("String"), Type::String);
        assert_eq!(Type::from_name("Bool"), Type::Bool);
        assert_eq!(Type::from_name("Void"), Type::Void);
    }

    #[test]
    fn from_name_unknown_type() {
        assert_eq!(
            Type::from_name("MyStruct"),
            Type::Named("MyStruct".to_string())
        );
    }

    #[test]
    fn from_name_case_sensitive() {
        // Lowercase "int" is not a built-in; it should produce Named.
        assert_eq!(Type::from_name("int"), Type::Named("int".to_string()));
    }

    #[test]
    fn is_numeric_for_int_and_float() {
        assert!(Type::Int.is_numeric());
        assert!(Type::Float.is_numeric());
    }

    #[test]
    fn is_numeric_for_non_numeric() {
        assert!(!Type::String.is_numeric());
        assert!(!Type::Bool.is_numeric());
        assert!(!Type::Void.is_numeric());
        assert!(!Type::Named("Foo".to_string()).is_numeric());
        assert!(!Type::Error.is_numeric());
    }

    #[test]
    fn is_error_only_for_error() {
        assert!(Type::Error.is_error());
        assert!(!Type::Int.is_error());
        assert!(!Type::Float.is_error());
        assert!(!Type::String.is_error());
        assert!(!Type::Bool.is_error());
        assert!(!Type::Void.is_error());
        assert!(!Type::Named("X".to_string()).is_error());
    }

    #[test]
    fn display_all_variants() {
        assert_eq!(Type::Int.to_string(), "Int");
        assert_eq!(Type::Float.to_string(), "Float");
        assert_eq!(Type::String.to_string(), "String");
        assert_eq!(Type::Bool.to_string(), "Bool");
        assert_eq!(Type::Void.to_string(), "Void");
        assert_eq!(Type::Named("Foo".to_string()).to_string(), "Foo");
        assert_eq!(Type::Error.to_string(), "<error>");
    }
}
