//! Round-trip tests for `dyn Trait` operations in the IR interpreter.
//!
//! Exercises `Op::DynAlloc` / `Op::DynCall` through the IR interpreter and
//! compares output against the AST interpreter to confirm both paths agree
//! on dyn-dispatch semantics.

mod common;

use common::roundtrip;

/// Baseline: a function parameter typed `dyn Trait`, a zero-arg trait method.
/// Mirrors the Cranelift-side sanity test but exercises the IR-interp path.
#[test]
fn dyn_param_zero_arg_method() {
    roundtrip(
        r#"
trait Greet {
    function greet(self) -> String
}
struct Dog {
    String name
}
impl Greet for Dog {
    function greet(self) -> String { return "woof" }
}
function hello(g: dyn Greet) -> String { return g.greet() }
function main() { print(hello(Dog("Rex"))) }
"#,
    );
}

/// DynCall with multiple arguments (sema threads arg types correctly, the
/// IR interpreter prepends the concrete receiver, the call returns).
#[test]
fn dyn_call_with_multiple_args() {
    roundtrip(
        r#"
trait Mix {
    function mix(self, a: Int, b: String) -> String
}
struct Bowl {
    Int scale
}
impl Mix for Bowl {
    function mix(self, a: Int, b: String) -> String {
        return b
    }
}
function go(m: dyn Mix) -> String {
    return m.mix(7, "stir")
}
function main() { print(go(Bowl(2))) }
"#,
    );
}

/// A `dyn` value flowing through a second function call — the receiver is
/// already a `DynRef` at the inner call site, so no re-coercion should happen.
#[test]
fn dyn_value_forwarded_through_call() {
    roundtrip(
        r#"
trait Drawable {
    function draw(self) -> String
}
struct Circle {
    Int radius
}
impl Drawable for Circle {
    function draw(self) -> String { return "circle" }
}
struct Square {
    Int side
}
impl Drawable for Square {
    function draw(self) -> String { return "square" }
}
function inner(x: dyn Drawable) -> String { return x.draw() }
function outer(x: dyn Drawable) -> String { return inner(x) }
function main() {
    print(outer(Circle(3)))
    print(outer(Square(5)))
}
"#,
    );
}

/// `let x: dyn Trait = concrete` uses the sema-resolved annotation type,
/// so the `let` boundary coerces via `Op::DynAlloc`.
#[test]
fn dyn_let_annotation() {
    roundtrip(
        r#"
trait Drawable {
    function draw(self) -> String
}
struct Circle {
    Int radius
}
impl Drawable for Circle {
    function draw(self) -> String { return "circle" }
}
function main() {
    let x: dyn Drawable = Circle(1)
    print(x.draw())
}
"#,
    );
}

/// A type alias that expands to `dyn Trait` triggers the same coercion at
/// the `let` boundary as writing `dyn Trait` directly — verifies the
/// annotation-type plumbing handles aliases (regression test for the
/// previous AST-pattern-match-only approach).
#[test]
fn dyn_through_type_alias() {
    roundtrip(
        r#"
trait Drawable {
    function draw(self) -> String
}
type Drawn = dyn Drawable
struct Circle {
    Int radius
}
impl Drawable for Circle {
    function draw(self) -> String { return "circle" }
}
function main() {
    let x: Drawn = Circle(1)
    print(x.draw())
}
"#,
    );
}

/// Concrete value stored in an enum-variant field typed `dyn Trait`.
/// Regression test for the enum-side coercion path introduced alongside
/// the struct-ctor path.
#[test]
fn dyn_in_enum_variant_field() {
    roundtrip(
        r#"
trait Drawable {
    function draw(self) -> String
}
struct Circle { Int r }
impl Drawable for Circle {
    function draw(self) -> String { return "circle" }
}
enum Wrapper {
    Held(dyn Drawable)
    Empty
}
function describe(w: Wrapper) -> String {
    match w {
        Held(d) -> d.draw()
        Empty -> "empty"
    }
}
function main() {
    let w = Held(Circle(1))
    print(describe(w))
}
"#,
    );
}

