mod common;
use common::*;

use phoenix_common::span::SourceId;
use phoenix_interp::interpreter;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

#[test]
fn hello_world() {
    run_expect(
        r#"function main() { print("Hello, World!") }"#,
        &["Hello, World!"],
    );
}

#[test]
fn fizzbuzz() {
    run_expect(
        r#"
function fizzbuzz(n: Int) -> String {
  if n % 15 == 0 { return "FizzBuzz" }
  if n % 3 == 0 { return "Fizz" }
  if n % 5 == 0 { return "Buzz" }
  return toString(n)
}
function main() {
  print(fizzbuzz(15))
}
"#,
        &["FizzBuzz"],
    );
}

#[test]
fn fibonacci() {
    run_expect(
        r#"
function fib(n: Int) -> Int {
  if n <= 1 { return n }
  return fib(n - 1) + fib(n - 2)
}
function main() { print(fib(10)) }
"#,
        &["55"],
    );
}

#[test]
fn while_loop() {
    run_expect(
        r#"
function main() {
  let mut sum: Int = 0
  let mut i: Int = 1
  while i <= 10 {
    sum = sum + i
    i = i + 1
  }
  print(sum)
}
"#,
        &["55"],
    );
}

#[test]
fn for_loop() {
    run_expect(
        r#"
function main() {
  let mut sum: Int = 0
  for i in 0..10 {
    sum = sum + i
  }
  print(sum)
}
"#,
        &["45"],
    );
}

#[test]
fn else_if_chain() {
    run_expect(
        r#"
function main() {
  let x: Int = 42
  if x < 0 {
    print("negative")
  } else if x == 0 {
    print("zero")
  } else {
    print("positive")
  }
}
"#,
        &["positive"],
    );
}

#[test]
fn break_and_continue() {
    run_expect(
        r#"
function main() {
  let mut sum: Int = 0
  let mut i: Int = 0
  while i < 100 {
    i = i + 1
    if i % 2 == 0 { continue }
    if i > 10 { break }
    sum = sum + i
  }
  print(sum)
}
"#,
        &["25"],
    );
}

/// Float division by zero returns a clean runtime error.
#[test]
fn float_division_by_zero() {
    let tokens = tokenize("function main() { print(1.0 / 0.0) }", SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty());
    let check_result = checker::check(&program);
    assert!(check_result.diagnostics.is_empty());
    let result = interpreter::run(&program, check_result.lambda_captures);
    assert!(result.is_err());
}

/// Infinite recursion is caught with a stack overflow error, not a crash.
#[test]
fn infinite_recursion_caught() {
    expect_runtime_error(
        "function boom() { boom() }\nfunction main() { boom() }",
        "stack overflow",
    );
}

#[test]
fn float_division_by_zero_runtime_error() {
    let source = r#"
function main() {
  let x: Float = 1.0 / 0.0
}
"#;
    let tokens = phoenix_lexer::lexer::tokenize(source, phoenix_common::span::SourceId(0));
    let (program, parse_errors) = phoenix_parser::parser::parse(&tokens);
    assert!(parse_errors.is_empty());
    let check_result = phoenix_sema::checker::check(&program);
    assert!(check_result.diagnostics.is_empty());
    let result = phoenix_interp::interpreter::run(&program, check_result.lambda_captures);
    assert!(
        result.is_err(),
        "expected runtime error for float division by zero"
    );
    assert!(result.unwrap_err().to_string().contains("division by zero"));
}

#[test]
fn integer_division_by_zero_runtime_error() {
    let source = r#"
function main() {
  let x: Int = 1 / 0
}
"#;
    let tokens = phoenix_lexer::lexer::tokenize(source, phoenix_common::span::SourceId(0));
    let (program, parse_errors) = phoenix_parser::parser::parse(&tokens);
    assert!(parse_errors.is_empty());
    let check_result = phoenix_sema::checker::check(&program);
    assert!(check_result.diagnostics.is_empty());
    let result = phoenix_interp::interpreter::run(&program, check_result.lambda_captures);
    assert!(
        result.is_err(),
        "expected runtime error for integer division by zero"
    );
    assert!(result.unwrap_err().to_string().contains("division by zero"));
}

#[test]
fn unicode_identifiers() {
    run_expect(
        r#"
function main() {
  let über: Int = 42
  let café: String = "latte"
  print(über)
  print(café)
}
"#,
        &["42", "latte"],
    );
}

