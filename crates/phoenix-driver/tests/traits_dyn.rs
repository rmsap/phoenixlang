//! End-to-end tests for `dyn Trait` runtime dispatch via the AST
//! interpreter path (`phoenix_interp`) — mirrors the Cranelift-backend
//! tests in `phoenix-cranelift/tests/compile_dyn_trait.rs`.
//!
//! Exercises the AST interpreter's trait-object dispatch (introduced
//! alongside the Phase 2.2 vtable ABI) and the sema-level diagnostics
//! (object-safety, coexistence with generic bounds).

mod common;

use common::{expect_parse_error, expect_type_error, run_expect};

#[test]
fn dyn_drawable_through_function() {
    run_expect(
        r#"
trait Drawable {
    function draw(self) -> String
}
struct Circle {
    Int radius

    impl Drawable {
        function draw(self) -> String { return "circle" }
    }
}
struct Square {
    Int side

    impl Drawable {
        function draw(self) -> String { return "square" }
    }
}
function render(s: dyn Drawable) -> String {
    return s.draw()
}
function main() {
    print(render(Circle(3)))
    print(render(Square(5)))
}
"#,
        &["circle", "square"],
    );
}

/// Object-safety: a trait that returns `Self` cannot be used as
/// `dyn Trait`. Sema must catch this at the `dyn` type-expression site.
#[test]
fn non_object_safe_trait_rejects_self_return() {
    expect_type_error(
        r#"
trait Cloneable {
    function clone(self) -> Self
}
function f(x: dyn Cloneable) -> Int {
    return 0
}
function main() { print(1) }
"#,
        "not object-safe",
    );
}

/// Object-safety: a method that takes `Self` by value is not
/// dispatchable through a vtable.
#[test]
fn non_object_safe_trait_rejects_self_param() {
    expect_type_error(
        r#"
trait Eq {
    function equals(self, other: Self) -> Bool
}
function f(x: dyn Eq) -> Int {
    return 0
}
function main() { print(1) }
"#,
        "not object-safe",
    );
}

/// Coexistence with static-dispatch generic bounds: the same concrete
/// type can be passed to `<T: Trait>` (static) and `dyn Trait` (dynamic)
/// in the same program.
///
/// Note: this test uses the AST interpreter (which resolves trait-bound
/// method calls on type variables natively).  The Cranelift backend has
/// a separate, pre-existing gap: `<T: Trait>` method calls on TypeVar
/// receivers don't lower to a dispatchable IR op today (pre-dates the
/// dyn-Trait work).  That's tracked independently.
#[test]
fn static_and_dyn_dispatch_coexist() {
    run_expect(
        r#"
trait Drawable {
    function draw(self) -> String
}
struct Circle {
    Int radius

    impl Drawable {
        function draw(self) -> String { return "circle" }
    }
}
function print_dyn(x: dyn Drawable) {
    print(x.draw())
}
function main() {
    print_dyn(Circle(1))
}
"#,
        &["circle"],
    );
}

/// Using a non-existent trait name with `dyn` is a type error.
#[test]
fn unknown_trait_in_dyn_position() {
    expect_type_error(
        r#"
function f(x: dyn Nonexistent) -> Int { return 0 }
function main() { print(1) }
"#,
        "unknown trait",
    );
}

/// Using a struct name (not a trait) in `dyn` position is a type error —
/// sema must reject "dyn StructName" even though the ident resolves to a
/// known type.
#[test]
fn struct_name_in_dyn_position_rejected() {
    expect_type_error(
        r#"
struct Point {
    Int x
    Int y
}
function f(x: dyn Point) -> Int { return 0 }
function main() { print(1) }
"#,
        "unknown trait",
    );
}

/// `dyn Self` outside a trait body is a type error — `Self` is not a
/// trait name at module scope.
#[test]
fn dyn_self_at_module_scope_rejected() {
    expect_type_error(
        r#"
function f(x: dyn Self) -> Int { return 0 }
function main() { print(1) }
"#,
        "unknown trait",
    );
}

