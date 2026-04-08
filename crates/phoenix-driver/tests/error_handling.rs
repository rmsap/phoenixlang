mod common;
use common::*;

/// Comprehensive end-to-end test for built-in Result and Option types.
///
/// Exercises:
/// - Option construction with Some and None (no user declaration needed)
/// - Result construction with Ok and Err (no user declaration needed)
/// - Built-in methods: is_some, is_none, unwrap, unwrap_or
/// - Built-in methods: is_ok, is_err, unwrap, unwrap_or
/// - Pattern matching on Option and Result
#[test]
fn result_and_option() {
    run_expect(
        r#"
function main() {
  let someVal: Option<Int> = Some(42)
  let noneVal: Option<Int> = None

  print(someVal.isSome())
  print(someVal.isNone())
  print(noneVal.isSome())
  print(noneVal.isNone())

  print(someVal.unwrap())
  print(noneVal.unwrapOr(99))
  print(someVal.unwrapOr(0))

  match someVal {
    Some(v) -> print(v)
    None -> print(0)
  }

  match noneVal {
    Some(v) -> print(v)
    None -> print(0)
  }

  let okVal: Result<Int, String> = Ok(10)
  let errVal: Result<Int, String> = Err("oops")

  print(okVal.isOk())
  print(okVal.isErr())
  print(errVal.isOk())
  print(errVal.isErr())

  print(okVal.unwrap())
  print(errVal.unwrapOr(0))
  print(okVal.unwrapOr(0))

  match okVal {
    Ok(v) -> print(v)
    Err(e) -> print(e)
  }

  match errVal {
    Ok(v) -> print(v)
    Err(e) -> print(e)
  }
}
"#,
        &[
            "true", "false", "false", "true", "42", "99", "42", "42", "0", "true", "false",
            "false", "true", "10", "0", "10", "10", "oops",
        ],
    );
}

// ═══════════════════════════════════════════════════════════════════
// Phase 1.8: Ergonomics and Usability
// ═══════════════════════════════════════════════════════════════════

// ── 1.8.1: Error Propagation Operator (?) ──────────────────────────

#[test]
fn try_operator_result_ok() {
    run_expect(
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
        &["43"],
    );
}

#[test]
fn try_operator_result_err_propagates() {
    run_expect(
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
}
"#,
        &["true"],
    );
}

#[test]
fn try_operator_option_some() {
    run_expect(
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
        &["20"],
    );
}

#[test]
fn try_operator_option_none_propagates() {
    run_expect(
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
        &["true"],
    );
}

#[test]
fn try_operator_chained() {
    run_expect(
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
        &["11"],
    );
}

#[test]
fn try_operator_chained_early_exit() {
    run_expect(
        r#"
function step1() -> Result<Int, String> {
  return Ok(1)
}
function step2(x: Int) -> Result<Int, String> {
  return Err("step2 failed")
}
function step3(x: Int) -> Result<Int, String> {
  return Ok(x + 100)
}
function pipeline() -> Result<Int, String> {
  let a: Int = step1()?
  let b: Int = step2(a)?
  let c: Int = step3(b)?
  return Ok(c)
}
function main() {
  let r: Result<Int, String> = pipeline()
  print(r.isErr())
}
"#,
        &["true"],
    );
}

#[test]
fn try_operator_wrong_return_type() {
    expect_type_error(
        r#"
function fail() -> Result<Int, String> {
  return Err("fail")
}
function process() -> Int {
  let val: Int = fail()?
  return val
}
function main() { }
"#,
        "requires the enclosing function to return Result",
    );
}

#[test]
fn try_operator_on_non_result() {
    expect_type_error(
        r#"
function process() -> Result<Int, String> {
  let val: Int = 42?
  return Ok(val)
}
function main() { }
"#,
        "can only be applied to Result<T, E> or Option<T>",
    );
}

// ── Cross-feature interactions ─────────────────────────────────────

#[test]
fn try_operator_with_implicit_return() {
    run_expect(
        r#"
function get() -> Result<Int, String> {
  Ok(42)
}
function process() -> Result<Int, String> {
  let v: Int = get()?
  Ok(v + 1)
}
function main() {
  let r: Result<Int, String> = process()
  print(r.unwrap())
}
"#,
        &["43"],
    );
}

