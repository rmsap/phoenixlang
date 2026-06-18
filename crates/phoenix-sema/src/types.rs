use phoenix_common::module_path::bare_name;

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
    /// A binary file type, valid **only** in Phoenix Gen endpoint request/response
    /// bodies (multipart uploads / binary downloads). A struct field of type
    /// `File` makes its containing struct a body-only, non-JSON transport type;
    /// sema rejects `File` in any other position. Because endpoints are
    /// compile-time-only, `File` never reaches IR/runtime — `lower_type` has an
    /// `unreachable!` arm. The restriction is liftable when the language later
    /// gains real file-handle semantics (a non-breaking relaxation).
    File,
    /// An instant in time, serialized on the wire as an RFC 3339 / ISO 8601
    /// string (e.g. `2026-06-16T12:00:00Z`). A Phoenix Gen scalar usable
    /// wherever `String` is (struct fields, query/header params, scalar
    /// responses, multipart form fields) — unlike `File`, it is NOT
    /// position-restricted. Lowers to `IrType::StringRef` (see `lower_type`);
    /// it has no literals or operations in the language yet. See
    /// `docs/design-decisions.md` (DateTime & UUID scalar types).
    DateTime,
    /// A UUID, serialized on the wire as the canonical hyphenated string (e.g.
    /// `550e8400-e29b-41d4-a716-446655440000`). A Phoenix Gen scalar usable
    /// wherever `String` is — like `DateTime`, NOT position-restricted. Lowers to
    /// `IrType::StringRef` (see `lower_type`); it has no literals or operations in
    /// the language yet. The targets validate it to differing degrees (Python
    /// `uuid.UUID` and the TS `parseUuid` decode pass check the format; Go keeps
    /// it a `string` checked only in `Validate()`). See `docs/design-decisions.md`
    /// (DateTime & UUID scalar types).
    Uuid,
    /// An exact base-10 decimal, serialized on the wire as a string (e.g.
    /// `"19.99"`) to avoid the precision loss of a JSON float. A Phoenix Gen
    /// scalar usable wherever `String` is — like `DateTime`/`Uuid`, NOT
    /// position-restricted. Lowers to `IrType::StringRef`. Transport-only for now:
    /// Python gets real `decimal.Decimal` arithmetic (free, stdlib); TS/Go carry
    /// it as a validated string (no arithmetic). `Money` (Decimal + currency) and
    /// arithmetic via MIT libs are deferred. See `docs/design-decisions.md`
    /// (Decimal scalar type).
    Decimal,
    /// A monetary amount: an exact `Decimal` amount plus an ISO-4217 currency
    /// code, serialized on the wire as the object `{ "amount": "19.99",
    /// "currency": "USD" }`. A Phoenix Gen *composite* built-in (each target emits
    /// a `Money` type definition) with the same legal positions as a struct —
    /// struct/body fields and responses; `check_endpoint` rejects it in query-param
    /// or header position (a composite isn't URL/header-encodable). Lowers to
    /// `IrType::StringRef` only as a never-hit placeholder (Gen never lowers to IR,
    /// and the language has no `Money` literal). See `docs/design-decisions.md`
    /// (Money type).
    Money,
    /// The unit type, used for functions that do not return a value.
    Void,
    /// An opaque JavaScript-host value handle.
    ///
    /// Produced and consumed only at the `extern js` boundary: Phoenix never
    /// inspects a `JsValue`, it only holds one and passes it back to a host
    /// function. The runtime representation is per-backend (an `i32` handle
    /// table index on `wasm32-linear`, an `externref` on `wasm32-gc`, an opaque
    /// host handle in the interpreters / native) — see `docs/design-decisions.md`
    /// §Phase 2.5 (decision D). There is no `JsValue` literal in the language;
    /// the only way to obtain one is an extern call's return value. Until the
    /// per-backend lowering lands (PR 3+), `lower_type` maps it to a placeholder
    /// `IrType`. That arm is unreachable today not because `JsValue` is
    /// unspellable (it *is* spellable, unlike `Money`) but because a program
    /// using `extern js` is rejected on every execution path before lowering —
    /// see `reject_extern_js_for_execution` in `phoenix-driver` and the note on
    /// the `JsValue` arm of `lower_type`.
    JsValue,
    /// A named type that hasn't been resolved yet or is user-defined.
    ///
    /// Post-Phase 2.6 the payload is the canonical *qualified* key
    /// (`lib::User`); `Display` strips the prefix for user-facing output.
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
    ///
    /// Post-Phase 2.6 the base-name payload is the canonical *qualified* key
    /// (`lib::Pair`); `Display` strips the prefix for user-facing output.
    Generic(std::string::String, Vec<Type>),
    /// A trait-object type (`dyn TraitName`).  Concrete values are carried
    /// behind a `(data_ptr, vtable_ptr)` pair at the IR/runtime level.
    /// The inner string is the trait name.  See `docs/design-decisions.md`
    /// for the "why explicit `dyn`" rationale — bare `TraitName` as a type
    /// remains an error; `dyn TraitName` is the runtime-dispatch opt-in.
    ///
    /// Post-Phase 2.6 the trait-name payload is the canonical *qualified*
    /// key (`shapes::Drawable`); `Display` strips the prefix for
    /// user-facing output.
    Dyn(std::string::String),
    /// Error type — used when type checking fails to avoid cascading errors.
    Error,
}

