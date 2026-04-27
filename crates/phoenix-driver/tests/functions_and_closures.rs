mod common;
use common::*;

/// Comprehensive integration test for closures and higher-order functions.
///
/// Exercises:
/// - Lambda creation and variable storage
/// - Passing lambdas as arguments to higher-order functions
/// - Closure capture of outer variables (makeAdder pattern)
/// - Inline lambda expressions as arguments
/// - Function-typed return values
#[test]
fn closures_and_higher_order() {
    run_expect(
        r#"
function apply(f: (Int) -> Int, x: Int) -> Int {
  return f(x)
}

function makeAdder(n: Int) -> (Int) -> Int {
  return function(x: Int) -> Int { return x + n }
}

function main() {
  let double: (Int) -> Int = function(x: Int) -> Int { return x * 2 }
  print(double(3))

  print(apply(double, 10))

  let add5: (Int) -> Int = makeAdder(5)
  print(add5(100))

  let result: Int = apply(function(x: Int) -> Int { return x + 1 }, 41)
  print(result)

  let offset: Int = 7
  let addOffset: (Int) -> Int = function(x: Int) -> Int { return x + offset }
  print(addOffset(3))
}
"#,
        &["6", "20", "105", "42", "10"],
    );
}

/// Higher-order functions combined with closures and generics.
#[test]
fn higher_order_with_closures() {
    run_expect(
        r#"
function applyTwice<T>(f: (T) -> T, x: T) -> T {
  return f(f(x))
}

function compose<A, B, C>(f: (B) -> C, g: (A) -> B) -> (A) -> C {
  return function(x: A) -> C { return f(g(x)) }
}

function makeAdder(n: Int) -> (Int) -> Int {
  return function(x: Int) -> Int { return x + n }
}

function main() {
  let doubled: Int = applyTwice(function(x: Int) -> Int { return x * 2 }, 3)
  print(doubled)

  let add5: (Int) -> Int = makeAdder(5)
  let add10: (Int) -> Int = makeAdder(10)
  print(add5(1))
  print(add10(1))

  let add15: (Int) -> Int = compose(add5, add10)
  print(add15(0))

  let increment: (Int) -> Int = function(x: Int) -> Int { return x + 1 }
  let result: Int = applyTwice(increment, 0)
  print(result)
}
"#,
        &["12", "6", "11", "15", "2"],
    );
}

/// Deeply nested closures work end-to-end.
#[test]
fn deeply_nested_closures() {
    run_expect(
        r#"
function main() {
  let a: Int = 1
  let f: (Int) -> (Int) -> (Int) -> Int = function(b: Int) -> (Int) -> (Int) -> Int {
    return function(c: Int) -> (Int) -> Int {
      return function(d: Int) -> Int {
        return a + b + c + d
      }
    }
  }
  let g: (Int) -> (Int) -> Int = f(2)
  let h: (Int) -> Int = g(3)
  print(h(4))
}
"#,
        &["10"],
    );
}

// ── 1.8.5: Implicit Return ────────────────────────────────────────

#[test]
fn implicit_return_simple() {
    run_expect(
        r#"
function add(a: Int, b: Int) -> Int {
  a + b
}
function main() {
  print(add(3, 4))
}
"#,
        &["7"],
    );
}

#[test]
fn implicit_return_with_explicit_return() {
    run_expect(
        r#"
function abs(x: Int) -> Int {
  if x < 0 { return -x }
  x
}
function main() {
  print(abs(-5))
  print(abs(3))
}
"#,
        &["5", "3"],
    );
}

#[test]
fn implicit_return_closure() {
    run_expect(
        r#"
function main() {
  let double: (Int) -> Int = function(x: Int) -> Int { x * 2 }
  print(double(5))
}
"#,
        &["10"],
    );
}

#[test]
fn implicit_return_method() {
    run_expect(
        r#"
struct Point {
  Int x
  Int y

  function sum(self) -> Int {
    self.x + self.y
  }
}
function main() {
  let p: Point = Point(3, 4)
  print(p.sum())
}
"#,
        &["7"],
    );
}

#[test]
fn implicit_return_string() {
    run_expect(
        r#"
function greet(name: String) -> String {
  "Hello, " + name + "!"
}
function main() {
  print(greet("Alice"))
}
"#,
        &["Hello, Alice!"],
    );
}