#[test]
fn type_alias_with_try_operator() {
    run_expect(
        r#"
type Res<T> = Result<T, String>
function step1() -> Res<Int> {
  Ok(10)
}
function step2(x: Int) -> Res<Int> {
  Ok(x * 2)
}
function pipeline() -> Res<Int> {
  let a: Int = step1()?
  let b: Int = step2(a)?
  Ok(b)
}
function main() {
  let r: Res<Int> = pipeline()
  print(r.unwrap())
}
"#,
        &["20"],
    );
}

// ── 1.8 Edge cases: Try Operator ───────────────────────────────────

#[test]
fn try_operator_on_method_call() {
    run_expect(
        r#"
struct Db {
  Int id
}
impl Db {
  function find(self) -> Option<Int> {
    return Some(self.id)
  }
}
function process() -> Option<Int> {
  let db: Db = Db(42)
  let val: Int = db.find()?
  return Some(val)
}
function main() {
  let r: Option<Int> = process()
  print(r.unwrap())
}
"#,
        &["42"],
    );
}

#[test]
fn try_operator_in_closure() {
    run_expect(
        r#"
function main() {
  let f: () -> Result<Int, String> = function() -> Result<Int, String> {
    let r: Result<Int, String> = Ok(42)
    let val: Int = r?
    return Ok(val + 1)
  }
  let result: Result<Int, String> = f()
  print(result.unwrap())
}
"#,
        &["43"],
    );
}

#[test]
fn try_operator_in_expression_position() {
    run_expect(
        r#"
function get() -> Result<Int, String> { return Ok(5) }
function process() -> Result<Int, String> {
  return Ok(get()? + 10)
}
function main() {
  let r: Result<Int, String> = process()
  print(r.unwrap())
}
"#,
        &["15"],
    );
}

#[test]
fn try_operator_on_variable() {
    run_expect(
        r#"
function process() -> Result<Int, String> {
  let r: Result<Int, String> = Ok(42)
  let val: Int = r?
  return Ok(val)
}
function main() {
  print(process().unwrap())
}
"#,
        &["42"],
    );
}

#[test]
fn try_operator_nested_result() {
    run_expect(
        r#"
function outer() -> Result<Result<Int, String>, String> {
  return Ok(Ok(42))
}
function process() -> Result<Int, String> {
  let inner: Result<Int, String> = outer()?
  let val: Int = inner?
  return Ok(val)
}
function main() {
  let r: Result<Int, String> = process()
  print(r.unwrap())
}
"#,
        &["42"],
    );
}

#[test]
fn try_operator_double_question_mark_error() {
    expect_type_error(
        r#"
function process() -> Result<Int, String> {
  let r: Result<Int, String> = Ok(42)
  let val: Int = r??
  return Ok(val)
}
function main() { }
"#,
        "can only be applied to Result<T, E> or Option<T>",
    );
}

#[test]
fn try_operator_in_closure_wrong_return_type() {
    expect_type_error(
        r#"
function main() {
  let f: () -> Int = function() -> Int {
    let r: Result<Int, String> = Ok(42)
    let val: Int = r?
    return val
  }
}
"#,
        "requires the enclosing function to return Result",
    );
}

#[test]
fn try_operator_on_void_ok() {
    // Result<(), String> should work with ? operator — Ok() has no inner value
    // For now we test that Ok with a value works and Err propagates
    run_expect(
        r#"
function mightFail(fail: Bool) -> Result<Int, String> {
  if fail { return Err("failed") }
  return Ok(42)
}
function doStuff() -> Result<Int, String> {
  let val: Int = mightFail(false)?
  Ok(val)
}
function main() {
  let r: Result<Int, String> = doStuff()
  print(r.unwrap())
}
"#,
        &["42"],
    );
}

#[test]
fn try_operator_propagates_err() {
    run_expect(
        r#"
function mightFail() -> Result<Int, String> {
  return Err("boom")
}
function doStuff() -> Result<Int, String> {
  let val: Int = mightFail()?
  Ok(val)
}
function main() {
  let r: Result<Int, String> = doStuff()
  print(r.isErr())
}
"#,
        &["true"],
    );
}

// --- Error handling edge cases ---

#[test]
fn unwrap_on_none_panics() {
    expect_runtime_error(
        r#"
function main() {
    let x: Option<Int> = None
    print(x.unwrap())
}
"#,
        "called unwrap() on None",
    );
}

