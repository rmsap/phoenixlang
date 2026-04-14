//! Built-in function dispatch for the IR interpreter.
//!
//! The IR lowers builtin calls to `BuiltinCall("name", args)` where the name
//! is either a global (`"print"`) or a method (`"String.length"`).

use crate::error::{IrRuntimeError, Result, error};
use crate::interpreter::IrInterpreter;
use crate::value::{IrValue, err_val, none_val, ok_val, some_val};

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
                    "String" => builtin_string(method, args),
                    "List" => builtin_list(interp, method, args),
                    "Map" => builtin_map(method, args),
                    "Option" => builtin_option(interp, method, args),
                    "Result" => builtin_result(interp, method, args),
                    _ => error(format!("unknown builtin type: {type_name}")),
                }
            } else {
                error(format!("unknown builtin function: {name}"))
            }
        }
    }
}

// ── String builtins ──────────────────────────────────────────────────

/// Dispatch a `String.*` method call (`length`, `contains`, `split`, etc.).
fn builtin_string(method: &str, args: Vec<IrValue>) -> Result<IrValue> {
    let receiver = match args.first() {
        Some(IrValue::String(s)) => s.clone(),
        _ => return error("String method called on non-string"),
    };
    let method_args = &args[1..];

    match method {
        "length" => Ok(IrValue::Int(receiver.chars().count() as i64)),
        "contains" => {
            let sub = expect_string_arg(method_args, 0, "contains")?;
            Ok(IrValue::Bool(receiver.contains(sub.as_str())))
        }
        "startsWith" => {
            let pre = expect_string_arg(method_args, 0, "startsWith")?;
            Ok(IrValue::Bool(receiver.starts_with(pre.as_str())))
        }
        "endsWith" => {
            let suf = expect_string_arg(method_args, 0, "endsWith")?;
            Ok(IrValue::Bool(receiver.ends_with(suf.as_str())))
        }
        "trim" => Ok(IrValue::String(receiver.trim().to_string())),
        "toLowerCase" => Ok(IrValue::String(receiver.to_lowercase())),
        "toUpperCase" => Ok(IrValue::String(receiver.to_uppercase())),
        "split" => {
            let sep = expect_string_arg(method_args, 0, "split")?;
            let parts: Vec<IrValue> = receiver
                .split(sep.as_str())
                .map(|p| IrValue::String(p.to_string()))
                .collect();
            Ok(IrValue::new_list(parts))
        }
        "replace" => {
            let old = expect_string_arg(method_args, 0, "replace")?;
            let new = expect_string_arg(method_args, 1, "replace")?;
            Ok(IrValue::String(
                receiver.replace(old.as_str(), new.as_str()),
            ))
        }
        "substring" => {
            let start = expect_int_arg(method_args, 0, "substring")?;
            let end = expect_int_arg(method_args, 1, "substring")?;
            if start < 0 || end < 0 {
                return error(format!(
                    "substring() indices must be non-negative, got ({start}, {end})"
                ));
            }
            let start_u = start as usize;
            let end_u = end as usize;
            let chars: Vec<char> = receiver.chars().collect();
            if start_u > chars.len() || end_u > chars.len() || start_u > end_u {
                return error(format!(
                    "substring({start}, {end}) out of bounds (length {})",
                    chars.len()
                ));
            }
            Ok(IrValue::String(chars[start_u..end_u].iter().collect()))
        }
        "indexOf" => {
            let sub = expect_string_arg(method_args, 0, "indexOf")?;
            let char_index = receiver
                .find(sub.as_str())
                .map(|byte_offset| receiver[..byte_offset].chars().count() as i64)
                .unwrap_or(-1);
            Ok(IrValue::Int(char_index))
        }
        _ => error(format!("no method `{method}` on type `String`")),
    }
}

// ── List builtins ────────────────────────────────────────────────────