#[test]
fn implicit_return_bool() {
    run_expect(
        r#"
function isPositive(x: Int) -> Bool {
  x > 0
}
function main() {
  print(isPositive(5))
  print(isPositive(-1))
}
"#,
        &["true", "false"],
    );
}

#[test]
fn implicit_return_type_mismatch() {
    expect_type_error(
        r#"
function foo() -> Int {
  "hello"
}
function main() { }
"#,
        "implicit return type mismatch",
    );
}

#[test]
fn implicit_return_multiple_early_returns() {
    run_expect(
        r#"
function check(x: Int) -> String {
  if x < 0 { return "negative" }
  if x == 0 { return "zero" }
  "positive"
}
function main() {
  print(check(-1))
  print(check(0))
  print(check(1))
}
"#,
        &["negative", "zero", "positive"],
    );
}

#[test]
fn implicit_return_struct_literal() {
    run_expect(
        r#"
struct Point { Int x  Int y }
function origin() -> Point {
  Point(0, 0)
}
function main() {
  let p: Point = origin()
  print(p.x)
  print(p.y)
}
"#,
        &["0", "0"],
    );
}

#[test]
fn implicit_return_closure_with_capture() {
    run_expect(
        r#"
function main() {
  let base: Int = 10
  let add: (Int) -> Int = function(x: Int) -> Int { x + base }
  print(add(5))
}
"#,
        &["15"],
    );
}

#[test]
fn implicit_return_void_function_trailing_expr() {
    run_expect(
        r#"
function greet() {
  print("hello")
}
function main() {
  greet()
}
"#,
        &["hello"],
    );
}

#[test]
fn lambda_implicit_return_valid() {
    run_expect(
        r#"
function main() {
  let double: (Int) -> Int = function(x: Int) -> Int { x * 2 }
  print(double(21))
}
"#,
        &["42"],
    );
}

#[test]
fn lambda_implicit_return_type_error() {
    expect_type_error(
        r#"
function main() {
  let f: (Int) -> String = function(x: Int) -> String { x }
}
"#,
        "lambda return type mismatch",
    );
}

// ── Low-priority edge case tests ───────────────────────────────

#[test]
fn closure_captures_by_reference() {
    // Closures capture variables by reference. Mutating the variable after
    // closure creation IS visible inside the closure.
    run_expect(
        r#"
function main() {
  let mut x: Int = 10
  let add: (Int) -> Int = function(n: Int) -> Int { return x + n }
  x = 999
  print(add(5))
}
"#,
        &["1004"],
    );
    // Expected output: 1004, because closure sees x=999 via shared cell
}

#[test]
fn closure_mutation_visible_outside() {
    // Mutations to a captured `let mut` variable inside a closure are visible
    // in the enclosing scope.
    run_expect(
        r#"
function main() {
  let mut count: Int = 0
  let inc: () -> Void = function() { count = count + 1 }
  inc()
  inc()
  inc()
  print(count)
}
"#,
        &["3"],
    );
    // Expected output: 3
}

#[test]
fn closure_getter_setter_pattern() {
    // Two closures sharing the same mutable variable via by-reference capture.
    run_expect(
        r#"
function main() {
  let mut value: Int = 0
  let set: (Int) -> Void = function(x: Int) { value = x }
  let get: () -> Int = function() -> Int { return value }
  set(42)
  print(get())
  set(100)
  print(get())
}
"#,
        &["42", "100"],
    );
    // Expected output: 42, 100
}

#[test]
fn closure_capture_sees_latest_value() {
    // Closure always sees the current value of the captured variable, not
    // the value at capture time.
    run_expect(
        r#"
function main() {
  let mut x: Int = 1
  let f: () -> Int = function() -> Int { return x }
  x = 2
  print(f())
  x = 3
  print(f())
}
"#,
        &["2", "3"],
    );
    // Expected output: 2, 3
}

#[test]
fn closure_immutable_capture_cannot_mutate() {
    // Attempting to assign to a captured non-mut variable is a type error.
    expect_type_error(
        r#"
function main() {
  let x: Int = 10
  let f: () -> Void = function() { x = 20 }
}
"#,
        "cannot assign to immutable variable",
    );
}

#[test]
fn closure_does_not_pollute_outer_scope() {
    // Calling a closure should not leak variable bindings into the caller's scope.
    run_expect(
        r#"
function main() {
  let a: Int = 10
  let f: (Int) -> Int = function(n: Int) -> Int { return a + n }
  let result: Int = f(5)
  print(result)
  print(a)
}
"#,
        &["15", "10"],
    );
    // Expected output: 15, 10
}