#[test]
fn unicode_identifier_cjk() {
    run_expect(
        r#"
function main() {
  let 名前: String = "Phoenix"
  print(名前)
}
"#,
        &["Phoenix"],
    );
}

#[test]
fn integer_overflow_produces_error() {
    let source = r#"
function main() {
  let x: Int = 9223372036854775807
  let y: Int = x + 1
}
"#;
    let tokens = phoenix_lexer::lexer::tokenize(source, phoenix_common::span::SourceId(0));
    let (program, parse_errors) = phoenix_parser::parser::parse(&tokens);
    assert!(parse_errors.is_empty());
    let check_result = phoenix_sema::checker::check(&program);
    assert!(check_result.diagnostics.is_empty());
    let result = phoenix_interp::interpreter::run(&program, check_result.lambda_captures);
    assert!(
        result.is_err(),
        "expected runtime error for integer overflow"
    );
    assert!(result.unwrap_err().to_string().contains("integer overflow"));
}

#[test]
fn integer_multiply_overflow_produces_error() {
    let source = r#"
function main() {
  let x: Int = 9223372036854775807
  let y: Int = x * 2
}
"#;
    let tokens = phoenix_lexer::lexer::tokenize(source, phoenix_common::span::SourceId(0));
    let (program, parse_errors) = phoenix_parser::parser::parse(&tokens);
    assert!(parse_errors.is_empty());
    let check_result = phoenix_sema::checker::check(&program);
    assert!(check_result.diagnostics.is_empty());
    let result = phoenix_interp::interpreter::run(&program, check_result.lambda_captures);
    assert!(
        result.is_err(),
        "expected runtime error for integer overflow"
    );
    assert!(result.unwrap_err().to_string().contains("integer overflow"));
}

#[test]
fn short_circuit_and_does_not_evaluate_right() {
    // The right operand of `&&` should NOT be evaluated when left is false.
    // If it were evaluated, the division by zero would cause a runtime error.
    run_expect(
        r#"
function crashes() -> Bool {
  let x: Int = 1 / 0
  return true
}
function main() {
  let result: Bool = false && crashes()
  print(result)
}
"#,
        &["false"],
    );
}

#[test]
fn short_circuit_or_does_not_evaluate_right() {
    // The right operand of `||` should NOT be evaluated when left is true.
    run_expect(
        r#"
function crashes() -> Bool {
  let x: Int = 1 / 0
  return false
}
function main() {
  let result: Bool = true || crashes()
  print(result)
}
"#,
        &["true"],
    );
}

#[test]
fn short_circuit_and_evaluates_right_when_needed() {
    run_expect(
        r#"
function main() {
  let result: Bool = true && false
  print(result)
}
"#,
        &["false"],
    );
}

#[test]
fn short_circuit_or_evaluates_right_when_needed() {
    run_expect(
        r#"
function main() {
  let result: Bool = false || true
  print(result)
}
"#,
        &["true"],
    );
}

#[test]
fn integer_negation_overflow() {
    expect_runtime_error(
        r#"
function main() {
  let x: Int = -9223372036854775807 - 1
  let y: Int = -x
  print(y)
}
"#,
        "integer overflow",
    );
}

// --- Recursive function ---

#[test]
fn recursive_sum() {
    run_expect(
        r#"
function sum(n: Int) -> Int {
    if n <= 0 { return 0 }
    return n + sum(n - 1)
}
function main() {
    print(sum(10))
}
"#,
        &["55"],
    );
}

// --- Stack overflow detection ---

#[test]
fn stack_overflow_detected() {
    expect_runtime_error(
        r#"
function recurse(n: Int) -> Int {
    return recurse(n + 1)
}
function main() {
    print(recurse(0))
}
"#,
        "stack overflow",
    );
}

// --- For loop with range ---

#[test]
fn for_loop_empty_range() {
    run_expect(
        r#"
function main() {
    let mut count: Int = 0
    for i in 5..5 {
        count = count + 1
    }
    print(count)
}
"#,
        &["0"],
    );
}

#[test]
fn for_loop_break() {
    run_expect(
        r#"
function main() {
    let mut last: Int = 0
    for i in 0..100 {
        if i == 5 { break }
        last = i
    }
    print(last)
}
"#,
        &["4"],
    );
}

#[test]
fn for_loop_continue() {
    run_expect(
        r#"
function main() {
    let mut total: Int = 0
    for i in 0..10 {
        if i % 2 == 0 { continue }
        total = total + i
    }
    print(total)
}
"#,
        &["25"],
    );
}

