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
