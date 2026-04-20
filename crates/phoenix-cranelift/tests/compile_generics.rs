//! Integration tests for compiled user-defined generic functions.
//!
//! Exercises the IR monomorphization pass end-to-end: each generic function
//! is specialized for every concrete instantiation used in `main`, and the
//! Cranelift backend compiles the specialized copies.

mod common;

use common::compile_and_run;

// ─────────────────────────────────────────────────────────────────
// Generic functions over value types
// ─────────────────────────────────────────────────────────────────

#[test]
fn identity_int() {
    let out = compile_and_run(
        r#"
function identity<T>(x: T) -> T { x }
function main() {
    print(identity(42))
}
"#,
    );
    assert_eq!(out, vec!["42"]);
}

#[test]
fn identity_string() {
    let out = compile_and_run(
        r#"
function identity<T>(x: T) -> T { x }
function main() {
    print(identity("hello"))
}
"#,
    );
    assert_eq!(out, vec!["hello"]);
}

#[test]
fn identity_called_at_int_and_string() {
    let out = compile_and_run(
        r#"
function identity<T>(x: T) -> T { x }
function main() {
    print(identity(42))
    print(identity("world"))
}
"#,
    );
    assert_eq!(out, vec!["42", "world"]);
}

#[test]
fn identity_bool_and_float() {
    let out = compile_and_run(
        r#"
function identity<T>(x: T) -> T { x }
function main() {
    print(identity(true))
    print(identity(3.14))
}
"#,
    );
    assert_eq!(out, vec!["true", "3.14"]);
}

// ─────────────────────────────────────────────────────────────────
// Multi type-param generics
// ─────────────────────────────────────────────────────────────────

#[test]
fn multi_type_param_first() {
    let out = compile_and_run(
        r#"
function first<A, B>(a: A, b: B) -> A { a }
function main() {
    print(first(10, "ignored"))
    print(first("kept", 99))
}
"#,
    );
    assert_eq!(out, vec!["10", "kept"]);
}

#[test]
fn multi_type_param_same_type() {
    let out = compile_and_run(
        r#"
function choose<A, B>(a: A, b: B) -> A { a }
function main() {
    print(choose(1, 2))
    print(choose(10, 20))
}
"#,
    );
    assert_eq!(out, vec!["1", "10"]);
}

// ─────────────────────────────────────────────────────────────────
// Generic calling generic (nested specialization discovery)
// ─────────────────────────────────────────────────────────────────

#[test]
fn generic_calling_generic() {
    let out = compile_and_run(
        r#"
function inner<T>(x: T) -> T { x }
function outer<T>(x: T) -> T { inner(x) }
function main() {
    print(outer(42))
    print(outer("hello"))
}
"#,
    );
    assert_eq!(out, vec!["42", "hello"]);
}

// ─────────────────────────────────────────────────────────────────
// Specialization at reference types must not
// produce symbol-unsafe mangled names. List / Map / closure / struct /
// enum arguments all previously broke Cranelift's symbol emission.
// ─────────────────────────────────────────────────────────────────

#[test]
fn specialize_at_list_of_int_compiles_and_runs() {
    let out = compile_and_run(
        r#"
function lengthOf<T>(xs: List<T>) -> Int { xs.length() }
function main() {
    let a: List<Int> = [1, 2, 3]
    print(lengthOf(a))
}
"#,
    );
    assert_eq!(out, vec!["3"]);
}

#[test]
fn specialize_at_list_of_string() {
    let out = compile_and_run(
        r#"
function lengthOf<T>(xs: List<T>) -> Int { xs.length() }
function main() {
    let a: List<String> = ["a", "b"]
    print(lengthOf(a))
}
"#,
    );
    assert_eq!(out, vec!["2"]);
}

#[test]
fn specialize_at_struct_type() {
    let out = compile_and_run(
        r#"
struct Point {
    Int x
    Int y
}
function identity<T>(v: T) -> T { v }
function main() {
    let p = Point(3, 4)
    let q = identity(p)
    print(q.x)
    print(q.y)
}
"#,
    );
    assert_eq!(out, vec!["3", "4"]);
}

#[test]
fn generic_method_on_non_generic_struct() {
    let out = compile_and_run(
        r#"
struct Holder {
    Int tag
}
impl Holder {
    function wrap<U>(self, x: U) -> U { x }
}
function main() {
    let h = Holder(7)
    print(h.wrap(42))
    print(h.wrap("hello"))
}
"#,
    );
    assert_eq!(out, vec!["42", "hello"]);
}

#[test]
fn specialize_at_map_type() {
    let out = compile_and_run(
        r#"
function sizeOf<K, V>(m: Map<K, V>) -> Int { m.length() }
function main() {
    let m: Map<String, Int> = {"a": 1, "b": 2, "c": 3}
    print(sizeOf(m))
}
"#,
    );
    assert_eq!(out, vec!["3"]);
}

#[test]
fn specialize_at_closure_type() {
    // Instantiates `identity` at a closure type `(Int) -> Int`.
    // Exercises ClosureRef mangling end-to-end: the specialized function's
    // symbol name must stay in `[A-Za-z0-9_]` despite the source type
    // containing parens, commas, and `->`.
    let out = compile_and_run(
        r#"
function identity<T>(x: T) -> T { x }
function main() {
    let add: (Int) -> Int = function(x: Int) -> Int { x + 1 }
    let g = identity(add)
    print(g(5))
    print(g(99))
}
"#,
    );
    assert_eq!(out, vec!["6", "100"]);
}

#[test]
fn specialize_at_option_type() {
    // Generic specialized at Option<Int> — enum type mangling.
    let out = compile_and_run(
        r#"
function hasValue<T>(o: Option<T>) -> Bool { o.isSome() }
function main() {
    let some: Option<Int> = Some(42)
    let none: Option<Int> = None
    print(hasValue(some))
    print(hasValue(none))
}
"#,
    );
    assert_eq!(out, vec!["true", "false"]);
}

#[test]
fn nested_generic_chain_three_levels() {
    let out = compile_and_run(
        r#"
function inner<T>(x: T) -> T { x }
function middle<T>(x: T) -> T { inner(x) }
function outer<T>(x: T) -> T { middle(x) }
function main() {
    print(outer(42))
    print(outer("deep"))
}
"#,
    );
    assert_eq!(out, vec!["42", "deep"]);
}

#[test]
fn recursive_generic_at_source_level() {
    let out = compile_and_run(
        r#"
function countDown<T>(x: T, n: Int) -> T {
    if n <= 0 {
        x
    } else {
        countDown(x, n - 1)
    }
}
function main() {
    print(countDown(42, 3))
    print(countDown("done", 2))
}
"#,
    );
    assert_eq!(out, vec!["42", "done"]);
}