// --- Boolean short-circuit evaluation ---

#[test]
fn short_circuit_and_does_not_evaluate_rhs() {
    run_expect(
        r#"
function main() {
    let x: Bool = false && true
    print(x)
}
"#,
        &["false"],
    );
}

#[test]
fn short_circuit_or_does_not_evaluate_rhs() {
    run_expect(
        r#"
function main() {
    let x: Bool = true || false
    print(x)
}
"#,
        &["true"],
    );
}

// =========================================================================
// Comprehensive test audit: missing coverage
// =========================================================================

// ── Arithmetic & Operators ────────────────────────────────────────────

#[test]
fn float_addition() {
    run_expect(
        r#"
function main() {
    let x: Float = 1.5 + 2.5
    print(x)
}
"#,
        &["4"],
    );
}

#[test]
fn float_subtraction() {
    run_expect(
        r#"
function main() {
    let x: Float = 10.5 - 3.2
    print(x)
}
"#,
        &["7.3"],
    );
}

#[test]
fn float_multiplication() {
    run_expect(
        r#"
function main() {
    let x: Float = 2.5 * 4.0
    print(x)
}
"#,
        &["10"],
    );
}

#[test]
fn integer_modulo() {
    run_expect(
        r#"
function main() {
    print(10 % 3)
    print(7 % 2)
    print(6 % 3)
}
"#,
        &["1", "1", "0"],
    );
}

#[test]
fn modulo_by_zero_runtime_error() {
    expect_runtime_error(
        r#"
function main() {
    let x: Int = 10 % 0
}
"#,
        "modulo by zero",
    );
}

#[test]
fn float_modulo() {
    run_expect(
        r#"
function main() {
    let x: Float = 10.5 % 3.0
    print(x)
}
"#,
        &["1.5"],
    );
}

#[test]
fn float_modulo_by_zero_runtime_error() {
    expect_runtime_error(
        r#"
function main() {
    let x: Float = 10.5 % 0.0
}
"#,
        "modulo by zero",
    );
}

#[test]
fn unary_negation_float() {
    run_expect(
        r#"
function main() {
    let x: Float = 3.14
    let y: Float = -x
    print(y)
}
"#,
        &["-3.14"],
    );
}

#[test]
fn unary_not_operator() {
    run_expect(
        r#"
function main() {
    let a: Bool = true
    let b: Bool = !a
    print(b)
    print(!false)
}
"#,
        &["false", "true"],
    );
}

#[test]
fn comparison_operators_float() {
    run_expect(
        r#"
function main() {
    print(1.5 < 2.5)
    print(2.5 > 1.5)
    print(1.5 <= 1.5)
    print(2.5 >= 2.5)
    print(1.5 == 1.5)
    print(1.5 != 2.5)
}
"#,
        &["true", "true", "true", "true", "true", "true"],
    );
}

#[test]
fn comparison_operators_string_equality() {
    run_expect(
        r#"
function main() {
    print("abc" == "abc")
    print("abc" != "def")
    print("abc" == "def")
    print("abc" != "abc")
}
"#,
        &["true", "true", "false", "false"],
    );
}

#[test]
fn string_ordering_less_than() {
    run_expect(
        r#"
function main() {
    print("abc" < "def")
    print("def" < "abc")
    print("abc" < "abc")
}
"#,
        &["true", "false", "false"],
    );
}

#[test]
fn string_ordering_greater_than() {
    run_expect(
        r#"
function main() {
    print("xyz" > "abc")
    print("abc" > "xyz")
}
"#,
        &["true", "false"],
    );
}

#[test]
fn string_ordering_less_than_or_equal() {
    run_expect(
        r#"
function main() {
    print("abc" <= "abc")
    print("abc" <= "def")
    print("def" <= "abc")
}
"#,
        &["true", "true", "false"],
    );
}

#[test]
fn string_ordering_greater_than_or_equal() {
    run_expect(
        r#"
function main() {
    print("abc" >= "abc")
    print("def" >= "abc")
    print("abc" >= "def")
}
"#,
        &["true", "true", "false"],
    );
}

#[test]
fn string_ordering_in_conditional() {
    run_expect(
        r#"
function comesBefore(a: String, b: String) -> Bool {
    return a < b
}
function main() {
    print(comesBefore("apple", "banana"))
    print(comesBefore("zebra", "aardvark"))
}
"#,
        &["true", "false"],
    );
}

