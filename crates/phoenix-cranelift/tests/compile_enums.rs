//! Integration tests: enum compilation and pattern matching, including String fields.

mod common;
use common::roundtrip;

#[test]
fn enum_basic_match() {
    roundtrip(
        r#"
enum Shape {
    Circle(Float)
    Rect(Float, Float)
}
function main() {
    let s: Shape = Circle(5.0)
    match s {
        Circle(r) -> print(r)
        Rect(w, h) -> print(w + h)
    }
}
"#,
    );
}

#[test]
fn enum_multiple_variants() {
    roundtrip(
        r#"
enum Color {
    Red
    Green
    Blue
}
function main() {
    let c: Color = Green
    match c {
        Red -> print("red")
        Green -> print("green")
        Blue -> print("blue")
    }
}
"#,
    );
}

/// String fields in enum variants must
/// preserve both pointer and length (fat pointer).
#[test]
fn enum_string_variant_field() {
    roundtrip(
        r#"
enum Message {
    Text(String)
    Number(Int)
}
function main() {
    let m: Message = Text("hello")
    match m {
        Text(s) -> print(s)
        Number(n) -> print(n)
    }

    let m2: Message = Number(42)
    match m2 {
        Text(s) -> print(s)
        Number(n) -> print(n)
    }
}
"#,
    );
}

/// Two variants with same-typed fields at the same index
/// but different preceding field layouts (different slot offsets).
#[test]
fn enum_same_typed_fields_different_layouts() {
    roundtrip(
        r#"
enum Data {
    Text(String, Int)
    Pair(Int, Int)
}
function main() {
    let d1: Data = Text("hello", 42)
    match d1 {
        Text(s, n) -> {
            print(s)
            print(n)
        }
        Pair(a, b) -> print(a + b)
    }

    let d2: Data = Pair(10, 20)
    match d2 {
        Text(s, n) -> print(n)
        Pair(a, b) -> print(a + b)
    }
}
"#,
    );
}

#[test]
fn enum_method() {
    roundtrip(
        r#"
enum Shape {
    Circle(Float)
    Rect(Float, Float)

    function describe(self) -> Int {
        match self {
            Circle(r) -> 1
            Rect(w, h) -> 2
        }
    }
}
function main() {
    let s: Shape = Rect(3.0, 4.0)
    print(s.describe())
}
"#,
    );
}

/// Test enum containing a struct with a String field (nested fat pointer).
#[test]
fn enum_containing_struct_with_string() {
    roundtrip(
        r#"
struct Info {
    String label
    Int count
}
enum Container {
    WithInfo(Info)
    Empty
}
function main() {
    let info: Info = Info("test", 42)
    let c: Container = WithInfo(info)
    match c {
        WithInfo(i) -> {
            print(i.label)
            print(i.count)
        }
        Empty -> print("empty")
    }
}
"#,
    );
}

/// EnumGetField must use the correct variant_idx.
///
/// The IR interpreter has a `debug_assert` verifying that the
/// `variant_idx` in `EnumGetField` matches the runtime discriminant.
/// This test creates *every* variant of a 3-variant enum, matches each,
/// and extracts fields — exercising all variant indices in both the IR
/// interpreter roundtrip and the Cranelift backend.
#[test]
fn enum_variant_idx_regression() {
    roundtrip(
        r#"
enum Value {
    IntVal(Int)
    FloatVal(Float)
    Pair(Int, Int)
}

function main() {
    let a: Value = IntVal(10)
    let b: Value = FloatVal(3.14)
    let c: Value = Pair(1, 2)

    match a {
        IntVal(n) -> print(n)
        FloatVal(f) -> print(f)
        Pair(x, y) -> print(x + y)
    }

    match b {
        IntVal(n) -> print(n)
        FloatVal(f) -> print(f)
        Pair(x, y) -> print(x + y)
    }

    match c {
        IntVal(n) -> print(n)
        FloatVal(f) -> print(f)
        Pair(x, y) -> print(x + y)
    }
}
"#,
    );
}

// Round-trip a user-defined *generic* enum end-to-end is blocked on a
// separate limitation (user-defined generic enum layout specialization
// is a deferred feature — see `docs/design-decisions.md`). The IR-level
// propagation of generic args through `EnumRef(name, args)` is covered
// by `enum_type_at_preserves_args_for_generic_constructor_call` in the
// `phoenix-ir` crate, and the extended mangler grammar (`e_{name}__…_E`)
// is covered by `mangles_enum_ref_with_args_verbatim` in the same crate.
// Once generic-enum monomorphization lands, add an end-to-end test here.
