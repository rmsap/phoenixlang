//! Built-in function dispatch for the IR interpreter.
//!
//! The IR lowers builtin calls to `BuiltinCall("name", args)` where the name
//! is either a global (`"print"`) or a method (`"String.length"`).
//!
//! Submodules handle domain-specific dispatch:
//! - [`collections`]: String, List, and Map methods.
//! - [`option_result`]: Option and Result methods.

mod collections;
mod option_result;

use crate::error::{IrRuntimeError, Result, error};
use crate::interpreter::IrInterpreter;
use crate::value::IrValue;

/// Dispatch a builtin call by name.
pub(crate) fn dispatch(
    interp: &mut IrInterpreter<'_>,
    name: &str,
    args: Vec<IrValue>,
) -> Result<IrValue> {
    match name {
        "print" => {
            if args.len() != 1 {
                return error(format!("print() expects 1 argument, got {}", args.len()));
            }
            let s = args[0].format(interp.module());
            interp.write_output(&s)?;
            Ok(IrValue::Void)
        }
        "toString" => {
            if args.len() != 1 {
                return error(format!("toString() expects 1 argument, got {}", args.len()));
            }
            let s = args[0].format(interp.module());
            Ok(IrValue::String(s))
        }
        _ => {
            if let Some((type_name, method)) = name.split_once('.') {
                match type_name {
                    "String" => collections::builtin_string(method, args),
                    "List" => collections::builtin_list(interp, method, args),
                    "Map" => collections::builtin_map(method, args),
                    "Option" => option_result::builtin_option(interp, method, args),
                    "Result" => option_result::builtin_result(interp, method, args),
                    _ => error(format!("unknown builtin type: {type_name}")),
                }
            } else {
                error(format!("unknown builtin function: {name}"))
            }
        }
    }
}

// ── Shared helpers ──────────────────────────────────────────────────

/// Extract the first argument, erroring with a message naming `method`.
fn expect_one_arg(args: &[IrValue], method: &str) -> Result<IrValue> {
    args.first().cloned().ok_or_else(|| IrRuntimeError {
        message: format!("{method}() requires 1 argument"),
    })
}

/// Extract a `String` argument at `idx`, erroring with a message naming `method`.
fn expect_string_arg(args: &[IrValue], idx: usize, method: &str) -> Result<String> {
    match args.get(idx) {
        Some(IrValue::String(s)) => Ok(s.clone()),
        Some(_) => error(format!("{method}() argument must be String")),
        None => error(format!("{method}() missing argument {idx}")),
    }
}

/// Extract an `Int` argument at `idx`, erroring with a message naming `method`.
fn expect_int_arg(args: &[IrValue], idx: usize, method: &str) -> Result<i64> {
    match args.get(idx) {
        Some(IrValue::Int(n)) => Ok(*n),
        Some(_) => error(format!("{method}() argument must be Int")),
        None => error(format!("{method}() missing argument {idx}")),
    }
}