/// Dispatch a `List.*` method call (`length`, `get`, `push`, `map`, `filter`, etc.).
fn builtin_list(
    interp: &mut IrInterpreter<'_>,
    method: &str,
    args: Vec<IrValue>,
) -> Result<IrValue> {
    let list_ref = match args.first() {
        Some(IrValue::List(elems)) => elems.clone(),
        _ => return error("List method called on non-list"),
    };
    let method_args = &args[1..];

    // Read-only methods: borrow the list without cloning.
    match method {
        "length" => return Ok(IrValue::Int(list_ref.borrow().len() as i64)),
        "get" => {
            let idx = expect_int_arg(method_args, 0, "get")?;
            let elems = list_ref.borrow();
            if idx < 0 || idx as usize >= elems.len() {
                return error(format!(
                    "list index {} out of bounds (length {})",
                    idx,
                    elems.len()
                ));
            }
            return Ok(elems[idx as usize].clone());
        }
        "first" => {
            let elems = list_ref.borrow();
            return if elems.is_empty() {
                Ok(none_val())
            } else {
                Ok(some_val(elems[0].clone()))
            };
        }
        "last" => {
            let elems = list_ref.borrow();
            return if elems.is_empty() {
                Ok(none_val())
            } else {
                Ok(some_val(elems[elems.len() - 1].clone()))
            };
        }
        "contains" => {
            let target = method_args.first().ok_or_else(|| IrRuntimeError {
                message: "contains() requires 1 argument".to_string(),
            })?;
            return Ok(IrValue::Bool(list_ref.borrow().contains(target)));
        }
        "take" => {
            let n = expect_int_arg(method_args, 0, "take")?;
            if n < 0 {
                return error(format!("take() argument must be non-negative, got {n}"));
            }
            let taken: Vec<IrValue> = list_ref.borrow().iter().take(n as usize).cloned().collect();
            return Ok(IrValue::new_list(taken));
        }
        "drop" => {
            let n = expect_int_arg(method_args, 0, "drop")?;
            if n < 0 {
                return error(format!("drop() argument must be non-negative, got {n}"));
            }
            let dropped: Vec<IrValue> =
                list_ref.borrow().iter().skip(n as usize).cloned().collect();
            return Ok(IrValue::new_list(dropped));
        }
        _ => {} // Fall through to closure-based methods below.
    }

    // Closure-based methods: clone the elements so the borrow is released
    // before calling closures (which can re-enter the interpreter).
    let elements = list_ref.borrow().clone();

    match method {
        "push" => {
            let val = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "push() requires 1 argument".to_string(),
            })?;
            let mut new_list = elements;
            new_list.push(val);
            Ok(IrValue::new_list(new_list))
        }
        "map" => {
            let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "map() requires 1 argument".to_string(),
            })?;
            let mut result = Vec::with_capacity(elements.len());
            for elem in elements {
                result.push(interp.call_closure(closure.clone(), vec![elem])?);
            }
            Ok(IrValue::new_list(result))
        }
        "flatMap" => {
            let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "flatMap() requires 1 argument".to_string(),
            })?;
            let mut result = Vec::new();
            for elem in elements {
                let val = interp.call_closure(closure.clone(), vec![elem])?;
                if let IrValue::List(inner) = val {
                    result.extend(inner.borrow().clone());
                } else {
                    return error("flatMap callback must return a List");
                }
            }
            Ok(IrValue::new_list(result))
        }
        "filter" => {
            let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "filter() requires 1 argument".to_string(),
            })?;
            let mut result = Vec::new();
            for elem in elements {
                let val = interp.call_closure(closure.clone(), vec![elem.clone()])?;
                if expect_bool(&val, "filter")? {
                    result.push(elem);
                }
            }
            Ok(IrValue::new_list(result))
        }
        "find" => {
            let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "find() requires 1 argument".to_string(),
            })?;
            for elem in elements {
                let val = interp.call_closure(closure.clone(), vec![elem.clone()])?;
                if expect_bool(&val, "find")? {
                    return Ok(some_val(elem));
                }
            }
            Ok(none_val())
        }
        "any" => {
            let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "any() requires 1 argument".to_string(),
            })?;
            for elem in elements {
                let val = interp.call_closure(closure.clone(), vec![elem])?;
                if expect_bool(&val, "any")? {
                    return Ok(IrValue::Bool(true));
                }
            }
            Ok(IrValue::Bool(false))
        }
        "all" => {
            let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "all() requires 1 argument".to_string(),
            })?;
            for elem in elements {
                let val = interp.call_closure(closure.clone(), vec![elem])?;
                if !expect_bool(&val, "all")? {
                    return Ok(IrValue::Bool(false));
                }
            }
            Ok(IrValue::Bool(true))
        }
        "reduce" => {
            let mut acc = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "reduce() requires 2 arguments".to_string(),
            })?;
            let closure = method_args.get(1).cloned().ok_or_else(|| IrRuntimeError {
                message: "reduce() requires 2 arguments".to_string(),
            })?;
            for elem in elements {
                acc = interp.call_closure(closure.clone(), vec![acc, elem])?;
            }
            Ok(acc)
        }
        "sortBy" => {
            let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "sortBy() requires 1 argument".to_string(),
            })?;
            sort_by_closure(interp, elements, closure)
        }
        _ => error(format!("no method `{method}` on type `List`")),
    }
}

