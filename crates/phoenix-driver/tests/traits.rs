mod common;
use common::*;

/// Comprehensive end-to-end test for traits: trait declaration, impl, method
/// call, and trait-bounded generic function.
#[test]
fn traits() {
    run_expect(
        r#"
trait Display {
  function toString(self) -> String
}

struct Point {
  Int x
  Int y

  impl Display {
    function toString(self) -> String {
      return "Point"
    }
  }
}

function show<T: Display>(item: T) -> String {
  return item.toString()
}

function main() {
  let p: Point = Point(3, 4)
  let s: String = p.toString()
  print(s)
  let s2: String = show(p)
  print(s2)
}
"#,
        &["Point", "Point"],
    );
}

/// Trait with two methods, both implemented and both called at runtime.
#[test]
fn trait_multiple_methods() {
    run_expect(
        r#"
trait Shape {
  function area(self) -> Float
  function name(self) -> String
}

struct Circle {
  Float radius

  impl Shape {
    function area(self) -> Float { return 3.14 }
    function name(self) -> String { return "Circle" }
  }
}

function describe<T: Shape>(s: T) -> String {
  return s.name()
}

function main() {
  let c: Circle = Circle(5.0)
  let a: Float = c.area()
  print(a)
  let n: String = c.name()
  print(n)
  let d: String = describe(c)
  print(d)
}
"#,
        &["3.14", "Circle", "Circle"],
    );
}

#[test]
fn duplicate_trait_impl_error() {
    expect_type_error(
        r#"
trait Greet {
  function greet(self) -> String
}
struct Dog {
  String name

  impl Greet {
    function greet(self) -> String { return "woof" }
  }
  impl Greet {
    function greet(self) -> String { return "bark" }
  }
}
function main() { }
"#,
        "duplicate implementation of trait `Greet` for type `Dog`",
    );
}

// --- B6: trait-bounded method argument type checking ---

#[test]
fn trait_bound_method_wrong_arg_count() {
    expect_type_error(
        r#"
trait Processor {
    function process(self, x: Int) -> Int
}
function apply<T: Processor>(t: T) -> Int {
    return t.process(1, 2)
}
function main() { }
"#,
        "takes 1 argument(s), got 2",
    );
}

// --- Traits expanded coverage ---

#[test]
fn trait_bound_not_implemented() {
    expect_type_error(
        r#"
trait Display {
    function toString(self) -> String
}
struct Point {
    Int x
    Int y
}
function show<T: Display>(item: T) -> String {
    return item.toString()
}
function main() {
    let p: Point = Point(1, 2)
    show(p)
}
"#,
        "does not implement trait `Display`",
    );
}

#[test]
fn trait_method_returns_correct_type() {
    run_expect(
        r#"
trait Describable {
    function describe(self) -> String
}
struct Color {
    String name

    impl Describable {
        function describe(self) -> String {
            return "Color: " + self.name
        }
    }
}
function show<T: Describable>(item: T) -> String {
    return item.describe()
}
function main() {
    let c: Color = Color("red")
    print(show(c))
}
"#,
        &["Color: red"],
    );
}

#[test]
fn trait_impl_with_multiple_methods() {
    run_expect(
        r#"
trait Shape {
    function area(self) -> Float
    function name(self) -> String
}
struct Square {
    Float side

    impl Shape {
        function area(self) -> Float {
            return self.side * self.side
        }
        function name(self) -> String {
            return "square"
        }
    }
}
function main() {
    let s: Square = Square(5.0)
    print(s.area())
    print(s.name())
}
"#,
        &["25", "square"],
    );
}

// ── Traits Expanded ───────────────────────────────────────────────────

#[test]
fn multiple_traits_on_same_type() {
    run_expect(
        r#"
trait Display {
    function toString(self) -> String
}
trait Describable {
    function describe(self) -> String
}
struct Dog {
    String name

    impl Display {
        function toString(self) -> String { return self.name }
    }
    impl Describable {
        function describe(self) -> String { return "Dog: " + self.name }
    }
}
function show<T: Display>(item: T) -> String {
    return item.toString()
}
function main() {
    let d: Dog = Dog("Rex")
    print(show(d))
    print(d.describe())
}
"#,
        &["Rex", "Dog: Rex"],
    );
}