#[test]
fn closure_returned_from_function() {
    // Environment is correctly restored after closure execution
    run_expect(
        r#"
function makeAdder(n: Int) -> (Int) -> Int {
  return function(x: Int) -> Int { return x + n }
}
function main() {
  let add5: (Int) -> Int = makeAdder(5)
  let add10: (Int) -> Int = makeAdder(10)
  print(add5(1))
  print(add10(1))
}
"#,
        &["6", "11"],
    );
}

#[test]
fn nested_closure_capture() {
    // Closures that return closures should capture correctly
    run_expect(
        r#"
function main() {
  let a: Int = 1
  let make: (Int) -> (Int) -> Int = function(b: Int) -> (Int) -> Int {
    return function(c: Int) -> Int { return a + b + c }
  }
  let f: (Int) -> Int = make(2)
  print(f(3))
}
"#,
        &["6"],
    );
    // Expected: 1 + 2 + 3 = 6
}

#[test]
fn missing_return_in_non_void_function() {
    expect_type_error(
        r#"
function foo() -> Int {
  let x: Int = 42
}
function main() { }
"#,
        "does not return a value",
    );
}

#[test]
fn missing_return_not_triggered_with_implicit_return() {
    run_expect(
        r#"
function double(n: Int) -> Int {
  n * 2
}
function main() {
  print(double(5))
}
"#,
        &["10"],
    );
}

// --- B4: closure parameter count mismatch ---

#[test]
fn closure_wrong_param_count() {
    expect_type_error(
        r#"
function main() {
    let f: (Int, Int) -> Int = function(a: Int, b: Int) -> Int { return a + b }
    print(f(1))
}
"#,
        "takes 2 argument(s), got 1",
    );
}

#[test]
fn closure_too_many_args() {
    expect_type_error(
        r#"
function main() {
    let f: (Int) -> Int = function(a: Int) -> Int { return a }
    print(f(1, 2, 3))
}
"#,
        "takes 1 argument(s), got 3",
    );
}

// --- Function call wrong argument count ---

#[test]
fn function_call_too_few_args() {
    expect_type_error(
        r#"
function add(a: Int, b: Int) -> Int {
    return a + b
}
function main() {
    print(add(1))
}
"#,
        "missing argument(s): b",
    );
}

#[test]
fn function_call_too_many_args() {
    expect_type_error(
        r#"
function add(a: Int, b: Int) -> Int {
    return a + b
}
function main() {
    print(add(1, 2, 3))
}
"#,
        "takes 2 argument(s), got 3",
    );
}

// --- Empty function body ---

#[test]
fn void_function_no_return() {
    run_expect(
        r#"
function doNothing() { }
function main() {
    doNothing()
    print("ok")
}
"#,
        &["ok"],
    );
}

// --- Closure captures ---

#[test]
fn closure_captures_outer_variable() {
    run_expect(
        r#"
function makeAdder(n: Int) -> (Int) -> Int {
    return function(x: Int) -> Int { return x + n }
}
function main() {
    let add5: (Int) -> Int = makeAdder(5)
    print(add5(10))
    print(add5(20))
}
"#,
        &["15", "25"],
    );
}

#[test]
fn closure_captures_loop_variable() {
    run_expect(
        r#"
function main() {
    let base: Int = 100
    let f: (Int) -> Int = function(x: Int) -> Int { return x + base }
    print(f(1))
    print(f(2))
}
"#,
        &["101", "102"],
    );
}

// --- Implicit return in various contexts ---

#[test]
fn implicit_return_in_if_else() {
    run_expect(
        r#"
function abs(x: Int) -> Int {
    if x < 0 { return -x }
    x
}
function main() {
    print(abs(-5))
    print(abs(3))
}
"#,
        &["5", "3"],
    );
}

// --- Deeply nested closures ---

#[test]
fn deeply_nested_closure_capture() {
    run_expect(
        r#"
function main() {
    let a: Int = 1
    let f: () -> Int = function() -> Int {
        let b: Int = 2
        let g: () -> Int = function() -> Int {
            return a + b
        }
        return g()
    }
    print(f())
}
"#,
        &["3"],
    );
}

// ── Closure Edge Cases ────────────────────────────────────────────────

