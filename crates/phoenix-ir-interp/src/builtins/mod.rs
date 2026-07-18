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
                    "ListBuilder" => collections::builtin_list_builder(method, args),
                    "MapBuilder" => collections::builtin_map_builder(method, args),
                    "json" => builtin_json(interp, method, args),
                    _ => error(format!("unknown builtin type: {type_name}")),
                }
            } else {
                error(format!("unknown builtin function: {name}"))
            }
        }
    }
}

/// A parsed JSON DOM root in the interpreter's `json_arena`: the tree, or
/// the captured parse-error message. Mirrors the compiled runtime's opaque
/// pointer handles; handles are the arena index, passed through IR as
/// `i64`. A root doubles as its own top-level node — `json.root` returns
/// the handle unchanged rather than cloning the tree; `json.getField`
/// pushes each child it returns as its own arena entry (a subtree clone),
/// so child handles resolve exactly like roots.
pub(crate) struct JsonRoot(std::result::Result<serde_json::Value, String>);

/// Dispatch the intrinsic `json.*` builtins emitted by the JSON encode /
/// decode synthesis. Encode's `escapeString` is pure; the decode builtins
/// operate over the interpreter's DOM arena.
fn builtin_json(
    interp: &mut IrInterpreter<'_>,
    method: &str,
    args: Vec<IrValue>,
) -> Result<IrValue> {
    use phoenix_runtime::json as rt;
    match method {
        "escapeString" => match args.as_slice() {
            [IrValue::String(s)] => Ok(IrValue::String(phoenix_runtime::json_escape(s))),
            _ => error("json.escapeString expects a single string argument".to_string()),
        },
        "parse" => {
            let s = json_str_arg(&args, "json.parse")?;
            let parsed = serde_json::from_str::<serde_json::Value>(&s).map_err(|e| e.to_string());
            interp.json_arena.push(JsonRoot(parsed));
            Ok(IrValue::Int((interp.json_arena.len() - 1) as i64))
        }
        "free" => Ok(IrValue::Void), // arena grows for the interpreter's lifetime
        "parseFailed" => {
            let failed = json_handle(interp, &args, "json.parseFailed")?.0.is_err();
            Ok(IrValue::Bool(failed))
        }
        "parseError" => {
            let msg = match &json_handle(interp, &args, "json.parseError")?.0 {
                Err(m) => m.clone(),
                Ok(_) => String::new(),
            };
            Ok(IrValue::String(msg))
        }
        "root" => {
            // A root doubles as its own top-level node (`json_node` resolves
            // the parsed tree directly), so hand the handle back unchanged —
            // no tree clone. Still bounds-checked like every other use.
            let h = json_handle_arg(&args, "json.root")?;
            if interp.json_arena.get(h).is_none() {
                return error("json.root: invalid JSON handle".to_string());
            }
            Ok(IrValue::Int(h as i64))
        }
        "getField" => match args.as_slice() {
            [IrValue::Int(h), IrValue::String(key)] => {
                // Clone the child subtree (releasing the arena borrow) before
                // pushing it as its own arena entry (a child is always a
                // parsed node). Nested decoding re-clones grandchildren from
                // the clone and the arena only grows (`free` is a no-op) —
                // O(subtree × depth) work, accepted for a reference
                // interpreter; the compiled runtime borrows into the root
                // tree instead.
                // A failed-parse root or out-of-range handle here is an
                // IR-synthesis bug (decoders check `parseFailed` before
                // navigating) — surface it as an interpreter error rather
                // than masking it as a plausible `Err(MissingField)`.
                let child = match interp.json_arena.get(*h as usize) {
                    Some(JsonRoot(Ok(v))) => v.get(key).cloned(),
                    Some(JsonRoot(Err(_))) => {
                        return error("json.getField: handle is a failed-parse root".to_string());
                    }
                    None => return error("json.getField: invalid JSON handle".to_string()),
                };
                match child {
                    Some(c) => {
                        interp.json_arena.push(JsonRoot(Ok(c)));
                        Ok(IrValue::Int((interp.json_arena.len() - 1) as i64))
                    }
                    // Key absent → the missing-field sentinel (see `isMissing`).
                    None => Ok(IrValue::Int(-1)),
                }
            }
            _ => error("json.getField expects (handle, key)".to_string()),
        },
        "arrayGet" => match args.as_slice() {
            // Index an array node, cloning the element into its own arena entry
            // (same borrow-release pattern as `getField`). Out of range or a
            // non-array node → the missing sentinel (`-1`), tested by
            // `isMissing`; a failed-parse root is an IR-synthesis bug.
            [IrValue::Int(h), IrValue::Int(idx)] => {
                let elem = match interp.json_arena.get(*h as usize) {
                    Some(JsonRoot(Ok(v))) => v.get(*idx as usize).cloned(),
                    Some(JsonRoot(Err(_))) => {
                        return error("json.arrayGet: handle is a failed-parse root".to_string());
                    }
                    None => return error("json.arrayGet: invalid JSON handle".to_string()),
                };
                match elem {
                    Some(c) => {
                        interp.json_arena.push(JsonRoot(Ok(c)));
                        Ok(IrValue::Int((interp.json_arena.len() - 1) as i64))
                    }
                    None => Ok(IrValue::Int(-1)),
                }
            }
            _ => error("json.arrayGet expects (handle, index)".to_string()),
        },
        "arrayLen" => {
            let v = json_node(interp, &args, "json.arrayLen")?;
            Ok(IrValue::Int(match v {
                serde_json::Value::Array(a) => a.len() as i64,
                _ => 0,
            }))
        }
        "objectLen" => {
            let v = json_node(interp, &args, "json.objectLen")?;
            Ok(IrValue::Int(match v {
                serde_json::Value::Object(o) => o.len() as i64,
                _ => 0,
            }))
        }
        "objectKeyAt" => match args.as_slice() {
            // The i-th object key (serde's `Map` iterates in key order); empty
            // string when not an object / out of range. `nth` is O(index) —
            // a full decode walk is quadratic, matching the native runtime
            // (see the note on `phx_json_object_len`).
            [IrValue::Int(h), IrValue::Int(idx)] => {
                let key = match interp.json_arena.get(*h as usize) {
                    Some(JsonRoot(Ok(serde_json::Value::Object(o)))) => {
                        o.keys().nth(*idx as usize).cloned().unwrap_or_default()
                    }
                    Some(JsonRoot(Ok(_))) => String::new(),
                    Some(JsonRoot(Err(_))) => {
                        return error(
                            "json.objectKeyAt: handle is a failed-parse root".to_string(),
                        );
                    }
                    None => return error("json.objectKeyAt: invalid JSON handle".to_string()),
                };
                Ok(IrValue::String(key))
            }
            _ => error("json.objectKeyAt expects (handle, index)".to_string()),
        },
        "objectValueAt" => match args.as_slice() {
            // The i-th object value node, cloned into its own arena entry (same
            // borrow-release pattern as `getField`); missing sentinel when not
            // an object / out of range.
            [IrValue::Int(h), IrValue::Int(idx)] => {
                let val = match interp.json_arena.get(*h as usize) {
                    Some(JsonRoot(Ok(serde_json::Value::Object(o)))) => {
                        o.values().nth(*idx as usize).cloned()
                    }
                    Some(JsonRoot(Ok(_))) => None,
                    Some(JsonRoot(Err(_))) => {
                        return error(
                            "json.objectValueAt: handle is a failed-parse root".to_string(),
                        );
                    }
                    None => return error("json.objectValueAt: invalid JSON handle".to_string()),
                };
                match val {
                    Some(c) => {
                        interp.json_arena.push(JsonRoot(Ok(c)));
                        Ok(IrValue::Int((interp.json_arena.len() - 1) as i64))
                    }
                    None => Ok(IrValue::Int(-1)),
                }
            }
            _ => error("json.objectValueAt expects (handle, index)".to_string()),
        },
        "isMissing" => match args.as_slice() {
            [IrValue::Int(h)] => Ok(IrValue::Bool(*h == -1)),
            _ => error("json.isMissing expects a single handle argument".to_string()),
        },
        "kind" => {
            let v = json_node(interp, &args, "json.kind")?;
            let kind = match v {
                serde_json::Value::Null => rt::JSON_KIND_NULL,
                serde_json::Value::Bool(_) => rt::JSON_KIND_BOOL,
                serde_json::Value::Number(n) if n.is_i64() => rt::JSON_KIND_INT,
                serde_json::Value::Number(_) => rt::JSON_KIND_FLOAT,
                serde_json::Value::String(_) => rt::JSON_KIND_STRING,
                serde_json::Value::Array(_) => rt::JSON_KIND_ARRAY,
                serde_json::Value::Object(_) => rt::JSON_KIND_OBJECT,
            };
            Ok(IrValue::Int(kind))
        }
        "asInt" => Ok(IrValue::Int(
            json_node(interp, &args, "json.asInt")?
                .as_i64()
                .unwrap_or(0),
        )),
        "asFloat" => Ok(IrValue::Float(
            json_node(interp, &args, "json.asFloat")?
                .as_f64()
                .unwrap_or(0.0),
        )),
        "asBool" => Ok(IrValue::Bool(
            json_node(interp, &args, "json.asBool")?
                .as_bool()
                .unwrap_or(false),
        )),
        "asStr" => Ok(IrValue::String(
            json_node(interp, &args, "json.asStr")?
                .as_str()
                .unwrap_or("")
                .to_string(),
        )),
        _ => error(format!("unknown json builtin: {method}")),
    }
}

