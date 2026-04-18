//! Option and Result builtin method dispatch for the IR interpreter.

use crate::error::{IrRuntimeError, Result, error};
use crate::interpreter::IrInterpreter;
use crate::value::{IrValue, err_val, none_val, ok_val, some_val};
use phoenix_ir::types::{OPTION_ENUM, RESULT_ENUM};

use super::expect_one_arg;

// ── Shared enum variant info helper ──────────────────────────────────

/// Extract variant info from an enum IR value.
///
/// Matches on `IrValue::EnumVariant`, checks that `enum_name` matches
/// `expected_enum`, and maps discriminant 0 → `variant_names[0]`,
/// discriminant 1 → `variant_names[1]`.
fn enum_variant_info(
    val: &IrValue,
    expected_enum: &str,
    variant_names: [&'static str; 2],
    type_label: &str,
) -> Result<(&'static str, Vec<IrValue>, IrValue)> {
    match val {
        IrValue::EnumVariant(data) => {
            let data = data.borrow();
            if data.enum_name == expected_enum {
                let disc = data.discriminant as usize;
                if disc >= variant_names.len() {
                    return error(format!(
                        "{type_label} has discriminant {disc} but only {} variants are defined",
                        variant_names.len(),
                    ));
                }
                let name = variant_names[disc];
                Ok((name, data.fields.clone(), val.clone()))
            } else {
                error(format!(
                    "{type_label} method called on non-{type_label} enum: {}",
                    data.enum_name,
                ))
            }
        }
        _ => error(format!(
            "{type_label} method called on non-{type_label} value: {val}"
        )),
    }
}

// ── Option builtins ──────────────────────────────────────────────────

/// Extract Option variant info from an IR value.
///
/// The IR represents Option variants as `EnumAlloc("Option", disc, fields)`
/// → `IrValue::EnumVariant`.  Discriminant 0 = Some, 1 = None.
fn option_info(val: &IrValue) -> Result<(&'static str, Vec<IrValue>, IrValue)> {
    enum_variant_info(val, OPTION_ENUM, ["Some", "None"], "Option")
}

/// Dispatch an `Option.*` method call (`isSome`, `isNone`, `unwrap`, `map`, etc.).
pub(super) fn builtin_option(
    interp: &mut IrInterpreter<'_>,
    method: &str,
    args: Vec<IrValue>,
) -> Result<IrValue> {
    let receiver = args.first().ok_or_else(|| IrRuntimeError {
        message: "Option method called with no receiver".to_string(),
    })?;

    let (variant_name, fields, obj_clone) = option_info(receiver)?;

    let method_args = &args[1..];

    match method {
        "isSome" => Ok(IrValue::Bool(variant_name == "Some")),
        "isNone" => Ok(IrValue::Bool(variant_name == "None")),
        "unwrap" => {
            if variant_name == "Some" && !fields.is_empty() {
                Ok(fields[0].clone())
            } else {
                error("called unwrap() on None")
            }
        }
        "unwrapOr" => {
            let default = expect_one_arg(method_args, "unwrapOr")?;
            if variant_name == "Some" && !fields.is_empty() {
                Ok(fields[0].clone())
            } else {
                Ok(default)
            }
        }
        "map" => {
            let closure = expect_one_arg(method_args, "map")?;
            if variant_name == "Some" && !fields.is_empty() {
                let result = interp.call_closure(&closure, vec![fields[0].clone()])?;
                Ok(some_val(result))
            } else {
                Ok(none_val())
            }
        }
        "andThen" => {
            let closure = expect_one_arg(method_args, "andThen")?;
            if variant_name == "Some" && !fields.is_empty() {
                interp.call_closure(&closure, vec![fields[0].clone()])
            } else {
                Ok(none_val())
            }
        }
        "orElse" => {
            if variant_name == "Some" {
                Ok(obj_clone)
            } else {
                let closure = expect_one_arg(method_args, "orElse")?;
                interp.call_closure(&closure, vec![])
            }
        }
        "filter" => {
            let closure = expect_one_arg(method_args, "filter")?;
            if variant_name == "Some" && !fields.is_empty() {
                let result = interp.call_closure(&closure, vec![fields[0].clone()])?;
                if result == IrValue::Bool(true) {
                    Ok(obj_clone)
                } else {
                    Ok(none_val())
                }
            } else {
                Ok(none_val())
            }
        }
        "unwrapOrElse" => {
            if variant_name == "Some" && !fields.is_empty() {
                Ok(fields[0].clone())
            } else {
                let closure = expect_one_arg(method_args, "unwrapOrElse")?;
                interp.call_closure(&closure, vec![])
            }
        }
        "okOr" => {
            let err_default = expect_one_arg(method_args, "okOr")?;
            if variant_name == "Some" && !fields.is_empty() {
                Ok(ok_val(fields[0].clone()))
            } else {
                Ok(err_val(err_default))
            }
        }
        _ => error(format!("no method `{method}` on type `Option`")),
    }
}

// ── Result builtins ──────────────────────────────────────────────────

/// Extract Result variant info from an IR value.
///
/// The IR represents Result variants as `EnumAlloc("Result", disc, fields)`
/// → `IrValue::EnumVariant`.  Discriminant 0 = Ok, 1 = Err.
fn result_info(val: &IrValue) -> Result<(&'static str, Vec<IrValue>, IrValue)> {
    enum_variant_info(val, RESULT_ENUM, ["Ok", "Err"], "Result")
}

/// Dispatch a `Result.*` method call (`isOk`, `isErr`, `unwrap`, `map`, `mapErr`, etc.).
pub(super) fn builtin_result(
    interp: &mut IrInterpreter<'_>,
    method: &str,
    args: Vec<IrValue>,
) -> Result<IrValue> {
    let receiver = args.first().ok_or_else(|| IrRuntimeError {
        message: "Result method called with no receiver".to_string(),
    })?;

    let (variant_name, fields, obj_clone) = result_info(receiver)?;

    let method_args = &args[1..];

    match method {
        "isOk" => Ok(IrValue::Bool(variant_name == "Ok")),
        "isErr" => Ok(IrValue::Bool(variant_name == "Err")),
        "unwrap" => {
            if variant_name == "Ok" && !fields.is_empty() {
                Ok(fields[0].clone())
            } else {
                error("called unwrap() on Err")
            }
        }
        "unwrapOr" => {
            let default = expect_one_arg(method_args, "unwrapOr")?;
            if variant_name == "Ok" && !fields.is_empty() {
                Ok(fields[0].clone())
            } else {
                Ok(default)
            }
        }
        "map" => {
            let closure = expect_one_arg(method_args, "map")?;
            if variant_name == "Ok" && !fields.is_empty() {
                let result = interp.call_closure(&closure, vec![fields[0].clone()])?;
                Ok(ok_val(result))
            } else {
                Ok(obj_clone)
            }
        }
        "mapErr" => {
            let closure = expect_one_arg(method_args, "mapErr")?;
            if variant_name == "Err" && !fields.is_empty() {
                let result = interp.call_closure(&closure, vec![fields[0].clone()])?;
                Ok(err_val(result))
            } else {
                Ok(obj_clone)
            }
        }
        "andThen" => {
            let closure = expect_one_arg(method_args, "andThen")?;
            if variant_name == "Ok" && !fields.is_empty() {
                interp.call_closure(&closure, vec![fields[0].clone()])
            } else {
                Ok(obj_clone)
            }
        }
        "orElse" => {
            if variant_name == "Ok" {
                Ok(obj_clone)
            } else {
                let closure = expect_one_arg(method_args, "orElse")?;
                if !fields.is_empty() {
                    interp.call_closure(&closure, vec![fields[0].clone()])
                } else {
                    interp.call_closure(&closure, vec![])
                }
            }
        }
        "unwrapOrElse" => {
            if variant_name == "Ok" && !fields.is_empty() {
                Ok(fields[0].clone())
            } else {
                let closure = expect_one_arg(method_args, "unwrapOrElse")?;
                if variant_name == "Err" && !fields.is_empty() {
                    interp.call_closure(&closure, vec![fields[0].clone()])
                } else {
                    interp.call_closure(&closure, vec![])
                }
            }
        }
        "ok" => {
            if variant_name == "Ok" && !fields.is_empty() {
                Ok(some_val(fields[0].clone()))
            } else {
                Ok(none_val())
            }
        }
        "err" => {
            if variant_name == "Err" && !fields.is_empty() {
                Ok(some_val(fields[0].clone()))
            } else {
                Ok(none_val())
            }
        }
        _ => error(format!("no method `{method}` on type `Result`")),
    }
}
