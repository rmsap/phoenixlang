//! String, List, and Map builtin method dispatch for the IR interpreter.

use crate::error::{IrRuntimeError, Result, error};
use crate::interpreter::IrInterpreter;
use crate::value::{IrValue, map_key_eq, none_val, some_val};
use phoenix_common::map_key::{CanonicalMapKey, dedup_last_wins};

use super::{expect_bool, expect_int_arg, expect_one_arg, expect_string_arg, expect_two_args};

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
                if map_key_eq(k, key) {
                    return Ok(some_val(v.clone()));
                }
            }
            Ok(none_val())
        }
        "contains" => {
            let key = method_args.first().ok_or_else(|| IrRuntimeError {
                message: "contains() requires 1 argument".to_string(),
            })?;
            let found = entries.iter().any(|(k, _)| map_key_eq(k, key));
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
            if let Some(entry) = new_entries.iter_mut().find(|(k, _)| map_key_eq(k, &key)) {
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
            let new_entries: Vec<(IrValue, IrValue)> = entries
                .iter()
                .filter(|(k, _)| !map_key_eq(k, key))
                .cloned()
                .collect();
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

/// Dispatch a `ListBuilder.*` builtin (`alloc`, `push`, `freeze`).
///
/// Transient-mutable accumulators. The IR lowering
/// emits them as `BuiltinCall("ListBuilder.alloc" | ".push" | ".freeze", …)`.
/// Semantics mirror the native `list_builder_methods.rs` byte-for-byte:
/// `alloc` makes a fresh empty builder, `push` appends in place and
/// returns nothing, `freeze` produces a fresh independent `List<T>` in
/// push order and marks the builder frozen. Use-after-freeze (`push` or
/// a second `freeze`) is a runtime error, matching native's
/// `assert_unfrozen` and the wasm-gc frozen trap.
pub(super) fn builtin_list_builder(method: &str, args: Vec<IrValue>) -> Result<IrValue> {
    match method {
        "alloc" => Ok(IrValue::new_list_builder()),
        "push" => {
            let buf = match args.first() {
                Some(IrValue::ListBuilder(buf)) => buf.clone(),
                _ => return error("ListBuilder.push called on non-builder"),
            };
            let val = expect_one_arg(&args[1..], "push")?;
            let mut state = buf.borrow_mut();
            if state.frozen {
                return already_frozen("ListBuilder", "push");
            }
            state.items.push(val);
            Ok(IrValue::Void)
        }
        "freeze" => {
            let buf = match args.first() {
                Some(IrValue::ListBuilder(buf)) => buf.clone(),
                _ => return error("ListBuilder.freeze called on non-builder"),
            };
            let mut state = buf.borrow_mut();
            if state.frozen {
                return already_frozen("ListBuilder", "freeze");
            }
            state.frozen = true;
            // Clone the buffer so the frozen list is independent of the
            // builder handle.
            Ok(IrValue::new_list(state.items.clone()))
        }
        _ => error(format!("no method `{method}` on type `ListBuilder`")),
    }
}

/// Dispatch a `MapBuilder.*` builtin (`alloc`, `set`, `freeze`).
///
/// Transient-mutable accumulator. `set` appends a `(k, v)`
/// pair verbatim (no dedup, no result); `freeze` produces a fresh
/// independent `Map<K, V>` applying **last-wins** dedup while keeping
/// each key's **first-insertion** position — matching
/// `phx_map_builder_freeze` → `phx_map_from_pairs` and the immutable
/// `Map.set` / map-literal dedup elsewhere in this crate — then marks
/// the builder frozen. Use-after-freeze is a runtime error.
pub(super) fn builtin_map_builder(method: &str, args: Vec<IrValue>) -> Result<IrValue> {
    match method {
        "alloc" => Ok(IrValue::new_map_builder()),
        "set" => {
            let buf = match args.first() {
                Some(IrValue::MapBuilder(buf)) => buf.clone(),
                _ => return error("MapBuilder.set called on non-builder"),
            };
            let (key, val) = expect_two_args(&args[1..], "set")?;
            let mut state = buf.borrow_mut();
            if state.frozen {
                return already_frozen("MapBuilder", "set");
            }
            // Append verbatim — dedup is deferred to `freeze`, mirroring
            // the native builder's append-only buffer.
            state.pairs.push((key, val));
            Ok(IrValue::Void)
        }
        "freeze" => {
            let buf = match args.first() {
                Some(IrValue::MapBuilder(buf)) => buf.clone(),
                _ => return error("MapBuilder.freeze called on non-builder"),
            };
            let mut state = buf.borrow_mut();
            if state.frozen {
                return already_frozen("MapBuilder", "freeze");
            }
            state.frozen = true;
            // Last-wins / first-insertion-position dedup via the shared
            // `dedup_last_wins` helper (O(n), byte-wise float keys).
            let out = dedup_last_wins(state.pairs.iter().cloned(), canonical_key);
            Ok(IrValue::new_map(out))
        }
        _ => error(format!("no method `{method}` on type `MapBuilder`")),
    }
}

/// Projects a key [`IrValue`] to its hashable [`CanonicalMapKey`], whose
/// `Hash`/`Eq` agree with [`map_key_eq`] (floats compared **byte-wise**:
/// `-0.0 != 0.0`, equal-bit `NaN`s collide). Phoenix map keys are always
/// scalar or string (sema rejects non-hashable keys); the `_` arm is a
/// defensive fallthrough that renders to a stable string.
fn canonical_key(v: &IrValue) -> CanonicalMapKey {
    match v {
        IrValue::Int(n) => CanonicalMapKey::Int(*n),
        IrValue::Float(f) => CanonicalMapKey::FloatBits(f.to_bits()),
        IrValue::Bool(b) => CanonicalMapKey::Bool(*b),
        IrValue::String(s) => CanonicalMapKey::String(s.clone()),
        other => CanonicalMapKey::Other(other.to_string()),
    }
}

/// Error returned when a `ListBuilder`/`MapBuilder` op runs on a builder
/// that `.freeze()` already consumed. Mirrors native's `assert_unfrozen`
/// message so use-after-freeze is rejected identically across backends.
fn already_frozen(ty: &str, method: &str) -> Result<IrValue> {
    error(format!(
        "{ty}.{method}: builder was already frozen (Phase 2.7 decision F: \
         runtime-checked use-after-freeze; static check is a future linearity story)"
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn as_int(v: &IrValue) -> i64 {
        match v {
            IrValue::Int(n) => *n,
            other => panic!("expected Int, got {other:?}"),
        }
    }

    /// `push` after `freeze` is a runtime error — use-after-freeze must be
    /// rejected here as it is on native (abort) and wasm-gc (trap), or the
    /// IR interpreter would silently diverge from the compiled backends.
    #[test]
    fn list_builder_push_after_freeze_errors() {
        let b = builtin_list_builder("alloc", vec![]).unwrap();
        builtin_list_builder("push", vec![b.clone(), IrValue::Int(1)]).unwrap();
        builtin_list_builder("freeze", vec![b.clone()]).unwrap();
        let err = builtin_list_builder("push", vec![b, IrValue::Int(2)]).unwrap_err();
        assert!(err.message.contains("frozen"), "got: {}", err.message);
    }

    /// Happy path: `push` accumulates on the shared buffer and `freeze`
    /// snapshots it into a `List` in push order. Localizes a regression to
    /// this crate (the `builders.phx` matrix fixture covers it end-to-end,
    /// but only when wasmtime/native are provisioned).
    #[test]
    fn list_builder_push_freeze_in_order() {
        let b = builtin_list_builder("alloc", vec![]).unwrap();
        for n in [3, 1, 2] {
            builtin_list_builder("push", vec![b.clone(), IrValue::Int(n)]).unwrap();
        }
        let frozen = builtin_list_builder("freeze", vec![b]).unwrap();
        let items = match frozen {
            IrValue::List(items) => items,
            other => panic!("expected List, got {other:?}"),
        };
        let got: Vec<i64> = items.borrow().iter().map(as_int).collect();
        assert_eq!(got, vec![3, 1, 2]);
    }

    /// A second `freeze` on a `MapBuilder` is a runtime error.
    #[test]
    fn map_builder_double_freeze_errors() {
        let b = builtin_map_builder("alloc", vec![]).unwrap();
        builtin_map_builder("set", vec![b.clone(), IrValue::Int(1), IrValue::Int(10)]).unwrap();
        builtin_map_builder("freeze", vec![b.clone()]).unwrap();
        let err = builtin_map_builder("freeze", vec![b]).unwrap_err();
        assert!(err.message.contains("frozen"), "got: {}", err.message);
    }

    /// `freeze` dedups last-wins while keeping each key's first-insertion
    /// position — key 3 keeps slot 0 but takes the later value 99.
    #[test]
    fn map_builder_freeze_dedups_last_wins_first_position() {
        let b = builtin_map_builder("alloc", vec![]).unwrap();
        for (k, v) in [(3, 1), (1, 2), (3, 99), (2, 5)] {
            builtin_map_builder("set", vec![b.clone(), IrValue::Int(k), IrValue::Int(v)]).unwrap();
        }
        let frozen = builtin_map_builder("freeze", vec![b]).unwrap();
        let entries = match frozen {
            IrValue::Map(entries) => entries,
            other => panic!("expected Map, got {other:?}"),
        };
        let got: Vec<(i64, i64)> = entries
            .borrow()
            .iter()
            .map(|(k, v)| (as_int(k), as_int(v)))
            .collect();
        assert_eq!(got, vec![(3, 99), (1, 2), (2, 5)]);
    }

    /// `freeze`'s dedup is **byte-wise** on float keys (§Phase 2.4 K.9):
    /// `-0.0` and `0.0` stay distinct entries, while equal-bit `NaN`s
    /// collapse (last value wins). These are the ±0.0 / NaN edges
    /// `builders.phx` can't express in source, driven end-to-end through
    /// an actual builder `freeze` rather than only the `map_key` helper.
    #[test]
    fn map_builder_freeze_float_keys_are_byte_wise() {
        let b = builtin_map_builder("alloc", vec![]).unwrap();
        for (k, v) in [(-0.0_f64, 1), (0.0, 2), (f64::NAN, 3), (f64::NAN, 4)] {
            builtin_map_builder("set", vec![b.clone(), IrValue::Float(k), IrValue::Int(v)])
                .unwrap();
        }
        let frozen = builtin_map_builder("freeze", vec![b]).unwrap();
        let entries = match frozen {
            IrValue::Map(entries) => entries,
            other => panic!("expected Map, got {other:?}"),
        };
        let got: Vec<(u64, i64)> = entries
            .borrow()
            .iter()
            .map(|(k, v)| match k {
                IrValue::Float(f) => (f.to_bits(), as_int(v)),
                other => panic!("expected Float key, got {other:?}"),
            })
            .collect();
        // ±0.0 survive as two entries (first-insertion order), the two
        // NaNs collapse to one with the last value.
        assert_eq!(
            got,
            vec![
                ((-0.0_f64).to_bits(), 1),
                (0.0_f64.to_bits(), 2),
                (f64::NAN.to_bits(), 4),
            ]
        );
    }
}
