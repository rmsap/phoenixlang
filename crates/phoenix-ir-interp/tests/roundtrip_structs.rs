//! Round-trip tests: structs, enums, match, and traits.

mod common;
use common::roundtrip;

// ── Structs ──────────────────────────────────────────────────────────

#[test]
fn struct_basic() {
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
    print(p)
}
"#,
    );
}

#[test]
fn struct_method() {
    roundtrip(
        r#"
struct Rect {
    Int width
    Int height
}
impl Rect {
    function area(self) -> Int {
        return self.width * self.height
    }
}
function main() {
    let r: Rect = Rect(5, 10)
    print(r.area())
}
"#,
    );
}

#[test]
fn struct_field_mutation() {
    roundtrip(
        r#"
struct Counter {
    Int value
}
function main() {
    let mut c: Counter = Counter(0)
    c.value = 10
    print(c.value)
    c.value = c.value + 5
    print(c.value)
}
"#,
    );
}

// ── Enums and matching ──────────────────────────────────────────────

#[test]
fn enum_basic() {
    roundtrip(
        r#"
enum Color {
    Red
    Green
    Blue
}
function main() {
    let c: Color = Red
    print(c)
    match c {
        Red -> print("red")
        Green -> print("green")
        Blue -> print("blue")
    }
}
"#,
    );
}

#[test]
fn enum_with_fields() {
    roundtrip(
        r#"
enum Shape {
    Circle(Float)
    Rect(Float, Float)
}
function describe(s: Shape) -> String {
    return match s {
        Circle(r) -> "Circle: " + toString(r)
        Rect(w, h) -> "Rect: " + toString(w) + "x" + toString(h)
    }
}
function main() {
    print(describe(Circle(3.14)))
    print(describe(Rect(5.0, 10.0)))
}
"#,
    );
}

// ── Traits ───────────────────────────────────────────────────────────

#[test]
fn trait_basic() {
    roundtrip(
        r#"
trait Describable {
    function describe(self) -> String
}
struct Dog {
    String name
}
impl Describable for Dog {
    function describe(self) -> String {
        return "Dog: " + self.name
    }
}
function main() {
    let d: Dog = Dog("Rex")
    print(d.describe())
}
"#,
    );
}
