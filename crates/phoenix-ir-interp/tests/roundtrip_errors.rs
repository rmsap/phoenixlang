//! Error-case tests: verify the IR interpreter produces correct runtime errors.

mod common;
use common::ir_run_result;

#[test]
fn error_division_by_zero_int() {
    let result = ir_run_result("function main() { print(1 / 0) }");
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("division by zero"));
}

/// Float division by zero produces IEEE 754 infinity, matching the compiler.
#[test]
fn float_division_by_zero_produces_inf() {
    let result = ir_run_result("function main() { print(1.0 / 0.0) }");
    assert!(
        result.is_ok(),
        "float div by zero should succeed with IEEE 754 semantics"
    );
}

#[test]
fn error_modulo_by_zero_int() {
    let result = ir_run_result("function main() { print(1 % 0) }");
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("modulo by zero"));
}

/// Float modulo by zero produces IEEE 754 NaN, matching the compiler.
#[test]
fn float_modulo_by_zero_produces_nan() {
    let result = ir_run_result("function main() { print(1.0 % 0.0) }");
    assert!(
        result.is_ok(),
        "float mod by zero should succeed with IEEE 754 semantics"
    );
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