#[test]
fn boolean_equality() {
    run_expect(
        r#"
function main() {
    print(true == true)
    print(false == false)
    print(true != false)
    print(true == false)
}
"#,
        &["true", "true", "true", "false"],
    );
}

#[test]
fn not_equals_operator() {
    run_expect(
        r#"
function main() {
    print(1 != 2)
    print(1 != 1)
    print("a" != "b")
    print("a" != "a")
}
"#,
        &["true", "false", "true", "false"],
    );
}

#[test]
fn integer_subtraction_overflow() {
    expect_runtime_error(
        r#"
function main() {
    let x: Int = -9223372036854775807 - 1
    let y: Int = x - 1
}
"#,
        "integer overflow",
    );
}

#[test]
fn mixed_type_arithmetic_error() {
    expect_type_error(
        r#"
function main() {
    let x: Int = 1 + 1.5
}
"#,
        "cannot apply",
    );
}

#[test]
fn add_incompatible_types_error() {
    expect_type_error(
        r#"
function main() {
    let x: Int = 1 + "hello"
}
"#,
        "cannot apply",
    );
}

// ── Variable Scoping ──────────────────────────────────────────────────

#[test]
fn variable_shadowing_inner_scope() {
    run_expect(
        r#"
function main() {
    let x: Int = 10
    if true {
        let x: Int = 20
        print(x)
    }
    print(x)
}
"#,
        &["20", "10"],
    );
}

#[test]
fn variable_not_visible_outside_if_block() {
    expect_type_error(
        r#"
function main() {
    if true {
        let inner: Int = 42
    }
    print(inner)
}
"#,
        "undefined variable",
    );
}

#[test]
fn variable_not_visible_outside_while_block() {
    expect_type_error(
        r#"
function main() {
    while false {
        let inner: Int = 42
    }
    print(inner)
}
"#,
        "undefined variable",
    );
}

#[test]
fn variable_not_visible_outside_for_block() {
    expect_type_error(
        r#"
function main() {
    for i in 0..5 {
        let inner: Int = 42
    }
    print(inner)
}
"#,
        "undefined variable",
    );
}

#[test]
fn for_loop_variable_scoped_to_body() {
    expect_type_error(
        r#"
function main() {
    for i in 0..5 { }
    print(i)
}
"#,
        "undefined variable",
    );
}

// ── For Loop Edge Cases ───────────────────────────────────────────────

#[test]
fn for_loop_reversed_range_does_nothing() {
    run_expect(
        r#"
function main() {
    let mut count: Int = 0
    for i in 10..0 {
        count = count + 1
    }
    print(count)
}
"#,
        &["0"],
    );
}

#[test]
fn for_loop_with_expression_bounds() {
    run_expect(
        r#"
function getStart() -> Int { return 2 }
function getEnd() -> Int { return 5 }
function main() {
    let mut sum: Int = 0
    for i in getStart()..getEnd() {
        sum = sum + i
    }
    print(sum)
}
"#,
        &["9"],
    );
}

#[test]
fn for_loop_nested() {
    run_expect(
        r#"
function main() {
    let mut sum: Int = 0
    for i in 0..3 {
        for j in 0..3 {
            sum = sum + 1
        }
    }
    print(sum)
}
"#,
        &["9"],
    );
}

#[test]
fn for_loop_return_from_function() {
    run_expect(
        r#"
function findFirstEven() -> Int {
    for i in 1..10 {
        if i % 2 == 0 { return i }
    }
    return -1
}
function main() {
    print(findFirstEven())
}
"#,
        &["2"],
    );
}

// ── toString() Coverage ───────────────────────────────────────────────

#[test]
fn to_string_on_all_types() {
    run_expect(
        r#"
function main() {
    print(toString(42))
    print(toString(3.14))
    print(toString(true))
    print(toString(false))
    print(toString("hello"))
}
"#,
        &["42", "3.14", "true", "false", "hello"],
    );
}

#[test]
fn to_string_on_struct() {
    run_expect(
        r#"
struct Point { Int x  Int y }
function main() {
    let p: Point = Point(3, 4)
    print(toString(p))
}
"#,
        &["Point(x: 3, y: 4)"],
    );
}

#[test]
fn to_string_on_list() {
    run_expect(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3]
    print(toString(nums))
}
"#,
        &["[1, 2, 3]"],
    );
}

// ── Block Comments ────────────────────────────────────────────────────

