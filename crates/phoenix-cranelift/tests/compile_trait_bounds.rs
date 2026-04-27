//! Integration tests for trait-bounded generic methods in compiled mode
//! (`<T: Trait>` → `x.method()` on a type-variable receiver).
//!
//! IR lowering emits these as `Op::UnresolvedTraitMethod(method_name,
//! method_targs, args)`.  The monomorphization pass resolves them to
//! direct `Op::Call`s after substituting the receiver's type — see
//! `resolve_trait_bound_method_calls` in
//! `phoenix-ir/src/monomorphize.rs`.
//!
//! The AST interpreter has always handled these correctly via runtime
//! type inspection.  Every test here uses [`three_way_roundtrip`] by
//! default so AST interp, IR interp, and compiled mode must all agree
//! — a silent divergence in any single backend surfaces immediately.

mod common;

use common::{compile_and_run, three_way_roundtrip};

/// Baseline: one trait, one impl, one call site via `<T: Trait>`.
#[test]
fn trait_bound_single_impl() {
    three_way_roundtrip(
        r#"
trait Greet {
    function greet(self) -> String
}

struct Dog {
    String name

    impl Greet {
        function greet(self) -> String { return "woof" }
    }
}

function sayHi<T: Greet>(animal: T) -> String {
    return animal.greet()
}

function main() {
    print(sayHi(Dog("Rex")))
}
"#,
    );
}

/// Two concrete types implementing the same trait.  The specialized
/// `sayHi__s_Dog` and `sayHi__s_Cat` must each dispatch to the right
/// impl — a bug in the method-index lookup would either return the
/// template FuncId (Cranelift filters templates out → missing-func
/// error) or cross-wire the two.
#[test]
fn trait_bound_two_impls() {
    three_way_roundtrip(
        r#"
trait Greet {
    function greet(self) -> String
}

struct Dog {
    String name

    impl Greet {
        function greet(self) -> String { return "woof from " + self.name }
    }
}

struct Cat {
    String name

    impl Greet {
        function greet(self) -> String { return "meow from " + self.name }
    }
}

function sayHi<T: Greet>(animal: T) -> String {
    return animal.greet()
}

function main() {
    print(sayHi(Dog("Rex")))
    print(sayHi(Cat("Whiskers")))
}
"#,
    );
}

/// A trait method whose name shadows a known builtin (`toString`).  The
/// monomorphization pass must route the call through `method_index` —
/// if the placeholder were resolved as a builtin, the compiled output
/// would either recurse forever on `toString` or call into Phoenix
/// runtime's `toString` instead of the user impl.
#[test]
fn trait_bound_method_name_shadows_builtin() {
    three_way_roundtrip(
        r#"
trait Display {
    function toString(self) -> String
}

struct Point {
    Int x
    Int y

    impl Display {
        function toString(self) -> String { return "Point" }
    }
}

function show<T: Display>(item: T) -> String {
    return item.toString()
}

function main() {
    let p: Point = Point(3, 4)
    print(show(p))
}
"#,
    );
}

/// Trait-bounded generic over a receiver that returns a string built
/// from a field — confirms that after the dispatch rewrite, the method
/// body still accesses the receiver's fields correctly.  Regression
/// guard for a bug where swapping the call target would also mangle
/// argument ordering.
#[test]
fn trait_bound_method_uses_receiver_field() {
    three_way_roundtrip(
        r#"
trait Name {
    function name(self) -> String
}

struct Person {
    String first

    impl Name {
        function name(self) -> String { return self.first }
    }
}

function label<T: Name>(x: T) -> String {
    return "Hello, " + x.name()
}

function main() {
    print(label(Person("Ada")))
}
"#,
    );
}

/// Trait method that takes additional (non-self) arguments.  Pins the
/// argument ordering in the rewrite: `args[0]` is the receiver, the
/// rest are passed through in source order.
#[test]
fn trait_bound_method_with_extra_args() {
    three_way_roundtrip(
        r#"
trait Combine {
    function combine(self, other: Int) -> Int
}

struct Counter {
    Int start

    impl Combine {
        function combine(self, other: Int) -> Int { return self.start + other }
    }
}

function addTwo<T: Combine>(c: T, x: Int, y: Int) -> Int {
    return c.combine(x) + y
}

function main() {
    let c: Counter = Counter(10)
    print(toString(addTwo(c, 5, 7)))
}
"#,
    );
}

