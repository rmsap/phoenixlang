//! Runtime value representation for the IR interpreter.

use phoenix_ir::instruction::FuncId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::{OPTION_ENUM, RESULT_ENUM};
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

/// Data for a struct instance.
#[derive(Debug, Clone)]
pub struct StructData {
    /// The struct type name.
    pub name: String,
    /// Field values in declaration order.
    pub fields: Vec<IrValue>,
}

/// Backing state for an [`IrValue::ListBuilder`]: the accumulated items
/// and a one-shot `frozen` flag. `.freeze()` sets `frozen`; any later
/// `.push()`/`.freeze()` is a runtime error, mirroring native's
/// `assert_unfrozen` and wasm-gc's frozen trap so use-after-freeze
/// behaves identically across all five backends.
#[derive(Debug, Clone)]
pub struct ListBuilderState {
    /// Elements pushed so far, in push order.
    pub items: Vec<IrValue>,
    /// Set by `.freeze()`; once true the builder rejects further use.
    pub frozen: bool,
}

/// Backing state for an [`IrValue::MapBuilder`]: appended `(key, value)`
/// pairs (no dedup until freeze) and the same one-shot `frozen` flag as
/// [`ListBuilderState`].
#[derive(Debug, Clone)]
pub struct MapBuilderState {
    /// Pairs appended so far, verbatim and undeduped.
    pub pairs: Vec<(IrValue, IrValue)>,
    /// Set by `.freeze()`; once true the builder rejects further use.
    pub frozen: bool,
}

/// Data for an enum variant instance.
#[derive(Debug, Clone)]
pub struct EnumData {
    /// The enum type name (e.g. `"Option"`, `"Result"`).
    pub enum_name: String,
    /// The variant index (discriminant).
    pub discriminant: u32,
    /// Field values for this variant.
    pub fields: Vec<IrValue>,
}

/// A runtime value in the IR interpreter.
#[derive(Debug, Clone)]
pub enum IrValue {
    /// A 64-bit signed integer.
    Int(i64),
    /// A 64-bit floating-point number.
    Float(f64),
    /// A heap-allocated string.
    String(String),
    /// A boolean.
    Bool(bool),
    /// The unit value.
    Void,
    /// A struct instance (shared reference for mutation via `StructSetField`).
    Struct(Rc<RefCell<StructData>>),
    /// An enum variant (shared reference).
    EnumVariant(Rc<RefCell<EnumData>>),
    /// A list of values (shared reference).
    List(Rc<RefCell<Vec<IrValue>>>),
    /// A map of key-value pairs (shared reference).
    Map(Rc<RefCell<Vec<(IrValue, IrValue)>>>),
    /// A transient mutable `ListBuilder<T>` accumulator (Phase 2.7).
    ///
    /// Distinct from [`IrValue::List`] at the type level: `.push()`
    /// mutates the shared buffer in place and `.freeze()` produces a
    /// fresh independent [`IrValue::List`] by cloning the buffer, then
    /// marks the builder frozen. The native backend uses a dedicated
    /// handle/buffer pair (`list_builder_methods.rs`); the interpreter
    /// mirrors the observable semantics, including use-after-freeze
    /// rejection (see [`ListBuilderState`]).
    ListBuilder(Rc<RefCell<ListBuilderState>>),
    /// A transient mutable `MapBuilder<K, V>` accumulator (Phase 2.7).
    ///
    /// `.set()` appends a `(key, value)` pair verbatim (no dedup, no
    /// result); `.freeze()` produces a fresh [`IrValue::Map`] applying
    /// last-wins dedup with first-insertion key position, matching
    /// `phx_map_builder_freeze` → `phx_map_from_pairs`, and marks the
    /// builder frozen (use-after-freeze is a runtime error).
    MapBuilder(Rc<RefCell<MapBuilderState>>),
    /// A closure: target function ID and captured values.
    Closure(FuncId, Vec<IrValue>),
    /// A `dyn Trait` fat-pointer value, parallel to the Cranelift
    /// `(data_ptr, vtable_ptr)` ABI. `Op::DynCall` dispatches by looking
    /// up `(concrete_type, trait_name)` in [`IrModule::dyn_vtables`] and
    /// indexing into the slot array — the same path the compiled backend
    /// takes, so vtable-registration bugs surface in both.
    Dyn {
        /// The concrete receiver value.
        concrete: Box<IrValue>,
        /// The concrete type's name (e.g. `"Circle"`).
        concrete_type: String,
        /// The trait name the value was object-ified under.
        trait_name: String,
    },
    /// An opaque JavaScript-host value handle (`JsValue`).
    ///
    /// The IR-interpreter mirror of `IrType::JsValue`: a handle into a
    /// host-owned object space the interpreter never inspects, produced and
    /// consumed only at the `extern js` boundary
    /// ([`crate::instruction::Op::ExternCall`]). Mirrors
    /// `phoenix_common::host::HostValue::JsValue`.
    JsValue(u64),
}