#[test]
fn block_comment_basic() {
    run_expect(
        r#"
/* this is a comment */
function main() {
    print("ok")
}
"#,
        &["ok"],
    );
}

#[test]
fn block_comment_nested() {
    run_expect(
        r#"
/* outer /* inner */ still comment */
function main() {
    print("ok")
}
"#,
        &["ok"],
    );
}

#[test]
fn line_comment_basic() {
    run_expect(
        r#"
// this is a comment
function main() {
    // another comment
    print("ok") // inline comment
}
"#,
        &["ok"],
    );
}

// ── Type Checking Edge Cases ──────────────────────────────────────────

#[test]
fn if_condition_must_be_bool() {
    expect_type_error(
        r#"
function main() {
    if 42 {
        print("bad")
    }
}
"#,
        "if condition must be Bool",
    );
}

#[test]
fn while_condition_must_be_bool() {
    expect_type_error(
        r#"
function main() {
    while "yes" {
        print("bad")
    }
}
"#,
        "while condition must be Bool",
    );
}

#[test]
fn break_outside_loop_error() {
    expect_type_error(
        r#"
function main() {
    break
}
"#,
        "outside of loop",
    );
}

#[test]
fn continue_outside_loop_error() {
    expect_type_error(
        r#"
function main() {
    continue
}
"#,
        "outside of loop",
    );
}

#[test]
fn cannot_negate_string() {
    expect_type_error(
        r#"
function main() {
    let x: Int = -"hello"
}
"#,
        "cannot negate",
    );
}

#[test]
fn cannot_not_int() {
    expect_type_error(
        r#"
function main() {
    let x: Bool = !42
}
"#,
        "cannot apply `!`",
    );
}

#[test]
fn and_requires_bool_operands() {
    expect_type_error(
        r#"
function main() {
    let x: Bool = 1 && 2
}
"#,
        "must be Bool",
    );
}

#[test]
fn or_requires_bool_operands() {
    expect_type_error(
        r#"
function main() {
    let x: Bool = 1 || 2
}
"#,
        "must be Bool",
    );
}

#[test]
fn for_range_must_be_int() {
    expect_type_error(
        r#"
function main() {
    for i in "a".."z" {
        print(i)
    }
}
"#,
        "must be Int",
    );
}

#[test]
fn while_loop_with_option() {
    run_expect(
        r#"
function main() {
    let mut opt: Option<Int> = Some(5)
    let mut sum: Int = 0
    while opt.isSome() {
        let val: Int = opt.unwrap()
        sum = sum + val
        if val <= 1 {
            opt = None
        } else {
            opt = Some(val - 1)
        }
    }
    print(sum)
}
"#,
        &["15"],
    );
}

#[test]
fn deeply_nested_if_else() {
    run_expect(
        r#"
function classify(n: Int) -> String {
    if n < 0 {
        return "negative"
    } else if n == 0 {
        return "zero"
    } else if n < 10 {
        return "small"
    } else if n < 100 {
        return "medium"
    } else {
        return "large"
    }
}
function main() {
    print(classify(-5))
    print(classify(0))
    print(classify(5))
    print(classify(50))
    print(classify(500))
}
"#,
        &["negative", "zero", "small", "medium", "large"],
    );
}

#[test]
fn while_loop_with_break_return_value() {
    run_expect(
        r#"
function findFirstDivisible(target: Int) -> Int {
    let mut i: Int = 1
    while i < 100 {
        if target % i == 0 && i > 1 {
            return i
        }
        i = i + 1
    }
    return -1
}
function main() {
    print(findFirstDivisible(12))
}
"#,
        &["2"],
    );
}

// ── Comparison on incompatible types (runtime) ────────────────────────

#[test]
fn compare_incompatible_types_at_runtime_error() {
    expect_type_error(
        r#"
function main() {
    print(42 < "hello")
}
"#,
        "cannot compare",
    );
}

// ── For loop with non-int range (type error) ──────────────────────────

#[test]
fn for_range_start_must_be_int() {
    expect_type_error(
        r#"
function main() {
    for i in 1.5..10 {
        print(i)
    }
}
"#,
        "for range start must be Int",
    );
}

#[test]
fn for_range_end_must_be_int() {
    expect_type_error(
        r#"
function main() {
    for i in 0..10.5 {
        print(i)
    }
}
"#,
        "for range end must be Int",
    );
}

// ── Print and toString on Void ────────────────────────────────────────