/// Extract the single `String` argument of a `json.*` builtin.
fn json_str_arg(args: &[IrValue], method: &str) -> Result<String> {
    match args {
        [IrValue::String(s)] => Ok(s.clone()),
        _ => error(format!("{method} expects a single string argument")),
    }
}

/// Extract the single `i64` handle argument of a `json.*` builtin.
fn json_handle_arg(args: &[IrValue], method: &str) -> Result<usize> {
    match args {
        [IrValue::Int(i)] => Ok(*i as usize),
        _ => error(format!("{method} expects a single handle argument")),
    }
}

/// Resolve a `json.*` builtin's handle arg to its arena entry (bounds-checked;
/// an out-of-range handle is a clean error, never a panic).
fn json_handle<'a>(
    interp: &'a IrInterpreter<'_>,
    args: &[IrValue],
    method: &str,
) -> Result<&'a JsonRoot> {
    let h = json_handle_arg(args, method)?;
    match interp.json_arena.get(h) {
        Some(handle) => Ok(handle),
        None => error(format!("{method}: invalid JSON handle")),
    }
}

/// Resolve the node value referenced by a `json.*` builtin's handle arg.
/// A failed-parse root has no node — the decoders branch on `parseFailed`
/// before navigating, so hitting one here is a clean interpreter error.
fn json_node<'a>(
    interp: &'a IrInterpreter<'_>,
    args: &[IrValue],
    method: &str,
) -> Result<&'a serde_json::Value> {
    match &json_handle(interp, args, method)?.0 {
        Ok(v) => Ok(v),
        Err(_) => error(format!("{method}: invalid JSON node handle")),
    }
}

// ── Shared helpers ──────────────────────────────────────────────────

/// Extract the first argument, erroring with a message naming `method`.
fn expect_one_arg(args: &[IrValue], method: &str) -> Result<IrValue> {
    args.first().cloned().ok_or_else(|| IrRuntimeError {
        message: format!("{method}() requires 1 argument"),
    })
}

/// Extract exactly two arguments as `(a, b)`, erroring with a message
/// naming `method`. The two-arg analogue of [`expect_one_arg`]; rejects
/// extra arguments rather than silently ignoring them.
fn expect_two_args(args: &[IrValue], method: &str) -> Result<(IrValue, IrValue)> {
    match args {
        [a, b] => Ok((a.clone(), b.clone())),
        _ => Err(IrRuntimeError {
            message: format!("{method}() requires 2 arguments"),
        }),
    }
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
        IrModule::new()
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