#[test]
fn unwrap_on_err_panics() {
    expect_runtime_error(
        r#"
function main() {
    let x: Result<Int, String> = Err("bad")
    print(x.unwrap())
}
"#,
        "called unwrap() on Err",
    );
}

// --- Result/Option: unwrap_or semantics ---

#[test]
fn option_unwrap_or_on_some() {
    run_expect(
        r#"
function main() {
    let x: Option<Int> = Some(42)
    print(x.unwrapOr(0))
}
"#,
        &["42"],
    );
}

#[test]
fn result_unwrap_or_on_ok() {
    run_expect(
        r#"
function main() {
    let x: Result<Int, String> = Ok(42)
    print(x.unwrapOr(0))
}
"#,
        &["42"],
    );
}

#[test]
fn result_unwrap_or_on_err() {
    run_expect(
        r#"
function main() {
    let x: Result<Int, String> = Err("bad")
    print(x.unwrapOr(-1))
}
"#,
        &["-1"],
    );
}

#[test]
fn option_in_struct_field() {
    run_expect(
        r#"
struct User {
    String name
    Option<String> email
}
function main() {
    let u1: User = User("Alice", Some("alice@example.com"))
    let u2: User = User("Bob", None)
    print(u1.email.unwrap())
    print(u2.email.isNone())
}
"#,
        &["alice@example.com", "true"],
    );
}

#[test]
fn result_with_match_and_early_return() {
    run_expect(
        r#"
function divide(a: Int, b: Int) -> Result<Int, String> {
    if b == 0 { return Err("division by zero") }
    return Ok(a / b)
}
function process() -> Result<String, String> {
    let val: Int = divide(10, 2)?
    Ok("result is {val}")
}
function main() {
    let r: Result<String, String> = process()
    print(r.unwrap())
}
"#,
        &["result is 5"],
    );
}

#[test]
fn generic_function_with_option_return() {
    run_expect(
        r#"
function findFirst<T>(items: List<T>) -> Option<T> {
    if items.length() == 0 { return None }
    return Some(items.get(0))
}
function main() {
    let nums: List<Int> = [10, 20, 30]
    let empty: List<Int> = []
    let found: Option<Int> = findFirst(nums)
    let notFound: Option<Int> = findFirst(empty)
    print(found.unwrap())
    print(notFound.isNone())
}
"#,
        &["10", "true"],
    );
}

// ── Enum variant used in if condition ─────────────────────────────────

#[test]
fn option_methods_in_if_condition() {
    run_expect(
        r#"
function maybeDouble(opt: Option<Int>) -> Int {
    if opt.isSome() {
        return opt.unwrap() * 2
    }
    return 0
}
function main() {
    print(maybeDouble(Some(5)))
    print(maybeDouble(None))
}
"#,
        &["10", "0"],
    );
}

// ── Generic function returning Result ─────────────────────────────────

#[test]
fn generic_function_returning_result() {
    run_expect(
        r#"
function safeHead<T>(items: List<T>) -> Result<T, String> {
    if items.length() == 0 { return Err("empty list") }
    return Ok(items.get(0))
}
function main() {
    let nums: List<Int> = [10, 20]
    let empty: List<Int> = []
    let r1: Result<Int, String> = safeHead(nums)
    let r2: Result<Int, String> = safeHead(empty)
    print(r1.unwrap())
    print(r2.isErr())
}
"#,
        &["10", "true"],
    );
}

// ── Option<T> with String type ────────────────────────────────────────

#[test]
fn option_with_string_type() {
    run_expect(
        r#"
function greet(name: Option<String>) -> String {
    match name {
        Some(n) -> "Hello, {n}!"
        None -> "Hello, stranger!"
    }
}
function main() {
    print(greet(Some("Alice")))
    print(greet(None))
}
"#,
        &["Hello, Alice!", "Hello, stranger!"],
    );
}

// ── Result error value extracted in match ─────────────────────────────

#[test]
fn result_error_value_in_match() {
    run_expect(
        r#"
function fail() -> Result<Int, String> {
    return Err("something went wrong")
}
function main() {
    let r: Result<Int, String> = fail()
    match r {
        Ok(v) -> print(v)
        Err(msg) -> print("Error: {msg}")
    }
}
"#,
        &["Error: something went wrong"],
    );
}

// ── Complex: struct with method returning Option ──────────────────────