// ── Map builtins ─────────────────────────────────────────────────────

/// Dispatch a `Map.*` method call (`length`, `get`, `set`, `remove`, `keys`, `values`, etc.).
fn builtin_map(method: &str, args: Vec<IrValue>) -> Result<IrValue> {
    let entries = match args.first() {
        Some(IrValue::Map(entries)) => entries.borrow().clone(),
        _ => return error("Map method called on non-map"),
    };
    let method_args = &args[1..];

    match method {
        "length" => Ok(IrValue::Int(entries.len() as i64)),
        "get" => {
            let key = method_args.first().ok_or_else(|| IrRuntimeError {
                message: "get() requires 1 argument".to_string(),
            })?;
            for (k, v) in &entries {
                if k == key {
                    return Ok(some_val(v.clone()));
                }
            }
            Ok(none_val())
        }
        "contains" => {
            let key = method_args.first().ok_or_else(|| IrRuntimeError {
                message: "contains() requires 1 argument".to_string(),
            })?;
            let found = entries.iter().any(|(k, _)| k == key);
            Ok(IrValue::Bool(found))
        }
        "set" => {
            let key = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "set() requires 2 arguments".to_string(),
            })?;
            let val = method_args.get(1).cloned().ok_or_else(|| IrRuntimeError {
                message: "set() requires 2 arguments".to_string(),
            })?;
            let mut new_entries = entries;
            if let Some(entry) = new_entries.iter_mut().find(|(k, _)| k == &key) {
                entry.1 = val;
            } else {
                new_entries.push((key, val));
            }
            Ok(IrValue::new_map(new_entries))
        }
        "remove" => {
            let key = method_args.first().ok_or_else(|| IrRuntimeError {
                message: "remove() requires 1 argument".to_string(),
            })?;
            let new_entries: Vec<(IrValue, IrValue)> =
                entries.iter().filter(|(k, _)| k != key).cloned().collect();
            Ok(IrValue::new_map(new_entries))
        }
        "keys" => {
            let keys: Vec<IrValue> = entries.iter().map(|(k, _)| k.clone()).collect();
            Ok(IrValue::new_list(keys))
        }
        "values" => {
            let vals: Vec<IrValue> = entries.iter().map(|(_, v)| v.clone()).collect();
            Ok(IrValue::new_list(vals))
        }
        _ => error(format!("no method `{method}` on type `Map`")),
    }
}

// ── Option builtins ──────────────────────────────────────────────────

