//! Error-case tests: verify the IR interpreter produces correct runtime errors.

mod common;
use common::ir_run_result;

#[test]
fn error_division_by_zero_int() {
    let result = ir_run_result("function main() { print(1 / 0) }");
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("division by zero"));
}

#[test]
fn error_division_by_zero_float() {
    let result = ir_run_result("function main() { print(1.0 / 0.0) }");
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("division by zero"));
}

#[test]
fn error_modulo_by_zero_int() {
    let result = ir_run_result("function main() { print(1 % 0) }");
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("modulo by zero"));
}

#[test]
fn error_modulo_by_zero_float() {
    let result = ir_run_result("function main() { print(1.0 % 0.0) }");
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("modulo by zero"));
}

#[test]
fn error_stack_overflow() {
    let result = ir_run_result(
        r#"
function recurse(n: Int) -> Int {
    return recurse(n + 1)
}
function main() {
    print(recurse(0))
}
"#,
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("stack overflow"));
}

#[test]
fn error_list_out_of_bounds() {
    let result = ir_run_result(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3]
    print(nums.get(10))
}
"#,
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("out of bounds"));
}

#[test]
fn error_unwrap_none() {
    let result = ir_run_result(
        r#"
function main() {
    let x: Option<Int> = None
    print(x.unwrap())
}
"#,
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("unwrap"));
}