/// A concrete value stored in a struct field typed `dyn Trait` is coerced
/// at the struct-constructor boundary (`coerce_args_to_expected` against
/// the struct's field types in IR lowering).
#[test]
fn dyn_in_struct_field() {
    roundtrip(
        r#"
trait Drawable {
    function draw(self) -> String
}
struct Circle {
    Int radius
}
impl Drawable for Circle {
    function draw(self) -> String { return "circle" }
}
struct Scene {
    dyn Drawable hero
}
function main() {
    let s = Scene(Circle(3))
    print(s.hero.draw())
}
"#,
    );
}

/// Reassignment to a `let mut x: dyn Trait` binding must coerce the new
/// concrete value via `Op::DynAlloc`. Without the coercion on Store, the
/// slot would hold a single-slot concrete after reassignment and DynCall
/// would load out of bounds.
#[test]
fn dyn_mut_reassignment_coerces_on_store() {
    roundtrip(
        r#"
trait Drawable {
    function draw(self) -> String
}
struct Circle {
    Int radius
}
impl Drawable for Circle {
    function draw(self) -> String { return "circle" }
}
struct Square {
    Int side
}
impl Drawable for Square {
    function draw(self) -> String { return "square" }
}
function main() {
    let mut d: dyn Drawable = Circle(1)
    print(d.draw())
    d = Square(2)
    print(d.draw())
    d = Circle(3)
    print(d.draw())
}
"#,
    );
}

/// Implicit-return (block without trailing `return`) from a function
/// declared `-> dyn Trait` must coerce the final expression. Separate
/// from the explicit-`return` path, which goes through `lower_return`.
#[test]
fn dyn_implicit_return_coerces() {
    roundtrip(
        r#"
trait Drawable {
    function draw(self) -> String
}
struct Circle {
    Int radius
}
impl Drawable for Circle {
    function draw(self) -> String { return "circle" }
}
function make() -> dyn Drawable {
    Circle(1)
}
function main() {
    let d = make()
    print(d.draw())
}
"#,
    );
}

/// Lambda returning `dyn Trait` — implicit-return path in a closure body.
/// Coercion runs at the lambda body boundary in `lower_lambda`.
#[test]
fn dyn_lambda_implicit_return_coerces() {
    roundtrip(
        r#"
trait Drawable {
    function draw(self) -> String
}
struct Circle {
    Int radius
}
impl Drawable for Circle {
    function draw(self) -> String { return "circle" }
}
function main() {
    let f: (Int) -> dyn Drawable = function(n: Int) -> dyn Drawable { Circle(n) }
    let d = f(5)
    print(d.draw())
}
"#,
    );
}

///
/// **Ignored** for the same reason as the compile-side mirror —
/// sema rejects match-arm union before lowering. See
/// `docs/known-issues.md` and the doc comment on the compile-side
/// test.
#[test]
#[ignore = "bidirectional inference gap on match arms — see known-issues.md"]
fn dyn_match_arm_coerces_to_function_return_type() {
    roundtrip(
        r#"
trait Drawable {
    function draw(self) -> String
}
struct Circle { Int r }
impl Drawable for Circle {
    function draw(self) -> String { return "circle" }
}
struct Square { Int s }
impl Drawable for Square {
    function draw(self) -> String { return "square" }
}
function choose(k: Int) -> dyn Drawable {
    return match k {
        0 -> Circle(1)
        _ -> Square(2)
    }
}
function main() {
    print(choose(0).draw())
    print(choose(1).draw())
}
"#,
    );
}

#[test]
fn closure_captures_dyn_value_and_dispatches() {
    roundtrip(
        r#"
trait Drawable {
    function draw(self) -> String
}
struct Circle { Int r }
impl Drawable for Circle {
    function draw(self) -> String { return "circle" }
}
function main() {
    let d: dyn Drawable = Circle(1)
    let f: () -> String = function() -> String { d.draw() }
    print(f())
    print(f())
}
"#,
    );
}

#[test]
fn same_concrete_two_traits_yields_two_vtables() {
    roundtrip(
        r#"
trait Speak { function say(self) -> String }
trait Tag { function tag(self) -> String }
struct Dog {}
impl Speak for Dog { function say(self) -> String { return "woof" } }
impl Tag for Dog { function tag(self) -> String { return "dog" } }
function main() {
    let s: dyn Speak = Dog()
    let t: dyn Tag = Dog()
    print(s.say())
    print(t.tag())
}
"#,
    );
}

