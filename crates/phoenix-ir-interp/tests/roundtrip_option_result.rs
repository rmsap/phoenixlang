//! Round-trip tests: Option, Result, and the try (`?`) operator.

mod common;
use common::roundtrip;

// ── Option ──────────────────────────────────────────────────────────

#[test]
fn option_some_none() {
    roundtrip(
        r#"
function main() {
    let a: Option<Int> = Some(42)
    let b: Option<Int> = None
    print(a.isSome())
    print(b.isNone())
    print(a.unwrap())
    print(b.unwrapOr(99))
}
"#,
    );
}

#[test]
fn option_map() {
    roundtrip(
        r#"
function main() {
    let a: Option<Int> = Some(5)
    let b: Option<Int> = None
    let mapped: Option<Int> = a.map(function(x: Int) -> Int { return x * 3 })
    print(mapped.unwrap())
    let mapped2: Option<Int> = b.map(function(x: Int) -> Int { return x * 3 })
    print(mapped2.isNone())
}
"#,
    );
}

#[test]
fn option_and_then() {
    roundtrip(
        r#"
function main() {
    let a: Option<Int> = Some(10)
    let b: Option<Int> = a.andThen(function(x: Int) -> Option<Int> {
        if x > 0 { return Some(x + 1) }
        return None
    })
    print(b.unwrap())
}
"#,
    );
}

#[test]
fn option_or_else() {
    roundtrip(
        r#"
function main() {
    let a: Option<Int> = None
    let b: Option<Int> = a.orElse(function() -> Option<Int> { return Some(42) })
    print(b.unwrap())
}
"#,
    );
}

#[test]
fn option_filter() {
    roundtrip(
        r#"
function main() {
    let a: Option<Int> = Some(10)
    let kept: Option<Int> = a.filter(function(x: Int) -> Bool { return x > 5 })
    print(kept.isSome())
    let dropped: Option<Int> = a.filter(function(x: Int) -> Bool { return x > 20 })
    print(dropped.isNone())
}
"#,
    );
}

#[test]
fn option_unwrap_or_else() {
    roundtrip(
        r#"
function main() {
    let a: Option<Int> = None
    let val: Int = a.unwrapOrElse(function() -> Int { return 99 })
    print(val)
}
"#,
    );
}

#[test]
fn option_ok_or() {
    roundtrip(
        r#"
function main() {
    let a: Option<Int> = Some(5)
    let b: Option<Int> = None
    let r1: Result<Int, String> = a.okOr("missing")
    let r2: Result<Int, String> = b.okOr("missing")
    print(r1.isOk())
    print(r1.unwrap())
    print(r2.isErr())
}
"#,
    );
}

// ── Result ──────────────────────────────────────────────────────────

#[test]
fn result_basic() {
    roundtrip(
        r#"
function main() {
    let a: Result<Int, String> = Ok(42)
    let b: Result<Int, String> = Err("oops")
    print(a.isOk())
    print(a.isErr())
    print(b.isOk())
    print(b.isErr())
    print(a.unwrap())
    print(b.unwrapOr(99))
}
"#,
    );
}

#[test]
fn result_map() {
    roundtrip(
        r#"
function main() {
    let a: Result<Int, String> = Ok(10)
    let b: Result<Int, String> = Err("fail")
    let mapped: Result<Int, String> = a.map(function(x: Int) -> Int { return x * 2 })
    print(mapped.unwrap())
    let mapped2: Result<Int, String> = b.map(function(x: Int) -> Int { return x * 2 })
    print(mapped2.isErr())
}
"#,
    );
}

#[test]
fn result_map_err() {
    roundtrip(
        r#"
function main() {
    let a: Result<Int, String> = Err("bad")
    let mapped: Result<Int, String> = a.mapErr(function(e: String) -> String { return "error: " + e })
    print(mapped.err())
}
"#,
    );
}

#[test]
fn result_and_then() {
    roundtrip(
        r#"
function main() {
    let a: Result<Int, String> = Ok(5)
    let b: Result<Int, String> = a.andThen(function(x: Int) -> Result<Int, String> {
        if x > 0 { return Ok(x * 10) }
        return Err("negative")
    })
    print(b.unwrap())
}
"#,
    );
}

#[test]
fn result_or_else() {
    roundtrip(
        r#"
function main() {
    let a: Result<Int, String> = Err("fail")
    let b: Result<Int, String> = a.orElse(function(e: String) -> Result<Int, String> {
        return Ok(99)
    })
    print(b.unwrap())
}
"#,
    );
}

#[test]
fn result_unwrap_or_else() {
    roundtrip(
        r#"
function main() {
    let a: Result<Int, String> = Err("oops")
    let val: Int = a.unwrapOrElse(function(e: String) -> Int { return 42 })
    print(val)
}
"#,
    );
}

#[test]
fn result_ok_and_err() {
    roundtrip(
        r#"
function main() {
    let a: Result<Int, String> = Ok(10)
    let b: Result<Int, String> = Err("bad")
    print(a.ok())
    print(a.err())
    print(b.ok())
    print(b.err())
}
"#,
    );
}

// ── Try operator ────────────────────────────────────────────────────

#[test]
fn try_result_success() {
    roundtrip(
        r#"
function getValue() -> Result<Int, String> {
    return Ok(42)
}
function process() -> Result<Int, String> {
    let val: Int = getValue()?
    return Ok(val + 1)
}
function main() {
    let r: Result<Int, String> = process()
    print(r.unwrap())
}
"#,
    );
}

#[test]
fn try_result_error_propagation() {
    roundtrip(
        r#"
function fail() -> Result<Int, String> {
    return Err("oops")
}
function process() -> Result<Int, String> {
    let val: Int = fail()?
    return Ok(val + 1)
}
function main() {
    let r: Result<Int, String> = process()
    print(r.isErr())
    print(r.err())
}
"#,
    );
}

#[test]
fn try_result_chained() {
    roundtrip(
        r#"
function step1() -> Result<Int, String> {
    return Ok(1)
}
function step2(x: Int) -> Result<Int, String> {
    return Ok(x + 10)
}
function pipeline() -> Result<Int, String> {
    let a: Int = step1()?
    let b: Int = step2(a)?
    return Ok(b)
}
function main() {
    let r: Result<Int, String> = pipeline()
    print(r.unwrap())
}
"#,
    );
}

#[test]
fn try_option_success() {
    roundtrip(
        r#"
function find() -> Option<Int> {
    return Some(10)
}
function process() -> Option<Int> {
    let val: Int = find()?
    return Some(val * 2)
}
function main() {
    let r: Option<Int> = process()
    print(r.unwrap())
}
"#,
    );
}

#[test]
fn try_option_none_propagation() {
    roundtrip(
        r#"
function find() -> Option<Int> {
    return None
}
function process() -> Option<Int> {
    let val: Int = find()?
    return Some(val * 2)
}
function main() {
    let r: Option<Int> = process()
    print(r.isNone())
}
"#,
    );
}