#[test]
fn zero_parameter_closure() {
    run_expect(
        r#"
function main() {
    let greet: () -> String = function() -> String { return "hello" }
    print(greet())
}
"#,
        &["hello"],
    );
}

#[test]
fn closure_reassigned_to_mut_variable() {
    run_expect(
        r#"
function main() {
    let mut f: (Int) -> Int = function(x: Int) -> Int { return x + 1 }
    print(f(5))
    f = function(x: Int) -> Int { return x * 2 }
    print(f(5))
}
"#,
        &["6", "10"],
    );
}

#[test]
fn immediately_invoked_lambda() {
    run_expect(
        r#"
function apply(f: (Int) -> Int, x: Int) -> Int {
    return f(x)
}
function main() {
    let result: Int = apply(function(x: Int) -> Int { return x * 3 }, 7)
    print(result)
}
"#,
        &["21"],
    );
}

#[test]
fn closure_multiple_params() {
    run_expect(
        r#"
function main() {
    let add: (Int, Int) -> Int = function(a: Int, b: Int) -> Int { return a + b }
    print(add(3, 4))
}
"#,
        &["7"],
    );
}

#[test]
fn function_already_defined_error() {
    expect_type_error(
        r#"
function foo() { }
function foo() { }
function main() { }
"#,
        "already defined",
    );
}

#[test]
fn undefined_function_error() {
    expect_type_error(
        r#"
function main() {
    nonexistent()
}
"#,
        "undefined function",
    );
}

#[test]
fn print_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    print(1, 2)
}
"#,
        "print() takes 1 argument",
    );
}

#[test]
fn to_string_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    toString(1, 2)
}
"#,
        "toString() takes 1 argument",
    );
}

#[test]
fn closure_with_struct_capture() {
    run_expect(
        r#"
struct Config { Int multiplier }
function main() {
    let cfg: Config = Config(3)
    let multiply: (Int) -> Int = function(x: Int) -> Int { return x * cfg.multiplier }
    print(multiply(5))
}
"#,
        &["15"],
    );
}

#[test]
fn implicit_return_in_nested_function_calls() {
    run_expect(
        r#"
function add(a: Int, b: Int) -> Int { a + b }
function mul(a: Int, b: Int) -> Int { a * b }
function main() {
    print(add(mul(2, 3), mul(4, 5)))
}
"#,
        &["26"],
    );
}

#[test]
fn void_return_from_non_void_function_error() {
    expect_type_error(
        r#"
function foo() -> Int {
    return
}
function main() { }
"#,
        "expected return value",
    );
}

// ── Function argument type mismatches ─────────────────────────────────

#[test]
fn function_arg_type_mismatch() {
    expect_type_error(
        r#"
function add(a: Int, b: Int) -> Int {
    return a + b
}
function main() {
    print(add(1, "hello"))
}
"#,
        "expected `Int` but got `String`",
    );
}

// ── Calling non-callable values ───────────────────────────────────────

#[test]
fn call_non_callable_value() {
    expect_type_error(
        r#"
function main() {
    let x: Int = 42
    x()
}
"#,
        "cannot call value of type",
    );
}

// ── Method on Void ────────────────────────────────────────────────────

#[test]
fn method_on_void_error() {
    expect_type_error(
        r#"
function doNothing() { }
function main() {
    doNothing().foo()
}
"#,
        "cannot call method on Void",
    );
}

// ── Function-typed variable edge cases ────────────────────────────────

#[test]
fn function_variable_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    let f: (Int, Int) -> Int = function(a: Int, b: Int) -> Int { return a + b }
    print(f(1))
}
"#,
        "takes 2 argument(s), got 1",
    );
}

#[test]
fn function_variable_arg_type_mismatch() {
    expect_type_error(
        r#"
function main() {
    let f: (Int) -> Int = function(a: Int) -> Int { return a }
    print(f("hello"))
}
"#,
        "expected Int but got String",
    );
}

// ── Closure equality (always false) ───────────────────────────────────

#[test]
fn closure_equality_always_false() {
    run_expect(
        r#"
function main() {
    let f: (Int) -> Int = function(x: Int) -> Int { return x }
    let g: (Int) -> Int = f
    print(f == g)
}
"#,
        &["false"],
    );
}

#[test]
fn output_implicit_return() {
    run_expect(
        r#"
function double(x: Int) -> Int { x * 2 }
function main() {
  print(double(21))
}
"#,
        &["42"],
    );
}

// ── Pipe operator ───────────────────────────────────────────────────────

