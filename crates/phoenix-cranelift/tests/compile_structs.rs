//! Integration tests: struct compilation, including String fields.

mod common;
use common::roundtrip;

#[test]
fn struct_int_fields() {
    roundtrip(
        r#"
struct Point {
    Int x
    Int y
}
function main() {
    let p: Point = Point(3, 4)
    print(p.x)
    print(p.y)
}
"#,
    );
}

/// String fields in structs must preserve
/// both the pointer and length (fat pointer), not just the pointer.
#[test]
fn struct_string_field() {
    roundtrip(
        r#"
struct User {
    String name
    Int age
}
function main() {
    let u: User = User("Alice", 30)
    print(u.name)
    print(u.age)
}
"#,
    );
}

/// Test struct with multiple String fields to ensure all are stored correctly.
#[test]
fn struct_multiple_string_fields() {
    roundtrip(
        r#"
struct Contact {
    String first
    String last
    String email
}
function main() {
    let c: Contact = Contact("Jane", "Doe", "jane@example.com")
    print(c.first)
    print(c.last)
    print(c.email)
}
"#,
    );
}

/// Test mixed String and non-String fields to ensure slot offsets are correct.
#[test]
fn struct_mixed_fields() {
    roundtrip(
        r#"
struct Record {
    Int id
    String label
    Bool active
    String note
}
function main() {
    let r: Record = Record(42, "hello", true, "world")
    print(r.id)
    print(r.label)
    print(r.active)
    print(r.note)
}
"#,
    );
}

#[test]
fn struct_field_assignment() {
    roundtrip(
        r#"
struct Counter {
    Int value
}
function main() {
    let mut c: Counter = Counter(0)
    c.value = 10
    print(c.value)
}
"#,
    );
}

/// Test setting a String field on a struct.
#[test]
fn struct_string_field_assignment() {
    roundtrip(
        r#"
struct Named {
    String name
    Int count
}
function main() {
    let mut n: Named = Named("old", 1)
    n.name = "new"
    print(n.name)
    print(n.count)
}
"#,
    );
}

/// Nested struct: a struct containing another struct as a field.
#[test]
fn struct_nested() {
    roundtrip(
        r#"
struct Inner {
    Int value
}
struct Outer {
    Inner inner
    Int extra
}
function main() {
    let i: Inner = Inner(42)
    let o: Outer = Outer(i, 7)
    print(o.inner.value)
    print(o.extra)
}
"#,
    );
}

#[test]
fn struct_method_call() {
    roundtrip(
        r#"
struct Pair {
    Int a
    Int b

    function sum(self) -> Int {
        self.a + self.b
    }
}
function main() {
    let p: Pair = Pair(3, 7)
    print(p.sum())
}
"#,
    );
}

/// Test mutating a String field on a struct.
#[test]
fn struct_string_field_mutation() {
    roundtrip(
        r#"
struct User {
    String name
    Int age
}
function main() {
    let mut u: User = User("Alice", 30)
    print(u.name)
    u.name = "Bob"
    print(u.name)
    print(u.age)
}
"#,
    );
}
