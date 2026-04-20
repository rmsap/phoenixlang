//! Integration tests for compiled `Result<T, E>` combinator methods.
//!
//! Covers isOk, isErr, unwrap, unwrapOr, map, mapErr, andThen, orElse,
//! unwrapOrElse, and various Ok/Err type combinations.

mod common;

use common::{compile_and_run, roundtrip};

#[test]
fn result_is_ok_err() {
    roundtrip(
        r#"
function main() {
    let x: Result<Int, String> = Ok(42)
    let y: Result<Int, String> = Err("fail")
    print(x.isOk())
    print(x.isErr())
    print(y.isOk())
    print(y.isErr())
}
"#,
    );
}

#[test]
fn result_unwrap() {
    roundtrip(
        r#"
function main() {
    let x: Result<Int, String> = Ok(42)
    print(x.unwrap())
}
"#,
    );
}

#[test]
fn result_unwrap_or() {
    roundtrip(
        r#"
function main() {
    let x: Result<Int, String> = Ok(42)
    let y: Result<Int, String> = Err("fail")
    print(x.unwrapOr(0))
    print(y.unwrapOr(0))
}
"#,
    );
}

#[test]
fn result_map() {
    roundtrip(
        r#"
function main() {
    let x: Result<Int, String> = Ok(5)
    let y: Result<Int, String> = Err("fail")
    let mx = x.map(function(v: Int) -> Int { v * 2 })
    let my = y.map(function(v: Int) -> Int { v * 2 })
    match mx {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
    match my {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
}
"#,
    );
}

#[test]
fn result_map_err() {
    roundtrip(
        r#"
function main() {
    let x: Result<Int, String> = Ok(5)
    let y: Result<Int, String> = Err("fail")
    let mx = x.mapErr(function(e: String) -> String { e + "!" })
    let my = y.mapErr(function(e: String) -> String { e + "!" })
    match mx {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
    match my {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
}
"#,
    );
}

#[test]
fn result_and_then() {
    roundtrip(
        r#"
function main() {
    let x: Result<Int, String> = Ok(5)
    let y: Result<Int, String> = Err("fail")
    let rx = x.andThen(function(v: Int) -> Result<Int, String> {
        if v > 3 {
            return Ok(v * 10)
        }
        return Err("too small")
    })
    let ry = y.andThen(function(v: Int) -> Result<Int, String> { Ok(v * 10) })
    match rx {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
    match ry {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
}
"#,
    );
}

#[test]
fn result_or_else() {
    roundtrip(
        r#"
function main() {
    let x: Result<Int, String> = Ok(5)
    let y: Result<Int, String> = Err("fail")
    let rx = x.orElse(function(e: String) -> Result<Int, String> { Ok(99) })
    let ry = y.orElse(function(e: String) -> Result<Int, String> { Ok(99) })
    match rx {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
    match ry {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
}
"#,
    );
}

#[test]
fn result_unwrap_or_else() {
    roundtrip(
        r#"
function main() {
    let x: Result<Int, String> = Ok(42)
    let y: Result<Int, String> = Err("fail")
    print(x.unwrapOrElse(function(e: String) -> Int { 0 }))
    print(y.unwrapOrElse(function(e: String) -> Int { 0 }))
}
"#,
    );
}

#[test]
fn result_string_unwrap() {
    roundtrip(
        r#"
function main() {
    let x: Result<String, String> = Ok("hello")
    print(x.unwrap())
}
"#,
    );
}

#[test]
fn result_string_unwrap_or() {
    roundtrip(
        r#"
function main() {
    let x: Result<String, String> = Ok("hello")
    let y: Result<String, String> = Err("fail")
    print(x.unwrapOr("default"))
    print(y.unwrapOr("default"))
}
"#,
    );
}

#[test]
fn result_string_unwrap_or_else() {
    roundtrip(
        r#"
function main() {
    let x: Result<String, String> = Ok("hello")
    let y: Result<String, String> = Err("fail")
    print(x.unwrapOrElse(function(e: String) -> String { "recovered: " + e }))
    print(y.unwrapOrElse(function(e: String) -> String { "recovered: " + e }))
}
"#,
    );
}

#[test]
fn result_string_map() {
    roundtrip(
        r#"
function main() {
    let x: Result<String, String> = Ok("hello")
    let y: Result<String, String> = Err("fail")
    let mx = x.map(function(s: String) -> String { s + "!" })
    let my = y.map(function(s: String) -> String { s + "!" })
    match mx {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
    match my {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
}
"#,
    );
}

#[test]
fn result_string_map_err() {
    roundtrip(
        r#"
function main() {
    let x: Result<String, String> = Ok("hello")
    let y: Result<String, String> = Err("fail")
    let mx = x.mapErr(function(e: String) -> String { e + "!" })
    let my = y.mapErr(function(e: String) -> String { e + "!" })
    match mx {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
    match my {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
}
"#,
    );
}

#[test]
fn result_string_and_then() {
    roundtrip(
        r#"
function main() {
    let x: Result<String, String> = Ok("hello")
    let result = x.andThen(function(s: String) -> Result<String, String> {
        Ok(s + " world")
    })
    match result {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
}
"#,
    );
}

#[test]
fn result_string_or_else() {
    roundtrip(
        r#"
function main() {
    let x: Result<String, String> = Ok("hello")
    let y: Result<String, String> = Err("fail")
    let rx = x.orElse(function(e: String) -> Result<String, String> { Ok("recovered") })
    let ry = y.orElse(function(e: String) -> Result<String, String> { Ok("recovered") })
    match rx {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
    match ry {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
}
"#,
    );
}

#[test]
fn result_unwrap_err_panics() {
    common::expect_panic(
        r#"
function main() {
    let x: Result<Int, String> = Err("bad")
    print(x.unwrap())
}
"#,
        "unwrap()",
    );
}

#[test]
fn result_and_then_returns_err() {
    // Test andThen where the closure returns Err on an Ok input
    roundtrip(
        r#"
function main() {
    let x: Result<Int, String> = Ok(42)
    let y: Result<Int, String> = x.andThen(function(v: Int) -> Result<Int, String> { Err("nope") })
    print(y.isErr())
    print(y.unwrapOr(0))
}
"#,
    );
}

#[test]
fn result_chained_operations() {
    roundtrip(
        r#"
function main() {
    let x: Result<Int, String> = Ok(5)
    let mapped: Result<Int, String> = x.map(function(v: Int) -> Int { v * 2 })
    let y: Int = mapped.unwrapOr(0)
    print(y)
}
"#,
    );
}

#[test]
fn result_int_error_map_err() {
    roundtrip(
        r#"
function main() {
    let x: Result<String, Int> = Ok("hello")
    let y: Result<String, Int> = Err(42)
    let mx = x.mapErr(function(e: Int) -> Int { e * 2 })
    let my = y.mapErr(function(e: Int) -> Int { e * 2 })
    match mx {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
    match my {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
}
"#,
    );
}

#[test]
fn result_int_error_unwrap_or() {
    roundtrip(
        r#"
function main() {
    let x: Result<Int, Int> = Ok(1)
    let y: Result<Int, Int> = Err(99)
    print(x.unwrapOr(0))
    print(y.unwrapOr(0))
}
"#,
    );
}

#[test]
fn result_or_else_closure_returns_err() {
    let out = compile_and_run(
        r#"
function main() {
    let x: Result<Int, String> = Err("fail")
    let y: Result<Int, String> = x.orElse(function(e: String) -> Result<Int, String> {
        Err("still fail")
    })
    print(y.isErr())
}
"#,
    );
    assert_eq!(out, vec!["true"]);
}

#[test]
fn result_chained_combinators() {
    let out = compile_and_run(
        r#"
function main() {
    let x: Result<Int, String> = Ok(5)
    let result: Int = x.map(function(v: Int) -> Int { v * 3 }).unwrapOr(0)
    print(result)
}
"#,
    );
    assert_eq!(out, vec!["15"]);
}

#[test]
fn result_unwrap_err_string_ok_panics() {
    common::expect_panic(
        r#"
function main() {
    let x: Result<String, String> = Err("oops")
    print(x.unwrap())
}
"#,
        "unwrap",
    );
}

#[test]
fn result_float_ok_unwrap() {
    let out = compile_and_run(
        r#"
function main() {
    let x: Result<Float, String> = Ok(2.718)
    print(x.isOk())
    print(x.unwrap())
    print(x.unwrapOr(0.0))
}
"#,
    );
    assert_eq!(out, vec!["true", "2.718", "2.718"]);
}

#[test]
fn result_float_err_unwrap_or() {
    let out = compile_and_run(
        r#"
function main() {
    let x: Result<Float, String> = Err("fail")
    print(x.isErr())
    print(x.unwrapOr(0.0))
}
"#,
    );
    assert_eq!(out, vec!["true", "0"]);
}

#[test]
fn result_map_with_capture() {
    let out = compile_and_run(
        r#"
function main() {
    let offset: Int = 100
    let x: Result<Int, String> = Ok(5)
    let y: Result<Int, String> = x.map(function(v: Int) -> Int { v + offset })
    print(y.unwrapOr(0))
}
"#,
    );
    assert_eq!(out, vec!["105"]);
}

#[test]
fn result_and_then_with_capture() {
    let out = compile_and_run(
        r#"
function main() {
    let threshold: Int = 3
    let x: Result<Int, String> = Ok(5)
    let y: Result<Int, String> = x.andThen(function(v: Int) -> Result<Int, String> {
        match v > threshold {
            true -> Ok(v * 2)
            false -> Err("too small")
        }
    })
    print(y.unwrapOr(0))
}
"#,
    );
    assert_eq!(out, vec!["10"]);
}

#[test]
fn result_unwrap_or_else_with_capture() {
    let out = compile_and_run(
        r#"
function main() {
    let default_val: Int = 42
    let x: Result<Int, String> = Err("fail")
    let y: Int = x.unwrapOrElse(function(e: String) -> Int { default_val })
    print(y)
}
"#,
    );
    assert_eq!(out, vec!["42"]);
}

#[test]
fn result_int_error_unwrap_or_else() {
    let out = compile_and_run(
        r#"
function main() {
    let x: Result<Int, Int> = Err(5)
    let y: Int = x.unwrapOrElse(function(e: Int) -> Int { e * 10 })
    print(y)
}
"#,
    );
    assert_eq!(out, vec!["50"]);
}

/// Result.map that changes the Ok type from Int to String.
#[test]
fn result_map_type_change_int_to_string() {
    let out = compile_and_run(
        r#"
function main() {
    let x: Result<Int, String> = Ok(42)
    let y: Result<String, String> = x.map(function(v: Int) -> String { toString(v) })
    match y {
        Ok(s) -> print(s)
        Err(e) -> print(e)
    }
}
"#,
    );
    assert_eq!(out, vec!["42"]);
}

/// Result.map type change on Err variant should pass through the error.
#[test]
fn result_map_type_change_on_err() {
    let out = compile_and_run(
        r#"
function main() {
    let x: Result<Int, String> = Err("fail")
    let y: Result<String, String> = x.map(function(v: Int) -> String { toString(v) })
    match y {
        Ok(s) -> print(s)
        Err(e) -> print(e)
    }
}
"#,
    );
    assert_eq!(out, vec!["fail"]);
}

#[test]
fn result_maperr_with_captures() {
    let out = compile_and_run(
        r#"
function main() {
    let prefix: String = "error: "
    let x: Result<Int, String> = Err("not found")
    let y: Result<Int, String> = x.mapErr(function(e: String) -> String { prefix + e })
    match y {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
    let z: Result<Int, String> = Ok(42)
    let w: Result<Int, String> = z.mapErr(function(e: String) -> String { prefix + e })
    match w {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
}
"#,
    );
    assert_eq!(out, vec!["error: not found", "42"]);
}

#[test]
fn result_ok_on_ok() {
    let out = compile_and_run(
        r#"
function main() {
    let r: Result<Int, String> = Ok(42)
    let o: Option<Int> = r.ok()
    match o {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
    assert_eq!(out, vec!["42"]);
}

#[test]
fn result_ok_on_err() {
    let out = compile_and_run(
        r#"
function main() {
    let r: Result<Int, String> = Err("boom")
    let o: Option<Int> = r.ok()
    match o {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
    assert_eq!(out, vec!["-1"]);
}

#[test]
fn result_err_on_ok() {
    let out = compile_and_run(
        r#"
function main() {
    let r: Result<Int, String> = Ok(42)
    let o: Option<String> = r.err()
    match o {
        Some(e) -> print(e)
        None -> print("no error")
    }
}
"#,
    );
    assert_eq!(out, vec!["no error"]);
}

#[test]
fn result_err_on_err() {
    let out = compile_and_run(
        r#"
function main() {
    let r: Result<Int, String> = Err("boom")
    let o: Option<String> = r.err()
    match o {
        Some(e) -> print(e)
        None -> print("no error")
    }
}
"#,
    );
    assert_eq!(out, vec!["boom"]);
}

#[test]
fn result_ok_with_string_payload() {
    let out = compile_and_run(
        r#"
function main() {
    let r: Result<String, Int> = Ok("hello")
    let o: Option<String> = r.ok()
    match o {
        Some(s) -> print(s)
        None -> print("nope")
    }
}
"#,
    );
    assert_eq!(out, vec!["hello"]);
}

#[test]
fn result_err_with_int_payload() {
    let out = compile_and_run(
        r#"
function main() {
    let r: Result<String, Int> = Err(404)
    let o: Option<Int> = r.err()
    match o {
        Some(code) -> print(code)
        None -> print(-1)
    }
}
"#,
    );
    assert_eq!(out, vec!["404"]);
}

/// `mapErr` on a `Result<String, String>` received as a function
/// parameter. Mirrors `option_okor_*_via_function_parameter` — exercises
/// Strategy 0 (`try_result_payload_types_from_args`) for Result.
#[test]
fn result_map_err_string_payload_via_function_parameter() {
    let out = compile_and_run(
        r#"
function rewrap(r: Result<String, String>) -> Result<String, String> {
    return r.mapErr(function(e: String) -> String { "wrapped: " + e })
}
function main() {
    let r1 = rewrap(Ok("hello"))
    match r1 {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
    let r2 = rewrap(Err("boom"))
    match r2 {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
}
"#,
    );
    assert_eq!(out, vec!["hello", "wrapped: boom"]);
}

/// Strategy 0 coverage: `unwrap` on a `Result<String, String>` received as
/// a function parameter.  Exercises the Ok-slot read without any closure
/// arg present to drive Strategy 3.
#[test]
fn result_unwrap_string_ok_via_function_parameter() {
    let out = compile_and_run(
        r#"
function get_ok(r: Result<String, String>) -> String {
    return r.unwrap()
}
function main() {
    print(get_ok(Ok("hello")))
}
"#,
    );
    assert_eq!(out, vec!["hello"]);
}

/// Strategy 0 coverage: `orElse` on a `Result<Int, String>` received as a
/// function parameter.  The closure returns another `Result`, so Strategy
/// 3's `try_type_from_closure_arg` would give us the *input* of the closure
/// (Err type) — Strategy 0 is what tells us the Ok type so the combiner
/// lays out both slots correctly.
#[test]
fn result_or_else_via_function_parameter() {
    let out = compile_and_run(
        r#"
function recover(r: Result<Int, String>) -> Result<Int, String> {
    return r.orElse(function(_e: String) -> Result<Int, String> { Ok(0) })
}
function main() {
    let r1 = recover(Ok(7))
    match r1 {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
    let r2 = recover(Err("bad"))
    match r2 {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
}
"#,
    );
    assert_eq!(out, vec!["7", "0"]);
}

/// Strategy 0 coverage: `unwrapOrElse` on a `Result<String, Int>` received
/// as a function parameter.  Both Ok and Err payloads are resolved from
/// the receiver's `EnumRef` args (Ok via Strategy 1 via `result_type`,
/// Err via Strategy 0 since the closure's input tells us Err).
#[test]
fn result_unwrap_or_else_via_function_parameter() {
    let out = compile_and_run(
        r#"
function or_default(r: Result<String, Int>) -> String {
    return r.unwrapOrElse(function(code: Int) -> String { "code=" + toString(code) })
}
function main() {
    print(or_default(Ok("good")))
    print(or_default(Err(42)))
}
"#,
    );
    assert_eq!(out, vec!["good", "code=42"]);
}

/// Strategy 0 coverage: `map` on a `Result<String, String>` received as a
/// function parameter, with the closure rebuilding a multi-slot payload.
/// Guards against layout fallbacks silently picking an `I64` Ok slot.
#[test]
fn result_map_string_via_function_parameter() {
    let out = compile_and_run(
        r#"
function shout(r: Result<String, String>) -> Result<String, String> {
    return r.map(function(s: String) -> String { s + "!" })
}
function main() {
    let r = shout(Ok("hi"))
    match r {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
    let r2 = shout(Err("nope"))
    match r2 {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
}
"#,
    );
    assert_eq!(out, vec!["hi!", "nope"]);
}

/// Strategy 0 coverage: `andThen` on a `Result<Int, String>` received as a
/// function parameter, where the closure returns a different Ok type
/// (`Result<String, String>`).  Ok slot is driven by closure input;
/// Err slot must come from the receiver's args (Strategy 0).
#[test]
fn result_and_then_via_function_parameter() {
    let out = compile_and_run(
        r#"
function stringify(r: Result<Int, String>) -> Result<String, String> {
    return r.andThen(function(n: Int) -> Result<String, String> { Ok("n=" + toString(n)) })
}
function main() {
    let r = stringify(Ok(7))
    match r {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
    let r2 = stringify(Err("bad"))
    match r2 {
        Ok(v) -> print(v)
        Err(e) -> print(e)
    }
}
"#,
    );
    assert_eq!(out, vec!["n=7", "bad"]);
}

/// Deep nesting: `Result<Option<String>, String>` via a function parameter.
/// Exercises `try_type_from_enum_args` reading an outer `EnumRef("Result",
/// [EnumRef("Option", [String]), String])` and downstream inference of the
/// inner `Option<String>`'s payload via another Strategy 0 read.
#[test]
fn result_of_option_string_via_function_parameter() {
    let out = compile_and_run(
        r#"
function extract(r: Result<Option<String>, String>) -> String {
    match r {
        Ok(opt) -> opt.unwrapOr("empty")
        Err(e) -> e
    }
}
function main() {
    print(extract(Ok(Some("hello"))))
    print(extract(Ok(None)))
    print(extract(Err("boom")))
}
"#,
    );
    assert_eq!(out, vec!["hello", "empty", "boom"]);
}

/// Chained method dispatch on nested generics: `Result<Option<Int>,
/// String>` → `.map(closure that calls Option.unwrapOr)` → `.unwrapOr`.
/// Exercises Strategy 0 threading the Option args through the closure
/// parameter's `type_map` entry and then through the Option method's
/// own payload inference. Previously only pattern matching covered the
/// nested-generics case; this guards the method-dispatch path too.
#[test]
fn result_map_inner_option_unwrap_or_via_function_parameter() {
    let out = compile_and_run(
        r#"
function process(r: Result<Option<Int>, String>) -> Int {
    return r.map(function(opt: Option<Int>) -> Int { opt.unwrapOr(0) }).unwrapOr(-1)
}
function main() {
    print(toString(process(Ok(Some(42)))))
    print(toString(process(Ok(None))))
    print(toString(process(Err("boom"))))
}
"#,
    );
    assert_eq!(out, vec!["42", "0", "-1"]);
}

/// Asymmetric slot counts: `Result<String, Int>` via function parameter.
/// This is the exact shape of the pre-fix bug — Ok is 2 slots (pointer +
/// length), Err is 1. The old fallback picked a single `I64` for both
/// slots and silently corrupted the Ok payload. Strategy 0 must read the
/// receiver's args and produce the right Cranelift slot counts for each
/// branch so the Ok value survives the round trip intact.
#[test]
fn result_asymmetric_slot_counts_via_function_parameter() {
    let out = compile_and_run(
        r#"
function render(r: Result<String, Int>) -> String {
    match r {
        Ok(s) -> s
        Err(code) -> "code=" + toString(code)
    }
}
function main() {
    print(render(Ok("hello world")))
    print(render(Err(404)))
}
"#,
    );
    assert_eq!(out, vec!["hello world", "code=404"]);
}

/// Nested enums with different payload shapes: `Result<Option<Int>,
/// Option<String>>` via function parameter. Exercises `EnumRef` arg
/// recursion at both Ok and Err positions with different inner payload
/// types, guarding against a uniform fallback that would pick one shape
/// for both slots.
#[test]
fn result_nested_option_asymmetric_via_function_parameter() {
    let out = compile_and_run(
        r#"
function describe(r: Result<Option<Int>, Option<String>>) -> String {
    match r {
        Ok(opt) -> match opt {
            Some(n) -> "ok=" + toString(n)
            None -> "ok=none"
        }
        Err(opt) -> match opt {
            Some(e) -> "err=" + e
            None -> "err=none"
        }
    }
}
function main() {
    print(describe(Ok(Some(7))))
    let ok_none: Option<Int> = None
    print(describe(Ok(ok_none)))
    print(describe(Err(Some("boom"))))
    let err_none: Option<String> = None
    print(describe(Err(err_none)))
}
"#,
    );
    assert_eq!(out, vec!["ok=7", "ok=none", "err=boom", "err=none"]);
}

/// `Result<List<Int>, String>` via function parameter with `.map` whose
/// closure reads the inner List. Multi-slot list payload must not fall
/// back to an `I64` dummy.
#[test]
fn result_map_inner_list_via_function_parameter() {
    let out = compile_and_run(
        r#"
function first_or(r: Result<List<Int>, String>, default: Int) -> Int {
    return r.map(function(xs: List<Int>) -> Int { xs.first().unwrapOr(default) }).unwrapOr(default)
}
function main() {
    let xs: List<Int> = [7, 8, 9]
    print(toString(first_or(Ok(xs), -1)))
    print(toString(first_or(Err("nope"), -1)))
}
"#,
    );
    assert_eq!(out, vec!["7", "-1"]);
}