/// Parser error: `dyn` without a following ident.
#[test]
fn dyn_without_trait_name_parse_error() {
    expect_parse_error(
        r#"
function main() {
    let x: dyn = 1
    print(0)
}
"#,
        "expected trait name after `dyn`",
    );
}

/// Object-safety rejects `Self` nested deep in a generic type argument
/// (e.g. `Map<String, Self>`) — the recursive check in
/// `phoenix-sema/src/object_safety.rs` must descend into all generic args.
#[test]
fn object_safety_rejects_deeply_nested_self() {
    expect_type_error(
        r#"
trait Boxed {
    function unbox(self) -> Map<String, Self>
}
function f(x: dyn Boxed) -> Int { return 0 }
function main() { print(1) }
"#,
        "not object-safe",
    );
}

/// Coercing an unbounded type variable into `dyn Trait` must be rejected
/// — without the bound, sema has no basis to emit a vtable. Regression
/// test for the previous rule that accepted any TypeVar via a blanket
/// compatibility wildcard.
#[test]
fn dyn_coercion_from_unbounded_type_var_rejected() {
    expect_type_error(
        r#"
trait Drawable {
    function draw(self) -> String
}
function wrap<T>(x: T) -> String {
    let d: dyn Drawable = x
    return d.draw()
}
function main() { print(1) }
"#,
        "Drawable",
    );
}

/// Generic traits cannot be used as `dyn` — method signatures
/// containing unbound type parameters would silently accept any arg via
/// the TypeVar wildcard. Reject at the `dyn` type site.
#[test]
fn dyn_over_generic_trait_rejected() {
    expect_type_error(
        r#"
trait Foo<T> {
    function f(self, x: T) -> String
}
function go(f: dyn Foo) -> String {
    return "x"
}
function main() { print(1) }
"#,
        "generic trait",
    );
}

/// Coercion from a type parameter with the matching bound is accepted
/// (the check consults `current_type_param_bounds`). Runs under the AST
/// interpreter, which resolves trait-bound dispatch natively.
#[test]
fn dyn_coercion_from_bounded_type_var_accepted() {
    run_expect(
        r#"
trait Drawable {
    function draw(self) -> String
}
struct Circle {
    Int r

    impl Drawable {
        function draw(self) -> String { return "circle" }
    }
}
function wrap<T: Drawable>(x: T) -> String {
    let d: dyn Drawable = x
    return d.draw()
}
function main() {
    print(wrap(Circle(1)))
}
"#,
        &["circle"],
    );
}

/// **README example pin.** The "Static and dynamic dispatch" snippet in
/// `README.md` advertises that `dyn Trait` and `<T: Trait>` parameters
/// type-check side-by-side. This test pulls the snippet's two function
/// declarations into a runnable program (with the `Display` trait from
/// the immediately-preceding README example, plus a concrete impl and a
/// `main`) and asserts both paths produce the expected string.
///
/// Run via the AST interpreter so both paths execute today — the
/// compiled-mode `<T: Trait>`-method-call path is documented as broken
/// in `docs/known-issues.md`. If the README snippet is ever updated,
/// update this test in lockstep.
#[test]
fn readme_static_and_dyn_dispatch_example_runs() {
    run_expect(
        r#"
trait Display {
    function toString(self) -> String
}

struct Tag {
    String label

    impl Display {
        function toString(self) -> String { return self.label }
    }
}

// Verbatim from README.md "Static and dynamic dispatch":
function describeStatic<T: Display>(item: T) -> String { item.toString() }
function describeDyn(item: dyn Display) -> String      { item.toString() }

function main() {
    print(describeStatic(Tag("static")))
    print(describeDyn(Tag("dyn")))
}
"#,
        &["static", "dyn"],
    );
}