#[test]
fn two_types_implementing_same_trait() {
    run_expect(
        r#"
trait Greet {
    function greet(self) -> String
}
struct Dog {
    String name

    impl Greet {
        function greet(self) -> String { return "Woof from " + self.name }
    }
}
struct Cat {
    String name

    impl Greet {
        function greet(self) -> String { return "Meow from " + self.name }
    }
}
function sayHi<T: Greet>(animal: T) -> String {
    return animal.greet()
}
function main() {
    let d: Dog = Dog("Rex")
    let c: Cat = Cat("Whiskers")
    print(sayHi(d))
    print(sayHi(c))
}
"#,
        &["Woof from Rex", "Meow from Whiskers"],
    );
}

#[test]
fn trait_method_with_interpolation() {
    run_expect(
        r#"
trait Named {
    function name(self) -> String
}
struct User {
    String first
    String last

    impl Named {
        function name(self) -> String {
            return "{self.first} {self.last}"
        }
    }
}
function main() {
    let u: User = User("Alice", "Smith")
    print(u.name())
}
"#,
        &["Alice Smith"],
    );
}

// =========================================================================
// Second audit: missing edge case coverage
// =========================================================================

// ── Trait impl validation (errors only tested as checker unit tests) ───

#[test]
fn trait_impl_missing_method_error() {
    expect_type_error(
        r#"
trait Shape {
    function area(self) -> Float
    function name(self) -> String
}
struct Circle {
    Float radius

    impl Shape {
        function area(self) -> Float { return 3.14 }
    }
}
function main() { }
"#,
        "missing method `name`",
    );
}

#[test]
fn trait_impl_wrong_param_count_error() {
    expect_type_error(
        r#"
trait Greet {
    function hello(self) -> String
}
struct Person {
    String name

    impl Greet {
        function hello(self, extra: Int) -> String { return "hi" }
    }
}
function main() { }
"#,
        "parameter(s) but trait",
    );
}

#[test]
fn trait_impl_wrong_return_type_error() {
    expect_type_error(
        r#"
trait Greet {
    function hello(self) -> String
}
struct Person {
    String name

    impl Greet {
        function hello(self) -> Int { return 42 }
    }
}
function main() { }
"#,
        "returns `Int` but trait",
    );
}

#[test]
fn unknown_trait_in_impl_error() {
    expect_type_error(
        r#"
struct Point {
    Int x
    Int y

    impl NonexistentTrait {
        function foo(self) -> Int { return 0 }
    }
}
function main() { }
"#,
        "unknown trait",
    );
}

// ── Complex cross-feature: trait + closure + generics ──────────────────

#[test]
fn trait_bound_with_closure_argument() {
    run_expect(
        r#"
trait Mappable {
    function value(self) -> Int
}
struct Box {
    Int val

    impl Mappable {
        function value(self) -> Int { return self.val }
    }
}
function extract<T: Mappable>(item: T) -> Int {
    return item.value()
}
function main() {
    let b: Box = Box(42)
    let result: Int = extract(b)
    print(result)
}
"#,
        &["42"],
    );
}

#[test]
fn inline_trait_impl() {
    run_expect(
        r#"
trait Greetable {
    function greet(self) -> String
}

struct Person {
    String name

    impl Greetable {
        function greet(self) -> String {
            "Hello, {self.name}!"
        }
    }
}
function main() {
    let p: Person = Person("Alice")
    print(p.greet())
}
"#,
        &["Hello, Alice!"],
    );
}

/// Trait impl with wrong parameter type must be rejected at type-check time.
#[test]
fn trait_impl_wrong_param_type() {
    expect_type_error(
        r#"
trait Processor { function process(self, x: Int) -> Int }
struct MyProc {
  impl Processor {
    function process(self, x: String) -> Int { return 0 }
  }
}
"#,
        "parameter",
    );
    expect_type_error(
        r#"
trait Processor { function process(self, x: Int) -> Int }
struct MyProc {
  impl Processor {
    function process(self, x: String) -> Int { return 0 }
  }
}
"#,
        "type",
    );
}

// =========================================================================
// 1.13.5: Strengthened trait test coverage
// =========================================================================

// ── Trait method with parameters (beyond self) ───────────────────────────

#[test]
fn trait_method_with_parameter() {
    run_expect(
        r#"
trait Transformer {
    function transform(self, x: Int) -> Int
}
struct Doubler {
    impl Transformer {
        function transform(self, x: Int) -> Int { return x * 2 }
    }
}
struct Adder {
    Int offset

    impl Transformer {
        function transform(self, x: Int) -> Int { return x + self.offset }
    }
}
function apply<T: Transformer>(t: T, val: Int) -> Int {
    return t.transform(val)
}
function main() {
    let d: Doubler = Doubler()
    let a: Adder = Adder(10)
    print(apply(d, 5))
    print(apply(a, 5))
}
"#,
        &["10", "15"],
    );
}