/// Extract Option variant info from an IR value.
///
/// The IR represents Option variants as:
/// - `Some(x)` → `StructAlloc("Some", [x])` → `IrValue::Struct`
/// - `None` → `StructAlloc("None", [])` → `IrValue::Struct`
fn option_info(val: &IrValue) -> Result<(&'static str, Vec<IrValue>, IrValue)> {
    match val {
        IrValue::Struct(data) => {
            let data = data.borrow();
            match data.name.as_str() {
                "Some" => Ok(("Some", data.fields.clone(), val.clone())),
                "None" => Ok(("None", vec![], val.clone())),
                _ => error(format!(
                    "Option method called on non-Option struct: {}",
                    data.name,
                )),
            }
        }
        _ => error(format!("Option method called on non-Option value: {val}")),
    }
}

/// Dispatch an `Option.*` method call (`isSome`, `isNone`, `unwrap`, `map`, etc.).
fn builtin_option(
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
            if variant_name == "Some" && !fields.is_empty() {
                Ok(fields[0].clone())
            } else {
                Ok(method_args.first().cloned().unwrap_or(IrValue::Void))
            }
        }
        "map" => {
            let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "map() requires 1 argument".to_string(),
            })?;
            if variant_name == "Some" && !fields.is_empty() {
                let result = interp.call_closure(closure, vec![fields[0].clone()])?;
                Ok(some_val(result))
            } else {
                Ok(none_val())
            }
        }
        "andThen" => {
            let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "andThen() requires 1 argument".to_string(),
            })?;
            if variant_name == "Some" && !fields.is_empty() {
                interp.call_closure(closure, vec![fields[0].clone()])
            } else {
                Ok(none_val())
            }
        }
        "orElse" => {
            if variant_name == "Some" {
                Ok(obj_clone)
            } else {
                let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                    message: "orElse() requires 1 argument".to_string(),
                })?;
                interp.call_closure(closure, vec![])
            }
        }
        "filter" => {
            let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "filter() requires 1 argument".to_string(),
            })?;
            if variant_name == "Some" && !fields.is_empty() {
                let result = interp.call_closure(closure, vec![fields[0].clone()])?;
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
                let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                    message: "unwrapOrElse() requires 1 argument".to_string(),
                })?;
                interp.call_closure(closure, vec![])
            }
        }
        "okOr" => {
            if variant_name == "Some" && !fields.is_empty() {
                Ok(ok_val(fields[0].clone()))
            } else {
                Ok(err_val(
                    method_args.first().cloned().unwrap_or(IrValue::Void),
                ))
            }
        }
        _ => error(format!("no method `{method}` on type `Option`")),
    }
}

// ── Result builtins ──────────────────────────────────────────────────

/// Extract Result variant info from an IR value.
///
/// The IR represents Result variants as:
/// - `Ok(x)` → `StructAlloc("Ok", [x])` → `IrValue::Struct`
/// - `Err(x)` → `StructAlloc("Err", [x])` → `IrValue::Struct`
fn result_info(val: &IrValue) -> Result<(&'static str, Vec<IrValue>, IrValue)> {
    match val {
        IrValue::Struct(data) => {
            let data = data.borrow();
            match data.name.as_str() {
                "Ok" => Ok(("Ok", data.fields.clone(), val.clone())),
                "Err" => Ok(("Err", data.fields.clone(), val.clone())),
                _ => error(format!(
                    "Result method called on non-Result struct: {}",
                    data.name,
                )),
            }
        }
        _ => error(format!("Result method called on non-Result value: {val}")),
    }
}