impl Type {
    /// Resolve a type-name string to a [`Type`].
    ///
    /// The built-in type names (`"Int"`, `"Float"`, `"String"`, `"Bool"`,
    /// `"File"`, `"DateTime"`, `"Uuid"`, `"Decimal"`, `"Money"`, `"Void"`) are
    /// matched **case-sensitively** and mapped to the corresponding variant.  Any
    /// other name is wrapped in [`Type::Named`].  (`File` is only *legal* in
    /// endpoint-body position — that restriction is enforced in sema, not here.
    /// `DateTime`/`Uuid`/`Decimal` are plain scalars and `Money` a composite, all
    /// with no such restriction.)
    pub fn from_name(name: &str) -> Type {
        match name {
            "Int" => Type::Int,
            "Float" => Type::Float,
            "String" => Type::String,
            "Bool" => Type::Bool,
            "File" => Type::File,
            "DateTime" => Type::DateTime,
            "Uuid" => Type::Uuid,
            "Decimal" => Type::Decimal,
            "Money" => Type::Money,
            "JsValue" => Type::JsValue,
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

    /// Returns `true` if this type may cross the `extern js` host-FFI boundary.
    /// The marshallable set is the scalars `Int` / `Float` / `Bool`
    /// / `String`, the opaque `JsValue` handle, `Void`, and a
    /// `Function(...)` whose parameter and return types are themselves
    /// marshallable (closures-as-callbacks). `Void` is meaningful chiefly in
    /// return position; this predicate is position-agnostic and accepts it
    /// uniformly (a `Void` parameter is nonsensical but harmless — there is no
    /// value to marshal). Aggregates (`Named` structs/enums,
    /// `Generic` `List`/`Map`/`Option`/…, `Dyn`), type variables, the Gen-only
    /// scalars (`File`/`DateTime`/`Uuid`/`Decimal`/`Money`), and the `Error`
    /// sentinel are NOT marshallable — aggregate marshalling is deferred (see
    /// `docs/design-decisions.md` §Phase 2.5). Used by extern-signature
    /// registration to reject non-marshallable boundary types.
    #[must_use]
    pub fn is_js_marshallable(&self) -> bool {
        match self {
            Type::Int | Type::Float | Type::Bool | Type::String | Type::JsValue | Type::Void => {
                true
            }
            Type::Function(params, ret) => {
                params.iter().all(Type::is_js_marshallable) && ret.is_js_marshallable()
            }
            _ => false,
        }
    }

    /// Returns `true` if this type is — or has a generic argument that is —
    /// [`Type::Money`]. Recurses through generics so `Option<Money>`/`List<Money>`/
    /// `Map<_, Money>` are caught, but does NOT recurse into [`Type::Named`]
    /// structs (a struct *field* `Money` is a separate, legal case).
    ///
    /// Shared by sema (`check_endpoint`'s query/header position restriction) and
    /// codegen (`schema_uses_money`'s emit gate) so both agree on what "mentions
    /// `Money`" means.
    #[must_use]
    pub fn mentions_money(&self) -> bool {
        match self {
            Type::Money => true,
            Type::Generic(_, args) => args.iter().any(Type::mentions_money),
            _ => false,
        }
    }

    /// Returns `true` if this type is — or recursively contains —
    /// [`Type::JsValue`]. Recurses through `Generic` arguments and `Function`
    /// parameter/return types so `List<JsValue>` / `Option<JsValue>` /
    /// `(JsValue) -> Void` are caught; does NOT recurse into [`Type::Named`]
    /// (a struct that *has* a `JsValue` field is caught by scanning that
    /// struct's own fields).
    ///
    /// `JsValue` is an executable-language host-FFI handle (Phase 2.5) with no
    /// wire representation, so it has no place in a Phoenix Gen schema. Used by
    /// codegen's `schema_mentions_jsvalue` to reject such a schema uniformly
    /// across all targets (instead of each backend's type-mapper guessing a
    /// different fallback).
    #[must_use]
    pub fn mentions_jsvalue(&self) -> bool {
        match self {
            Type::JsValue => true,
            Type::Generic(_, args) => args.iter().any(Type::mentions_jsvalue),
            Type::Function(params, ret) => {
                params.iter().any(Type::mentions_jsvalue) || ret.mentions_jsvalue()
            }
            _ => false,
        }
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

/// Shorthand for constructing `ListBuilder<T>`. Phase-2.7 decision F:
/// transient mutable accumulator that `.freeze()`s to an immutable
/// `List<T>` in O(1). The element type matches what `freeze()` produces.
pub fn list_builder_of(element_type: Type) -> Type {
    Type::Generic("ListBuilder".to_string(), vec![element_type])
}

/// Shorthand for constructing `MapBuilder<K, V>`. Same role as
/// [`list_builder_of`] for maps.
pub fn map_builder_of(key_type: Type, value_type: Type) -> Type {
    Type::Generic("MapBuilder".to_string(), vec![key_type, value_type])
}

/// Builtin type names that expose static-method constructors (`.builder()`
/// today; future static methods on builtins land here too). Centralized
/// so the sema-side dispatch in `check_builtin_static_method`
/// (`crates/phoenix-sema/src/check_expr_call.rs`) and the IR-lowering
/// carve-out in `lower_method_call`
/// (`crates/phoenix-ir/src/lower_expr.rs`) consult one source of truth.
/// If the two sides ever diverge on which receiver names get the
/// static-method intercept, a program could type-check against the
/// builtin and then crash during IR lowering with "unknown identifier
/// `<Name>`".
pub const BUILTIN_STATIC_METHOD_TYPES: &[&str] = &["List", "Map"];

/// `true` when `name` is a builtin type that hosts static-method
/// constructors (currently `List.builder()` / `Map.builder()`). See
/// [`BUILTIN_STATIC_METHOD_TYPES`].
pub fn is_builtin_static_method_type(name: &str) -> bool {
    BUILTIN_STATIC_METHOD_TYPES.contains(&name)
}

/// Formats the type for user-facing messages.
///
/// Built-in types are displayed by their canonical name (e.g. `"Int"`,
/// `"Float"`).  [`Type::Error`] is rendered as `"<error>"`.
///
/// User-defined type names (`Named` / `Generic` / `Dyn`) carry the
/// canonical *qualified* key internally (`lib::User`) so symbol-table
/// lookups across modules resolve unambiguously, but Display strips
/// the module prefix so diagnostics and `print` output show the
/// user-source name (`User`) the source actually wrote.
impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Int => write!(f, "Int"),
            Type::Float => write!(f, "Float"),
            Type::String => write!(f, "String"),
            Type::Bool => write!(f, "Bool"),
            Type::File => write!(f, "File"),
            Type::DateTime => write!(f, "DateTime"),
            Type::Uuid => write!(f, "Uuid"),
            Type::Decimal => write!(f, "Decimal"),
            Type::Money => write!(f, "Money"),
            Type::JsValue => write!(f, "JsValue"),
            Type::Void => write!(f, "Void"),
            Type::Named(name) => write!(f, "{}", bare_name(name)),
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
                write!(f, "{}<", bare_name(name))?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", a)?;
                }
                write!(f, ">")
            }
            Type::Dyn(name) => write!(f, "dyn {}", bare_name(name)),
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

    #[test]
    fn mentions_jsvalue_recurses_through_generics_and_functions() {
        assert!(Type::JsValue.mentions_jsvalue());
        assert!(list_of(Type::JsValue).mentions_jsvalue());
        assert!(map_of(Type::String, Type::JsValue).mentions_jsvalue());
        assert!(Type::Function(vec![Type::JsValue], Box::new(Type::Void)).mentions_jsvalue());
        assert!(Type::Function(vec![Type::Int], Box::new(Type::JsValue)).mentions_jsvalue());
        // Does not recurse into Named (a struct that *has* a JsValue field is
        // caught by scanning that struct's own fields, not by this predicate).
        assert!(!Type::Named("Element".to_string()).mentions_jsvalue());
        assert!(!list_of(Type::Int).mentions_jsvalue());
    }

    #[test]
    fn is_js_marshallable_accepts_scalars_jsvalue_and_marshallable_closures() {
        assert!(Type::Int.is_js_marshallable());
        assert!(Type::String.is_js_marshallable());
        assert!(Type::JsValue.is_js_marshallable());
        assert!(Type::Void.is_js_marshallable());
        assert!(
            Type::Function(vec![Type::Int, Type::String], Box::new(Type::Bool))
                .is_js_marshallable()
        );
        // Aggregates / Gen scalars / closures-of-aggregates are not marshallable.
        assert!(!list_of(Type::Int).is_js_marshallable());
        assert!(!Type::Money.is_js_marshallable());
        assert!(!Type::Named("Foo".to_string()).is_js_marshallable());
        assert!(
            !Type::Function(vec![list_of(Type::Int)], Box::new(Type::Void)).is_js_marshallable()
        );
    }
}
