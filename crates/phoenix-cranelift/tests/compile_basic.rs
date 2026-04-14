//! Integration tests: basic compilation and execution of Phoenix programs.

mod common;
use common::roundtrip;

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
    let source = r#"
function main() {
    let x: Int = 0
    print(10 / x)
}
"#;
    let obj_bytes = common::compile_to_obj(source);

    let dir = std::env::temp_dir().join("phoenix_cranelift_tests");
    std::fs::create_dir_all(&dir).unwrap();
    let obj_path = dir.join("divzero_test.o");
    let exe_path = dir.join("divzero_test");

    std::fs::write(&obj_path, &obj_bytes).unwrap();

    // Link.
    let status = std::process::Command::new("cc")
        .arg("-o")
        .arg(exe_path.to_str().unwrap())
        .arg(obj_path.to_str().unwrap())
        .arg(format!("-L{}", common::runtime_dir()))
        .arg("-lphoenix_runtime")
        .arg("-lpthread")
        .arg("-ldl")
        .arg("-lm")
        .status()
        .unwrap();
    assert!(status.success(), "linking failed");

    // Run — should exit with non-zero.
    let output = std::process::Command::new(exe_path.to_str().unwrap())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "expected non-zero exit for division by zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("division by zero"),
        "expected 'division by zero' in stderr, got: {stderr}"
    );

    let _ = std::fs::remove_file(&obj_path);
    let _ = std::fs::remove_file(&exe_path);
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
    let module = phoenix_ir::lower(&program, &result);
    let err = phoenix_cranelift::compile(&module);
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
