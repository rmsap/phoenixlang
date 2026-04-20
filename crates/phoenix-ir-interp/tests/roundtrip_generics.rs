//! Round-trip tests for user-defined generic functions and methods.
//!
//! Exercises the IR monomorphization pass end-to-end on the interpreter
//! path: every generic function is specialized for each concrete
//! instantiation reached from `main`, the interpreter runs the
//! monomorphized IR, and its print output is compared against the AST
//! interpreter. The Cranelift backend has a parallel file
//! (`phoenix-cranelift/tests/compile_generics.rs`); this mirror ensures
//! the interpreter path is not silently broken by monomorphization
//! changes.

mod common;
use common::{ir_run, roundtrip};

// ── Generics over value types ────────────────────────────────────────

#[test]
fn identity_int() {
    roundtrip(
        r#"
function identity<T>(x: T) -> T { x }
function main() { print(identity(42)) }
"#,
    );
}

#[test]
fn identity_string() {
    roundtrip(
        r#"
function identity<T>(x: T) -> T { x }
function main() { print(identity("hello")) }
"#,
    );
}

#[test]
fn identity_called_at_int_and_string() {
    roundtrip(
        r#"
function identity<T>(x: T) -> T { x }
function main() {
    print(identity(42))
    print(identity("world"))
}
"#,
    );
}

#[test]
fn identity_bool_and_float() {
    roundtrip(
        r#"
function identity<T>(x: T) -> T { x }
function main() {
    print(identity(true))
    print(identity(3.14))
}
"#,
    );
}

// ── Multi type-parameter generics ────────────────────────────────────

#[test]
fn multi_type_param_first() {
    let out = ir_run(
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

// ── Generic-to-generic chain (nested specialization discovery) ───────

#[test]
fn generic_calling_generic() {
    roundtrip(
        r#"
function inner<T>(x: T) -> T { x }
function outer<T>(x: T) -> T { inner(x) }
function main() {
    print(outer(42))
    print(outer("hello"))
}
"#,
    );
}

#[test]
fn nested_generic_chain_three_levels() {
    roundtrip(
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
}

// ── Specialization at reference types (mangling coverage) ────────────

#[test]
fn specialize_at_list_of_int() {
    roundtrip(
        r#"
function lengthOf<T>(xs: List<T>) -> Int { xs.length() }
function main() {
    let a: List<Int> = [1, 2, 3]
    print(lengthOf(a))
}
"#,
    );
}

#[test]
fn specialize_at_map_type() {
    roundtrip(
        r#"
function sizeOf<K, V>(m: Map<K, V>) -> Int { m.length() }
function main() {
    let m: Map<String, Int> = {"a": 1, "b": 2, "c": 3}
    print(sizeOf(m))
}
"#,
    );
}

#[test]
fn specialize_at_option_type() {
    roundtrip(
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
}

#[test]
fn specialize_at_struct_type() {
    roundtrip(
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
}

// ── Generic method on non-generic struct (bug #3 regression) ─────────

#[test]
fn generic_method_on_non_generic_struct() {
    roundtrip(
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
}

// ── Recursion through generics ───────────────────────────────────────

#[test]
fn recursive_generic_at_source_level() {
    roundtrip(
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
}