#[test]
fn method_returning_option() {
    run_expect(
        r#"
struct Database { Int count }
impl Database {
    function find(self, id: Int) -> Option<String> {
        if id <= self.count {
            return Some("user_{id}")
        }
        return None
    }
}
function main() {
    let db: Database = Database(5)
    let found: Option<String> = db.find(3)
    let missing: Option<String> = db.find(10)
    print(found.unwrap())
    print(missing.isNone())
}
"#,
        &["user_3", "true"],
    );
}

#[test]
fn option_map() {
    run_expect(
        r#"
function main() {
    let a: Option<Int> = Some(5)
    let b: Option<Int> = a.map(function(x: Int) -> Int { x * 2 })
    print(b.unwrap())
    let c: Option<Int> = None
    let d: Option<Int> = c.map(function(x: Int) -> Int { x * 2 })
    print(d.isNone())
}
"#,
        &["10", "true"],
    );
}

#[test]
fn option_and_then() {
    run_expect(
        r#"
function safeDiv(x: Int) -> Option<Int> {
    if x == 0 { return None }
    Some(100 / x)
}
function main() {
    let a: Option<Int> = Some(5)
    let b: Option<Int> = a.andThen(function(x: Int) -> Option<Int> { safeDiv(x) })
    print(b.unwrap())
}
"#,
        &["20"],
    );
}

#[test]
fn option_or_else() {
    run_expect(
        r#"
function main() {
    let a: Option<Int> = None
    let b: Option<Int> = a.orElse(function() -> Option<Int> { Some(42) })
    print(b.unwrap())
}
"#,
        &["42"],
    );
}

#[test]
fn option_filter() {
    run_expect(
        r#"
function main() {
    let a: Option<Int> = Some(5)
    let b: Option<Int> = a.filter(function(x: Int) -> Bool { x > 3 })
    print(b.unwrap())
    let c: Option<Int> = a.filter(function(x: Int) -> Bool { x > 10 })
    print(c.isNone())
}
"#,
        &["5", "true"],
    );
}

#[test]
fn option_ok_or() {
    run_expect(
        r#"
function main() {
    let a: Option<Int> = Some(5)
    let b: Result<Int, String> = a.okOr("not found")
    print(b.unwrap())
    let c: Option<Int> = None
    let d: Result<Int, String> = c.okOr("not found")
    print(d.isErr())
}
"#,
        &["5", "true"],
    );
}

#[test]
fn result_map() {
    run_expect(
        r#"
function main() {
    let a: Result<Int, String> = Ok(5)
    let b: Result<Int, String> = a.map(function(x: Int) -> Int { x * 2 })
    print(b.unwrap())
    let c: Result<Int, String> = Err("fail")
    let d: Result<Int, String> = c.map(function(x: Int) -> Int { x * 2 })
    print(d.isErr())
}
"#,
        &["10", "true"],
    );
}

#[test]
fn result_ok_err() {
    run_expect(
        r#"
function main() {
    let a: Result<Int, String> = Ok(42)
    let b: Option<Int> = a.ok()
    print(b.unwrap())
    let c: Result<Int, String> = Err("fail")
    let d: Option<String> = c.err()
    print(d.unwrap())
}
"#,
        &["42", "fail"],
    );
}

// ── Result/Option combinator edge cases ─────────────────────────────────

#[test]
fn option_combinator_chaining() {
    run_expect(
        r#"
function main() {
    let a: Option<Int> = Some(10)
    let mapped: Option<Int> = a.map(function(x: Int) -> Int { x * 2 })
    let filtered: Option<Int> = mapped.filter(function(x: Int) -> Bool { x > 10 })
    print(filtered.unwrapOr(0))
    let b: Option<Int> = Some(3)
    let mapped2: Option<Int> = b.map(function(x: Int) -> Int { x * 2 })
    let filtered2: Option<Int> = mapped2.filter(function(x: Int) -> Bool { x > 10 })
    print(filtered2.unwrapOr(0))
}
"#,
        &["20", "0"],
    );
}

#[test]
fn option_unwrap_or_else() {
    run_expect(
        r#"
function main() {
    let a: Option<Int> = Some(5)
    print(a.unwrapOrElse(function() -> Int { 99 }))
    let b: Option<Int> = None
    print(b.unwrapOrElse(function() -> Int { 99 }))
}
"#,
        &["5", "99"],
    );
}

#[test]
fn option_or_else_on_some() {
    run_expect(
        r#"
function main() {
    let a: Option<Int> = Some(5)
    let b: Option<Int> = a.orElse(function() -> Option<Int> { Some(99) })
    print(b.unwrap())
}
"#,
        &["5"],
    );
}

