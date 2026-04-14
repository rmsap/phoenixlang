//! Integration tests: closure compilation, including captures and indirect calls.

mod common;
use common::roundtrip;

#[test]
fn closure_no_captures() {
    roundtrip(
        r#"
function main() {
    let f: (Int) -> Int = function(x: Int) -> Int { x * 2 }
    print(f(5))
}
"#,
    );
}

#[test]
fn closure_with_int_capture() {
    roundtrip(
        r#"
function makeAdder(n: Int) -> (Int) -> Int {
    function(x: Int) -> Int { x + n }
}
function main() {
    let add5: (Int) -> Int = makeAdder(5)
    print(add5(10))
    print(add5(20))
}
"#,
    );
}

/// Closures capturing String values must
/// handle fat pointers (ptr + len), not just a single value.
#[test]
fn closure_with_string_capture() {
    roundtrip(
        r#"
function makeGreeter(greeting: String) -> (String) -> String {
    function(name: String) -> String { greeting + " " + name }
}
function main() {
    let greet: (String) -> String = makeGreeter("Hello")
    print(greet("World"))
    print(greet("Phoenix"))
}
"#,
    );
}

/// Indirect calls (CallIndirect) must
/// load captured values from the closure object and pass them as the
/// first arguments to the target function.
#[test]
fn closure_indirect_call_with_capture() {
    roundtrip(
        r#"
function applyTwice(f: (Int) -> Int, x: Int) -> Int {
    f(f(x))
}
function main() {
    let mul3: (Int) -> Int = function(x: Int) -> Int { x * 3 }
    print(applyTwice(mul3, 2))
}
"#,
    );
}

/// Test closure used as a higher-order function with capture.
#[test]
fn closure_higher_order_with_capture() {
    roundtrip(
        r#"
function makeMultiplier(factor: Int) -> (Int) -> Int {
    function(x: Int) -> Int { x * factor }
}
function apply(f: (Int) -> Int, val: Int) -> Int {
    f(val)
}
function main() {
    let double: (Int) -> Int = makeMultiplier(2)
    let triple: (Int) -> Int = makeMultiplier(3)
    print(apply(double, 5))
    print(apply(triple, 5))
}
"#,
    );
}

/// Two closures with the same user signature but different capture types,
/// each called independently (not through a shared variable).
#[test]
fn closure_different_captures_same_signature() {
    roundtrip(
        r#"
function makeIntAdder(n: Int) -> (Int) -> Int {
    function(x: Int) -> Int { x + n }
}
function makeDoubler() -> (Int) -> Int {
    let factor: Int = 2
    function(x: Int) -> Int { x * factor }
}
function main() {
    let add10: (Int) -> Int = makeIntAdder(10)
    let double: (Int) -> Int = makeDoubler()
    print(add10(5))
    print(double(5))
}
"#,
    );
}

#[test]
fn closure_multiple_captures() {
    roundtrip(
        r#"
function main() {
    let a: Int = 10
    let b: Int = 20
    let f: () -> Int = function() -> Int { a + b }
    print(f())
}
"#,
    );
}

/// Test mutable closure variable reassigned in control flow.
///
/// Exercises Alloca/Store/Load for ClosureRef values, ensuring
/// single-pointer closure types work through mutable variables.
#[test]
fn closure_mutable_variable() {
    roundtrip(
        r#"
function main() {
    let mut f: (Int) -> Int = function(n: Int) -> Int { n + 1 }
    print(f(100))
    f = function(n: Int) -> Int { n * 2 }
    print(f(100))
}
"#,
    );
}

/// Test closure with String capture passed as higher-order argument.
#[test]
fn closure_string_capture_higher_order() {
    roundtrip(
        r#"
function apply(f: (String) -> String, s: String) -> String {
    f(s)
}
function main() {
    let prefix: String = "Hello"
    let greet: (String) -> String = function(name: String) -> String { prefix + " " + name }
    print(apply(greet, "Phoenix"))
}
"#,
    );
}