#[test]
fn pipe_operator_basic() {
    run_expect(
        r#"
function double(x: Int) -> Int { x * 2 }
function main() {
    let result: Int = 5 |> double()
    print(result)
}
"#,
        &["10"],
    );
}

#[test]
fn pipe_operator_with_extra_args() {
    run_expect(
        r#"
function add(a: Int, b: Int) -> Int { a + b }
function main() {
    let result: Int = 5 |> add(3)
    print(result)
}
"#,
        &["8"],
    );
}

#[test]
fn pipe_operator_chained() {
    run_expect(
        r#"
function double(x: Int) -> Int { x * 2 }
function addOne(x: Int) -> Int { x + 1 }
function main() {
    let result: Int = 5 |> double() |> addOne()
    print(result)
}
"#,
        &["11"],
    );
}

// ── Named / default parameters (Phase 1.9.6) ──────────────────────────

#[test]
fn default_parameter_value() {
    run_expect(
        r#"
function greet(name: String, greeting: String = "Hello") -> String {
    "{greeting}, {name}!"
}
function main() {
    print(greet("World"))
    print(greet("World", "Hi"))
}
"#,
        &["Hello, World!", "Hi, World!"],
    );
}

#[test]
fn named_arguments() {
    run_expect(
        r#"
function point(x: Int, y: Int) -> String {
    "({x}, {y})"
}
function main() {
    print(point(x: 1, y: 2))
    print(point(y: 20, x: 10))
}
"#,
        &["(1, 2)", "(10, 20)"],
    );
}

#[test]
fn named_args_with_defaults() {
    run_expect(
        r#"
function config(host: String = "localhost", port: Int = 8080) -> String {
    "{host}:{port}"
}
function main() {
    print(config())
    print(config(port: 3000))
    print(config(host: "0.0.0.0", port: 9090))
}
"#,
        &["localhost:8080", "localhost:3000", "0.0.0.0:9090"],
    );
}

// ── Pipe operator edge cases ────────────────────────────────────────────

#[test]
fn pipe_with_expression_lhs() {
    run_expect(
        r#"
function double(x: Int) -> Int { x * 2 }
function main() {
    let result: Int = (3 + 2) |> double()
    print(result)
}
"#,
        &["10"],
    );
}

// ── Named/default parameter edge cases ──────────────────────────────────

#[test]
fn named_params_all_positional_backward_compat() {
    run_expect(
        r#"
function add(a: Int, b: Int) -> Int { a + b }
function main() {
    print(add(3, 4))
}
"#,
        &["7"],
    );
}

#[test]
fn named_params_mix_positional_and_named() {
    run_expect(
        r#"
function make(x: Int, y: Int, z: Int) -> Int { x + y + z }
function main() {
    print(make(1, z: 3, y: 2))
}
"#,
        &["6"],
    );
}

// ── Named/default params: error cases ───────────────────────────────────

#[test]
fn named_param_wrong_type() {
    expect_type_error(
        r#"
function greet(name: String) -> String { "hi {name}" }
function main() {
    print(greet(name: 42))
}
"#,
        "expected `String` but got `Int`",
    );
}

// ── Pipe with string methods ────────────────────────────────────────────

#[test]
fn pipe_into_function_with_string() {
    run_expect(
        r#"
function exclaim(s: String) -> String { s.toUpperCase() }
function main() {
    let result: String = "hello" |> exclaim()
    print(result)
}
"#,
        &["HELLO"],
    );
}

// ── Cross-feature: named params + default + pipe ────────────────────────

#[test]
fn pipe_with_named_args() {
    run_expect(
        r#"
function format(s: String, prefix: String = "[", suffix: String = "]") -> String {
    "{prefix}{s}{suffix}"
}
function main() {
    let result: String = "hello" |> format(prefix: "(", suffix: ")")
    print(result)
}
"#,
        &["(hello)"],
    );
}

#[test]
fn pipe_operator_chain() {
    run_expect(
        r#"
function double(x: Int) -> Int { return x * 2 }
function addOne(x: Int) -> Int { return x + 1 }
function main() {
    let result: Int = 5 |> double() |> addOne()
    print(result)
}
"#,
        &["11"],
    );
}

#[test]
fn named_arg_unknown_param() {
    expect_type_error(
        r#"
function greet(name: String) -> String {
    return "hello " + name
}
function main() {
    print(greet(nonexistent: "world"))
}
"#,
        "nonexistent",
    );
}