impl IrValue {
    /// Construct a new `IrValue::List` from a `Vec<IrValue>`.
    pub fn new_list(elems: Vec<IrValue>) -> Self {
        IrValue::List(Rc::new(RefCell::new(elems)))
    }

    /// Construct a new `IrValue::Map` from key-value pairs.
    pub fn new_map(entries: Vec<(IrValue, IrValue)>) -> Self {
        IrValue::Map(Rc::new(RefCell::new(entries)))
    }

    /// Construct a fresh empty `IrValue::ListBuilder`.
    pub fn new_list_builder() -> Self {
        IrValue::ListBuilder(Rc::new(RefCell::new(ListBuilderState {
            items: Vec::new(),
            frozen: false,
        })))
    }

    /// Construct a fresh empty `IrValue::MapBuilder`.
    pub fn new_map_builder() -> Self {
        IrValue::MapBuilder(Rc::new(RefCell::new(MapBuilderState {
            pairs: Vec::new(),
            frozen: false,
        })))
    }

    /// Format this value for user-facing output, matching the AST interpreter's
    /// `Display for Value` exactly.
    pub fn format(&self, module: &IrModule) -> String {
        match self {
            IrValue::Int(n) => format!("{n}"),
            // Float prints via ryu's shortest-roundtrip d2s in scientific
            // form, matching `phoenix_runtime::format_f64` and the
            // tree-walk interpreter byte-for-byte. See
            // `docs/design-decisions.md` §Phase 2.4 K.6 (2026-06-09).
            IrValue::Float(n) => ryu::Buffer::new().format(*n).to_string(),
            IrValue::String(s) => s.clone(),
            IrValue::Bool(b) => format!("{b}"),
            IrValue::Void => "void".to_string(),
            IrValue::Struct(data) => {
                let data = data.borrow();
                // Regular structs: Name(field1: val1, field2: val2).
                let field_strs: Vec<String> =
                    if let Some(layout) = module.struct_layouts.get(&data.name) {
                        data.fields
                            .iter()
                            .enumerate()
                            .map(|(i, v)| {
                                let name = layout.get(i).map(|(n, _)| n.as_str()).unwrap_or("?");
                                format!("{}: {}", name, v.format(module))
                            })
                            .collect()
                    } else {
                        data.fields
                            .iter()
                            .enumerate()
                            .map(|(i, v)| format!("field{}: {}", i, v.format(module)))
                            .collect()
                    };
                format!("{}({})", data.name, field_strs.join(", "))
            }
            IrValue::EnumVariant(data) => {
                let data = data.borrow();
                let variant_name = module
                    .enum_layouts
                    .get(&data.enum_name)
                    .and_then(|variants| variants.get(data.discriminant as usize))
                    .map(|(name, _)| name.as_str())
                    .unwrap_or("?");
                if data.fields.is_empty() {
                    variant_name.to_string()
                } else {
                    let field_strs: Vec<String> =
                        data.fields.iter().map(|v| v.format(module)).collect();
                    format!("{}({})", variant_name, field_strs.join(", "))
                }
            }
            IrValue::List(elems) => {
                let elems = elems.borrow();
                let strs: Vec<String> = elems.iter().map(|v| v.format(module)).collect();
                format!("[{}]", strs.join(", "))
            }
            IrValue::Map(entries) => {
                let entries = entries.borrow();
                let strs: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k.format(module), v.format(module)))
                    .collect();
                format!("{{{}}}", strs.join(", "))
            }
            IrValue::Closure(_, _) => "<function>".to_string(),
            IrValue::Dyn { concrete, .. } => concrete.format(module),
            // Builders are opaque transient values; they are never
            // user-printed (the type system only exposes `.push`/`.set`
            // and `.freeze`). Render an opaque tag for diagnostics.
            IrValue::ListBuilder(_) => "<ListBuilder>".to_string(),
            IrValue::MapBuilder(_) => "<MapBuilder>".to_string(),
            IrValue::JsValue(_) => "<JsValue>".to_string(),
        }
    }

    /// Returns a fixed, module-free kind label for this value, for diagnostics
    /// that can't borrow the module (e.g. the `extern js` marshalling error).
    /// Aggregates report their kind, not their declared name — use
    /// [`Self::format`] when the source-level name is wanted.
    pub fn type_name(&self) -> &'static str {
        match self {
            IrValue::Int(_) => "Int",
            IrValue::Float(_) => "Float",
            IrValue::String(_) => "String",
            IrValue::Bool(_) => "Bool",
            IrValue::Void => "Void",
            IrValue::Struct(_) => "Struct",
            IrValue::EnumVariant(_) => "EnumVariant",
            IrValue::List(_) => "List",
            IrValue::Map(_) => "Map",
            IrValue::Closure(_, _) => "<function>",
            IrValue::Dyn { .. } => "Dyn",
            IrValue::ListBuilder(_) => "ListBuilder",
            IrValue::MapBuilder(_) => "MapBuilder",
            IrValue::JsValue(_) => "JsValue",
        }
    }

    /// Returns the variant name for an enum, looking up from the module.
    pub fn variant_name<'a>(&self, module: &'a IrModule) -> Option<&'a str> {
        if let IrValue::EnumVariant(data) = self {
            let data = data.borrow();
            module
                .enum_layouts
                .get(&data.enum_name)
                .and_then(|variants| variants.get(data.discriminant as usize))
                .map(|(name, _)| name.as_str())
        } else {
            None
        }
    }
}

