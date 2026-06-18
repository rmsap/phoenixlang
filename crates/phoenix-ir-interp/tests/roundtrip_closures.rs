//! Round-trip tests: closures and higher-order functions.

mod common;
use common::roundtrip;

#[test]
fn closure_basic() {
    roundtrip(
        r#"
function apply(f: (Int) -> Int, x: Int) -> Int {
    return f(x)
}
function main() {
    let double: (Int) -> Int = function(x: Int) -> Int { return x * 2 }
    print(apply(double, 5))
}
"#,
    );
}

#[test]
fn closure_capture() {
    roundtrip(
        r#"
function make_adder(n: Int) -> (Int) -> Int {
    return function(x: Int) -> Int { return x + n }
}
function main() {
    let add5: (Int) -> Int = make_adder(5)
    print(add5(10))
    print(add5(20))
}
"#,
    );
}

#[test]
fn nested_closures() {
    roundtrip(
        r#"
function make_multiplier(factor: Int) -> (Int) -> (Int) -> Int {
    return function(x: Int) -> (Int) -> Int {
        return function(y: Int) -> Int {
            return factor * x + y
        }
    }
}
function main() {
    let f: (Int) -> (Int) -> Int = make_multiplier(10)
    let g: (Int) -> Int = f(3)
    print(g(7))
}
"#,
    );
}

/// A `Void`-returning closure (`function() { print(...) }`, the common callback
/// shape) must lower and run on both interpreters. Regression for the lambda
/// body-coercion path that unconditionally coerced the trailing value — for a
/// `Void` body it looked up a type the void sentinel never recorded and tripped
/// a debug-only assert in `coerce_value_to_expected`. Now guarded on
/// `return_type != Void`, mirroring the top-level function-body path.
#[test]
fn void_returning_closure_called() {
    roundtrip(
        r#"
function run(f: () -> Void) {
    f()
}

function main() {
    let greet: () -> Void = function() { print("hi") }
    run(greet)
    run(function() { print("bye") })
}
"#,
    );
}