/// Trait method whose return type is a non-String primitive — pins
/// that the `method_targs` / `result_type` threading is independent of
/// the method's signature.
#[test]
fn trait_bound_method_int_return() {
    three_way_roundtrip(
        r#"
trait Size {
    function size(self) -> Int
}

struct Box {
    Int w
    Int h

    impl Size {
        function size(self) -> Int { return self.w * self.h }
    }
}

function measure<T: Size>(s: T) -> Int {
    return s.size()
}

function main() {
    print(toString(measure(Box(3, 4))))
}
"#,
    );
}

/// Trait method whose return type is `Bool` — another primitive-return
/// guard, independent of the int path.
#[test]
fn trait_bound_method_bool_return() {
    three_way_roundtrip(
        r#"
trait Truthy {
    function isTrue(self) -> Bool
}

struct Flag {
    Bool v

    impl Truthy {
        function isTrue(self) -> Bool { return self.v }
    }
}

function check<T: Truthy>(f: T) -> Bool {
    return f.isTrue()
}

function main() {
    print(toString(check(Flag(true))))
    print(toString(check(Flag(false))))
}
"#,
    );
}

/// Enum receiver implementing a trait.  `resolve_trait_bound_method_calls`'s
/// receiver-name extraction supports both `StructRef` and `EnumRef`;
/// nothing in the previous test suite exercises the enum branch.
#[test]
fn trait_bound_enum_receiver() {
    three_way_roundtrip(
        r#"
trait Describe {
    function describe(self) -> String
}

enum Shape {
    Circle(Int)
    Square(Int)

    impl Describe {
        function describe(self) -> String {
            match self {
                Circle(_) -> "circle"
                Square(_) -> "square"
            }
        }
    }
}

function tell<T: Describe>(s: T) -> String {
    return s.describe()
}

function main() {
    print(tell(Circle(5)))
    print(tell(Square(3)))
}
"#,
    );
}

/// Two independent trait-bound generic functions, each called with the
/// same concrete type.  Forces two specializations to exist side-by-side
/// with the same (receiver, method) shape — catches a bug where a
/// single specialization cache would collapse them.
#[test]
fn trait_bound_multiple_templates_same_receiver_type() {
    three_way_roundtrip(
        r#"
trait Greet {
    function greet(self) -> String
}

struct Dog {
    String name

    impl Greet {
        function greet(self) -> String { return "woof" }
    }
}

function sayHi<T: Greet>(x: T) -> String { return x.greet() }
function sayBye<U: Greet>(x: U) -> String { return x.greet() }

function main() {
    let d: Dog = Dog("Rex")
    print(sayHi(d))
    print(sayBye(d))
}
"#,
    );
}

/// Generic-struct receiver: `sayHi<T: Greet>(c: Container<Int>)` must
/// pass through the two-stage cooperation — function-mono resolves the
/// call to the `Container` template's method FuncId, then struct-mono
/// promotes it to the mangled `Container__i64` specialization.
/// Untested before this commit.
#[test]
fn trait_bound_generic_struct_receiver() {
    three_way_roundtrip(
        r#"
trait Stringify {
    function stringify(self) -> String
}

struct Container<T> {
    T v

    impl Stringify {
        function stringify(self) -> String { return toString(self.v) }
    }
}

function show<S: Stringify>(s: S) -> String {
    return s.stringify()
}

function main() {
    let a: Container<Int> = Container(42)
    print(show(a))
}
"#,
    );
}

/// Same generic-struct receiver idea, but with two instantiations
/// (`Container<Int>` and `Container<String>`).  Exercises the guard
/// that struct-mono's vtable / method rekeying keeps the two specialized
/// copies distinct — a bug would route both through the same FuncId.
#[test]
fn trait_bound_generic_struct_receiver_two_instantiations() {
    let output = compile_and_run(
        r#"
trait Stringify {
    function stringify(self) -> String
}

struct Container<T> {
    T v

    impl Stringify {
        function stringify(self) -> String { return toString(self.v) }
    }
}

function show<S: Stringify>(s: S) -> String {
    return s.stringify()
}

function main() {
    let a: Container<Int> = Container(7)
    let b: Container<String> = Container("hello")
    print(show(a))
    print(show(b))
}
"#,
    );
    assert_eq!(output, vec!["7", "hello"]);
}