impl PartialEq for IrValue {
    /// Compare two values for equality.
    ///
    /// Float comparison uses IEEE 754 semantics: `NaN != NaN`. This means
    /// `list.contains(NaN)` will never find a NaN element. This is by design
    /// and matches the compiled code path.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (IrValue::Int(a), IrValue::Int(b)) => a == b,
            (IrValue::Float(a), IrValue::Float(b)) => a == b,
            (IrValue::String(a), IrValue::String(b)) => a == b,
            (IrValue::Bool(a), IrValue::Bool(b)) => a == b,
            (IrValue::Void, IrValue::Void) => true,
            (IrValue::Struct(a), IrValue::Struct(b)) => {
                let a = a.borrow();
                let b = b.borrow();
                a.name == b.name && a.fields == b.fields
            }
            (IrValue::EnumVariant(a), IrValue::EnumVariant(b)) => {
                let a = a.borrow();
                let b = b.borrow();
                a.enum_name == b.enum_name
                    && a.discriminant == b.discriminant
                    && a.fields == b.fields
            }
            (IrValue::List(a), IrValue::List(b)) => *a.borrow() == *b.borrow(),
            (IrValue::Map(a), IrValue::Map(b)) => *a.borrow() == *b.borrow(),
            // Builders are opaque transient values with no value
            // equality; like closures they never compare equal.
            (IrValue::ListBuilder(_), IrValue::ListBuilder(_)) => false,
            (IrValue::MapBuilder(_), IrValue::MapBuilder(_)) => false,
            (IrValue::Closure(_, _), IrValue::Closure(_, _)) => false,
            // `JsValue` is an opaque host handle whose identity *is* its handle
            // id. Sema permits `==`/`!=` on it, so compare by handle: two
            // handles are equal iff they name the same host object.
            (IrValue::JsValue(a), IrValue::JsValue(b)) => a == b,
            // Trait-object equality: two `dyn` values are equal iff they
            // carry the same trait *and* the same concrete type *and*
            // their underlying values are equal. The trait-name check
            // short-circuits first (cheapest), so `dyn Foo` is never
            // equal to `dyn Bar` even if both wrap the same concrete —
            // their vtables differ and the values are not interchangeable.
            (
                IrValue::Dyn {
                    concrete: a,
                    concrete_type: at,
                    trait_name: atn,
                },
                IrValue::Dyn {
                    concrete: b,
                    concrete_type: bt,
                    trait_name: btn,
                },
            ) => atn == btn && at == bt && a == b,
            _ => false,
        }
    }
}