// ── Trait dispatch through generic function called with concrete type ────

#[test]
fn trait_dispatch_generic_called_with_concrete() {
    run_expect(
        r#"
trait Printable {
    function toStr(self) -> String
}
struct Num {
    Int val

    impl Printable {
        function toStr(self) -> String { return toString(self.val) }
    }
}
function formatItem<T: Printable>(item: T) -> String {
    return "[" + item.toStr() + "]"
}
function main() {
    let n: Num = Num(42)
    print(formatItem(n))
    let m: Num = Num(7)
    print(formatItem(m))
}
"#,
        &["[42]", "[7]"],
    );
}

// ── Trait on enum types ─────────────────────────────────────────────────

#[test]
fn trait_impl_on_enum() {
    run_expect(
        r#"
trait Describe {
    function desc(self) -> String
}
enum Color {
    Red
    Green
    Blue

    impl Describe {
        function desc(self) -> String {
            return match self {
                Red -> "red"
                Green -> "green"
                Blue -> "blue"
            }
        }
    }
}
function show<T: Describe>(item: T) -> String {
    return item.desc()
}
function main() {
    let c: Color = Green
    print(show(c))
    let r: Color = Red
    print(r.desc())
}
"#,
        &["green", "red"],
    );
}

// ── Inline trait impl on enum ───────────────────────────────────────────

#[test]
fn inline_trait_impl_on_enum() {
    run_expect(
        r#"
trait Describe {
    function desc(self) -> String
}
enum Shape {
    Circle(Float)
    Rect(Float, Float)

    impl Describe {
        function desc(self) -> String {
            return match self {
                Circle(r) -> "circle"
                Rect(w, h) -> "rectangle"
            }
        }
    }
}
function main() {
    let s: Shape = Circle(3.0)
    print(s.desc())
    let r: Shape = Rect(2.0, 5.0)
    print(r.desc())
}
"#,
        &["circle", "rectangle"],
    );
}

// ── Trait dispatch with concrete types through generic ───────────────────

#[test]
fn trait_dispatch_with_different_concrete_types() {
    run_expect(
        r#"
trait Sizeable {
    function size(self) -> Int
}
struct SmallBox {
    impl Sizeable {
        function size(self) -> Int { return 1 }
    }
}
struct BigBox {
    Int items

    impl Sizeable {
        function size(self) -> Int { return self.items }
    }
}
function totalSize<T: Sizeable>(items: List<T>) -> Int {
    let mut sum: Int = 0
    let mut i: Int = 0
    while i < items.length() {
        sum = sum + items.get(i).size()
        i = i + 1
    }
    return sum
}
function main() {
    let boxes: List<BigBox> = [BigBox(3), BigBox(7), BigBox(2)]
    print(totalSize(boxes))
}
"#,
        &["12"],
    );
}

// ── Trait method returning struct ────────────────────────────────────────

#[test]
fn trait_method_returning_struct() {
    run_expect(
        r#"
struct Point { Int x  Int y }
trait Locatable {
    function location(self) -> Point
}
struct Marker {
    Int px
    Int py

    impl Locatable {
        function location(self) -> Point {
            return Point(self.px, self.py)
        }
    }
}
function getX<T: Locatable>(item: T) -> Int {
    return item.location().x
}
function main() {
    let m: Marker = Marker(10, 20)
    print(getX(m))
    print(m.location().y)
}
"#,
        &["10", "20"],
    );
}

// ── Trait method using string interpolation with self fields ─────────────

#[test]
fn trait_method_complex_body() {
    run_expect(
        r#"
trait Summary {
    function summarize(self) -> String
}
struct Article {
    String title
    String author
    Int words

    impl Summary {
        function summarize(self) -> String {
            return "{self.title} by {self.author} ({self.words} words)"
        }
    }
}
function printSummary<T: Summary>(item: T) {
    print(item.summarize())
}
function main() {
    let a: Article = Article("Phoenix Guide", "Alice", 1500)
    printSummary(a)
}
"#,
        &["Phoenix Guide by Alice (1500 words)"],
    );
}

// ── Same trait implemented by struct and enum ────────────────────────────