#[test]
fn dyn_two_methods_invoked_separately_through_same_value() {
    roundtrip(
        r#"
trait TwoOps {
    function first(self) -> String
    function second(self) -> String
}
struct Pair {}
impl TwoOps for Pair {
    function first(self) -> String { return "a" }
    function second(self) -> String { return "b" }
}
function main() {
    let p: dyn TwoOps = Pair()
    print(p.first())
    print(p.second())
    let q = p
    print(q.second())
}
"#,
    );
}

#[test]
fn dyn_asymmetric_if_branches() {
    roundtrip(
        r#"
trait Drawable { function draw(self) -> String }
struct Circle { Int r }
impl Drawable for Circle {
    function draw(self) -> String { return "circle" }
}
function choose(flag: Bool, base: dyn Drawable) -> dyn Drawable {
    if flag { return Circle(1) } else { return base }
}
function main() {
    let d: dyn Drawable = Circle(99)
    print(choose(true, d).draw())
    print(choose(false, d).draw())
}
"#,
    );
}

#[test]
fn dyn_wide_trait_every_slot_resolves_in_ir_interp() {
    roundtrip(
        r#"
trait Wide {
    function m0(self) -> Int
    function m1(self) -> Int
    function m2(self) -> Int
    function m3(self) -> Int
    function m4(self) -> Int
    function m5(self) -> Int
    function m6(self) -> Int
    function m7(self) -> Int
    function m8(self) -> Int
    function m9(self) -> Int
    function m10(self) -> Int
    function m11(self) -> Int
}
struct Counter {}
impl Wide for Counter {
    function m0(self) -> Int { return 0 }
    function m1(self) -> Int { return 1 }
    function m2(self) -> Int { return 2 }
    function m3(self) -> Int { return 3 }
    function m4(self) -> Int { return 4 }
    function m5(self) -> Int { return 5 }
    function m6(self) -> Int { return 6 }
    function m7(self) -> Int { return 7 }
    function m8(self) -> Int { return 8 }
    function m9(self) -> Int { return 9 }
    function m10(self) -> Int { return 10 }
    function m11(self) -> Int { return 11 }
}
function main() {
    let w: dyn Wide = Counter()
    print(toString(w.m0()))
    print(toString(w.m5()))
    print(toString(w.m11()))
}
"#,
    );
}

#[test]
fn dyn_uncalled_function_with_dyn_param_compiles_in_ir_interp() {
    roundtrip(
        r#"
trait Drawable { function draw(self) -> String }
function unused(s: dyn Drawable) -> String { return s.draw() }
function main() { print("ok") }
"#,
    );
}

#[test]
fn dyn_method_with_reference_type_params_roundtrips() {
    roundtrip(
        r#"
trait Folder {
    function join(self, prefix: String, items: List<Int>) -> String
}
struct Glue {}
impl Folder for Glue {
    function join(self, prefix: String, items: List<Int>) -> String {
        let total: Int = items.reduce(0, function(a: Int, b: Int) -> Int { a + b })
        return prefix + toString(total)
    }
}
function main() {
    let f: dyn Folder = Glue()
    print(f.join("sum=", [1, 2, 3, 4]))
}
"#,
    );
}

#[test]
fn dyn_named_argument_coerces_roundtrips() {
    roundtrip(
        r#"
trait Drawable {
    function draw(self) -> String
}
struct Circle { Int r }
impl Drawable for Circle {
    function draw(self) -> String { return "circle" }
}
function render(label: String, s: dyn Drawable) -> String {
    return label + s.draw()
}
function main() {
    print(render(s: Circle(1), label: "tag="))
}
"#,
    );
}

#[test]
fn dyn_field_reassignment_coerces_roundtrips() {
    roundtrip(
        r#"
trait Drawable { function draw(self) -> String }
struct Circle { Int radius }
struct Square { Int side }
impl Drawable for Circle {
    function draw(self) -> String { return "circle" }
}
impl Drawable for Square {
    function draw(self) -> String { return "square" }
}
struct Scene { dyn Drawable hero }
function main() {
    let mut s: Scene = Scene(Circle(3))
    print(s.hero.draw())
    s.hero = Square(5)
    print(s.hero.draw())
}
"#,
    );
}