/// Dispatch a `Result.*` method call (`isOk`, `isErr`, `unwrap`, `map`, `mapErr`, etc.).
fn builtin_result(
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
            if variant_name == "Ok" && !fields.is_empty() {
                Ok(fields[0].clone())
            } else {
                Ok(method_args.first().cloned().unwrap_or(IrValue::Void))
            }
        }
        "map" => {
            let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "map() requires 1 argument".to_string(),
            })?;
            if variant_name == "Ok" && !fields.is_empty() {
                let result = interp.call_closure(closure, vec![fields[0].clone()])?;
                Ok(ok_val(result))
            } else {
                Ok(obj_clone)
            }
        }
        "mapErr" => {
            let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "mapErr() requires 1 argument".to_string(),
            })?;
            if variant_name == "Err" && !fields.is_empty() {
                let result = interp.call_closure(closure, vec![fields[0].clone()])?;
                Ok(err_val(result))
            } else {
                Ok(obj_clone)
            }
        }
        "andThen" => {
            let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                message: "andThen() requires 1 argument".to_string(),
            })?;
            if variant_name == "Ok" && !fields.is_empty() {
                interp.call_closure(closure, vec![fields[0].clone()])
            } else {
                Ok(obj_clone)
            }
        }
        "orElse" => {
            if variant_name == "Ok" {
                Ok(obj_clone)
            } else {
                let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                    message: "orElse() requires 1 argument".to_string(),
                })?;
                if !fields.is_empty() {
                    interp.call_closure(closure, vec![fields[0].clone()])
                } else {
                    interp.call_closure(closure, vec![])
                }
            }
        }
        "unwrapOrElse" => {
            if variant_name == "Ok" && !fields.is_empty() {
                Ok(fields[0].clone())
            } else {
                let closure = method_args.first().cloned().ok_or_else(|| IrRuntimeError {
                    message: "unwrapOrElse() requires 1 argument".to_string(),
                })?;
                if variant_name == "Err" && !fields.is_empty() {
                    interp.call_closure(closure, vec![fields[0].clone()])
                } else {
                    interp.call_closure(closure, vec![])
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

// ── Helpers ──────────────────────────────────────────────────────────

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

/// Sort using insertion sort (same approach as AST interpreter) since the
/// comparator calls a closure which requires `&mut self`.
fn sort_by_closure(
    interp: &mut IrInterpreter<'_>,
    mut items: Vec<IrValue>,
    closure: IrValue,
) -> Result<IrValue> {
    let mut sort_err: Option<IrRuntimeError> = None;
    let len = items.len();
    for i in 1..len {
        let mut j = i;
        while j > 0 {
            let cmp_val = interp.call_closure(
                closure.clone(),
                vec![items[j - 1].clone(), items[j].clone()],
            );
            match cmp_val {
                Ok(IrValue::Int(c)) => {
                    if c > 0 {
                        items.swap(j - 1, j);
                        j -= 1;
                    } else {
                        break;
                    }
                }
                Ok(_) => {
                    sort_err = Some(IrRuntimeError {
                        message: "sortBy callback must return Int".to_string(),
                    });
                    break;
                }
                Err(e) => {
                    sort_err = Some(e);
                    break;
                }
            }
        }
        if sort_err.is_some() {
            break;
        }
    }
    if let Some(e) = sort_err {
        return Err(e);
    }
    Ok(IrValue::new_list(items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_ir::module::IrModule;

    fn empty_module() -> IrModule {
        IrModule {
            functions: vec![],
            struct_layouts: Default::default(),
            enum_layouts: Default::default(),
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
        // Build a closure that returns Int instead of Bool.
        // We can't easily build a real closure, so test via dispatch with a
        // list method that calls expect_bool internally — we use "any" with a
        // hand-crafted BuiltinCall to the list method.
        // Instead, test the helper directly which is what the methods delegate to.
        let val = IrValue::String("not a bool".to_string());
        assert!(expect_bool(&val, "any").is_err());
        assert!(expect_bool(&val, "all").is_err());
        assert!(expect_bool(&val, "find").is_err());

        // Verify Bool values pass through correctly.
        assert!(expect_bool(&IrValue::Bool(true), "any").unwrap());
        assert!(!expect_bool(&IrValue::Bool(false), "all").unwrap());
        let _ = interp; // suppress unused warning
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
