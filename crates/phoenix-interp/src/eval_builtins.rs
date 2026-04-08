use crate::interpreter::{
    Interpreter, Result, RuntimeError, err_val, error, none_val, ok_val, some_val,
};
use crate::value::Value;
use phoenix_parser::ast::MethodCallExpr;

impl Interpreter {
    /// Evaluates a built-in String method call.
    pub(crate) fn eval_string_method(&mut self, s: String, mc: &MethodCallExpr) -> Result<Value> {
        match mc.method.as_str() {
            "length" => Ok(Value::Int(s.chars().count() as i64)),
            "contains" => {
                let args = self.expect_args("contains", mc, 1)?;
                if let Value::String(ref sub_s) = args[0] {
                    return Ok(Value::Bool(s.contains(sub_s.as_str())));
                }
                error("contains() argument must be String")
            }
            "startsWith" => {
                let args = self.expect_args("startsWith", mc, 1)?;
                if let Value::String(ref pre_s) = args[0] {
                    return Ok(Value::Bool(s.starts_with(pre_s.as_str())));
                }
                error("startsWith() argument must be String")
            }
            "endsWith" => {
                let args = self.expect_args("endsWith", mc, 1)?;
                if let Value::String(ref suf_s) = args[0] {
                    return Ok(Value::Bool(s.ends_with(suf_s.as_str())));
                }
                error("endsWith() argument must be String")
            }
            "trim" => Ok(Value::String(s.trim().to_string())),
            "toLowerCase" => Ok(Value::String(s.to_lowercase())),
            "toUpperCase" => Ok(Value::String(s.to_uppercase())),
            "split" => {
                let args = self.expect_args("split", mc, 1)?;
                if let Value::String(ref sep_s) = args[0] {
                    let parts: Vec<Value> = s
                        .split(sep_s.as_str())
                        .map(|p| Value::String(p.to_string()))
                        .collect();
                    return Ok(Value::List(parts));
                }
                error("split() argument must be String")
            }
            "replace" => {
                let args = self.expect_args("replace", mc, 2)?;
                if let (Value::String(old_s), Value::String(new_s)) = (&args[0], &args[1]) {
                    return Ok(Value::String(s.replace(old_s.as_str(), new_s.as_str())));
                }
                error("replace() arguments must be String")
            }
            "substring" => {
                let args = self.expect_args("substring", mc, 2)?;
                if let (Value::Int(start_i), Value::Int(end_i)) = (&args[0], &args[1]) {
                    if *start_i < 0 || *end_i < 0 {
                        return error(format!(
                            "substring() indices must be non-negative, got ({}, {})",
                            start_i, end_i
                        ));
                    }
                    let start_u = *start_i as usize;
                    let end_u = *end_i as usize;
                    let chars: Vec<char> = s.chars().collect();
                    if start_u > chars.len() || end_u > chars.len() || start_u > end_u {
                        return error(format!(
                            "substring({}, {}) out of bounds (length {})",
                            start_i,
                            end_i,
                            chars.len()
                        ));
                    }
                    return Ok(Value::String(chars[start_u..end_u].iter().collect()));
                }
                error("substring() arguments must be Int")
            }
            "indexOf" => {
                let args = self.expect_args("indexOf", mc, 1)?;
                if let Value::String(ref sub_s) = args[0] {
                    // Convert byte offset to character index for consistency
                    // with length() and substring() which use character indices.
                    let char_index = s
                        .find(sub_s.as_str())
                        .map(|byte_offset| s[..byte_offset].chars().count() as i64)
                        .unwrap_or(-1);
                    return Ok(Value::Int(char_index));
                }
                error("indexOf() argument must be String")
            }
            _ => error(format!("no method `{}` on type `String`", mc.method)),
        }
    }