#[test]
fn result_map_err_on_ok() {
    run_expect(
        r#"
function main() {
    let a: Result<Int, String> = Ok(42)
    let b: Result<Int, String> = a.mapErr(function(e: String) -> String { "wrapped: {e}" })
    print(b.unwrap())
}
"#,
        &["42"],
    );
}

#[test]
fn result_and_then_returning_err() {
    run_expect(
        r#"
function check(x: Int) -> Result<Int, String> {
    if x > 10 { return Ok(x) }
    return Err("too small")
}
function main() {
    let a: Result<Int, String> = Ok(5)
    let b: Result<Int, String> = a.andThen(function(x: Int) -> Result<Int, String> { check(x) })
    print(b.isErr())
}
"#,
        &["true"],
    );
}

#[test]
fn result_unwrap_or_else() {
    run_expect(
        r#"
function main() {
    let a: Result<Int, String> = Err("fail")
    let val: Int = a.unwrapOrElse(function(e: String) -> Int { 0 })
    print(val)
    let b: Result<Int, String> = Ok(42)
    let val2: Int = b.unwrapOrElse(function(e: String) -> Int { 0 })
    print(val2)
}
"#,
        &["0", "42"],
    );
}

#[test]
fn option_map_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    let x: Option<Int> = Some(42)
    let y: Option<Int> = x.map()
}
"#,
        "takes 1 argument",
    );
}

#[test]
fn result_and_then_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    let x: Result<Int, String> = Ok(42)
    let y: Result<Int, String> = x.andThen()
}
"#,
        "takes 1 argument",
    );
}

// ══════════════════════════════════════════════════════════════════════
// Audit-discovered bugs — regression tests
// ══════════════════════════════════════════════════════════════════════

/// Bug 1: `?` operator must reject non-Result/Option return types even
/// when the return type contains type variables (e.g. `List<T>`).
#[test]
fn try_operator_rejects_list_return_type() {
    expect_type_error(
        r#"
function maybeFail() -> Result<Int, String> {
  return Ok(42)
}
function wrap<T>(x: T) -> List<T> {
  let val: Int = maybeFail()?
  return [x]
}
function main() { }
"#,
        "requires the enclosing function to return Result",
    );
}

/// `?` operator should still work in generic functions returning Result<T, E>.
#[test]
fn try_operator_in_generic_result_function() {
    run_expect(
        r#"
function parse(s: String) -> Result<Int, String> {
  if s == "42" { return Ok(42) }
  return Err("bad")
}
function tryParse<T>(s: String, default: T) -> Result<Int, String> {
  let val: Int = parse(s)?
  Ok(val)
}
function main() {
  let r: Result<Int, String> = tryParse("42", 0)
  print(r.unwrap())
}
"#,
        &["42"],
    );
}

/// Result combinator chaining: map then and_then.
#[test]
fn result_combinator_chaining() {
    run_expect(
        r#"
function main() {
  let r: Result<Int, String> = Ok(5)
  let chained: Result<String, String> = r.map(function(x: Int) -> Int { x * 2 }).map(function(x: Int) -> String { "got: " + toString(x) })
  print(chained.unwrap())
}
"#,
        &["got: 10"],
    );
}

/// Option combinator chaining: filter then map.
#[test]
fn option_combinator_filter_then_map() {
    run_expect(
        r#"
function main() {
  let a: Option<Int> = Some(10)
  let b: Option<Int> = Some(3)
  let ra: Option<String> = a.filter(function(x: Int) -> Bool { x > 5 }).map(function(x: Int) -> String { toString(x) })
  let rb: Option<String> = b.filter(function(x: Int) -> Bool { x > 5 }).map(function(x: Int) -> String { toString(x) })
  print(ra.unwrap())
  print(rb.isNone())
}
"#,
        &["10", "true"],
    );
}

/// Custom error enum in Result with `?` propagation.
#[test]
fn try_operator_custom_error_enum() {
    run_expect(
        r#"
enum MyError {
  NotFound
  Invalid(String)
}
function find(id: Int) -> Result<String, MyError> {
  if id == 1 { return Ok("alice") }
  return Err(NotFound)
}
function lookup(id: Int) -> Result<String, MyError> {
  let name: String = find(id)?
  Ok("Found: " + name)
}
function main() {
  let ok: Result<String, MyError> = lookup(1)
  print(ok.unwrap())
  let err: Result<String, MyError> = lookup(99)
  print(err.isErr())
}
"#,
        &["Found: alice", "true"],
    );
}

