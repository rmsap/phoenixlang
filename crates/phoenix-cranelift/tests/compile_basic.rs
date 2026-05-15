//! Integration tests: basic compilation and execution of Phoenix programs.

mod common;
use common::{compile_and_run, roundtrip};

#[test]
fn hello_world() {
    roundtrip(r#"function main() { print("Hello, World!") }"#);
}

#[test]
fn integer_arithmetic() {
    roundtrip(
        r#"
function main() {
    print(2 + 3)
    print(10 - 4)
    print(3 * 7)
    print(15 / 4)
    print(17 % 5)
    print(-42)
}
"#,
    );
}

#[test]
fn float_arithmetic() {
    roundtrip(
        r#"
function main() {
    print(2.5 + 1.5)
    print(10.0 - 3.5)
    print(2.0 * 3.5)
    print(7.0 / 2.0)
}
"#,
    );
}

/// FMod must use truncation-toward-zero
/// semantics to match the interpreter (Rust `%`), not floor-based modulo.
#[test]
fn float_modulo_negative() {
    roundtrip(
        r#"
function main() {
    print(7.0 % 3.0)
    print(-7.0 % 3.0)
    print(7.0 % -3.0)
    print(-7.0 % -3.0)
}
"#,
    );
}

#[test]
fn boolean_ops() {
    roundtrip(
        r#"
function main() {
    print(true)
    print(false)
    print(1 == 1)
    print(1 != 2)
    print(3 < 5)
    print(5 > 3)
}
"#,
    );
}

#[test]
fn string_concat_and_print() {
    roundtrip(
        r#"
function main() {
    let a: String = "hello"
    let b: String = " world"
    print(a + b)
}
"#,
    );
}

#[test]
fn string_comparison() {
    roundtrip(
        r#"
function main() {
    print("abc" == "abc")
    print("abc" != "def")
    print("abc" < "def")
    print("xyz" > "abc")
}
"#,
    );
}

#[test]
fn if_else() {
    roundtrip(
        r#"
function main() {
    let x: Int = 10
    if x > 5 {
        print("big")
    } else {
        print("small")
    }
}
"#,
    );
}

#[test]
fn while_loop() {
    roundtrip(
        r#"
function main() {
    let mut i: Int = 0
    while i < 5 {
        print(i)
        i += 1
    }
}
"#,
    );
}

#[test]
fn function_calls() {
    roundtrip(
        r#"
function add(a: Int, b: Int) -> Int {
    a + b
}
function main() {
    print(add(3, 4))
    print(add(10, 20))
}
"#,
    );
}

#[test]
fn recursive_function() {
    roundtrip(
        r#"
function fib(n: Int) -> Int {
    if n <= 1 {
        return n
    }
    fib(n - 1) + fib(n - 2)
}
function main() {
    print(fib(10))
}
"#,
    );
}

#[test]
fn mutable_variables() {
    roundtrip(
        r#"
function main() {
    let mut x: Int = 1
    x = x + 10
    x += 5
    print(x)
}
"#,
    );
}

#[test]
fn to_string_builtin() {
    roundtrip(
        r#"
function main() {
    print(toString(42))
    print(toString(3.14))
    print(toString(true))
    print(toString("hello"))
}
"#,
    );
}

#[test]
fn string_interpolation() {
    roundtrip(
        r#"
function main() {
    let name: String = "Phoenix"
    let age: Int = 1
    print("name={name}, age={toString(age)}")
}
"#,
    );
}

/// Test that a compiled binary that divides by zero exits with a non-zero
/// exit code and prints a runtime error.
#[test]
fn division_by_zero_panics() {
    common::expect_panic(
        r#"
function main() {
    let x: Int = 0
    print(10 / x)
}
"#,
        "division by zero",
    );
}

/// Test deeply nested control flow: if inside while.
#[test]
fn nested_control_flow() {
    roundtrip(
        r#"
function main() {
    let mut i: Int = 0
    while i < 5 {
        if i % 2 == 0 {
            print(i)
        }
        i += 1
    }
}
"#,
    );
}

#[test]
fn no_main_function_error() {
    let tokens = phoenix_lexer::lexer::tokenize(
        "function foo() { print(1) }",
        phoenix_common::span::SourceId(0),
    );
    let (program, _) = phoenix_parser::parser::parse(&tokens);
    let result = phoenix_sema::checker::check(&program);
    let module = phoenix_ir::lower(&program, &result.module);
    let err = phoenix_cranelift::compile(&module, phoenix_cranelift::Target::Native);
    assert!(err.is_err());
    assert!(err.unwrap_err().message.contains("no main function"));
}

/// Test mutable String variables.
///
/// Exercises the Alloca/Load/Store path for StringRef (fat pointer),
/// which requires 16-byte stack slots and multi-word load/store.
#[test]
fn mutable_string_variable() {
    roundtrip(
        r#"
function main() {
    let mut s: String = "hello"
    print(s)
    s = "world"
    print(s)
}
"#,
    );
}

/// Test mutable String variable with concatenation.
#[test]
fn mutable_string_concat() {
    roundtrip(
        r#"
function main() {
    let mut msg: String = "hello"
    msg = msg + " world"
    print(msg)
}
"#,
    );
}

/// Test mutable Float variable to ensure non-StringRef types still work.
#[test]
fn mutable_float_variable() {
    roundtrip(
        r#"
function main() {
    let mut x: Float = 1.5
    x = x + 2.5
    print(x)
}
"#,
    );
}

/// Test mutable Bool variable.
#[test]
fn mutable_bool_variable() {
    roundtrip(
        r#"
function main() {
    let mut flag: Bool = true
    print(flag)
    flag = false
    print(flag)
}
"#,
    );
}

/// Negating i64::MIN should wrap (not panic).
#[test]
fn integer_overflow_negate_min() {
    roundtrip(
        r#"
function main() {
    let min = -9223372036854775807 - 1
    let neg = 0 - min
    print(neg)
}
"#,
    );
}

/// i64::MIN / -1 should wrap to i64::MIN (not panic).
#[test]
fn integer_overflow_div_min_by_neg1() {
    roundtrip(
        r#"
function main() {
    let min = -9223372036854775807 - 1
    let result = min / -1
    print(result)
}
"#,
    );
}

/// Assert that i64::MIN / -1 produces i64::MIN (wrapping behavior).
#[test]
fn integer_overflow_div_min_by_neg1_value() {
    let out = compile_and_run(
        r#"
function main() {
    let min = -9223372036854775807 - 1
    let result = min / -1
    print(result)
}
"#,
    );
    assert_eq!(out, vec!["-9223372036854775808"]);
}

/// Assert that -i64::MIN wraps to i64::MIN.
#[test]
fn integer_overflow_negate_min_value() {
    let out = compile_and_run(
        r#"
function main() {
    let min = -9223372036854775807 - 1
    let neg = 0 - min
    print(neg)
}
"#,
    );
    assert_eq!(out, vec!["-9223372036854775808"]);
}

/// 1.0 / 0.0 should produce "inf", not panic.
#[test]
fn float_div_by_zero_produces_inf() {
    let out = compile_and_run(
        r#"
function main() {
    let x: Float = 1.0
    let y: Float = 0.0
    print(x / y)
}
"#,
    );
    assert_eq!(out, vec!["inf"]);
}

/// 1.0 % 0.0 should produce "NaN", not panic.
#[test]
fn float_mod_by_zero_produces_nan() {
    let out = compile_and_run(
        r#"
function main() {
    let x: Float = 1.0
    let y: Float = 0.0
    print(x % y)
}
"#,
    );
    assert_eq!(out, vec!["NaN"]);
}

/// i64::MIN % -1 should produce 0 (safe modulo handling).
#[test]
fn integer_overflow_mod_min_by_neg1() {
    let out = compile_and_run(
        r#"
function main() {
    let x: Int = -9223372036854775807 - 1
    let result = x % -1
    print(result)
}
"#,
    );
    assert_eq!(out, vec!["0"]);
}

/// Two chained `?` operators in the same function body.
#[test]
fn try_operator_chained_two_steps() {
    let out = compile_and_run(
        r#"
function step1() -> Result<Int, String> { Ok(10) }
function step2(x: Int) -> Result<Int, String> { Ok(x + 1) }
function combined() -> Result<Int, String> {
    let a = step1()?
    let b = step2(a)?
    Ok(b)
}
function main() {
    match combined() {
        Ok(v) -> { print(v) }
        Err(e) -> { print(e) }
    }
}
"#,
    );
    assert_eq!(out, vec!["11"]);
}

/// Chained `?` operators where the second step fails.
#[test]
fn try_operator_chained_second_fails() {
    let out = compile_and_run(
        r#"
function step1() -> Result<Int, String> { Ok(10) }
function step2(x: Int) -> Result<Int, String> { Err("step2 failed") }
function combined() -> Result<Int, String> {
    let a = step1()?
    let b = step2(a)?
    Ok(b)
}
function main() {
    match combined() {
        Ok(v) -> { print(v) }
        Err(e) -> { print(e) }
    }
}
"#,
    );
    assert_eq!(out, vec!["step2 failed"]);
}

/// User-defined generic enums are not yet supported in Cranelift codegen.
#[test]
#[ignore = "requires user-defined generic enum codegen not yet implemented"]
fn user_defined_generic_enum() {
    let _out = compile_and_run(
        r#"
enum Either<L, R> {
    Left(L)
    Right(R)
}
function main() {
    let x: Either<Int, String> = Left(42)
    match x {
        Left(v) -> print(v)
        Right(s) -> print(s)
    }
}
"#,
    );
}