    /// Evaluates a built-in Map method call.
    pub(crate) fn eval_map_method(
        &mut self,
        entries: Vec<(Value, Value)>,
        mc: &MethodCallExpr,
    ) -> Result<Value> {
        match mc.method.as_str() {
            "length" => Ok(Value::Int(entries.len() as i64)),
            "get" => {
                let args = self.expect_args("get", mc, 1)?;
                for (k, v) in &entries {
                    if k == &args[0] {
                        return Ok(some_val(v.clone()));
                    }
                }
                Ok(none_val())
            }
            "contains" => {
                let args = self.expect_args("contains", mc, 1)?;
                let found = entries.iter().any(|(k, _)| k == &args[0]);
                Ok(Value::Bool(found))
            }
            "set" => {
                let args = self.expect_args("set", mc, 2)?;
                let mut args = args.into_iter();
                let key = args.next().unwrap();
                let val = args.next().unwrap();
                let mut new_entries = entries;
                if let Some(entry) = new_entries.iter_mut().find(|(k, _)| k == &key) {
                    entry.1 = val;
                } else {
                    new_entries.push((key, val));
                }
                Ok(Value::Map(new_entries))
            }
            "remove" => {
                let args = self.expect_args("remove", mc, 1)?;
                let new_entries: Vec<(Value, Value)> = entries
                    .iter()
                    .filter(|(k, _)| k != &args[0])
                    .cloned()
                    .collect();
                Ok(Value::Map(new_entries))
            }
            "keys" => {
                let keys: Vec<Value> = entries.iter().map(|(k, _)| k.clone()).collect();
                Ok(Value::List(keys))
            }
            "values" => {
                let vals: Vec<Value> = entries.iter().map(|(_, v)| v.clone()).collect();
                Ok(Value::List(vals))
            }
            _ => error(format!("no method `{}` on type `Map`", mc.method)),
        }
    }

