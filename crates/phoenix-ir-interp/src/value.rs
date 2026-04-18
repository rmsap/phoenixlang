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
    /// A closure: target function ID and captured values.
    Closure(FuncId, Vec<IrValue>),
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

    /// Format this value for user-facing output, matching the AST interpreter's
    /// `Display for Value` exactly.
    pub fn format(&self, module: &IrModule) -> String {
        match self {
            IrValue::Int(n) => format!("{n}"),
            IrValue::Float(n) => format!("{n}"),
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
            (IrValue::Closure(_, _), IrValue::Closure(_, _)) => false,
            _ => false,
        }
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
            IrValue::Float(n) => write!(f, "{n}"),
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