#[test]
fn implicit_return_if_without_else() {
    run_expect(
        r#"
function maybeVal(flag: Bool) -> Int {
    if flag {
        return 42
    }
    return 0
}
function main() {
    print(maybeVal(true))
    print(maybeVal(false))
}
"#,
        &["42", "0"],
    );
}

#[test]
fn multiple_closures_shared_capture() {
    run_expect(
        r#"
function main() {
    let mut x: Int = 0
    let inc: () -> Void = function() { x = x + 1 }
    let get: () -> Int = function() -> Int { x }
    inc()
    inc()
    inc()
    print(get())
}
"#,
        &["3"],
    );
}

/// Variable shadowing inside closures.
#[test]
fn variable_shadowing_in_closure() {
    run_expect(
        r#"
function main() {
  let x: Int = 10
  let f: () -> Int = function() -> Int {
    let x: Int = 20
    return x
  }
  print(f())
  print(x)
}
"#,
        &["20", "10"],
    );
}

/// Pipe operator chaining through multiple functions.
#[test]
fn pipe_operator_multi_step() {
    run_expect(
        r#"
function double(x: Int) -> Int { return x * 2 }
function addOne(x: Int) -> Int { return x + 1 }
function toStr(x: Int) -> String { return toString(x) }
function main() {
  let result: String = 5 |> double() |> addOne() |> toStr()
  print(result)
}
"#,
        &["11"],
    );
}

/// If/else as the last statement in a function body acts as an implicit return
/// when every branch ends with an expression.
#[test]
fn implicit_return_if_else() {
    run_expect(
        r#"
function classify(n: Int) -> String {
  if n > 0 {
    "positive"
  } else {
    "non-positive"
  }
}
function main() {
  print(classify(1))
  print(classify(-1))
}
"#,
        &["positive", "non-positive"],
    );
}

/// Nested if/else implicit return (else-if chains).
#[test]
fn implicit_return_nested_if_else() {
    run_expect(
        r#"
function classify(n: Int) -> String {
  if n > 100 {
    "big"
  } else if n > 0 {
    "small"
  } else {
    "non-positive"
  }
}
function main() {
  print(classify(200))
  print(classify(5))
  print(classify(-1))
}
"#,
        &["big", "small", "non-positive"],
    );
}

/// If without else still requires explicit return — cannot guarantee a value.
#[test]
fn implicit_return_if_without_else_error() {
    expect_type_error(
        r#"
function maybe(n: Int) -> String {
  if n > 0 {
    "positive"
  }
}
function main() { }
"#,
        "does not return a value",
    );
}

/// If/else implicit return with mismatched branch types.
#[test]
fn implicit_return_if_else_type_mismatch() {
    expect_type_error(
        r#"
function bad(flag: Bool) -> String {
  if flag {
    "yes"
  } else {
    42
  }
}
function main() { }
"#,
        "incompatible types",
    );
}

/// Closure capturing mutable variable in loop.
#[test]
fn closure_captures_mutable_in_loop() {
    run_expect(
        r#"
function main() {
  let mut total: Int = 0
  let adder: (Int) -> Void = function(x: Int) { total = total + x }
  let xs: List<Int> = [1, 2, 3, 4]
  for x in xs {
    adder(x)
  }
  print(total)
}
"#,
        &["10"],
    );
}

/// Named parameters: all named, out of order.
#[test]
fn named_parameters_all_named_reordered() {
    run_expect(
        r#"
function greet(first: String, last: String, age: Int) -> String {
  return first + " " + last + " age " + toString(age)
}
function main() {
  print(greet(age: 30, last: "Smith", first: "Alice"))
}
"#,
        &["Alice Smith age 30"],
    );
}

/// Free-variable analysis must capture variables used as named args.
#[test]
fn free_vars_captures_named_args() {
    run_expect(
        r#"
function add(a: Int, b: Int) -> Int { return a + b }
function main() {
  let x: Int = 10
  let f: () -> Void = function() { print(add(a: x, b: 20)) }
  f()
}
"#,
        &["30"],
    );
}

