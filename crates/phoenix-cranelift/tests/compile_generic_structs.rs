//! Integration tests for compiled generic user-defined structs.
//!
//! Covers the MVP scope:
//! - Baseline `struct Container<T>` field access.
//! - Methods that use the type parameter (`impl Container { function
//!   get(self) -> T { self.value } }`).
//! - Two instantiations coexist without collision (struct-mono mangles
//!   their names to distinguish `Container__i64` from `Container__str`).
//! - Nested generics (`struct Nested<T> { Pair<T> p }`).
//! - `dyn Trait` over a generic struct — vtable keying disambiguates
//!   `Box<Int>: Show` and `Box<String>: Show`.

mod common;

use common::{compile_and_run, roundtrip};

/// Baseline: a generic struct with a single type parameter, field
/// access after construction.  Verifies the fundamental
/// monomorphization + StructRef rewrite pipeline.
#[test]
fn generic_struct_baseline_field_access() {
    let output = compile_and_run(
        r#"
struct Container<T> {
    T value
}

function main() {
    let c = Container(42)
    print(c.value)
    let s = Container("hello")
    print(s.value)
}
"#,
    );
    assert_eq!(output, vec!["42", "hello"]);
}

/// A method that returns a field of type `T`.  Verifies that
/// struct-mono specializes the method body (substituting `T` in the
/// `self` parameter's StructRef args and the return type).
#[test]
fn generic_struct_method_using_type_param() {
    let output = compile_and_run(
        r#"
struct Container<T> {
    T value

    function get(self) -> T { self.value }
}

function main() {
    let c: Container<Int> = Container(42)
    print(c.get())
    let s: Container<String> = Container("hello")
    print(s.get())
}
"#,
    );
    assert_eq!(output, vec!["42", "hello"]);
}

/// Two instantiations coexist without collision. Before struct-mono,
/// `Container<Int>` and `Container<String>` would both key `Container`
/// in `struct_layouts` / `method_index` and the second registration
/// would silently clobber the first.
#[test]
fn generic_struct_two_instantiations_coexist() {
    let output = compile_and_run(
        r#"
struct Box<T> {
    T v

    function unwrap(self) -> T { self.v }
}

function main() {
    let a: Box<Int> = Box(1)
    let b: Box<String> = Box("hi")
    print(a.unwrap())
    print(b.unwrap())
}
"#,
    );
    assert_eq!(output, vec!["1", "hi"]);
}

/// Nested generic: a struct whose field type is another generic struct
/// at the same type parameter.  Verifies that struct-mono's fixed-point
/// worklist enqueues `Pair<T>` when specializing `Nested<T>` → the
/// inner layout needs its own specialization.
#[test]
fn generic_struct_nested() {
    let output = compile_and_run(
        r#"
struct Pair<T> {
    T first
    T second
}
struct Nested<T> {
    Pair<T> p
}

function main() {
    let n: Nested<Int> = Nested(Pair(1, 2))
    print(n.p.first)
    print(n.p.second)
}
"#,
    );
    assert_eq!(output, vec!["1", "2"]);
}

/// `dyn Trait` over a generic struct.  The vtable registration at
/// coercion time uses the template name `Box`; struct-mono must
/// rekey the entry to the mangled name so `Op::DynAlloc("Box__i64",
/// "Show", _)` finds its vtable.
#[test]
fn generic_struct_in_dyn_trait() {
    let output = compile_and_run(
        r#"
trait Show {
    function show(self) -> String
}

struct Box<T> {
    T v

    impl Show {
        function show(self) -> String {
            return "box"
        }
    }
}

function main() {
    let a: dyn Show = Box(1)
    let b: dyn Show = Box("x")
    print(a.show())
    print(b.show())
}
"#,
    );
    assert_eq!(output, vec!["box", "box"]);
}

/// Three-way agreement on the baseline (AST interp == IR interp ==
/// compiled).  Guards against one backend regressing silently where
/// the other two happen to paper over the issue.
#[test]
fn generic_struct_three_way_agreement() {
    roundtrip(
        r#"
struct Container<T> {
    T value
}

function main() {
    let c = Container(42)
    print(c.value)
}
"#,
    );
}

/// Multi-parameter generic struct.  `Pair<A, B>` exercises the mangling
/// grammar's arg-separator (`Pair__i64__str`) and the substitution-map
/// construction with more than one binding.
#[test]
fn generic_struct_multi_type_param() {
    let output = compile_and_run(
        r#"
struct Pair<A, B> {
    A first
    B second
}

function main() {
    let p: Pair<Int, String> = Pair(1, "hello")
    print(p.first)
    print(p.second)
    let q: Pair<String, Int> = Pair("answer", 42)
    print(q.first)
    print(q.second)
}
"#,
    );
    assert_eq!(output, vec!["1", "hello", "answer", "42"]);
}

/// Generic struct inside a `List`.  Exercises
/// `enqueue_generic_struct_refs`'s `ListRef` recursion — a
/// `List<Container<Int>>` element must enqueue `Container<Int>` for
/// specialization even though the `StructRef` is wrapped in a `ListRef`.
#[test]
fn generic_struct_inside_list() {
    let output = compile_and_run(
        r#"
struct Container<T> {
    T value
}

function main() {
    let items: List<Container<Int>> = [Container(1), Container(2), Container(3)]
    for item in items {
        print(item.value)
    }
}
"#,
    );
    assert_eq!(output, vec!["1", "2", "3"]);
}

/// Generic function whose parameter is a generic struct.  Exercises the
/// interaction between function-mono (specializes `unwrap<T>`) and
/// struct-mono (specializes `Container<T>`): function-mono must run
/// first and resolve `T := Int`, then struct-mono picks up
/// `Container<Int>` in the specialized body.
#[test]
fn generic_fn_over_generic_struct() {
    let output = compile_and_run(
        r#"
struct Container<T> {
    T value
}

function unwrap<T>(c: Container<T>) -> T {
    c.value
}

function main() {
    let c: Container<Int> = Container(7)
    print(unwrap(c))
    let s: Container<String> = Container("hi")
    print(unwrap(s))
}
"#,
    );
    assert_eq!(output, vec!["7", "hi"]);
}

/// Self-recursive generic via `Option<Node<T>>`.  The `next` field's
/// `EnumRef("Option", [StructRef("Node", [T])])` exposes the generic
/// struct nested inside an enum arg; struct-mono's worklist fixed-point
/// must discover it when specializing the layout for the outer
/// instantiation.
#[test]
fn generic_struct_self_recursive_via_option() {
    let output = compile_and_run(
        r#"
struct Node<T> {
    T val
    Option<Node<T>> next
}

function main() {
    let tail: Node<Int> = Node(2, None)
    let head: Node<Int> = Node(1, Some(tail))
    print(head.val)
    match head.next {
        Some(n) -> print(n.val)
        None -> print("nope")
    }
}
"#,
    );
    assert_eq!(output, vec!["1", "2"]);
}

/// Two-level nested generic: `Container<Box<Int>>`.  The inner
/// `Box<Int>` must be specialized before the outer `Container<Box<Int>>`
/// is keyed in `rename_map` — tests the "recurse first, then consult
/// the map" ordering in `rewrite_struct_refs_in_type`.
#[test]
fn generic_struct_two_level_nesting() {
    let output = compile_and_run(
        r#"
struct Box<T> {
    T v
}
struct Container<T> {
    T value
}

function main() {
    let inner: Box<Int> = Box(5)
    let outer: Container<Box<Int>> = Container(inner)
    print(outer.value.v)
}
"#,
    );
    assert_eq!(output, vec!["5"]);
}