#[test]
fn print_void_result() {
    run_expect(
        r#"
function sideEffect() {
    let x: Int = 42
}
function main() {
    sideEffect()
    print("after side effect")
}
"#,
        &["after side effect"],
    );
}

// ── Mutual recursion ──────────────────────────────────────────────────

#[test]
fn mutual_recursion() {
    run_expect(
        r#"
function isEven(n: Int) -> Bool {
    if n == 0 { return true }
    return isOdd(n - 1)
}
function isOdd(n: Int) -> Bool {
    if n == 0 { return false }
    return isEven(n - 1)
}
function main() {
    print(isEven(4))
    print(isOdd(3))
    print(isEven(5))
}
"#,
        &["true", "true", "false"],
    );
}

// ── Return from within nested if/else blocks ──────────────────────────

#[test]
fn return_from_nested_if_else() {
    run_expect(
        r#"
function classify(a: Int, b: Int) -> String {
    if a > 0 {
        if b > 0 {
            return "both positive"
        } else {
            return "a positive, b non-positive"
        }
    } else {
        if b > 0 {
            return "a non-positive, b positive"
        } else {
            return "both non-positive"
        }
    }
}
function main() {
    print(classify(1, 1))
    print(classify(1, -1))
    print(classify(-1, 1))
    print(classify(-1, -1))
}
"#,
        &[
            "both positive",
            "a positive, b non-positive",
            "a non-positive, b positive",
            "both non-positive",
        ],
    );
}

// ── Nested for loops with break ───────────────────────────────────────

#[test]
fn nested_for_loop_break_only_inner() {
    run_expect(
        r#"
function main() {
    let mut count: Int = 0
    for i in 0..3 {
        for j in 0..10 {
            if j == 2 { break }
            count = count + 1
        }
    }
    print(count)
}
"#,
        &["6"],
    );
}

// ============================================================================
// Output-validated tests (using run_expect / run_capturing)
// ============================================================================

#[test]
fn output_hello_world() {
    run_expect(
        r#"function main() { print("Hello, World!") }"#,
        &["Hello, World!"],
    );
}

#[test]
fn output_fizzbuzz() {
    run_expect(
        r#"
function fizzbuzz(n: Int) -> String {
  if n % 15 == 0 { return "FizzBuzz" }
  if n % 3 == 0 { return "Fizz" }
  if n % 5 == 0 { return "Buzz" }
  return toString(n)
}
function main() {
  print(fizzbuzz(15))
  print(fizzbuzz(3))
  print(fizzbuzz(5))
  print(fizzbuzz(7))
}
"#,
        &["FizzBuzz", "Fizz", "Buzz", "7"],
    );
}

#[test]
fn output_fibonacci() {
    run_expect(
        r#"
function fib(n: Int) -> Int {
  if n <= 1 { return n }
  return fib(n - 1) + fib(n - 2)
}
function main() { print(fib(10)) }
"#,
        &["55"],
    );
}

#[test]
fn output_while_loop_sum() {
    run_expect(
        r#"
function main() {
  let mut sum: Int = 0
  let mut i: Int = 1
  while i <= 10 {
    sum = sum + i
    i = i + 1
  }
  print(sum)
}
"#,
        &["55"],
    );
}

#[test]
fn output_for_loop_sum() {
    run_expect(
        r#"
function main() {
  let mut sum: Int = 0
  for i in 0..10 {
    sum = sum + i
  }
  print(sum)
}
"#,
        &["45"],
    );
}

#[test]
fn output_option_unwrap() {
    run_expect(
        r#"
function main() {
  let a: Option<Int> = Some(42)
  let b: Option<Int> = None
  print(a.unwrap())
  print(b.isNone())
  print(a.unwrapOr(0))
  print(b.unwrapOr(99))
}
"#,
        &["42", "true", "42", "99"],
    );
}

// ── Loop else clauses ───────────────────────────────────────────────────

#[test]
fn loop_else_for_no_break() {
    run_expect(
        r#"
function main() {
    for i in 0..3 {
        print(i)
    } else {
        print("done")
    }
}
"#,
        &["0", "1", "2", "done"],
    );
}

#[test]
fn loop_else_for_with_break() {
    run_expect(
        r#"
function main() {
    for i in 0..5 {
        if i == 2 { break }
        print(i)
    } else {
        print("should not print")
    }
}
"#,
        &["0", "1"],
    );
}