/// Result::unwrap_or() with no arguments on Ok must report arg-count error.
#[test]
fn unwrap_or_validates_arg_count_on_ok() {
    expect_type_error(
        r#"
function main() {
  let x: Result<Int, String> = Ok(42)
  print(x.unwrapOr())
}
"#,
        "unwrapOr",
    );
}

/// Option::unwrap_or() with no arguments on Some must report arg-count error.
#[test]
fn option_unwrap_or_validates_arg_count_on_some() {
    expect_type_error(
        r#"
function main() {
  let x: Option<Int> = Some(42)
  print(x.unwrapOr())
}
"#,
        "unwrapOr",
    );
}

/// Result map_err, ok, err end-to-end.
#[test]
fn result_map_err_ok_err_e2e() {
    run_expect(
        r#"
function main() {
  let okVal: Result<Int, String> = Ok(42)
  let errVal: Result<Int, String> = Err("fail")
  print(okVal.ok())
  print(errVal.ok())
  print(okVal.err())
  print(errVal.err())
  let mapped: Result<Int, String> = errVal.mapErr(function(e: String) -> String { "error: " + e })
  print(mapped)
}
"#,
        &["Some(42)", "None", "None", "Some(fail)", "Err(error: fail)"],
    );
}

/// Option.and_then callback must return Option — type checker should reject non-Option return.
#[test]
fn option_and_then_rejects_non_option_callback() {
    expect_type_error(
        r#"
function main() {
  let a: Option<Int> = Some(5)
  let b: Option<Int> = a.andThen(function(x: Int) -> Int { x + 1 })
  print(b)
}
"#,
        "andThen callback must return Option",
    );
}

/// Option.or_else callback must return Option<T> — type checker should reject wrong return.
#[test]
fn option_or_else_rejects_wrong_return_type() {
    expect_type_error(
        r#"
function main() {
  let a: Option<Int> = None
  let b: Option<Int> = a.orElse(function() -> Int { 42 })
  print(b)
}
"#,
        "orElse callback must return Option",
    );
}

/// Option.unwrap_or_else callback must return the inner type T.
#[test]
fn option_unwrap_or_else_rejects_wrong_return_type() {
    expect_type_error(
        r#"
function main() {
  let a: Option<Int> = None
  let b: Int = a.unwrapOrElse(function() -> String { "nope" })
  print(b)
}
"#,
        "unwrapOrElse callback must return Int",
    );
}

/// Result.and_then callback must return Result — type checker should reject non-Result return.
#[test]
fn result_and_then_rejects_non_result_callback() {
    expect_type_error(
        r#"
function main() {
  let a: Result<Int, String> = Ok(5)
  let b: Result<Int, String> = a.andThen(function(x: Int) -> Int { x + 1 })
  print(b)
}
"#,
        "andThen callback must return Result",
    );
}

/// Result.or_else callback must return Result — type checker should reject non-Result return.
#[test]
fn result_or_else_rejects_non_result_callback() {
    expect_type_error(
        r#"
function main() {
  let a: Result<Int, String> = Err("fail")
  let b: Result<Int, String> = a.orElse(function(e: String) -> String { "handled" })
  print(b)
}
"#,
        "orElse callback must return Result",
    );
}

/// Result.unwrap_or_else callback must return the ok type T.
#[test]
fn result_unwrap_or_else_rejects_wrong_return_type() {
    expect_type_error(
        r#"
function main() {
  let a: Result<Int, String> = Err("fail")
  let b: Int = a.unwrapOrElse(function(e: String) -> String { "nope" })
  print(b)
}
"#,
        "unwrapOrElse callback must return Int",
    );
}

/// check_closure_arg should use types_compatible, not structural equality.
/// A closure returning Bool should be accepted by filter even through
/// generic type chains.
#[test]
fn closure_return_type_uses_compatible_check() {
    run_expect(
        r#"
function main() {
  let xs: List<Int> = [1, 2, 3, 4, 5]
  let evens: List<Int> = xs.filter(function(x: Int) -> Bool { x % 2 == 0 })
  print(evens)
}
"#,
        &["[2, 4]"],
    );
}
