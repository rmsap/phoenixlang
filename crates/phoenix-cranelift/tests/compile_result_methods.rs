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
