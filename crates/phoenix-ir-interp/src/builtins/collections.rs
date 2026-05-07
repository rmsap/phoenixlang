//! String, List, and Map builtin method dispatch for the IR interpreter.

use crate::error::{IrRuntimeError, Result, error};
use crate::interpreter::IrInterpreter;
use crate::value::{IrValue, none_val, some_val};

use super::{expect_bool, expect_int_arg, expect_one_arg, expect_string_arg};

/// Dispatch a `String.*` method call (`length`, `contains`, `split`, etc.).
pub(super) fn builtin_string(method: &str, args: Vec<IrValue>) -> Result<IrValue> {
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
            let chars: Vec<char> = receiver.chars().collect();
            let char_len = chars.len();
            // Clamp indices to valid range, matching runtime semantics.
            let start_u = (start.max(0) as usize).min(char_len);
            let end_u = (end.max(0) as usize).min(char_len);
            let end_u = end_u.max(start_u);
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

/// Dispatch a `List.*` method call (`length`, `get`, `push`, `map`, `filter`, etc.).
pub(super) fn builtin_list(
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
            let val = expect_one_arg(method_args, "push")?;
            let mut new_list = elements;
            new_list.push(val);
            Ok(IrValue::new_list(new_list))
        }
        "map" => {
            let closure = expect_one_arg(method_args, "map")?;
            let mut result = Vec::with_capacity(elements.len());
            for elem in elements {
                result.push(interp.call_closure(&closure, vec![elem])?);
            }
            Ok(IrValue::new_list(result))
        }
        "flatMap" => {
            let closure = expect_one_arg(method_args, "flatMap")?;
            let mut result = Vec::new();
            for elem in elements {
                let val = interp.call_closure(&closure, vec![elem])?;
                if let IrValue::List(inner) = val {
                    result.extend(inner.borrow().clone());
                } else {
                    return error("flatMap callback must return a List");
                }
            }
            Ok(IrValue::new_list(result))
        }
        "filter" => {
            let closure = expect_one_arg(method_args, "filter")?;
            let mut result = Vec::new();
            for elem in elements {
                let val = interp.call_closure(&closure, vec![elem.clone()])?;
                if expect_bool(&val, "filter")? {
                    result.push(elem);
                }
            }
            Ok(IrValue::new_list(result))
        }
        "find" => {
            let closure = expect_one_arg(method_args, "find")?;
            for elem in elements {
                let val = interp.call_closure(&closure, vec![elem.clone()])?;
                if expect_bool(&val, "find")? {
                    return Ok(some_val(elem));
                }
            }
            Ok(none_val())
        }
        "any" => {
            let closure = expect_one_arg(method_args, "any")?;
            for elem in elements {
                let val = interp.call_closure(&closure, vec![elem])?;
                if expect_bool(&val, "any")? {
                    return Ok(IrValue::Bool(true));
                }
            }
            Ok(IrValue::Bool(false))
        }
        "all" => {
            let closure = expect_one_arg(method_args, "all")?;
            for elem in elements {
                let val = interp.call_closure(&closure, vec![elem])?;
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
                acc = interp.call_closure(&closure, vec![acc, elem])?;
            }
            Ok(acc)
        }
        "sortBy" => {
            let closure = expect_one_arg(method_args, "sortBy")?;
            sort_by_closure(interp, elements, closure)
        }
        _ => error(format!("no method `{method}` on type `List`")),
    }
}

/// Dispatch a `Map.*` method call (`length`, `get`, `set`, `remove`, `keys`, `values`, etc.).
pub(super) fn builtin_map(method: &str, args: Vec<IrValue>) -> Result<IrValue> {
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

/// Sort via bottom-up iterative merge sort using a closure comparator.
/// The algorithm itself lives in
/// [`phoenix_common::algorithms::merge_sort_by`]; this function
/// supplies the comparator that calls back into the IR interpreter's
/// closure dispatch. Stable. **O(n log n)** worst case.
fn sort_by_closure(
    interp: &mut IrInterpreter<'_>,
    items: Vec<IrValue>,
    closure: IrValue,
) -> Result<IrValue> {
    let sorted = phoenix_common::algorithms::merge_sort_by(items, |a, b| {
        match interp.call_closure(&closure, vec![a.clone(), b.clone()])? {
            IrValue::Int(c) => Ok(c),
            _ => Err(crate::error::IrRuntimeError {
                message: "sortBy callback must return Int".to_string(),
            }),
        }
    })?;
    Ok(IrValue::new_list(sorted))
}