#[test]
fn loop_else_while_no_break() {
    run_expect(
        r#"
function main() {
    let mut i: Int = 0
    while i < 3 {
        print(i)
        i = i + 1
    } else {
        print("done")
    }
}
"#,
        &["0", "1", "2", "done"],
    );
}

#[test]
fn loop_else_while_with_break() {
    run_expect(
        r#"
function main() {
    let mut i: Int = 0
    while i < 10 {
        if i == 2 { break }
        print(i)
        i = i + 1
    } else {
        print("should not print")
    }
}
"#,
        &["0", "1"],
    );
}

// ── Loop else edge cases ────────────────────────────────────────────────

#[test]
fn loop_else_with_continue() {
    // continue should NOT prevent the else block from running
    run_expect(
        r#"
function main() {
    for i in 0..5 {
        if i % 2 == 0 { continue }
        print(i)
    } else {
        print("done")
    }
}
"#,
        &["1", "3", "done"],
    );
}

#[test]
fn loop_else_empty_range() {
    run_expect(
        r#"
function main() {
    for i in 0..0 {
        print(i)
    } else {
        print("empty")
    }
}
"#,
        &["empty"],
    );
}

#[test]
fn loop_else_nested() {
    run_expect(
        r#"
function main() {
    for i in 0..2 {
        for j in 0..2 {
            if j == 1 { break }
            print(j)
        } else {
            print("inner done")
        }
    } else {
        print("outer done")
    }
}
"#,
        &["0", "0", "outer done"],
    );
}

// ── Loop else: with return, while+continue ──────────────────────────────

#[test]
fn loop_else_return_from_else() {
    run_expect(
        r#"
function findOrDefault() -> Int {
    for i in 0..3 {
        if i == 99 { return i }
    } else {
        return 42
    }
    return 0
}
function main() {
    print(findOrDefault())
}
"#,
        &["42"],
    );
}

#[test]
fn while_else_with_continue() {
    run_expect(
        r#"
function main() {
    let mut i: Int = 0
    while i < 5 {
        i = i + 1
        if i % 2 == 0 { continue }
        print(i)
    } else {
        print("done")
    }
}
"#,
        &["1", "3", "5", "done"],
    );
}

#[test]
fn for_empty_range_no_iterations() {
    run_expect(
        r#"
function main() {
    let mut count: Int = 0
    for i in 5..5 {
        count = count + 1
    }
    print(count)
}
"#,
        &["0"],
    );
}

#[test]
fn for_reversed_range_no_iterations_verified() {
    run_expect(
        r#"
function main() {
    let mut count: Int = 0
    for i in 5..3 {
        count = count + 1
    }
    print(count)
}
"#,
        &["0"],
    );
}

#[test]
fn while_else_condition_false_from_start() {
    run_expect(
        r#"
function main() {
    while false {
        print("body")
    } else {
        print("else")
    }
}
"#,
        &["else"],
    );
}

#[test]
fn negative_float_modulo() {
    run_expect(
        r#"
function main() {
    let x: Float = -5.0 % 3.0
    print(x)
}
"#,
        &["-2"],
    );
}

// ══════════════════════════════════════════════════════════════════════
// P1 — Critical feature interaction gaps
// ══════════════════════════════════════════════════════════════════════

/// Variable shadowing inside if blocks.
#[test]
fn variable_shadowing_in_if_block() {
    run_expect(
        r#"
function main() {
  let x: Int = 1
  if true {
    let x: Int = 2
    print(x)
  }
  print(x)
}
"#,
        &["2", "1"],
    );
}

/// Variable shadowing inside while loops.
#[test]
fn variable_shadowing_in_while_loop() {
    run_expect(
        r#"
function main() {
  let x: String = "outer"
  let mut i: Int = 0
  while i < 2 {
    let x: String = "inner"
    print(x)
    i = i + 1
  }
  print(x)
}
"#,
        &["inner", "inner", "outer"],
    );
}

/// Integer overflow on addition produces error.
#[test]
fn integer_overflow_addition() {
    expect_runtime_error(
        r#"
function main() {
  let max: Int = 9223372036854775807
  let result: Int = max + 1
}
"#,
        "integer overflow",
    );
}

/// Integer overflow on subtraction produces error.
#[test]
fn integer_underflow_subtraction() {
    expect_runtime_error(
        r#"
function main() {
  let min: Int = -9223372036854775807
  let result: Int = min - 2
}
"#,
        "integer overflow",
    );
}