/// Extract a `Bool` from a callback return value, erroring if it isn't one.
fn expect_bool(val: &IrValue, method: &str) -> Result<bool> {
    match val {
        IrValue::Bool(b) => Ok(*b),
        other => error(format!("{method}() callback must return Bool, got {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{EnumData, none_val, ok_val, some_val};
    use phoenix_ir::module::IrModule;
    use phoenix_ir::types::OPTION_ENUM;

    fn empty_module() -> IrModule {
        IrModule {
            functions: vec![],
            struct_layouts: Default::default(),
            enum_layouts: Default::default(),
            enum_type_params: Default::default(),
            function_index: Default::default(),
            method_index: Default::default(),
        }
    }

    #[test]
    fn expect_bool_rejects_non_bool() {
        let result = expect_bool(&IrValue::Int(42), "filter");
        assert!(result.is_err());
        let msg = result.unwrap_err().message;
        assert!(
            msg.contains("filter"),
            "message should name the method: {msg}"
        );
        assert!(msg.contains("Bool"), "message should mention Bool: {msg}");
    }

    #[test]
    fn filter_rejects_non_bool_callback() {
        let module = empty_module();
        let interp = IrInterpreter::new(&module, Box::new(std::io::sink()));
        let val = IrValue::String("not a bool".to_string());
        assert!(expect_bool(&val, "any").is_err());
        assert!(expect_bool(&val, "all").is_err());
        assert!(expect_bool(&val, "find").is_err());

        // Verify Bool values pass through correctly.
        assert!(expect_bool(&IrValue::Bool(true), "any").unwrap());
        assert!(!expect_bool(&IrValue::Bool(false), "all").unwrap());
        let _ = interp; // suppress unused warning
    }

    /// Option.unwrapOr with no arguments must error.
    #[test]
    fn option_unwrap_or_missing_arg_errors() {
        let module = empty_module();
        let mut interp = IrInterpreter::new(&module, Box::new(std::io::sink()));
        let receiver = some_val(IrValue::Int(1));
        let result = dispatch(&mut interp, "Option.unwrapOr", vec![receiver]);
        assert!(result.is_err());
        let msg = result.unwrap_err().message;
        assert!(
            msg.contains("unwrapOr"),
            "error should mention unwrapOr: {msg}"
        );
    }

    /// Result.unwrapOr with no arguments must error.
    #[test]
    fn result_unwrap_or_missing_arg_errors() {
        let module = empty_module();
        let mut interp = IrInterpreter::new(&module, Box::new(std::io::sink()));
        let receiver = ok_val(IrValue::Int(1));
        let result = dispatch(&mut interp, "Result.unwrapOr", vec![receiver]);
        assert!(result.is_err());
        let msg = result.unwrap_err().message;
        assert!(
            msg.contains("unwrapOr"),
            "error should mention unwrapOr: {msg}"
        );
    }

    /// Option.okOr with no arguments must error.
    #[test]
    fn option_ok_or_missing_arg_errors() {
        let module = empty_module();
        let mut interp = IrInterpreter::new(&module, Box::new(std::io::sink()));
        let receiver = none_val();
        let result = dispatch(&mut interp, "Option.okOr", vec![receiver]);
        assert!(result.is_err());
        let msg = result.unwrap_err().message;
        assert!(msg.contains("okOr"), "error should mention okOr: {msg}");
    }

    /// An EnumVariant with discriminant >= 2 must produce an error.
    #[test]
    fn enum_variant_info_out_of_range_discriminant() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let bad_variant = IrValue::EnumVariant(Rc::new(RefCell::new(EnumData {
            enum_name: OPTION_ENUM.to_string(),
            discriminant: 2,
            fields: vec![],
        })));

        let module = empty_module();
        let mut interp = IrInterpreter::new(&module, Box::new(std::io::sink()));
        let result = dispatch(&mut interp, "Option.isSome", vec![bad_variant]);
        assert!(result.is_err());
        let msg = result.unwrap_err().message;
        assert!(
            msg.contains("discriminant") || msg.contains("2"),
            "error should mention the bad discriminant: {msg}"
        );
    }

    #[test]
    fn print_rejects_zero_args() {
        let module = empty_module();
        let mut interp = IrInterpreter::new(&module, Box::new(std::io::sink()));
        let result = dispatch(&mut interp, "print", vec![]);
        assert!(result.is_err());
        let msg = result.unwrap_err().message;
        assert!(msg.contains("expects 1 argument"), "wrong message: {msg}");
    }

    #[test]
    fn print_rejects_extra_args() {
        let module = empty_module();
        let mut interp = IrInterpreter::new(&module, Box::new(std::io::sink()));
        let result = dispatch(&mut interp, "print", vec![IrValue::Int(1), IrValue::Int(2)]);
        assert!(result.is_err());
        let msg = result.unwrap_err().message;
        assert!(msg.contains("expects 1 argument"), "wrong message: {msg}");
    }

    #[test]
    fn to_string_rejects_zero_args() {
        let module = empty_module();
        let mut interp = IrInterpreter::new(&module, Box::new(std::io::sink()));
        let result = dispatch(&mut interp, "toString", vec![]);
        assert!(result.is_err());
        let msg = result.unwrap_err().message;
        assert!(msg.contains("expects 1 argument"), "wrong message: {msg}");
    }

    #[test]
    fn to_string_rejects_extra_args() {
        let module = empty_module();
        let mut interp = IrInterpreter::new(&module, Box::new(std::io::sink()));
        let result = dispatch(
            &mut interp,
            "toString",
            vec![IrValue::Int(1), IrValue::Int(2)],
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().message;
        assert!(msg.contains("expects 1 argument"), "wrong message: {msg}");
    }
}