#[test]
fn trait_shared_by_struct_and_enum() {
    run_expect(
        r#"
trait Label {
    function label(self) -> String
}
struct Named {
    String name

    impl Label {
        function label(self) -> String { return self.name }
    }
}
enum Status {
    Active
    Inactive

    impl Label {
        function label(self) -> String {
            return match self {
                Active -> "active"
                Inactive -> "inactive"
            }
        }
    }
}
function showLabel<T: Label>(item: T) {
    print(item.label())
}
function main() {
    showLabel(Named("hello"))
    showLabel(Active)
    showLabel(Inactive)
}
"#,
        &["hello", "active", "inactive"],
    );
}

// ── Trait bound not satisfied — enum ─────────────────────────────────────

#[test]
fn trait_bound_not_satisfied_for_enum() {
    expect_type_error(
        r#"
trait Printable {
    function toStr(self) -> String
}
enum Color { Red  Blue }
function show<T: Printable>(item: T) -> String {
    return item.toStr()
}
function main() {
    show(Red)
}
"#,
        "does not implement trait `Printable`",
    );
}

// ── Trait method called on result of another method ──────────────────────

#[test]
fn trait_method_chained_with_regular_method() {
    run_expect(
        r#"
trait Stringable {
    function toStr(self) -> String
}
struct Wrapper {
    Int val

    function doubled(self) -> Wrapper {
        return Wrapper(self.val * 2)
    }

    impl Stringable {
        function toStr(self) -> String {
            return toString(self.val)
        }
    }
}
function main() {
    let w: Wrapper = Wrapper(21)
    print(w.doubled().toStr())
}
"#,
        &["42"],
    );
}

// ── Trait with method returning Option ───────────────────────────────────

#[test]
fn trait_method_returning_option() {
    run_expect(
        r#"
trait Finder {
    function find(self, target: Int) -> Option<Int>
}
struct Numbers {
    List<Int> data

    impl Finder {
        function find(self, target: Int) -> Option<Int> {
            let mut i: Int = 0
            while i < self.data.length() {
                if self.data.get(i) == target {
                    return Some(i)
                }
                i = i + 1
            }
            return None
        }
    }
}
function main() {
    let nums: Numbers = Numbers([10, 20, 30, 40])
    let found: Option<Int> = nums.find(30)
    print(found.unwrap())
    let missing: Option<Int> = nums.find(99)
    print(missing.isNone())
}
"#,
        &["2", "true"],
    );
}

// ── Trait with method returning Result ───────────────────────────────────

#[test]
fn trait_method_returning_result() {
    run_expect(
        r#"
trait Parser {
    function parse(self, input: String) -> Result<Int, String>
}
struct IntParser {
    impl Parser {
        function parse(self, input: String) -> Result<Int, String> {
            if input == "42" { return Ok(42) }
            return Err("not a number")
        }
    }
}
function tryParse<T: Parser>(p: T, s: String) -> Result<Int, String> {
    return p.parse(s)
}
function main() {
    let p: IntParser = IntParser()
    let ok: Result<Int, String> = tryParse(p, "42")
    print(ok.unwrap())
    let err: Result<Int, String> = tryParse(p, "abc")
    print(err.isErr())
}
"#,
        &["42", "true"],
    );
}

// ── Trait used with collection functional methods ────────────────────────

#[test]
fn trait_with_collection_map() {
    run_expect(
        r#"
trait HasValue {
    function value(self) -> Int
}
struct Item {
    Int v

    impl HasValue {
        function value(self) -> Int { return self.v }
    }
}
function main() {
    let items: List<Item> = [Item(1), Item(2), Item(3)]
    let values: List<Int> = items.map(function(it: Item) -> Int { return it.value() })
    print(values)
}
"#,
        &["[1, 2, 3]"],
    );
}

// ── Inline trait impl with method using fields ──────────────────────────

#[test]
fn inline_trait_impl_accesses_fields() {
    run_expect(
        r#"
trait Display {
    function toString(self) -> String
}
struct Point {
    Int x
    Int y

    impl Display {
        function toString(self) -> String {
            return "({self.x}, {self.y})"
        }
    }
}
function show<T: Display>(item: T) {
    print(item.toString())
}
function main() {
    let p: Point = Point(3, 4)
    show(p)
}
"#,
        &["(3, 4)"],
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Negative test: multiple trait bounds (T: Foo + Bar) — not yet supported
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn multiple_trait_bounds_parse_error() {
    // Phoenix does not yet support `T: Foo + Bar` syntax.
    // The parser should reject it clearly.
    expect_parse_error(
        r#"
trait Display {
  function toString(self) -> String
}
trait Clone {
  function clone(self) -> Self
}
function show<T: Display + Clone>(item: T) -> String {
  return item.toString()
}
function main() { }
"#,
        "expected '>'",
    );
}