/// Loop else with early return — else should NOT run.
#[test]
fn loop_else_with_early_return() {
    run_expect(
        r#"
function findFirstEven(xs: List<Int>) -> String {
  for x in xs {
    if x % 2 == 0 {
      return "found: " + toString(x)
    }
  } else {
    return "none found"
  }
  return "unreachable"
}
function main() {
  print(findFirstEven([1, 3, 4, 5]))
  print(findFirstEven([1, 3, 5]))
}
"#,
        &["found: 4", "none found"],
    );
}

/// Nested block comments.
#[test]
fn nested_block_comments() {
    run_expect(
        r#"
function main() {
  /* outer /* inner */ still comment */
  print("alive")
}
"#,
        &["alive"],
    );
}

/// Negating Int::MIN must produce an overflow error at runtime (e2e).
#[test]
fn integer_negation_overflow_e2e() {
    expect_runtime_error(
        r#"
function main() {
  let x: Int = -9223372036854775807 - 1
  print(-x)
}
"#,
        "overflow",
    );
}

/// The `..` range operator suppresses newlines after it in for loops.
#[test]
fn dotdot_newline_suppression_in_for_loop() {
    run_expect(
        r#"
function main() {
  let mut sum: Int = 0
  let end: Int = 5
  for i in 0..
    end {
    sum = sum + i
  }
  print(sum)
}
"#,
        &["10"],
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Edge case: variable shadowing across scopes
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn variable_shadowing_in_for_loop() {
    run_expect(
        r#"
function main() {
  let x: String = "outer"
  for i in 0..3 {
    let x: Int = i * 10
    print(x)
  }
  print(x)
}
"#,
        &["0", "10", "20", "outer"],
    );
}

#[test]
fn variable_shadowing_in_function_params() {
    run_expect(
        r#"
function show(x: Int) {
  print(x)
}
function main() {
  let x: Int = 42
  print(x)
  show(99)
  print(x)
}
"#,
        &["42", "99", "42"],
    );
}

#[test]
fn same_scope_redefinition_is_error() {
    expect_type_error(
        r#"
function main() {
  let x: Int = 10
  let x: String = "hello"
}
"#,
        "already defined",
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Edge case: floating-point precision
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn float_point_one_plus_point_two() {
    // IEEE 754 double-precision: 0.1 + 0.2 != 0.3
    run_expect(
        r#"
function main() {
  let result: Float = 0.1 + 0.2
  print(result)
  print(result == 0.3)
}
"#,
        &["0.30000000000000004", "false"],
    );
}

#[test]
fn float_precision_subtraction() {
    run_expect(
        r#"
function main() {
  let a: Float = 1.0
  let b: Float = 0.9
  let diff: Float = a - b
  // Should not be exactly 0.1 due to IEEE 754
  print(diff == 0.1)
}
"#,
        &["false"],
    );
}

#[test]
fn float_large_value_precision() {
    run_expect(
        r#"
function main() {
  let big: Float = 9007199254740993.0
  print(big == 9007199254740992.0)
}
"#,
        &["true"],
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Edge case: i64::MIN overflow
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn i64_min_minus_one_overflows() {
    expect_runtime_error(
        r#"
function main() {
  let x: Int = -9223372036854775807 - 1
  print(x - 1)
}
"#,
        "integer overflow",
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Compound assignment operators (+=, -=, *=, /=, %=)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn compound_plus_equals() {
    run_expect(
        r#"
function main() {
  let mut x: Int = 1
  x += 2
  print(x)
}
"#,
        &["3"],
    );
}

#[test]
fn compound_minus_equals() {
    run_expect(
        r#"
function main() {
  let mut x: Int = 10
  x -= 3
  print(x)
}
"#,
        &["7"],
    );
}

#[test]
fn compound_star_equals() {
    run_expect(
        r#"
function main() {
  let mut x: Int = 4
  x *= 5
  print(x)
}
"#,
        &["20"],
    );
}

#[test]
fn compound_slash_equals() {
    run_expect(
        r#"
function main() {
  let mut x: Int = 10
  x /= 2
  print(x)
}
"#,
        &["5"],
    );
}

#[test]
fn compound_percent_equals() {
    run_expect(
        r#"
function main() {
  let mut x: Int = 10
  x %= 3
  print(x)
}
"#,
        &["1"],
    );
}

#[test]
fn compound_assignment_in_loop() {
    run_expect(
        r#"
function main() {
  let mut sum: Int = 0
  for i in 0..5 {
    sum += i
  }
  print(sum)
}
"#,
        &["10"],
    );
}