/// Closure captures a variable that is passed via a named argument.
#[test]
fn closure_captures_named_arg_variable() {
    run_expect(
        r#"
function make(val: Int) -> () -> Void {
  return function() {
    print(val)
  }
}
function main() {
  let f: () -> Void = make(val: 42)
  f()
}
"#,
        &["42"],
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Feature interaction: generics + closures
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn generic_function_with_closure_argument() {
    run_expect(
        r#"
function apply<T>(f: (T) -> T, x: T) -> T {
  return f(x)
}
function main() {
  let offset: Int = 10
  let result: Int = apply(function(x: Int) -> Int { return x + offset }, 5)
  print(result)
}
"#,
        &["15"],
    );
}

#[test]
fn generic_function_returns_closure() {
    run_expect(
        r#"
function makeMapper<T>(f: (T) -> T) -> (T) -> T {
  return f
}
function main() {
  let double: (Int) -> Int = makeMapper(function(x: Int) -> Int { x * 2 })
  print(double(21))
}
"#,
        &["42"],
    );
}

#[test]
fn generic_function_with_closure_capturing_generic_value() {
    run_expect(
        r#"
function wrap<T>(val: T) -> () -> T {
  return function() -> T { return val }
}
function main() {
  let getInt: () -> Int = wrap(42)
  let getStr: () -> String = wrap("hello")
  print(getInt())
  print(getStr())
}
"#,
        &["42", "hello"],
    );
}

#[test]
fn generic_struct_with_closure_field_usage() {
    run_expect(
        r#"
struct Pair<A, B> {
  A first
  B second
}
function main() {
  let p: Pair<Int, String> = Pair(10, "hello")
  let getFirst: () -> Int = function() -> Int { return p.first }
  let getSecond: () -> String = function() -> String { return p.second }
  print(getFirst())
  print(getSecond())
}
"#,
        &["10", "hello"],
    );
}

#[test]
fn closure_inside_generic_list_map() {
    run_expect(
        r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let factor: Int = 10
  let scaled: List<Int> = nums.map(function(x: Int) -> Int { x * factor })
  print(scaled)
}
"#,
        &["[10, 20, 30]"],
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Feature interaction: try operator (?) in nested closures
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn try_operator_in_nested_closures() {
    run_expect(
        r#"
function main() {
  let outer: () -> Result<Int, String> = function() -> Result<Int, String> {
    let inner: () -> Result<Int, String> = function() -> Result<Int, String> {
      let r: Result<Int, String> = Ok(42)
      let val: Int = r?
      return Ok(val + 1)
    }
    let v: Int = inner()?
    return Ok(v * 2)
  }
  print(outer().unwrap())
}
"#,
        &["86"],
    );
}

#[test]
fn try_operator_nested_closure_propagates_err() {
    run_expect(
        r#"
function main() {
  let outer: () -> Result<Int, String> = function() -> Result<Int, String> {
    let inner: () -> Result<Int, String> = function() -> Result<Int, String> {
      let r: Result<Int, String> = Err("inner failed")
      let val: Int = r?
      return Ok(val)
    }
    let v: Int = inner()?
    return Ok(v * 2)
  }
  let result: Result<Int, String> = outer()
  print(result.isErr())
}
"#,
        &["true"],
    );
}

#[test]
fn try_operator_in_nested_closure_with_option() {
    run_expect(
        r#"
function main() {
  let f: () -> Option<Int> = function() -> Option<Int> {
    let inner: () -> Option<Int> = function() -> Option<Int> {
      let x: Option<Int> = Some(10)
      let val: Int = x?
      return Some(val + 5)
    }
    let v: Int = inner()?
    return Some(v * 3)
  }
  print(f().unwrap())
}
"#,
        &["45"],
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Edge case: multiple closures capturing the same variable
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn three_closures_capture_same_mutable_variable() {
    run_expect(
        r#"
function main() {
  let mut counter: Int = 0
  let inc: () -> Void = function() { counter = counter + 1 }
  let dec: () -> Void = function() { counter = counter - 1 }
  let get: () -> Int = function() -> Int { return counter }
  inc()
  inc()
  inc()
  dec()
  print(get())
}
"#,
        &["2"],
    );
}

#[test]
fn closures_capturing_same_variable_see_updates() {
    run_expect(
        r#"
function main() {
  let mut x: Int = 0
  let set: (Int) -> Void = function(v: Int) { x = v }
  let get: () -> Int = function() -> Int { return x }
  let add: (Int) -> Void = function(v: Int) { x = x + v }
  set(10)
  print(get())
  add(5)
  print(get())
  set(100)
  add(1)
  print(get())
}
"#,
        &["10", "15", "101"],
    );
}
