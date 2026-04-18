//! Integration tests for compiled `Option<T>` combinator methods.
//!
//! Covers isSome, isNone, unwrap, unwrapOr, map, andThen, orElse,
//! filter, okOr, unwrapOrElse, and chained combinators.

mod common;

use common::{compile_and_run, roundtrip};

#[test]
fn option_is_some_none() {
    roundtrip(
        r#"
function main() {
    let x: Option<Int> = Some(42)
    let y: Option<Int> = None
    print(x.isSome())
    print(x.isNone())
    print(y.isSome())
    print(y.isNone())
}
"#,
    );
}

#[test]
fn option_unwrap() {
    roundtrip(
        r#"
function main() {
    let x: Option<Int> = Some(42)
    print(x.unwrap())
}
"#,
    );
}

#[test]
fn option_unwrap_or() {
    roundtrip(
        r#"
function main() {
    let x: Option<Int> = Some(42)
    let y: Option<Int> = None
    print(x.unwrapOr(0))
    print(y.unwrapOr(0))
}
"#,
    );
}

#[test]
fn option_map() {
    roundtrip(
        r#"
function main() {
    let x: Option<Int> = Some(5)
    let y: Option<Int> = None
    let mapped_x = x.map(function(v: Int) -> Int { v * 2 })
    let mapped_y = y.map(function(v: Int) -> Int { v * 2 })
    match mapped_x {
        Some(v) -> print(v)
        None -> print(-1)
    }
    match mapped_y {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn option_and_then() {
    roundtrip(
        r#"
function main() {
    let x: Option<Int> = Some(5)
    let y: Option<Int> = None
    let result_x = x.andThen(function(v: Int) -> Option<Int> {
        if v > 3 {
            return Some(v * 10)
        }
        return None
    })
    let result_y = y.andThen(function(v: Int) -> Option<Int> { Some(v * 10) })
    match result_x {
        Some(v) -> print(v)
        None -> print(-1)
    }
    match result_y {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn option_or_else() {
    roundtrip(
        r#"
function main() {
    let x: Option<Int> = Some(5)
    let y: Option<Int> = None
    let result_x = x.orElse(function() -> Option<Int> { Some(99) })
    let result_y = y.orElse(function() -> Option<Int> { Some(99) })
    match result_x {
        Some(v) -> print(v)
        None -> print(-1)
    }
    match result_y {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn option_filter() {
    roundtrip(
        r#"
function main() {
    let x: Option<Int> = Some(10)
    let y: Option<Int> = Some(3)
    let z: Option<Int> = None
    let fx = x.filter(function(v: Int) -> Bool { v > 5 })
    let fy = y.filter(function(v: Int) -> Bool { v > 5 })
    let fz = z.filter(function(v: Int) -> Bool { v > 5 })
    match fx {
        Some(v) -> print(v)
        None -> print(-1)
    }
    match fy {
        Some(v) -> print(v)
        None -> print(-1)
    }
    match fz {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn option_unwrap_or_else() {
    roundtrip(
        r#"
function main() {
    let x: Option<Int> = Some(42)
    let y: Option<Int> = None
    print(x.unwrapOrElse(function() -> Int { 0 }))
    print(y.unwrapOrElse(function() -> Int { 0 }))
}
"#,
    );
}

#[test]
fn option_ok_or() {
    roundtrip(
        r#"
function main() {
    let x: Option<Int> = Some(42)
    let y: Option<Int> = None
    let rx = x.okOr("error")
    let ry = y.okOr("error")
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
fn option_string_unwrap() {
    roundtrip(
        r#"
function main() {
    let x: Option<String> = Some("hello")
    print(x.unwrap())
}
"#,
    );
}

#[test]
fn option_string_unwrap_or() {
    roundtrip(
        r#"
function main() {
    let x: Option<String> = Some("hello")
    let y: Option<String> = None
    print(x.unwrapOr("default"))
    print(y.unwrapOr("default"))
}
"#,
    );
}

#[test]
fn option_string_unwrap_or_else() {
    roundtrip(
        r#"
function main() {
    let x: Option<String> = Some("hello")
    let y: Option<String> = None
    print(x.unwrapOrElse(function() -> String { "fallback" }))
    print(y.unwrapOrElse(function() -> String { "fallback" }))
}
"#,
    );
}

#[test]
fn option_string_map() {
    roundtrip(
        r#"
function main() {
    let x: Option<String> = Some("hello")
    let y: Option<String> = None
    let mx = x.map(function(s: String) -> String { s + "!" })
    let my = y.map(function(s: String) -> String { s + "!" })
    match mx {
        Some(v) -> print(v)
        None -> print("none")
    }
    match my {
        Some(v) -> print(v)
        None -> print("none")
    }
}
"#,
    );
}

#[test]
fn option_string_and_then() {
    roundtrip(
        r#"
function main() {
    let x: Option<String> = Some("hello")
    let result = x.andThen(function(s: String) -> Option<String> {
        if s == "hello" {
            return Some(s + " world")
        }
        return None
    })
    match result {
        Some(v) -> print(v)
        None -> print("none")
    }
}
"#,
    );
}

#[test]
fn option_string_filter() {
    roundtrip(
        r#"
function main() {
    let x: Option<String> = Some("hello")
    let y: Option<String> = Some("bye")
    let fx = x.filter(function(s: String) -> Bool { s == "hello" })
    let fy = y.filter(function(s: String) -> Bool { s == "hello" })
    match fx {
        Some(v) -> print(v)
        None -> print("none")
    }
    match fy {
        Some(v) -> print(v)
        None -> print("none")
    }
}
"#,
    );
}

#[test]
fn option_unwrap_none_panics() {
    common::expect_panic(
        r#"
function main() {
    let x: Option<Int> = None
    print(x.unwrap())
}
"#,
        "unwrap()",
    );
}

#[test]
fn option_string_ok_or() {
    roundtrip(
        r#"
function main() {
    let x: Option<String> = Some("hello")
    let r: Result<String, String> = x.okOr("none")
    print(r.unwrap())
}
"#,
    );
}

#[test]
fn option_string_or_else() {
    roundtrip(
        r#"
function main() {
    let x: Option<String> = None
    let y: Option<String> = x.orElse(function() -> Option<String> { Some("fallback") })
    print(y.unwrap())
}
"#,
    );
}

#[test]
fn option_map_type_change() {
    let out = compile_and_run(
        r#"
function main() {
    let x: Option<Int> = Some(42)
    let y: Option<String> = x.map(function(v: Int) -> String { toString(v) })
    match y {
        Some(s) -> print(s)
        None -> print("none")
    }
}
"#,
    );
    assert_eq!(out, vec!["42"]);
}

#[test]
fn option_and_then_returns_none() {
    let out = compile_and_run(
        r#"
function helper(v: Int) -> Option<Int> {
    if v > 3 {
        return Some(v * 10)
    }
    None
}

function main() {
    let x: Option<Int> = Some(1)
    let y: Option<Int> = x.andThen(function(v: Int) -> Option<Int> { helper(v) })
    print(y.isSome())
    print(y.unwrapOr(0))
}
"#,
    );
    assert_eq!(out, vec!["false", "0"]);
}

#[test]
fn option_chained_combinators() {
    let out = compile_and_run(
        r#"
function main() {
    let x: Option<Int> = Some(5)
    let result: Int = x.map(function(v: Int) -> Int { v * 2 }).unwrapOr(0)
    print(result)
}
"#,
    );
    assert_eq!(out, vec!["10"]);
}

#[test]
fn option_unwrap_none_string_panics() {
    common::expect_panic(
        r#"
function main() {
    let x: Option<String> = None
    print(x.unwrap())
}
"#,
        "unwrap",
    );
}

#[test]
fn option_float_unwrap_and_or() {
    let out = compile_and_run(
        r#"
function main() {
    let x: Option<Float> = Some(3.14)
    let y: Option<Float> = None
    print(x.isSome())
    print(y.isNone())
    print(x.unwrapOr(0.0))
    print(y.unwrapOr(9.99))
}
"#,
    );
    assert_eq!(out, vec!["true", "true", "3.14", "9.99"]);
}

#[test]
fn option_bool_unwrap_and_or() {
    let out = compile_and_run(
        r#"
function main() {
    let x: Option<Bool> = Some(true)
    let y: Option<Bool> = None
    print(x.isSome())
    print(y.isNone())
    print(x.unwrapOr(false))
    print(y.unwrapOr(false))
}
"#,
    );
    assert_eq!(out, vec!["true", "true", "true", "false"]);
}

#[test]
fn option_map_with_capture() {
    let out = compile_and_run(
        r#"
function main() {
    let offset: Int = 100
    let x: Option<Int> = Some(5)
    let y: Option<Int> = x.map(function(v: Int) -> Int { v + offset })
    print(y.unwrapOr(0))
}
"#,
    );
    assert_eq!(out, vec!["105"]);
}

#[test]
fn option_and_then_with_capture() {
    let out = compile_and_run(
        r#"
function main() {
    let threshold: Int = 3
    let x: Option<Int> = Some(5)
    let y: Option<Int> = x.andThen(function(v: Int) -> Option<Int> {
        match v > threshold {
            true -> Some(v * 2)
            false -> None
        }
    })
    print(y.unwrapOr(0))
}
"#,
    );
    assert_eq!(out, vec!["10"]);
}

#[test]
fn option_filter_with_capture() {
    let out = compile_and_run(
        r#"
function main() {
    let min: Int = 10
    let x: Option<Int> = Some(5)
    let y: Option<Int> = x.filter(function(v: Int) -> Bool { v >= min })
    print(y.isSome())
    let z: Option<Int> = Some(20)
    let w: Option<Int> = z.filter(function(v: Int) -> Bool { v >= min })
    print(w.unwrapOr(0))
}
"#,
    );
    assert_eq!(out, vec!["false", "20"]);
}

#[test]
fn option_orelse_with_captures() {
    let out = compile_and_run(
        r#"
function main() {
    let fallback_val: Int = 99
    let x: Option<Int> = None
    let y: Option<Int> = x.orElse(function() -> Option<Int> { Some(fallback_val) })
    print(y.unwrapOr(0))
    let z: Option<Int> = Some(5)
    let w: Option<Int> = z.orElse(function() -> Option<Int> { Some(fallback_val) })
    print(w.unwrapOr(0))
}
"#,
    );
    assert_eq!(out, vec!["99", "5"]);
}

#[test]
fn option_unwrap_or_else_with_capture() {
    let out = compile_and_run(
        r#"
function main() {
    let default_val: Int = 42
    let x: Option<Int> = None
    let y: Int = x.unwrapOrElse(function() -> Int { default_val })
    print(y)
}
"#,
    );
    assert_eq!(out, vec!["42"]);
}

/// A function returning Option<Int> that uses ? on an inner Option call.
/// When the inner call returns Some, the ? extracts the value.
#[test]
fn option_try_operator_some() {
    let out = compile_and_run(
        r#"
function findValue() -> Option<Int> { Some(10) }
function process() -> Option<Int> {
    let v = findValue()?
    Some(v * 2)
}
function main() {
    match process() {
        Some(v) -> print(v)
        None -> print("none")
    }
}
"#,
    );
    assert_eq!(out, vec!["20"]);
}

/// When the inner Option call returns None, ? propagates None.
#[test]
fn option_try_operator_none_propagates() {
    let out = compile_and_run(
        r#"
function findValue() -> Option<Int> { None }
function process() -> Option<Int> {
    let v = findValue()?
    Some(v * 2)
}
function main() {
    match process() {
        Some(v) -> print(v)
        None -> print("none")
    }
}
"#,
    );
    assert_eq!(out, vec!["none"]);
}

#[test]
fn nested_option_some_some() {
    roundtrip(
        r#"
function main() {
    let inner: Option<Int> = Some(42)
    let outer: Option<Option<Int>> = Some(inner)
    match outer {
        Some(opt) -> match opt {
            Some(v) -> print(v)
            None -> print(-1)
        }
        None -> print(-2)
    }
}
"#,
    );
}

#[test]
fn nested_option_some_none() {
    roundtrip(
        r#"
function main() {
    let inner: Option<Int> = None
    let outer: Option<Option<Int>> = Some(inner)
    match outer {
        Some(opt) -> match opt {
            Some(v) -> print(v)
            None -> print(-1)
        }
        None -> print(-2)
    }
}
"#,
    );
}
