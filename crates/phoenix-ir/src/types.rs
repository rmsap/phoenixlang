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
/// The name of the built-in `JsonError` enum in the IR.
/// Non-generic, with three `String`-carrying variants.
pub const JSON_ERROR_ENUM: &str = "JsonError";

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
    /// Heap-allocated struct instance.  The [`String`] is the struct name
    /// used for layout lookup; the [`Vec<IrType>`] carries the concrete
    /// generic arguments at the use site (e.g. `StructRef("Container",
    /// [I64])` for `Container<Int>`).  Empty for non-generic structs.
    ///
    /// Pre-monomorphization, args may contain `TypeVar`s (a template
    /// method's `self` parameter on a generic struct).
    /// Post-monomorphization, the struct-specialization pass rewrites
    /// every `StructRef(name, non_empty_args)` into
    /// `StructRef(mangled_name, Vec::new())` and registers a specialized
    /// layout under `mangled_name` in
    /// [`crate::module::IrModule::struct_layouts`]. Every concrete
    /// function post-mono sees `StructRef` with empty args, and the
    /// Cranelift backend looks up layouts by bare string as before.
    StructRef(String, Vec<IrType>),
    /// Heap-allocated enum value (tagged union).  The [`String`] is the enum
    /// name, used for variant layout lookup.  The [`Vec<IrType>`] carries the
    /// concrete generic arguments at the use site (e.g. `EnumRef("Option",
    /// [I64])` for `Option<Int>`).  Empty for non-generic enums.
    ///
    /// Enum *types* are keyed by name + args, but enum *layouts* in
    /// [`crate::module::IrModule::enum_layouts`] are keyed by name alone —
    /// the args exist so payload-type inference in the Cranelift backend
    /// can read them directly instead of relying on fallback strategies.
    /// See the "Enum layouts are keyed by name" subsection of
    /// `docs/design-decisions.md#generic-function-monomorphization-strategy`
    /// for the rationale and the condition under which layouts would need
    /// to start keying on args too (inline-packed payload specialization).
    ///
    /// # Args-vector invariants
    ///
    /// - For stdlib `Option`/`Result`, `args` is parallel to the payload
    ///   slots: `Option<T>` has `args = [T]` (Some's payload); `Result<T,
    ///   E>` has `args = [T, E]` (Ok and Err payloads). The slot index
    ///   equals the variant index of the variant that carries that slot's
    ///   payload. Backend inference helpers rely on this.
    /// - For user-defined generic enums, `args` matches the enum's
    ///   declared type-parameter order.
    /// - An arg slot MAY be the [`GENERIC_PLACEHOLDER`] sentinel when
    ///   lowering couldn't resolve the concrete type (e.g. an expression
    ///   whose sema type still contains a `TypeVar` after
    ///   [`IrType::erase_type_vars`] is applied). Consumers that want the
    ///   concrete type MUST call [`IrType::is_generic_placeholder`] on
    ///   the slot before trusting it; a placeholder is a "don't know
    ///   yet," not a valid type.
    /// - `args` MAY also be empty when lowering has no type information
    ///   at all (e.g. the `self` parameter on a user-defined method of a
    ///   non-generic enum). Consumers must fall back to other inference
    ///   strategies when `args.get(i)` is `None`.
    EnumRef(String, Vec<IrType>),
    /// A trait object `dyn TraitName`: a `(data_ptr, vtable_ptr)` pair,
    /// represented inline as two slots (parallel to [`IrType::StringRef`]'s
    /// two-slot `(ptr, len)` layout).  The string is the trait name; the
    /// vtable layout for a given `(concrete_type, trait)` pair lives in
    /// [`crate::module::IrModule::dyn_vtables`].  See `docs/design-decisions.md`
    /// for the ABI rationale and the "why explicit `dyn`" discussion.
    DynRef(String),
    /// Heap-allocated `List<T>`.
    ListRef(Box<IrType>),
    /// Heap-allocated `Map<K, V>`.
    MapRef(Box<IrType>, Box<IrType>),
    /// Heap-allocated `ListBuilder<T>` — transient mutable accumulator
    /// (Phase 2.7 decision F). Same single-pointer ABI as `ListRef`
    /// (the runtime handle is a single pointer; the buffer lives in
    /// a separate allocation reachable only through the handle).
    ListBuilderRef(Box<IrType>),
    /// Heap-allocated `MapBuilder<K, V>` — same role as
    /// `ListBuilderRef` for maps.
    MapBuilderRef(Box<IrType>, Box<IrType>),
    /// Heap-allocated closure object (code pointer + captured environment).
    ClosureRef {
        /// Parameter types of the closure.
        param_types: Vec<IrType>,
        /// Return type of the closure.
        return_type: Box<IrType>,
    },

    /// An opaque JavaScript-host value handle.
    ///
    /// The IR mirror of `phoenix_sema::types::Type::JsValue`. It is **not** a
    /// Phoenix-heap pointer: at runtime it is a single scalar handle the host
    /// owns (an `i32` index into a JS-side table on `wasm32-linear`, an
    /// `externref` on `wasm32-gc`, an opaque host handle in the interpreters /
    /// native). Phoenix never dereferences it — it only holds one and passes it
    /// back to an `extern js` function. Because it is not a Phoenix-managed
    /// reference, [`is_value_type`](Self::is_value_type) classifies it as a
    /// value type (register-resident, not shadow-stack-rooted). The only way to
    /// obtain one is an [`crate::instruction::Op::ExternCall`] return value; the
    /// per-backend binding of that op lands incrementally (the interpreters in
    /// Phase 2.5 PR 4, native in PR 9, the WASM backends in PRs 5–8 / 12–15),
    /// so until then each backend rejects `ExternCall` with a clean error.
    JsValue,

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
            // `JsValue` is an opaque host handle (a single scalar — `i32`/
            // `externref`/host pointer), NOT a Phoenix-heap pointer, so it is a
            // value type for classification purposes: register-resident and not
            // shadow-stack-rooted (Phase 2.5). See the `IrType::JsValue` doc.
            IrType::I64 | IrType::F64 | IrType::Bool | IrType::Void | IrType::JsValue => true,
            IrType::StringRef
            | IrType::StructRef(_, _)
            | IrType::EnumRef(_, _)
            | IrType::DynRef(_)
            | IrType::ListRef(_)
            | IrType::MapRef(_, _)
            | IrType::ListBuilderRef(_)
            | IrType::MapBuilderRef(_, _)
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

    /// `true` iff this type is `StringRef` — the canonical predicate
    /// behind the `key_is_string` / `is_string` flag threaded into the
    /// runtime's map and list `contains` ABIs. Call sites cast to the
    /// integer width they need (`as i64` / `as i32` / `as i8`); the
    /// single predicate keeps the cranelift and wasm backends provably
    /// in agreement on what counts as a string element.
    pub fn string_flag(&self) -> bool {
        matches!(self, IrType::StringRef)
    }

    /// `true` iff this type is `F64` — the canonical predicate behind
    /// the `is_float` flag passed to `phx_list_contains` so float
    /// elements compare under IEEE 754 (not raw bytes). Companion to
    /// [`Self::string_flag`].
    pub fn float_flag(&self) -> bool {
        matches!(self, IrType::F64)
    }

    /// Returns `true` if this type is the [`GENERIC_PLACEHOLDER`] sentinel,
    /// representing an unresolved generic type parameter.
    pub fn is_generic_placeholder(&self) -> bool {
        matches!(self, IrType::StructRef(n, args) if n == GENERIC_PLACEHOLDER && args.is_empty())
    }

    /// Returns `true` if this type has the [`GENERIC_PLACEHOLDER`] as an
    /// **enum type argument**, at any nesting reached through containers
    /// (`Result<Int, __generic>`, `Option<__generic>`,
    /// `List<Option<__generic>>`).
    ///
    /// This is the precise signal of the imprecision that miscompiles on
    /// wasm32-gc: an enum is a *nominal* type there, so a `__generic` in
    /// its argument slot can't be pinned to a unique instantiation when
    /// siblings of the same template exist. It arises from
    /// **phantom-parameter constructors** (`Ok(99)` doesn't constrain the
    /// error type, `None` constrains neither) — exactly what sema's
    /// expected-type pinning (`pin_inferred_type_to_annotation`) resolves.
    /// Used by the IR verifier (§Phase 2.4 K.12).
    ///
    /// Deliberately *not* flagged: a placeholder that is a bare type
    /// (`__generic`), a list/map *element* (`List<__generic>`), a struct
    /// *argument*, or a closure parameter/return — these come from inert
    /// sources (a dead generic closure's erased capture, an unconstrained
    /// empty literal no nominal codegen consumes) and run identically on
    /// every backend, so rejecting them would be a regression. The walk
    /// still *recurses through* those containers to catch an enum nested
    /// inside them.
    ///
    /// The match is **asymmetric** about nesting direction: an enum *inside*
    /// a container (`List<Option<__generic>>`) is caught — the walk recurses
    /// through the container into the enum arg — but a container-with-an-
    /// inert-element *inside* an enum (`Option<List<__generic>>`) is **not**:
    /// the enum arg is the `List`, which is not itself the placeholder, and
    /// the placeholder it holds is an inert list *element*. That is the
    /// intended scope (the inner `List<__generic>` is always an empty,
    /// run-identically-everywhere literal); it is not full coverage of every
    /// nested placeholder. This shape is not known to arise in practice (it
    /// would need a `Some(<unconstrained empty list>)`).
    ///
    /// The non-nesting arm (the scalars `I64`/`F64`/`Bool`/`Void`,
    /// `StringRef`, `DynRef`, and `TypeVar`) deliberately returns `false`
    /// without recursing: none of those nest another `IrType`. It is written
    /// as an *explicit* enumeration rather than a `_` wildcard precisely so
    /// the match stays exhaustive — every variant that *does* nest a type
    /// (Struct/Enum/List/Map/ListBuilder/MapBuilder/Closure) is matched
    /// explicitly above, so a future `IrType` with a new nesting variant
    /// (e.g. a tuple or array type) is a compile error here rather than a
    /// silent false-negative. Do **not** collapse it to `_ => false`.
    pub fn contains_placeholder_in_enum_arg(&self) -> bool {
        match self {
            IrType::EnumRef(_, args) => args
                .iter()
                .any(|a| a.is_generic_placeholder() || a.contains_placeholder_in_enum_arg()),
            IrType::StructRef(_, args) => args.iter().any(Self::contains_placeholder_in_enum_arg),
            IrType::ListRef(e) | IrType::ListBuilderRef(e) => e.contains_placeholder_in_enum_arg(),
            IrType::MapRef(k, v) | IrType::MapBuilderRef(k, v) => {
                k.contains_placeholder_in_enum_arg() || v.contains_placeholder_in_enum_arg()
            }
            IrType::ClosureRef {
                param_types,
                return_type,
            } => {
                param_types
                    .iter()
                    .any(Self::contains_placeholder_in_enum_arg)
                    || return_type.contains_placeholder_in_enum_arg()
            }
            IrType::I64
            | IrType::F64
            | IrType::Bool
            | IrType::Void
            | IrType::StringRef
            | IrType::DynRef(_)
            | IrType::JsValue
            | IrType::TypeVar(_) => false,
        }
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
            IrType::TypeVar(_) => IrType::StructRef(GENERIC_PLACEHOLDER.to_string(), Vec::new()),
            IrType::ListRef(inner) => IrType::ListRef(Box::new(inner.erase_type_vars())),
            IrType::MapRef(k, v) => {
                IrType::MapRef(Box::new(k.erase_type_vars()), Box::new(v.erase_type_vars()))
            }
            IrType::ListBuilderRef(inner) => {
                IrType::ListBuilderRef(Box::new(inner.erase_type_vars()))
            }
            IrType::MapBuilderRef(k, v) => {
                IrType::MapBuilderRef(Box::new(k.erase_type_vars()), Box::new(v.erase_type_vars()))
            }
            IrType::ClosureRef {
                param_types,
                return_type,
            } => IrType::ClosureRef {
                param_types: param_types.iter().map(IrType::erase_type_vars).collect(),
                return_type: Box::new(return_type.erase_type_vars()),
            },
            IrType::EnumRef(name, args) => IrType::EnumRef(
                name.clone(),
                args.iter().map(IrType::erase_type_vars).collect(),
            ),
            IrType::StructRef(name, args) => IrType::StructRef(
                name.clone(),
                args.iter().map(IrType::erase_type_vars).collect(),
            ),
            IrType::I64
            | IrType::F64
            | IrType::Bool
            | IrType::Void
            | IrType::StringRef
            | IrType::DynRef(_)
            | IrType::JsValue => self.clone(),
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
            IrType::StructRef(name, args) => {
                write!(f, "struct.{name}")?;
                if !args.is_empty() {
                    write!(f, "<")?;
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{a}")?;
                    }
                    write!(f, ">")?;
                }
                Ok(())
            }
            IrType::EnumRef(name, args) => {
                write!(f, "enum.{name}")?;
                if !args.is_empty() {
                    write!(f, "<")?;
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{a}")?;
                    }
                    write!(f, ">")?;
                }
                Ok(())
            }
            IrType::ListRef(elem) => write!(f, "list<{elem}>"),
            IrType::MapRef(k, v) => write!(f, "map<{k}, {v}>"),
            IrType::ListBuilderRef(elem) => write!(f, "list_builder<{elem}>"),
            IrType::MapBuilderRef(k, v) => write!(f, "map_builder<{k}, {v}>"),
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
            IrType::DynRef(name) => write!(f, "dyn.{name}"),
            IrType::JsValue => write!(f, "jsvalue"),
            IrType::TypeVar(name) => write!(f, "{name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_ref_display_with_no_args() {
        assert_eq!(
            format!("{}", IrType::EnumRef("Point".into(), Vec::new())),
            "enum.Point"
        );
    }

    #[test]
    fn enum_ref_display_with_single_arg() {
        assert_eq!(
            format!("{}", IrType::EnumRef("Option".into(), vec![IrType::I64])),
            "enum.Option<i64>"
        );
    }

    #[test]
    fn enum_ref_display_with_multiple_args() {
        assert_eq!(
            format!(
                "{}",
                IrType::EnumRef("Result".into(), vec![IrType::StringRef, IrType::I64])
            ),
            "enum.Result<string, i64>"
        );
    }

    #[test]
    fn js_value_classifies_as_value_type() {
        // `JsValue` is an opaque host handle, not a Phoenix-heap pointer, so it
        // must classify as a value type (register-resident, never shadow-stack-
        // rooted). The GC-rooting contract the backend layouts rely on hinges on
        // this; pin it so a future reclassification can't silently start rooting
        // host handles. See the `IrType::JsValue` doc + its `type_layout` arm.
        assert!(IrType::JsValue.is_value_type());
        assert!(!IrType::JsValue.is_ref_type());
    }

    #[test]
    fn js_value_displays_as_jsvalue() {
        assert_eq!(format!("{}", IrType::JsValue), "jsvalue");
    }

    #[test]
    fn erase_type_vars_recurses_into_enum_args() {
        let ty = IrType::EnumRef("Option".into(), vec![IrType::TypeVar("T".into())]);
        let erased = ty.erase_type_vars();
        assert_eq!(
            erased,
            IrType::EnumRef(
                "Option".into(),
                vec![IrType::StructRef(
                    GENERIC_PLACEHOLDER.to_string(),
                    Vec::new()
                )]
            )
        );
    }

    #[test]
    fn erase_type_vars_recurses_into_nested_enum_args() {
        let ty = IrType::EnumRef(
            "Result".into(),
            vec![
                IrType::ListRef(Box::new(IrType::TypeVar("T".into()))),
                IrType::TypeVar("E".into()),
            ],
        );
        let erased = ty.erase_type_vars();
        let expected_placeholder = IrType::StructRef(GENERIC_PLACEHOLDER.to_string(), Vec::new());
        assert_eq!(
            erased,
            IrType::EnumRef(
                "Result".into(),
                vec![
                    IrType::ListRef(Box::new(expected_placeholder.clone())),
                    expected_placeholder,
                ]
            )
        );
    }

    /// The [`GENERIC_PLACEHOLDER`] sentinel as a value type.
    fn placeholder() -> IrType {
        IrType::StructRef(GENERIC_PLACEHOLDER.to_string(), Vec::new())
    }

    #[test]
    fn placeholder_in_enum_arg_flags_direct_and_nested() {
        // `Option<__generic>` — a direct placeholder in an enum arg.
        assert!(
            IrType::EnumRef("Option".into(), vec![placeholder()])
                .contains_placeholder_in_enum_arg()
        );
        // `Result<Int, __generic>` — phantom error slot.
        assert!(
            IrType::EnumRef("Result".into(), vec![IrType::I64, placeholder()])
                .contains_placeholder_in_enum_arg()
        );
        // `List<Option<__generic>>` — reached by recursing through a
        // container into a nested enum arg.
        assert!(
            IrType::ListRef(Box::new(IrType::EnumRef(
                "Option".into(),
                vec![placeholder()]
            )))
            .contains_placeholder_in_enum_arg()
        );
        // `Map<Int, Option<__generic>>` — nested in the value position.
        assert!(
            IrType::MapRef(
                Box::new(IrType::I64),
                Box::new(IrType::EnumRef("Option".into(), vec![placeholder()])),
            )
            .contains_placeholder_in_enum_arg()
        );
        // `Box<Result<Int, __generic>>` — nested through a struct arg.
        assert!(
            IrType::StructRef(
                "Box".into(),
                vec![IrType::EnumRef(
                    "Result".into(),
                    vec![IrType::I64, placeholder()]
                )],
            )
            .contains_placeholder_in_enum_arg()
        );
    }

    #[test]
    fn placeholder_outside_enum_arg_is_inert() {
        // A *bare* placeholder is inert.
        assert!(!placeholder().contains_placeholder_in_enum_arg());
        // `List<__generic>` — a placeholder as a list *element*, not an
        // enum arg (an unconstrained empty literal); must stay unflagged.
        assert!(!IrType::ListRef(Box::new(placeholder())).contains_placeholder_in_enum_arg());
        // `Map<__generic, __generic>` — placeholders as map key/value.
        assert!(
            !IrType::MapRef(Box::new(placeholder()), Box::new(placeholder()))
                .contains_placeholder_in_enum_arg()
        );
        // `Box<__generic>` — a placeholder as a *struct* arg (a dead
        // generic closure's erased capture); inert.
        assert!(
            !IrType::StructRef("Box".into(), vec![placeholder()])
                .contains_placeholder_in_enum_arg()
        );
        // A placeholder in a closure param / return — inert.
        assert!(
            !IrType::ClosureRef {
                param_types: vec![placeholder()],
                return_type: Box::new(placeholder()),
            }
            .contains_placeholder_in_enum_arg()
        );
        // A fully-concrete enum carries no placeholder.
        assert!(
            !IrType::EnumRef("Result".into(), vec![IrType::I64, IrType::StringRef])
                .contains_placeholder_in_enum_arg()
        );
    }
}
