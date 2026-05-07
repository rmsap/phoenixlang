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
                let key = args.next().expect("expect_args validated 2 args");
                let val = args.next().expect("expect_args validated 2 args");
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
                let new_val = args
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
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
                let closure = self
                    .expect_args("map", mc, 1)?
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
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
                    .expect("expect_args validated arg count");
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
                    .expect("expect_args validated arg count");
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
                let closure = self
                    .expect_args("find", mc, 1)?
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
                for elem in elements {
                    let val = self.call_closure(closure.clone(), vec![elem.clone()])?;
                    if let Value::Bool(true) = val {
                        return Ok(some_val(elem));
                    }
                }
                Ok(none_val())
            }
            "any" => {
                let closure = self
                    .expect_args("any", mc, 1)?
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
                for elem in elements {
                    if let Value::Bool(true) = self.call_closure(closure.clone(), vec![elem])? {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }
            "all" => {
                let closure = self
                    .expect_args("all", mc, 1)?
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
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
                let mut acc = args.next().expect("expect_args validated arg count");
                let closure = args.next().expect("expect_args validated arg count");
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
                    .expect("expect_args validated arg count");
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
                Ok(Some(
                    args.into_iter()
                        .next()
                        .expect("expect_args validated arg count"),
                ))
            }
            "map" => {
                let args = self.expect_args("map", mc, 1)?;
                let closure = args
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
                if variant == "Some" && !fields.is_empty() {
                    let result = self.call_closure(closure, vec![fields[0].clone()])?;
                    return Ok(Some(some_val(result)));
                }
                Ok(Some(none_val()))
            }
            "andThen" => {
                let args = self.expect_args("andThen", mc, 1)?;
                let closure = args
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
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
                let closure = args
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
                Ok(Some(self.call_closure(closure, vec![])?))
            }
            "filter" => {
                let args = self.expect_args("filter", mc, 1)?;
                let closure = args
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
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
                let closure = args
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
                Ok(Some(self.call_closure(closure, vec![])?))
            }
            "okOr" => {
                let args = self.expect_args("okOr", mc, 1)?;
                if variant == "Some" && !fields.is_empty() {
                    return Ok(Some(ok_val(fields[0].clone())));
                }
                Ok(Some(err_val(
                    args.into_iter()
                        .next()
                        .expect("expect_args validated arg count"),
                )))
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
                Ok(Some(
                    args.into_iter()
                        .next()
                        .expect("expect_args validated arg count"),
                ))
            }
            "map" => {
                let args = self.expect_args("map", mc, 1)?;
                let closure = args
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
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
                let closure = args
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
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
                let closure = args
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
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
                let closure = args
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
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
                let closure = args
                    .into_iter()
                    .next()
                    .expect("expect_args validated arg count");
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

    /// Sorts a list using a closure-based comparator via bottom-up
    /// iterative merge sort. The algorithm itself lives in
    /// [`phoenix_common::algorithms::merge_sort_by`]; this method
    /// supplies the comparator that calls back into the closure
    /// dispatch path. Stable. **O(n log n)** worst case.
    fn sort_by_closure(&mut self, items: Vec<Value>, closure: Value) -> Result<Value> {
        let sorted = phoenix_common::algorithms::merge_sort_by(items, |a, b| {
            match self.call_closure(closure.clone(), vec![a.clone(), b.clone()])? {
                Value::Int(c) => Ok(c),
                _ => Err(RuntimeError {
                    message: "sortBy callback must return Int".to_string(),
                    try_return_value: None,
                }),
            }
        })?;
        Ok(Value::List(sorted))
    }
}

#[cfg(test)]
mod tests {
    use phoenix_common::span::SourceId;
    use phoenix_lexer::lexer::tokenize;
    use phoenix_parser::parser;
    use phoenix_sema::checker;

    /// Run a Phoenix program through the full pipeline (lex -> parse -> check -> interpret)
    /// and return the captured `print()` output lines.
    fn run_program(source: &str) -> Vec<String> {
        let tokens = tokenize(source, SourceId(0));
        let (program, parse_errors) = parser::parse(&tokens);
        assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
        let result = checker::check(&program);
        assert!(
            result.diagnostics.is_empty(),
            "type errors: {:?}",
            result.diagnostics
        );
        crate::interpreter::run_and_capture(&program, result.module.lambda_captures)
            .expect("runtime error")
    }

    /// Run a Phoenix program and expect a runtime error containing the given substring.
    fn run_program_expect_error(source: &str, expected_substring: &str) {
        let tokens = tokenize(source, SourceId(0));
        let (program, parse_errors) = parser::parse(&tokens);
        assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
        let result = checker::check(&program);
        assert!(
            result.diagnostics.is_empty(),
            "type errors: {:?}",
            result.diagnostics
        );
        let run_result =
            crate::interpreter::run_and_capture(&program, result.module.lambda_captures);
        assert!(run_result.is_err(), "expected runtime error");
        let err_msg = run_result.unwrap_err().to_string();
        assert!(
            err_msg.contains(expected_substring),
            "expected error containing '{}', got: {}",
            expected_substring,
            err_msg
        );
    }

    // ════════════════════════════════════════════════════════════════════
    // String methods
    // ════════════════════════════════════════════════════════════════════

    #[test]
    fn string_length() {
        let out = run_program(
            r#"
function main() {
    print("hello".length())
    print("".length())
    print("ab cd".length())
}
"#,
        );
        assert_eq!(out, vec!["5", "0", "5"]);
    }

    #[test]
    fn string_length_unicode() {
        let out = run_program(
            r#"
function main() {
    print("café".length())
}
"#,
        );
        assert_eq!(out, vec!["4"]);
    }

    #[test]
    fn string_contains() {
        let out = run_program(
            r#"
function main() {
    print("hello world".contains("world"))
    print("hello world".contains("xyz"))
    print("hello".contains(""))
    print("".contains(""))
}
"#,
        );
        assert_eq!(out, vec!["true", "false", "true", "true"]);
    }

    #[test]
    fn string_starts_with() {
        let out = run_program(
            r#"
function main() {
    print("hello world".startsWith("hello"))
    print("hello world".startsWith("world"))
    print("hello".startsWith(""))
    print("".startsWith(""))
}
"#,
        );
        assert_eq!(out, vec!["true", "false", "true", "true"]);
    }

    #[test]
    fn string_ends_with() {
        let out = run_program(
            r#"
function main() {
    print("hello world".endsWith("world"))
    print("hello world".endsWith("hello"))
    print("hello".endsWith(""))
    print("".endsWith(""))
}
"#,
        );
        assert_eq!(out, vec!["true", "false", "true", "true"]);
    }

    #[test]
    fn string_trim() {
        let out = run_program(
            r#"
function main() {
    print("  hello  ".trim())
    print("hello".trim())
    print("".trim())
    print("   ".trim())
}
"#,
        );
        assert_eq!(out, vec!["hello", "hello", "", ""]);
    }

    #[test]
    fn string_to_lower_case() {
        let out = run_program(
            r#"
function main() {
    print("Hello World".toLowerCase())
    print("ALLCAPS".toLowerCase())
    print("already".toLowerCase())
    print("".toLowerCase())
}
"#,
        );
        assert_eq!(out, vec!["hello world", "allcaps", "already", ""]);
    }

    #[test]
    fn string_to_upper_case() {
        let out = run_program(
            r#"
function main() {
    print("Hello World".toUpperCase())
    print("alllower".toUpperCase())
    print("ALREADY".toUpperCase())
    print("".toUpperCase())
}
"#,
        );
        assert_eq!(out, vec!["HELLO WORLD", "ALLLOWER", "ALREADY", ""]);
    }

    #[test]
    fn string_split() {
        let out = run_program(
            r#"
function main() {
    let parts: List<String> = "a,b,c".split(",")
    print(parts.length())
    print(parts.get(0))
    print(parts.get(1))
    print(parts.get(2))
}
"#,
        );
        assert_eq!(out, vec!["3", "a", "b", "c"]);
    }

    #[test]
    fn string_split_empty_string() {
        let out = run_program(
            r#"
function main() {
    let parts: List<String> = "".split(",")
    print(parts.length())
    print(parts.get(0))
}
"#,
        );
        assert_eq!(out, vec!["1", ""]);
    }

    #[test]
    fn string_replace() {
        let out = run_program(
            r#"
function main() {
    print("hello world".replace("world", "phoenix"))
    print("aaa".replace("a", "bb"))
    print("hello".replace("xyz", "abc"))
    print("".replace("a", "b"))
}
"#,
        );
        assert_eq!(out, vec!["hello phoenix", "bbbbbb", "hello", ""]);
    }

    #[test]
    fn string_substring() {
        let out = run_program(
            r#"
function main() {
    print("hello world".substring(0, 5))
    print("hello world".substring(6, 11))
    print("hello".substring(0, 0))
    print("hello".substring(2, 2))
}
"#,
        );
        assert_eq!(out, vec!["hello", "world", "", ""]);
    }

    #[test]
    fn string_substring_out_of_bounds() {
        run_program_expect_error(
            r#"
function main() {
    print("hello".substring(0, 100))
}
"#,
            "out of bounds",
        );
    }

    #[test]
    fn string_substring_start_greater_than_end() {
        run_program_expect_error(
            r#"
function main() {
    print("hello".substring(3, 1))
}
"#,
            "out of bounds",
        );
    }

    #[test]
    fn string_substring_negative_index() {
        run_program_expect_error(
            r#"
function main() {
    print("hello".substring(-1, 3))
}
"#,
            "non-negative",
        );
    }

    #[test]
    fn string_index_of() {
        let out = run_program(
            r#"
function main() {
    print("hello world".indexOf("world"))
    print("hello world".indexOf("hello"))
    print("hello world".indexOf("xyz"))
    print("hello".indexOf(""))
}
"#,
        );
        assert_eq!(out, vec!["6", "0", "-1", "0"]);
    }

    #[test]
    fn string_index_of_unicode() {
        // "café!" has 5 chars but "é" is multi-byte. indexOf("!") should return char index 4.
        let out = run_program(
            r#"
function main() {
    let s: String = "café!"
    print(s.indexOf("!"))
}
"#,
        );
        assert_eq!(out, vec!["4"]);
    }

    // ════════════════════════════════════════════════════════════════════
    // Map methods
    // ════════════════════════════════════════════════════════════════════

    #[test]
    fn map_length() {
        let out = run_program(
            r#"
function main() {
    let m: Map<String, Int> = {"a": 1, "b": 2, "c": 3}
    print(m.length())
    let empty: Map<String, Int> = {:}
    print(empty.length())
}
"#,
        );
        assert_eq!(out, vec!["3", "0"]);
    }

    #[test]
    fn map_get() {
        let out = run_program(
            r#"
function main() {
    let m: Map<String, Int> = {"x": 42, "y": 99}
    print(m.get("x").unwrap())
    print(m.get("z").isNone())
}
"#,
        );
        assert_eq!(out, vec!["42", "true"]);
    }

    #[test]
    fn map_get_empty() {
        let out = run_program(
            r#"
function main() {
    let m: Map<String, Int> = {:}
    print(m.get("x").isNone())
}
"#,
        );
        assert_eq!(out, vec!["true"]);
    }

    #[test]
    fn map_contains() {
        let out = run_program(
            r#"
function main() {
    let m: Map<String, Int> = {"a": 1, "b": 2}
    print(m.contains("a"))
    print(m.contains("z"))
}
"#,
        );
        assert_eq!(out, vec!["true", "false"]);
    }

    #[test]
    fn map_contains_empty() {
        let out = run_program(
            r#"
function main() {
    let m: Map<String, Int> = {:}
    print(m.contains("a"))
}
"#,
        );
        assert_eq!(out, vec!["false"]);
    }

    #[test]
    fn map_set() {
        let out = run_program(
            r#"
function main() {
    let m: Map<String, Int> = {"a": 1}
    let m2: Map<String, Int> = m.set("b", 2)
    print(m2.length())
    print(m2.get("b").unwrap())
}
"#,
        );
        assert_eq!(out, vec!["2", "2"]);
    }

    #[test]
    fn map_set_overwrite() {
        let out = run_program(
            r#"
function main() {
    let m: Map<String, Int> = {"a": 1}
    let m2: Map<String, Int> = m.set("a", 99)
    print(m2.get("a").unwrap())
    print(m2.length())
}
"#,
        );
        assert_eq!(out, vec!["99", "1"]);
    }

    #[test]
    fn map_remove() {
        let out = run_program(
            r#"
function main() {
    let m: Map<String, Int> = {"a": 1, "b": 2, "c": 3}
    let m2: Map<String, Int> = m.remove("b")
    print(m2.length())
    print(m2.contains("b"))
    print(m.length())
}
"#,
        );
        assert_eq!(out, vec!["2", "false", "3"]);
    }

    #[test]
    fn map_remove_nonexistent() {
        let out = run_program(
            r#"
function main() {
    let m: Map<String, Int> = {"a": 1}
    let m2: Map<String, Int> = m.remove("z")
    print(m2.length())
}
"#,
        );
        assert_eq!(out, vec!["1"]);
    }

    #[test]
    fn map_keys_and_values() {
        let out = run_program(
            r#"
function main() {
    let m: Map<String, Int> = {"x": 10, "y": 20}
    let ks: List<String> = m.keys()
    let vs: List<Int> = m.values()
    print(ks.length())
    print(vs.length())
}
"#,
        );
        assert_eq!(out, vec!["2", "2"]);
    }

    #[test]
    fn map_keys_values_empty() {
        let out = run_program(
            r#"
function main() {
    let m: Map<String, Int> = {:}
    print(m.keys().length())
    print(m.values().length())
}
"#,
        );
        assert_eq!(out, vec!["0", "0"]);
    }

    #[test]
    fn map_int_keys() {
        let out = run_program(
            r#"
function main() {
    let m: Map<Int, String> = {1: "one", 2: "two"}
    print(m.get(1).unwrap())
    print(m.get(2).unwrap())
    print(m.get(3).isNone())
}
"#,
        );
        assert_eq!(out, vec!["one", "two", "true"]);
    }

    // ════════════════════════════════════════════════════════════════════
    // List methods
    // ════════════════════════════════════════════════════════════════════

    #[test]
    fn list_length() {
        let out = run_program(
            r#"
function main() {
    print([1, 2, 3].length())
    let empty: List<Int> = []
    print(empty.length())
}
"#,
        );
        assert_eq!(out, vec!["3", "0"]);
    }

    #[test]
    fn list_get() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [10, 20, 30]
    print(xs.get(0))
    print(xs.get(1))
    print(xs.get(2))
}
"#,
        );
        assert_eq!(out, vec!["10", "20", "30"]);
    }

    #[test]
    fn list_get_out_of_bounds() {
        run_program_expect_error(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    print(xs.get(5))
}
"#,
            "out of bounds",
        );
    }

    #[test]
    fn list_get_negative_index() {
        run_program_expect_error(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    print(xs.get(-1))
}
"#,
            "out of bounds",
        );
    }

    #[test]
    fn list_push() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [1, 2]
    let ys: List<Int> = xs.push(3)
    print(ys.length())
    print(ys.get(2))
    print(xs.length())
}
"#,
        );
        assert_eq!(out, vec!["3", "3", "2"]);
    }

    #[test]
    fn list_first() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [10, 20, 30]
    print(xs.first().unwrap())
    let empty: List<Int> = []
    print(empty.first().isNone())
}
"#,
        );
        assert_eq!(out, vec!["10", "true"]);
    }

    #[test]
    fn list_last() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [10, 20, 30]
    print(xs.last().unwrap())
    let empty: List<Int> = []
    print(empty.last().isNone())
}
"#,
        );
        assert_eq!(out, vec!["30", "true"]);
    }

    #[test]
    fn list_contains() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    print(xs.contains(2))
    print(xs.contains(99))
    let empty: List<Int> = []
    print(empty.contains(1))
}
"#,
        );
        assert_eq!(out, vec!["true", "false", "false"]);
    }

    #[test]
    fn list_take() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3, 4, 5]
    print(xs.take(3))
    print(xs.take(0))
    print(xs.take(100))
}
"#,
        );
        assert_eq!(out, vec!["[1, 2, 3]", "[]", "[1, 2, 3, 4, 5]"]);
    }

    #[test]
    fn list_take_negative() {
        run_program_expect_error(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    let ys: List<Int> = xs.take(-1)
}
"#,
            "non-negative",
        );
    }

    #[test]
    fn list_drop() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3, 4, 5]
    print(xs.drop(3))
    print(xs.drop(0))
    print(xs.drop(100))
}
"#,
        );
        assert_eq!(out, vec!["[4, 5]", "[1, 2, 3, 4, 5]", "[]"]);
    }

    #[test]
    fn list_drop_negative() {
        run_program_expect_error(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    let ys: List<Int> = xs.drop(-1)
}
"#,
            "non-negative",
        );
    }

    #[test]
    fn list_map() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    let ys: List<Int> = xs.map(function(x: Int) -> Int { x * 2 })
    print(ys)
}
"#,
        );
        assert_eq!(out, vec!["[2, 4, 6]"]);
    }

    #[test]
    fn list_map_empty() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = []
    let ys: List<Int> = xs.map(function(x: Int) -> Int { x + 1 })
    print(ys)
}
"#,
        );
        assert_eq!(out, vec!["[]"]);
    }

    #[test]
    fn list_flat_map() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    let ys: List<Int> = xs.flatMap(function(x: Int) -> List<Int> { [x, x * 10] })
    print(ys)
}
"#,
        );
        assert_eq!(out, vec!["[1, 10, 2, 20, 3, 30]"]);
    }

    #[test]
    fn list_flat_map_empty() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = []
    let ys: List<Int> = xs.flatMap(function(x: Int) -> List<Int> { [x] })
    print(ys)
}
"#,
        );
        assert_eq!(out, vec!["[]"]);
    }

    #[test]
    fn list_filter() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3, 4, 5]
    let evens: List<Int> = xs.filter(function(x: Int) -> Bool { x % 2 == 0 })
    print(evens)
}
"#,
        );
        assert_eq!(out, vec!["[2, 4]"]);
    }

    #[test]
    fn list_filter_empty() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = []
    let ys: List<Int> = xs.filter(function(x: Int) -> Bool { x > 0 })
    print(ys)
}
"#,
        );
        assert_eq!(out, vec!["[]"]);
    }

    #[test]
    fn list_find() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3, 4, 5]
    print(xs.find(function(x: Int) -> Bool { x > 3 }))
    print(xs.find(function(x: Int) -> Bool { x > 100 }))
}
"#,
        );
        assert_eq!(out, vec!["Some(4)", "None"]);
    }

    #[test]
    fn list_find_empty() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = []
    print(xs.find(function(x: Int) -> Bool { x > 0 }))
}
"#,
        );
        assert_eq!(out, vec!["None"]);
    }

    #[test]
    fn list_any() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    print(xs.any(function(x: Int) -> Bool { x > 2 }))
    print(xs.any(function(x: Int) -> Bool { x > 10 }))
    let empty: List<Int> = []
    print(empty.any(function(x: Int) -> Bool { x > 0 }))
}
"#,
        );
        assert_eq!(out, vec!["true", "false", "false"]);
    }

    #[test]
    fn list_all() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    print(xs.all(function(x: Int) -> Bool { x > 0 }))
    print(xs.all(function(x: Int) -> Bool { x > 2 }))
    let empty: List<Int> = []
    print(empty.all(function(x: Int) -> Bool { x > 0 }))
}
"#,
        );
        assert_eq!(out, vec!["true", "false", "true"]);
    }

    #[test]
    fn list_reduce() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3, 4]
    let sum: Int = xs.reduce(0, function(acc: Int, x: Int) -> Int { acc + x })
    print(sum)
}
"#,
        );
        assert_eq!(out, vec!["10"]);
    }

    #[test]
    fn list_reduce_empty() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = []
    let sum: Int = xs.reduce(42, function(acc: Int, x: Int) -> Int { acc + x })
    print(sum)
}
"#,
        );
        assert_eq!(out, vec!["42"]);
    }

    #[test]
    fn list_sort_by() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [3, 1, 4, 1, 5]
    let sorted: List<Int> = xs.sortBy(function(a: Int, b: Int) -> Int { a - b })
    print(sorted)
}
"#,
        );
        assert_eq!(out, vec!["[1, 1, 3, 4, 5]"]);
    }

    #[test]
    fn list_sort_by_descending() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [3, 1, 4, 1, 5]
    let sorted: List<Int> = xs.sortBy(function(a: Int, b: Int) -> Int { b - a })
    print(sorted)
}
"#,
        );
        assert_eq!(out, vec!["[5, 4, 3, 1, 1]"]);
    }

    #[test]
    fn list_sort_by_empty() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = []
    let sorted: List<Int> = xs.sortBy(function(a: Int, b: Int) -> Int { a - b })
    print(sorted)
}
"#,
        );
        assert_eq!(out, vec!["[]"]);
    }

    #[test]
    fn list_sort_by_single_element() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [42]
    let sorted: List<Int> = xs.sortBy(function(a: Int, b: Int) -> Int { a - b })
    print(sorted)
}
"#,
        );
        assert_eq!(out, vec!["[42]"]);
    }

    #[test]
    fn list_sort_by_comparator_error_propagates() {
        // Comparator does an out-of-bounds list access on its first
        // call, which raises a runtime error. The Phase 2.2 insertion
        // sort propagated the error via a manual `sort_err: Option<_>`
        // bag; the new merge sort delegates to `merge_sort_by`, which
        // `?`-propagates instead. This test pins that the two paths
        // are equivalent — the error still reaches the caller, just
        // via the shared helper now.
        run_program_expect_error(
            r#"
function main() {
    let oob: List<Int> = []
    let xs: List<Int> = [3, 1, 4, 1, 5]
    let sorted: List<Int> = xs.sortBy(function(a: Int, b: Int) -> Int {
        let _ = oob.get(0)
        a - b
    })
    print(sorted)
}
"#,
            "out of bounds",
        );
    }

    // ════════════════════════════════════════════════════════════════════
    // Option methods
    // ════════════════════════════════════════════════════════════════════

    #[test]
    fn option_is_some_is_none() {
        let out = run_program(
            r#"
function main() {
    let s: Option<Int> = Some(42)
    let n: Option<Int> = None
    print(s.isSome())
    print(s.isNone())
    print(n.isSome())
    print(n.isNone())
}
"#,
        );
        assert_eq!(out, vec!["true", "false", "false", "true"]);
    }

    #[test]
    fn option_unwrap_some() {
        let out = run_program(
            r#"
function main() {
    let s: Option<Int> = Some(42)
    print(s.unwrap())
}
"#,
        );
        assert_eq!(out, vec!["42"]);
    }

    #[test]
    fn option_unwrap_none() {
        run_program_expect_error(
            r#"
function main() {
    let n: Option<Int> = None
    print(n.unwrap())
}
"#,
            "called unwrap() on None",
        );
    }

    #[test]
    fn option_unwrap_or() {
        let out = run_program(
            r#"
function main() {
    let s: Option<Int> = Some(42)
    let n: Option<Int> = None
    print(s.unwrapOr(0))
    print(n.unwrapOr(99))
}
"#,
        );
        assert_eq!(out, vec!["42", "99"]);
    }

    #[test]
    fn option_map_some() {
        let out = run_program(
            r#"
function main() {
    let s: Option<Int> = Some(5)
    let mapped: Option<Int> = s.map(function(x: Int) -> Int { x * 2 })
    print(mapped.unwrap())
}
"#,
        );
        assert_eq!(out, vec!["10"]);
    }

    #[test]
    fn option_map_none() {
        let out = run_program(
            r#"
function main() {
    let n: Option<Int> = None
    let mapped: Option<Int> = n.map(function(x: Int) -> Int { x * 2 })
    print(mapped.isNone())
}
"#,
        );
        assert_eq!(out, vec!["true"]);
    }

    #[test]
    fn option_and_then_some() {
        let out = run_program(
            r#"
function safeDiv(x: Int) -> Option<Int> {
    if x == 0 { return None }
    Some(100 / x)
}
function main() {
    let s: Option<Int> = Some(5)
    let result: Option<Int> = s.andThen(function(x: Int) -> Option<Int> { safeDiv(x) })
    print(result.unwrap())
}
"#,
        );
        assert_eq!(out, vec!["20"]);
    }

    #[test]
    fn option_and_then_none() {
        let out = run_program(
            r#"
function main() {
    let n: Option<Int> = None
    let result: Option<Int> = n.andThen(function(x: Int) -> Option<Int> { Some(x + 1) })
    print(result.isNone())
}
"#,
        );
        assert_eq!(out, vec!["true"]);
    }

    #[test]
    fn option_or_else_some() {
        let out = run_program(
            r#"
function main() {
    let s: Option<Int> = Some(5)
    let result: Option<Int> = s.orElse(function() -> Option<Int> { Some(99) })
    print(result.unwrap())
}
"#,
        );
        assert_eq!(out, vec!["5"]);
    }

    #[test]
    fn option_or_else_none() {
        let out = run_program(
            r#"
function main() {
    let n: Option<Int> = None
    let result: Option<Int> = n.orElse(function() -> Option<Int> { Some(42) })
    print(result.unwrap())
}
"#,
        );
        assert_eq!(out, vec!["42"]);
    }

    #[test]
    fn option_filter_passes() {
        let out = run_program(
            r#"
function main() {
    let s: Option<Int> = Some(10)
    let result: Option<Int> = s.filter(function(x: Int) -> Bool { x > 5 })
    print(result.unwrap())
}
"#,
        );
        assert_eq!(out, vec!["10"]);
    }

    #[test]
    fn option_filter_rejects() {
        let out = run_program(
            r#"
function main() {
    let s: Option<Int> = Some(3)
    let result: Option<Int> = s.filter(function(x: Int) -> Bool { x > 5 })
    print(result.isNone())
}
"#,
        );
        assert_eq!(out, vec!["true"]);
    }

    #[test]
    fn option_filter_none() {
        let out = run_program(
            r#"
function main() {
    let n: Option<Int> = None
    let result: Option<Int> = n.filter(function(x: Int) -> Bool { x > 0 })
    print(result.isNone())
}
"#,
        );
        assert_eq!(out, vec!["true"]);
    }

    #[test]
    fn option_unwrap_or_else_some() {
        let out = run_program(
            r#"
function main() {
    let s: Option<Int> = Some(5)
    print(s.unwrapOrElse(function() -> Int { 99 }))
}
"#,
        );
        assert_eq!(out, vec!["5"]);
    }

    #[test]
    fn option_unwrap_or_else_none() {
        let out = run_program(
            r#"
function main() {
    let n: Option<Int> = None
    print(n.unwrapOrElse(function() -> Int { 99 }))
}
"#,
        );
        assert_eq!(out, vec!["99"]);
    }

    #[test]
    fn option_ok_or_some() {
        let out = run_program(
            r#"
function main() {
    let s: Option<Int> = Some(5)
    let r: Result<Int, String> = s.okOr("not found")
    print(r.unwrap())
}
"#,
        );
        assert_eq!(out, vec!["5"]);
    }

    #[test]
    fn option_ok_or_none() {
        let out = run_program(
            r#"
function main() {
    let n: Option<Int> = None
    let r: Result<Int, String> = n.okOr("not found")
    print(r.isErr())
}
"#,
        );
        assert_eq!(out, vec!["true"]);
    }

    // ════════════════════════════════════════════════════════════════════
    // Result methods
    // ════════════════════════════════════════════════════════════════════

    #[test]
    fn result_is_ok_is_err() {
        let out = run_program(
            r#"
function main() {
    let ok: Result<Int, String> = Ok(42)
    let err: Result<Int, String> = Err("fail")
    print(ok.isOk())
    print(ok.isErr())
    print(err.isOk())
    print(err.isErr())
}
"#,
        );
        assert_eq!(out, vec!["true", "false", "false", "true"]);
    }

    #[test]
    fn result_unwrap_ok() {
        let out = run_program(
            r#"
function main() {
    let ok: Result<Int, String> = Ok(42)
    print(ok.unwrap())
}
"#,
        );
        assert_eq!(out, vec!["42"]);
    }

    #[test]
    fn result_unwrap_err() {
        run_program_expect_error(
            r#"
function main() {
    let err: Result<Int, String> = Err("bad")
    print(err.unwrap())
}
"#,
            "called unwrap() on Err",
        );
    }

    #[test]
    fn result_unwrap_or() {
        let out = run_program(
            r#"
function main() {
    let ok: Result<Int, String> = Ok(42)
    let err: Result<Int, String> = Err("fail")
    print(ok.unwrapOr(0))
    print(err.unwrapOr(-1))
}
"#,
        );
        assert_eq!(out, vec!["42", "-1"]);
    }

    #[test]
    fn result_map_ok() {
        let out = run_program(
            r#"
function main() {
    let ok: Result<Int, String> = Ok(5)
    let mapped: Result<Int, String> = ok.map(function(x: Int) -> Int { x * 2 })
    print(mapped.unwrap())
}
"#,
        );
        assert_eq!(out, vec!["10"]);
    }

    #[test]
    fn result_map_err_variant() {
        let out = run_program(
            r#"
function main() {
    let err: Result<Int, String> = Err("fail")
    let mapped: Result<Int, String> = err.map(function(x: Int) -> Int { x * 2 })
    print(mapped.isErr())
}
"#,
        );
        assert_eq!(out, vec!["true"]);
    }

    #[test]
    fn result_map_err_method() {
        let out = run_program(
            r#"
function main() {
    let err: Result<Int, String> = Err("fail")
    let mapped: Result<Int, String> = err.mapErr(function(e: String) -> String { "error: " + e })
    print(mapped)
}
"#,
        );
        assert_eq!(out, vec!["Err(error: fail)"]);
    }

    #[test]
    fn result_map_err_on_ok() {
        let out = run_program(
            r#"
function main() {
    let ok: Result<Int, String> = Ok(42)
    let mapped: Result<Int, String> = ok.mapErr(function(e: String) -> String { "wrapped: " + e })
    print(mapped.unwrap())
}
"#,
        );
        assert_eq!(out, vec!["42"]);
    }

    #[test]
    fn result_and_then_ok() {
        let out = run_program(
            r#"
function check(x: Int) -> Result<Int, String> {
    if x > 10 { return Ok(x) }
    return Err("too small")
}
function main() {
    let ok: Result<Int, String> = Ok(20)
    let result: Result<Int, String> = ok.andThen(function(x: Int) -> Result<Int, String> { check(x) })
    print(result.unwrap())
}
"#,
        );
        assert_eq!(out, vec!["20"]);
    }

    #[test]
    fn result_and_then_err_passthrough() {
        let out = run_program(
            r#"
function main() {
    let err: Result<Int, String> = Err("original")
    let result: Result<Int, String> = err.andThen(function(x: Int) -> Result<Int, String> { Ok(x + 1) })
    print(result.isErr())
}
"#,
        );
        assert_eq!(out, vec!["true"]);
    }

    #[test]
    fn result_or_else_ok() {
        let out = run_program(
            r#"
function main() {
    let ok: Result<Int, String> = Ok(42)
    let result: Result<Int, String> = ok.orElse(function(e: String) -> Result<Int, String> { Ok(0) })
    print(result.unwrap())
}
"#,
        );
        assert_eq!(out, vec!["42"]);
    }

    #[test]
    fn result_or_else_err() {
        let out = run_program(
            r#"
function main() {
    let err: Result<Int, String> = Err("fail")
    let result: Result<Int, String> = err.orElse(function(e: String) -> Result<Int, String> { Ok(99) })
    print(result.unwrap())
}
"#,
        );
        assert_eq!(out, vec!["99"]);
    }

    #[test]
    fn result_unwrap_or_else_ok() {
        let out = run_program(
            r#"
function main() {
    let ok: Result<Int, String> = Ok(42)
    let val: Int = ok.unwrapOrElse(function(e: String) -> Int { 0 })
    print(val)
}
"#,
        );
        assert_eq!(out, vec!["42"]);
    }

    #[test]
    fn result_unwrap_or_else_err() {
        let out = run_program(
            r#"
function main() {
    let err: Result<Int, String> = Err("fail")
    let val: Int = err.unwrapOrElse(function(e: String) -> Int { -1 })
    print(val)
}
"#,
        );
        assert_eq!(out, vec!["-1"]);
    }

    #[test]
    fn result_ok_method() {
        let out = run_program(
            r#"
function main() {
    let ok: Result<Int, String> = Ok(42)
    let err: Result<Int, String> = Err("fail")
    print(ok.ok())
    print(err.ok())
}
"#,
        );
        assert_eq!(out, vec!["Some(42)", "None"]);
    }

    #[test]
    fn result_err_method() {
        let out = run_program(
            r#"
function main() {
    let ok: Result<Int, String> = Ok(42)
    let err: Result<Int, String> = Err("fail")
    print(ok.err())
    print(err.err())
}
"#,
        );
        assert_eq!(out, vec!["None", "Some(fail)"]);
    }

    // ════════════════════════════════════════════════════════════════════
    // Cross-cutting / chaining tests
    // ════════════════════════════════════════════════════════════════════

    #[test]
    fn list_method_chaining() {
        let out = run_program(
            r#"
function main() {
    let xs: List<Int> = [1, 2, 3, 4, 5, 6]
    let result: Int = xs.filter(function(x: Int) -> Bool { x % 2 == 0 }).map(function(x: Int) -> Int { x * 10 }).reduce(0, function(acc: Int, x: Int) -> Int { acc + x })
    print(result)
}
"#,
        );
        assert_eq!(out, vec!["120"]);
    }

    #[test]
    fn option_combinator_chaining() {
        let out = run_program(
            r#"
function main() {
    let a: Option<Int> = Some(10)
    let result: Option<String> = a.filter(function(x: Int) -> Bool { x > 5 }).map(function(x: Int) -> String { toString(x) })
    print(result.unwrap())
    let b: Option<Int> = Some(3)
    let result2: Option<String> = b.filter(function(x: Int) -> Bool { x > 5 }).map(function(x: Int) -> String { toString(x) })
    print(result2.isNone())
}
"#,
        );
        assert_eq!(out, vec!["10", "true"]);
    }

    #[test]
    fn result_combinator_chaining() {
        let out = run_program(
            r#"
function main() {
    let r: Result<Int, String> = Ok(5)
    let chained: Result<String, String> = r.map(function(x: Int) -> Int { x * 2 }).map(function(x: Int) -> String { "got: " + toString(x) })
    print(chained.unwrap())
}
"#,
        );
        assert_eq!(out, vec!["got: 10"]);
    }

    #[test]
    fn string_split_then_list_methods() {
        let out = run_program(
            r#"
function main() {
    let words: List<String> = "hello world foo".split(" ")
    print(words.length())
    print(words.first().unwrap())
    print(words.last().unwrap())
    print(words.contains("world"))
}
"#,
        );
        assert_eq!(out, vec!["3", "hello", "foo", "true"]);
    }

    #[test]
    fn map_keys_then_list_methods() {
        let out = run_program(
            r#"
function main() {
    let m: Map<String, Int> = {"a": 1, "b": 2, "c": 3}
    let ks: List<String> = m.keys()
    print(ks.contains("b"))
    print(ks.length())
}
"#,
        );
        assert_eq!(out, vec!["true", "3"]);
    }
}