    /// Evaluates a built-in List method call.
    pub(crate) fn eval_list_method(
        &mut self,
        elements: Vec<Value>,
        mc: &MethodCallExpr,
    ) -> Result<Value> {
        match mc.method.as_str() {
            "length" => Ok(Value::Int(elements.len() as i64)),
            "get" => {
                let args = self.expect_args("get", mc, 1)?;
                if let Value::Int(idx) = args[0] {
                    if idx < 0 || idx as usize >= elements.len() {
                        return error(format!(
                            "list index {} out of bounds (length {})",
                            idx,
                            elements.len()
                        ));
                    }
                    Ok(elements[idx as usize].clone())
                } else {
                    error("list index must be Int")
                }
            }
            "push" => {
                let args = self.expect_args("push", mc, 1)?;
                let new_val = args.into_iter().next().unwrap();
                let mut new_list = elements;
                new_list.push(new_val);
                Ok(Value::List(new_list))
            }
            // Tier 1 — No closures
            "first" => {
                if elements.is_empty() {
                    return Ok(none_val());
                }
                Ok(some_val(elements[0].clone()))
            }
            "last" => {
                if elements.is_empty() {
                    return Ok(none_val());
                }
                Ok(some_val(elements[elements.len() - 1].clone()))
            }
            "contains" => {
                let args = self.expect_args("contains", mc, 1)?;
                let target = &args[0];
                Ok(Value::Bool(elements.contains(target)))
            }
            "take" => {
                let args = self.expect_args("take", mc, 1)?;
                if let Value::Int(n) = args[0] {
                    if n < 0 {
                        return error(format!("take() argument must be non-negative, got {}", n));
                    }
                    let taken: Vec<Value> = elements.iter().take(n as usize).cloned().collect();
                    Ok(Value::List(taken))
                } else {
                    error("take() argument must be Int")
                }
            }
            "drop" => {
                let args = self.expect_args("drop", mc, 1)?;
                if let Value::Int(n) = args[0] {
                    if n < 0 {
                        return error(format!("drop() argument must be non-negative, got {}", n));
                    }
                    let dropped: Vec<Value> = elements.iter().skip(n as usize).cloned().collect();
                    Ok(Value::List(dropped))
                } else {
                    error("drop() argument must be Int")
                }
            }
            // Tier 2 — Closure-accepting
            "map" => {
                let closure = self.expect_args("map", mc, 1)?.into_iter().next().unwrap();
                let mut result = Vec::with_capacity(elements.len());
                for elem in elements {
                    result.push(self.call_closure(closure.clone(), vec![elem])?);
                }
                Ok(Value::List(result))
            }
            "flatMap" => {
                let closure = self
                    .expect_args("flatMap", mc, 1)?
                    .into_iter()
                    .next()
                    .unwrap();
                let mut result = Vec::new();
                for elem in elements {
                    let val = self.call_closure(closure.clone(), vec![elem])?;
                    if let Value::List(inner) = val {
                        result.extend(inner);
                    } else {
                        return error("flatMap callback must return a List");
                    }
                }
                Ok(Value::List(result))
            }
            "filter" => {
                let closure = self
                    .expect_args("filter", mc, 1)?
                    .into_iter()
                    .next()
                    .unwrap();
                let mut result = Vec::new();
                for elem in elements {
                    let val = self.call_closure(closure.clone(), vec![elem.clone()])?;
                    if let Value::Bool(true) = val {
                        result.push(elem);
                    }
                }
                Ok(Value::List(result))
            }
            "find" => {
                let closure = self.expect_args("find", mc, 1)?.into_iter().next().unwrap();
                for elem in elements {
                    let val = self.call_closure(closure.clone(), vec![elem.clone()])?;
                    if let Value::Bool(true) = val {
                        return Ok(some_val(elem));
                    }
                }
                Ok(none_val())
            }
            "any" => {
                let closure = self.expect_args("any", mc, 1)?.into_iter().next().unwrap();
                for elem in elements {
                    if let Value::Bool(true) = self.call_closure(closure.clone(), vec![elem])? {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }
            "all" => {
                let closure = self.expect_args("all", mc, 1)?.into_iter().next().unwrap();
                for elem in elements {
                    if let Value::Bool(false) = self.call_closure(closure.clone(), vec![elem])? {
                        return Ok(Value::Bool(false));
                    }
                }
                Ok(Value::Bool(true))
            }
            // Tier 3 — Accumulator
            "reduce" => {
                let args = self.expect_args("reduce", mc, 2)?;
                let mut args = args.into_iter();
                let mut acc = args.next().unwrap();
                let closure = args.next().unwrap();
                for elem in elements {
                    acc = self.call_closure(closure.clone(), vec![acc, elem])?;
                }
                Ok(acc)
            }
            "sortBy" => {
                let closure = self
                    .expect_args("sortBy", mc, 1)?
                    .into_iter()
                    .next()
                    .unwrap();
                self.sort_by_closure(elements, closure)
            }
            _ => error(format!("no method `{}` on type `List`", mc.method)),
        }
    }

    /// Evaluates a built-in Option method call.
    ///
    /// Returns `Ok(Some(value))` if the method was handled, or `Ok(None)` if
    /// the method was not recognized (allowing fall-through to user-defined methods).
    pub(crate) fn eval_option_method(
        &mut self,
        variant: &str,
        fields: &[Value],
        obj: &Value,
        mc: &MethodCallExpr,
    ) -> Result<Option<Value>> {
        match mc.method.as_str() {
            "isSome" => Ok(Some(Value::Bool(variant == "Some"))),
            "isNone" => Ok(Some(Value::Bool(variant == "None"))),
            "unwrap" => {
                if variant == "Some" && !fields.is_empty() {
                    return Ok(Some(fields[0].clone()));
                }
                error("called unwrap() on None")
            }
            "unwrapOr" => {
                let args = self.expect_args("unwrapOr", mc, 1)?;
                if variant == "Some" && !fields.is_empty() {
                    return Ok(Some(fields[0].clone()));
                }
                Ok(Some(args.into_iter().next().unwrap()))
            }
            "map" => {
                let args = self.expect_args("map", mc, 1)?;
                let closure = args.into_iter().next().unwrap();
                if variant == "Some" && !fields.is_empty() {
                    let result = self.call_closure(closure, vec![fields[0].clone()])?;
                    return Ok(Some(some_val(result)));
                }
                Ok(Some(none_val()))
            }
            "andThen" => {
                let args = self.expect_args("andThen", mc, 1)?;
                let closure = args.into_iter().next().unwrap();
                if variant == "Some" && !fields.is_empty() {
                    return Ok(Some(self.call_closure(closure, vec![fields[0].clone()])?));
                }
                Ok(Some(none_val()))
            }
            "orElse" => {
                if variant == "Some" {
                    return Ok(Some(obj.clone()));
                }
                let args = self.expect_args("orElse", mc, 1)?;
                let closure = args.into_iter().next().unwrap();
                Ok(Some(self.call_closure(closure, vec![])?))
            }
            "filter" => {
                let args = self.expect_args("filter", mc, 1)?;
                let closure = args.into_iter().next().unwrap();
                if variant == "Some" && !fields.is_empty() {
                    let result = self.call_closure(closure, vec![fields[0].clone()])?;
                    if result == Value::Bool(true) {
                        return Ok(Some(obj.clone()));
                    }
                }
                Ok(Some(none_val()))
            }
            "unwrapOrElse" => {
                if variant == "Some" && !fields.is_empty() {
                    return Ok(Some(fields[0].clone()));
                }
                let args = self.expect_args("unwrapOrElse", mc, 1)?;
                let closure = args.into_iter().next().unwrap();
                Ok(Some(self.call_closure(closure, vec![])?))
            }
            "okOr" => {
                let args = self.expect_args("okOr", mc, 1)?;
                if variant == "Some" && !fields.is_empty() {
                    return Ok(Some(ok_val(fields[0].clone())));
                }
                Ok(Some(err_val(args.into_iter().next().unwrap())))
            }
            _ => Ok(None),
        }
    }

    /// Evaluates a built-in Result method call.
    ///
    /// Returns `Ok(Some(value))` if the method was handled, or `Ok(None)` if
    /// the method was not recognized (allowing fall-through to user-defined methods).
    pub(crate) fn eval_result_method(
        &mut self,
        variant: &str,
        fields: &[Value],
        obj: &Value,
        mc: &MethodCallExpr,
    ) -> Result<Option<Value>> {
        match mc.method.as_str() {
            "isOk" => Ok(Some(Value::Bool(variant == "Ok"))),
            "isErr" => Ok(Some(Value::Bool(variant == "Err"))),
            "unwrap" => {
                if variant == "Ok" && !fields.is_empty() {
                    return Ok(Some(fields[0].clone()));
                }
                error("called unwrap() on Err")
            }
            "unwrapOr" => {
                let args = self.expect_args("unwrapOr", mc, 1)?;
                if variant == "Ok" && !fields.is_empty() {
                    return Ok(Some(fields[0].clone()));
                }
                Ok(Some(args.into_iter().next().unwrap()))
            }
            "map" => {
                let args = self.expect_args("map", mc, 1)?;
                let closure = args.into_iter().next().unwrap();
                if variant == "Ok" && !fields.is_empty() {
                    let result = self.call_closure(closure, vec![fields[0].clone()])?;
                    return Ok(Some(ok_val(result)));
                }
                Ok(Some(Value::EnumVariant(
                    "Result".to_string(),
                    "Err".to_string(),
                    fields.to_vec(),
                )))
            }
            "mapErr" => {
                let args = self.expect_args("mapErr", mc, 1)?;
                let closure = args.into_iter().next().unwrap();
                if variant == "Err" && !fields.is_empty() {
                    let result = self.call_closure(closure, vec![fields[0].clone()])?;
                    return Ok(Some(err_val(result)));
                }
                Ok(Some(Value::EnumVariant(
                    "Result".to_string(),
                    "Ok".to_string(),
                    fields.to_vec(),
                )))
            }
            "andThen" => {
                let args = self.expect_args("andThen", mc, 1)?;
                let closure = args.into_iter().next().unwrap();
                if variant == "Ok" && !fields.is_empty() {
                    return Ok(Some(self.call_closure(closure, vec![fields[0].clone()])?));
                }
                Ok(Some(Value::EnumVariant(
                    "Result".to_string(),
                    "Err".to_string(),
                    fields.to_vec(),
                )))
            }
            "orElse" => {
                if variant == "Ok" {
                    return Ok(Some(obj.clone()));
                }
                let args = self.expect_args("orElse", mc, 1)?;
                let closure = args.into_iter().next().unwrap();
                if !fields.is_empty() {
                    return Ok(Some(self.call_closure(closure, vec![fields[0].clone()])?));
                }
                Ok(Some(self.call_closure(closure, vec![])?))
            }
            "unwrapOrElse" => {
                if variant == "Ok" && !fields.is_empty() {
                    return Ok(Some(fields[0].clone()));
                }
                let args = self.expect_args("unwrapOrElse", mc, 1)?;
                let closure = args.into_iter().next().unwrap();
                if variant == "Err" && !fields.is_empty() {
                    return Ok(Some(self.call_closure(closure, vec![fields[0].clone()])?));
                }
                Ok(Some(self.call_closure(closure, vec![])?))
            }
            "ok" => {
                if variant == "Ok" && !fields.is_empty() {
                    return Ok(Some(some_val(fields[0].clone())));
                }
                Ok(Some(none_val()))
            }
            "err" => {
                if variant == "Err" && !fields.is_empty() {
                    return Ok(Some(some_val(fields[0].clone())));
                }
                Ok(Some(none_val()))
            }
            _ => Ok(None),
        }
    }

    /// Sorts a list using a closure-based comparator via insertion sort.
    ///
    /// Insertion sort is used instead of `slice::sort_by` because the
    /// comparator calls a Phoenix closure, which requires `&mut self`.
    fn sort_by_closure(&mut self, mut items: Vec<Value>, closure: Value) -> Result<Value> {
        let mut sort_err: Option<RuntimeError> = None;
        let len = items.len();
        for i in 1..len {
            let mut j = i;
            while j > 0 {
                let cmp_val = self.call_closure(
                    closure.clone(),
                    vec![items[j - 1].clone(), items[j].clone()],
                );
                match cmp_val {
                    Ok(Value::Int(c)) => {
                        if c > 0 {
                            items.swap(j - 1, j);
                            j -= 1;
                        } else {
                            break;
                        }
                    }
                    Ok(_) => {
                        sort_err = Some(RuntimeError {
                            message: "sortBy callback must return Int".to_string(),
                            try_return_value: None,
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
        Ok(Value::List(items))
    }
}