/// Key equality for `Map` operations (`get` / `contains` / `set` /
/// `remove` and literal dedup).
///
/// Map keys compare **byte-wise** for floats: `-0.0 != 0.0`, and two
/// `NaN`s are equal iff their bits match. This matches native's
/// `Map<Float,V>` key comparison and the wasm32-gc lowering
/// (`i64.reinterpret_f64` + `i64.eq`) — see §Phase 2.4 K.9 — and is
/// deliberately *not* [`IrValue`]'s IEEE `==` (which treats `±0.0` as
/// equal and `NaN` as never-equal; that IEEE rule is correct for
/// `List.contains` but wrong for map keys). Routing every map-key
/// comparison through here keeps `Map` semantics byte-identical across
/// all five backends. Non-float keys fall through to `==`.
pub(crate) fn map_key_eq(a: &IrValue, b: &IrValue) -> bool {
    match (a, b) {
        (IrValue::Float(x), IrValue::Float(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

impl PartialOrd for IrValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (IrValue::Int(a), IrValue::Int(b)) => a.partial_cmp(b),
            (IrValue::Float(a), IrValue::Float(b)) => a.partial_cmp(b),
            (IrValue::String(a), IrValue::String(b)) => a.partial_cmp(b),
            _ => None,
        }
    }
}

impl fmt::Display for IrValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IrValue::Int(n) => write!(f, "{n}"),
            // Float prints via ryu's shortest-roundtrip d2s (K.6 2026-06-09).
            IrValue::Float(n) => write!(f, "{}", ryu::Buffer::new().format(*n)),
            IrValue::String(s) => write!(f, "{s}"),
            IrValue::Bool(b) => write!(f, "{b}"),
            IrValue::Void => write!(f, "void"),
            IrValue::Struct(data) => {
                let data = data.borrow();
                write!(f, "{}(...)", data.name)
            }
            IrValue::EnumVariant(data) => {
                let data = data.borrow();
                write!(f, "variant_{}", data.discriminant)
            }
            IrValue::List(elems) => {
                let elems = elems.borrow();
                write!(f, "[")?;
                for (i, v) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            IrValue::Map(entries) => {
                let entries = entries.borrow();
                write!(f, "{{")?;
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "}}")
            }
            IrValue::Closure(_, _) => write!(f, "<function>"),
            IrValue::Dyn { concrete, .. } => write!(f, "{concrete}"),
            IrValue::ListBuilder(_) => write!(f, "<ListBuilder>"),
            IrValue::MapBuilder(_) => write!(f, "<MapBuilder>"),
            IrValue::JsValue(_) => write!(f, "<JsValue>"),
        }
    }
}

/// Construct an `Option::Some` IR value.
///
/// The IR lowering represents `Some(x)` as `EnumAlloc(@Option, discriminant 0, [x])`.
pub fn some_val(val: IrValue) -> IrValue {
    IrValue::EnumVariant(Rc::new(RefCell::new(EnumData {
        enum_name: OPTION_ENUM.to_string(),
        discriminant: 0,
        fields: vec![val],
    })))
}

/// Construct an `Option::None` IR value.
pub fn none_val() -> IrValue {
    IrValue::EnumVariant(Rc::new(RefCell::new(EnumData {
        enum_name: OPTION_ENUM.to_string(),
        discriminant: 1,
        fields: vec![],
    })))
}

/// Construct a `Result::Ok` IR value.
pub fn ok_val(val: IrValue) -> IrValue {
    IrValue::EnumVariant(Rc::new(RefCell::new(EnumData {
        enum_name: RESULT_ENUM.to_string(),
        discriminant: 0,
        fields: vec![val],
    })))
}

/// Construct a `Result::Err` IR value.
pub fn err_val(val: IrValue) -> IrValue {
    IrValue::EnumVariant(Rc::new(RefCell::new(EnumData {
        enum_name: RESULT_ENUM.to_string(),
        discriminant: 1,
        fields: vec![val],
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `map_key_eq` is byte-wise for floats (K.9), unlike `IrValue`'s
    /// IEEE `==`. Native and wasm32-gc compare float keys by bits, so the
    /// IR interpreter must too. Pins the two cases where IEEE and
    /// byte-wise disagree — `±0.0` and `NaN` — plus a scalar sanity check.
    #[test]
    fn map_key_eq_is_byte_wise_for_floats() {
        // ±0.0: IEEE-equal but bit-distinct → NOT map-key-equal.
        let neg_zero = -0.0_f64;
        assert!(0.0_f64 == neg_zero, "precondition: IEEE treats ±0.0 equal");
        assert!(!map_key_eq(&IrValue::Float(0.0), &IrValue::Float(-0.0)));
        assert!(!map_key_eq(&IrValue::Float(-0.0), &IrValue::Float(0.0)));

        // NaN: IEEE-unequal but same bits → map-key-equal.
        let nan = f64::NAN;
        assert!(nan != nan, "precondition: IEEE treats NaN as never-equal");
        assert!(map_key_eq(&IrValue::Float(nan), &IrValue::Float(nan)));

        // Ordinary floats and non-float keys fall through to `==`.
        assert!(map_key_eq(&IrValue::Float(1.5), &IrValue::Float(1.5)));
        assert!(!map_key_eq(&IrValue::Float(1.5), &IrValue::Float(2.5)));
        assert!(map_key_eq(&IrValue::Int(7), &IrValue::Int(7)));
        assert!(map_key_eq(
            &IrValue::String("a".into()),
            &IrValue::String("a".into())
        ));
        assert!(!map_key_eq(&IrValue::Int(7), &IrValue::Float(7.0)));
    }
}
