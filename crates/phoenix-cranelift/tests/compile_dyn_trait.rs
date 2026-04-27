//! Integration tests for `dyn Trait` trait-object dispatch.
//!
//! Covers the Phase 2.2 MVP scope:
//! - Function parameters and return types typed `dyn Trait`.
//! - `let` bindings typed `dyn Trait` with concrete initializers (the
//!   coercion happens at the `let` boundary via `Op::DynAlloc`).
//! - Object-safety diagnostics at trait-declaration time.
//! - Coexistence with static-dispatch generics (`<T: Trait>` path).
//!
//! **Intentionally not covered** (documented in `docs/known-issues.md` and
//! `docs/design-decisions.md`):
//! - Heterogeneous list literals (`[Circle(1), Square(2)]` typed as
//!   `List<dyn Drawable>`) — blocked on bidirectional type inference in
//!   list-literal checking.
//! - `dyn Foo + Bar` multi-bound trait objects.
//! - Supertrait upcasting.
//! - `<T: Foo + Bar>` multi-bound generic parameters.

mod common;

use common::{compile_and_run, roundtrip, three_way_roundtrip, with_drawable_prelude};

#[test]
fn dyn_param_and_return() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
function render(s: dyn Drawable) -> String {
    return s.draw()
}

function main() {
    print(render(Circle(3)))
    print(render(Square(5)))
}
"#,
    ));
    assert_eq!(output, vec!["circle", "square"]);
}

#[test]
fn dyn_let_binding_with_branch_polymorphism() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
function choose(flag: Bool) -> dyn Drawable {
    if flag {
        return Circle(3)
    } else {
        return Square(5)
    }
}

function main() {
    let x: dyn Drawable = choose(true)
    print(x.draw())
    let y: dyn Drawable = choose(false)
    print(y.draw())
}
"#,
    ));
    assert_eq!(output, vec!["circle", "square"]);
}

#[test]
fn dyn_round_trips_through_ir_interpreter() {
    roundtrip(
        r#"
trait Greet {
    function greet(self) -> String
}

struct Dog {
    String name

    impl Greet {
        function greet(self) -> String {
            return "woof"
        }
    }
}

function hello(g: dyn Greet) -> String {
    return g.greet()
}

function main() {
    print(hello(Dog("Rex")))
}
"#,
    );
}

/// `Op::DynCall` with multiple explicit arguments: the data pointer is
/// prepended automatically; the rest of the arg list flows through
/// unchanged.
#[test]
fn dyn_call_with_multiple_args() {
    let output = compile_and_run(
        r#"
trait Mix {
    function mix(self, a: Int, b: String) -> String
}

struct Bowl {
    Int scale

    impl Mix {
        function mix(self, a: Int, b: String) -> String {
            return b
        }
    }
}

function go(m: dyn Mix) -> String {
    return m.mix(7, "stir")
}

function main() {
    print(go(Bowl(2)))
}
"#,
    );
    assert_eq!(output, vec!["stir"]);
}

/// A `dyn` value is forwarded from one function to another — the inner
/// site sees a `DynRef` receiver, not a concrete value. The IR lowering
/// must not re-wrap an already-coerced value.
#[test]
fn dyn_value_forwarded_through_call() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
function inner(x: dyn Drawable) -> String { return x.draw() }
function outer(x: dyn Drawable) -> String { return inner(x) }

function main() {
    print(outer(Circle(3)))
    print(outer(Square(5)))
}
"#,
    ));
    assert_eq!(output, vec!["circle", "square"]);
}

/// A concrete value stored in a struct field typed `dyn Trait` is coerced
/// at the struct-constructor boundary (both the paren-call and
/// struct-literal lowering paths apply `coerce_args_to_expected`).
#[test]
fn dyn_in_struct_field() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
struct Scene {
    dyn Drawable hero
}

function main() {
    let s = Scene(Circle(3))
    print(s.hero.draw())
}
"#,
    ));
    assert_eq!(output, vec!["circle"]);
}

/// `let x: AliasToDyn = concrete` — the sema-resolved annotation drives
/// the coercion, so type aliases that expand to `dyn Trait` must produce
/// the same `Op::DynAlloc` as writing the dyn type directly.
#[test]
fn dyn_through_type_alias() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
type Drawn = dyn Drawable

function main() {
    let x: Drawn = Circle(1)
    print(x.draw())
}
"#,
    ));
    assert_eq!(output, vec!["circle"]);
}

/// Three-way agreement (AST interp == IR interp == compiled binary) on
/// the baseline case. Protects against a regression in one backend that
/// the other two happen to paper over.
#[test]
fn three_way_agreement_on_baseline() {
    three_way_roundtrip(&with_drawable_prelude(
        r#"
function render(s: dyn Drawable) -> String { return s.draw() }
function main() {
    print(render(Circle(3)))
    print(render(Square(5)))
}
"#,
    ));
}

/// `dyn Trait` over an enum receiver — the concrete type is an enum
/// variant, so coercion flows through the `IrType::EnumRef` branch of
/// `coerce_to_expected`.
#[test]
fn dyn_over_enum() {
    let output = compile_and_run(
        r#"
trait Tag {
    function tag(self) -> String
}
enum Shape {
    Circle(Int)
    Square(Int)

    impl Tag {
        function tag(self) -> String {
            match self {
                Circle(_) -> "circ"
                Square(_) -> "sq"
            }
        }
    }
}
function describe(s: dyn Tag) -> String { return s.tag() }
function main() {
    print(describe(Circle(3)))
    print(describe(Square(5)))
}
"#,
    );
    assert_eq!(output, vec!["circ", "sq"]);
}

/// Trait method returning non-String types. Exercises DynCall return
/// paths where inst_results produces a different slot shape (Int: 1
/// slot; Bool: 1 slot i8).
#[test]
fn dyn_call_non_string_return_types() {
    let output = compile_and_run(
        r#"
trait Measure {
    function area(self) -> Int
    function is_big(self) -> Bool
}
struct Room {
    Int w
    Int h

    impl Measure {
        function area(self) -> Int { return self.w * self.h }
        function is_big(self) -> Bool { return self.area() > 50 }
    }
}
function main() {
    let r: dyn Measure = Room(4, 5)
    print(toString(r.area()))
    print(toString(r.is_big()))
    let big: dyn Measure = Room(10, 10)
    print(toString(big.is_big()))
}
"#,
    );
    assert_eq!(output, vec!["20", "false", "true"]);
}

/// Trait with three methods; the call site goes through slot 1 (not 0).
/// Verifies the vtable offset arithmetic doesn't assume the first slot.
#[test]
fn dyn_call_through_non_first_slot() {
    let output = compile_and_run(
        r#"
trait Multi {
    function first(self) -> String
    function second(self) -> String
    function third(self) -> String
}
struct Tri {
    impl Multi {
        function first(self) -> String { return "1" }
        function second(self) -> String { return "2" }
        function third(self) -> String { return "3" }
    }
}
function main() {
    let m: dyn Multi = Tri()
    print(m.second())
    print(m.third())
    print(m.first())
}
"#,
    );
    assert_eq!(output, vec!["2", "3", "1"]);
}

/// The same `(concrete, trait)` pair appears at several DynAlloc sites;
/// the vtable must be emitted once (cache hit). The observable assertion
/// is that the program behaves correctly — internal dedup is tested via
/// runtime output stability.
#[test]
fn dyn_same_concrete_trait_pair_reused() {
    let output = compile_and_run(
        r#"
trait Show {
    function show(self) -> String
}
struct Dot {
    impl Show {
        function show(self) -> String { return "." }
    }
}
function one() -> dyn Show { return Dot() }
function two() -> dyn Show { return Dot() }
function main() {
    print(one().show())
    print(two().show())
    let a: dyn Show = Dot()
    print(a.show())
}
"#,
    );
    assert_eq!(output, vec![".", ".", "."]);
}

/// A `let mut x: dyn Trait` reassigned to a different concrete type.
/// The initial Alloca stores a DynRef; reassignment must also coerce.
#[test]
fn dyn_mutable_let_reassignment() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
function main() {
    let mut x: dyn Drawable = Circle(1)
    print(x.draw())
    x = Square(2)
    print(x.draw())
}
"#,
    ));
    assert_eq!(output, vec!["circle", "square"]);
}

/// A trait with zero methods is vacuously object-safe; `dyn EmptyTrait`
/// compiles and can hold any implementor, even though it has nothing
/// callable.
#[test]
fn dyn_over_zero_method_trait() {
    let output = compile_and_run(
        r#"
trait Marker {}
struct Thing {
    impl Marker {}
}
function take(_: dyn Marker) -> String { return "ok" }
function main() {
    print(take(Thing()))
}
"#,
    );
    assert_eq!(output, vec!["ok"]);
}

/// `dyn Trait` empty-trait case also round-trips through the IR
/// interpreter.  Mirrors `dyn_over_zero_method_trait` but ensures the
/// IR-interp path agrees (the vtable has zero slots, so any
/// discrepancy in slot-index arithmetic would surface here too).
#[test]
fn dyn_over_zero_method_trait_roundtrips() {
    roundtrip(
        r#"
trait Marker {}
struct Thing {
    impl Marker {}
}
function take(_: dyn Marker) -> String { return "ok" }
function main() { print(take(Thing())) }
"#,
    );
}

/// Library function that declares `dyn Trait` in its signature but is
/// never invoked at a coercion site (no `DynAlloc` is emitted).  The
/// verifier and codegen must still succeed — regression test for the
/// bug where both required a registered vtable for every trait that
/// appeared in a `DynCall`.
#[test]
fn uncalled_dyn_receiving_function_still_compiles() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
// Never called, but references `dyn Drawable` in params — previously
// caused a verifier crash because no vtable was registered.
function unused(s: dyn Drawable) -> String {
    return s.draw()
}
function main() {
    print("ok")
}
"#,
    ));
    assert_eq!(output, vec!["ok"]);
}

/// A concrete value stored in an enum-variant field typed `dyn Trait`
/// is coerced at the `EnumAlloc` boundary.  Regression test for a
/// previous gap where only struct-ctor paths applied the coercion.
#[test]
fn dyn_in_enum_variant_field() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
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
    ));
    assert_eq!(output, vec!["circle"]);
}

/// A `dyn Trait` value flowing through a trait *method* parameter
/// (not just a free-function parameter).  The receiver is `dyn` and
/// one of the method args is also `dyn` — both must coerce.
#[test]
fn dyn_trait_method_with_dyn_param() {
    let output = compile_and_run(
        r#"
trait Speak {
    function greet(self) -> String
}
trait Echo {
    function echo(self, other: dyn Speak) -> String
}
struct Dog {
    impl Speak {
        function greet(self) -> String { return "woof" }
    }
}
struct Room {
    impl Echo {
        function echo(self, other: dyn Speak) -> String { return other.greet() }
    }
}
function main() {
    let r: dyn Echo = Room()
    print(r.echo(Dog()))
}
"#,
    );
    assert_eq!(output, vec!["woof"]);
}

/// `List<dyn Trait>` built incrementally via `push()` — the workaround
/// documented in `docs/known-issues.md` for the (still-unsupported)
/// literal form.
///
/// **Ignored** because the workaround is itself not currently
/// functional: sema types the empty list literal `[]` as `List<T>` (no
/// context-sensitive inference into the annotation), so the let-binding
/// rejects the init with a "type mismatch" diagnostic before lowering
/// even runs.  Tracked in `docs/known-issues.md` under *List<dyn Trait>
/// literal initialization in compiled mode* — unblocking the literal
/// path and the push path requires the same bidirectional inference
/// work.
/// Vtable stress: a trait with 12 methods exercises `method_idx * SLOT_SIZE`
/// offset arithmetic beyond small indices. Every method-index must resolve
/// to the correct rodata slot; a bug in offset computation (e.g. signed
/// overflow, wrong stride, off-by-one at the boundary) would show up as a
/// method returning the wrong value at some index.
#[test]
fn dyn_vtable_wide_trait_every_slot_resolves() {
    let output = compile_and_run(
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
struct Counter {
    impl Wide {
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
}
function main() {
    let w: dyn Wide = Counter()
    print(toString(w.m0()))
    print(toString(w.m1()))
    print(toString(w.m2()))
    print(toString(w.m3()))
    print(toString(w.m4()))
    print(toString(w.m5()))
    print(toString(w.m6()))
    print(toString(w.m7()))
    print(toString(w.m8()))
    print(toString(w.m9()))
    print(toString(w.m10()))
    print(toString(w.m11()))
}
"#,
    );
    assert_eq!(
        output,
        vec!["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11"]
    );
}

/// Trait method whose parameter and return types are *reference* types
/// (List<Int>, String). Exercises the fat-pointer ABI boundary: the
/// receiver's `data_ptr` is prepended to the ABI-normal argument list,
/// and reference-type args must flow through without getting collapsed
/// into a single slot by a confused arg-packer. Regression guard for
/// codegen that only tested value-typed method params.
#[test]
fn dyn_method_with_reference_type_params_and_return() {
    let output = compile_and_run(
        r#"
trait Folder {
    function join(self, prefix: String, items: List<Int>) -> String
}
struct Glue {
    impl Folder {
        function join(self, prefix: String, items: List<Int>) -> String {
            let total: Int = items.reduce(0, function(a: Int, b: Int) -> Int { a + b })
            return prefix + toString(total)
        }
    }
}
function main() {
    let f: dyn Folder = Glue()
    print(f.join("sum=", [1, 2, 3, 4]))
}
"#,
    );
    assert_eq!(output, vec!["sum=10"]);
}

#[test]
#[ignore = "List<dyn Trait> literal init unsupported — see known-issues.md"]
fn dyn_list_via_push_workaround() {
    let output = compile_and_run(
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
struct Square {
    Int s

    impl Drawable {
        function draw(self) -> String { return "square" }
    }
}
function main() {
    let mut shapes: List<dyn Drawable> = []
    shapes.push(Circle(1))
    shapes.push(Square(2))
    for s in shapes {
        print(s.draw())
    }
}
"#,
    );
    assert_eq!(output, vec!["circle", "square"]);
}

/// Three-way agreement (AST == IR == compiled) on the MVP coverage.
/// Catches regressions in any single backend that the other two
/// happen to paper over.
#[test]
fn three_way_agreement_on_multi_arg_dispatch() {
    three_way_roundtrip(
        r#"
trait Mix {
    function mix(self, a: Int, b: String) -> String
}
struct Bowl {
    Int scale

    impl Mix {
        function mix(self, a: Int, b: String) -> String { return b }
    }
}
function main() {
    let m: dyn Mix = Bowl(2)
    print(m.mix(7, "stir"))
}
"#,
    );
}

/// A lambda that returns `dyn Trait`, invoked via a first-class
/// function value (`Op::CallIndirect`).  Exercises the dyn-coercion
/// path through the closure-call lowering, not just direct calls.
#[test]
fn dyn_returned_by_lambda_via_call_indirect() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
function main() {
    let make: () -> dyn Drawable = function() -> dyn Drawable { Circle(1) }
    print(make().draw())
}
"#,
    ));
    assert_eq!(output, vec!["circle"]);
}

/// Implicit-return coercion: a function declared to return `dyn Trait`
/// whose body is a trailing concrete-value expression (no explicit
/// `return`) must still get the concrete wrapped at the function
/// boundary.  Regression test for the implicit-return path, which is
/// separate from `Statement::Return(val)`.
#[test]
fn dyn_implicit_return_from_top_level_function() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
// No explicit `return` — Circle(1) is the tail expression.
function make() -> dyn Drawable { Circle(1) }
function main() {
    print(make().draw())
}
"#,
    ));
    assert_eq!(output, vec!["circle"]);
}

/// Register-side idempotency: the same `(concrete_type, trait_name)`
/// pair appearing at many DynAlloc sites yields exactly one entry in
/// `IrModule::dyn_vtables`.  A direct unit test of
/// `register_dyn_vtable` isn't practical (it's tied to `LoweringContext`
/// which needs a `ResolvedModule`), so we exercise it end-to-end via the
/// public lowering entry point and inspect the resulting module.
#[test]
fn dyn_vtable_registration_is_idempotent() {
    use phoenix_common::span::SourceId;
    use phoenix_lexer::lexer::tokenize;
    use phoenix_parser::parser;
    use phoenix_sema::checker;

    let source = r#"
trait Drawable {
    function draw(self) -> String
}
struct Circle {
    Int r

    impl Drawable {
        function draw(self) -> String { return "circle" }
    }
}
function one() -> dyn Drawable { return Circle(1) }
function two() -> dyn Drawable { return Circle(2) }
function three(x: dyn Drawable) -> String { return x.draw() }
function main() {
    print(three(Circle(3)))
    print(one().draw())
    print(two().draw())
}
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty());
    let result = checker::check(&program);
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let module = phoenix_ir::lower(&program, &result.module);

    let matching: Vec<_> = module
        .dyn_vtables
        .keys()
        .filter(|(ct, tn)| ct == "Circle" && tn == "Drawable")
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "(Circle, Drawable) should be registered exactly once despite 3 coercion sites; \
         got: {matching:?}"
    );
}

/// Three-way agreement on struct-field-typed `dyn Trait`.
#[test]
fn three_way_agreement_on_struct_field() {
    three_way_roundtrip(&with_drawable_prelude(
        r#"
struct Scene { dyn Drawable hero }
function main() {
    let s = Scene(Circle(3))
    print(s.hero.draw())
}
"#,
    ));
}

/// **Ignored — sema's match-result inference does not propagate the
/// function's `dyn Trait` return type into arm-result unification.**
/// Instead it tries to unify the arm types directly (`Circle` vs
/// `Square`) and rejects with a "match arm type mismatch" diagnostic
/// before lowering even runs.  This is the bidirectional-inference gap
/// — same root cause as `List<dyn Trait>` literals — and the proper
/// fix lifts it for both. Tracked under "match-arm coercion to
/// `dyn Trait` return type" in `docs/known-issues.md`.
#[test]
#[ignore = "bidirectional inference gap on match arms — see known-issues.md"]
fn dyn_match_arm_coerces_to_function_return_type() {
    let output = compile_and_run(
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
struct Square {
    Int s

    impl Drawable {
        function draw(self) -> String { return "square" }
    }
}
// Match on an Int discriminator so the arm bodies are the only place
// the concrete-to-dyn coercion can happen — keeps the test focused
// on the join, not on enum-variant value plumbing.
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
    assert_eq!(output, vec!["circle", "square"]);
}

/// Trait-bounded `<T: Trait>` parameters that coerce into a
/// `dyn Trait` slot are materialized as `Op::UnresolvedDynAlloc` at
/// IR-lowering time, then rewritten to concrete `Op::DynAlloc`
/// (with the vtable keyed on the post-substitution type) during
/// function-monomorphization's Pass B. The specialized bodies
/// `dyn_describe__s_Circle` and `dyn_describe__s_Square` each
/// carry a concrete `DynAlloc("Drawable", <concrete>, ...)` and
/// register `(Circle, Drawable)` / `(Square, Drawable)` vtables.
///
/// Uses `three_way_roundtrip` to guarantee the AST interpreter, the
/// IR interpreter, and the compiled backend all agree — a silent
/// divergence between the mono-time placeholder resolution and the
/// AST interp's direct-call path would otherwise escape.
#[test]
fn dyn_alloc_inside_generic_bounded_function() {
    three_way_roundtrip(
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
struct Square {
    Int s

    impl Drawable {
        function draw(self) -> String { return "square" }
    }
}
function dyn_describe<T: Drawable>(x: T) -> String {
    let d: dyn Drawable = x
    return d.draw()
}
function main() {
    print(dyn_describe(Circle(1)))
    print(dyn_describe(Square(2)))
}
"#,
    );
}

/// `<T: Trait>` → `dyn Trait` coercion where `T` is instantiated to a
/// *generic struct* (`Container<Int>`) rather than a plain concrete
/// one.  This exercises the load-bearing handoff between
/// function-monomorphization and struct-monomorphization:
///
/// 1. Function-mono specializes `wrap<T: Drawable>` at `T = Container<Int>`.
///    `resolve_unresolved_dyn_allocs` derives the *bare* concrete name
///    (`"Container"`, not yet `"Container__i64"`) and registers
///    `dyn_vtables[("Container", "Drawable")]` with the template
///    method's FuncId — because `method_index` is still keyed by bare
///    names at this point.
/// 2. Struct-mono then mangles `Container<Int>` to `Container__i64`,
///    rekeys the vtable entry to `("Container__i64", "Drawable")`, and
///    re-resolves the method FuncId through the mangled `method_index`.
///
/// A silent reordering or omission of either pass would install a
/// template FuncId (inert stub) in the live vtable, which the
/// verifier accepts but Cranelift would crash on.  Pinned
/// three-way so the AST / IR / compiled backends all agree on the
/// resulting dispatch.
#[test]
fn dyn_alloc_inside_generic_bounded_function_with_generic_struct() {
    three_way_roundtrip(
        r#"
trait Drawable {
    function draw(self) -> String
}
struct Container<T> {
    T value

    impl Drawable {
        function draw(self) -> String { return "container" }
    }
}
function wrap<T: Drawable>(x: T) -> String {
    let d: dyn Drawable = x
    return d.draw()
}
function main() {
    let c: Container<Int> = Container(42)
    print(wrap(c))
    let s: Container<String> = Container("hi")
    print(wrap(s))
}
"#,
    );
}

/// Default-argument coercion into a `dyn Trait` slot: the callee's
/// default expression is lowered at the call site via
/// `merge_call_args` (see `crates/phoenix-ir/src/lower_expr.rs`),
/// then `coerce_call_args` wraps the concrete value in
/// `Op::DynAlloc` for the `dyn Drawable` parameter.  Three-way
/// roundtrip pins AST interp / IR interp / compiled agreement on the
/// default + coerce sequence.
#[test]
fn dyn_default_argument_coerces() {
    three_way_roundtrip(
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
function render(s: dyn Drawable = Circle(1)) -> String { return s.draw() }
function main() {
    print(render())
}
"#,
    );
}

/// Concrete default flowing into a `dyn Trait` slot on a *generic*
/// callee.  Exercises both 2026-04-24 fixes at once:
///
/// - `merge_call_args` synthesizes the missing slot from the default
///   expression (`Circle(1)`, typed `StructRef("Circle", [])`);
/// - `coerce_call_args` wraps the concrete value in `Op::DynAlloc`
///   for the `dyn Drawable` parameter;
/// - Function-mono then specializes the generic callee at `T = Int`,
///   leaving the defaulted/coerced `dyn Drawable` slot alone.
///
/// Regression site: a bug that runs default synthesis *after*
/// substitution (or that lets the callee's type-parametricity
/// contaminate the concrete default's typing) would surface as either
/// a sema reject or a mono-time `contains_type_var` panic.
#[test]
fn dyn_default_argument_coerces_in_generic_callee() {
    three_way_roundtrip(
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
function render<T>(tag: T, s: dyn Drawable = Circle(1)) -> String {
    return s.draw()
}
function main() {
    print(render(0))
    print(render("ignored"))
}
"#,
    );
}

/// Pins an orthogonal gap: sema does not propagate the outer generic's
/// trait bound through a nested generic call.  `outer<T: Drawable>`
/// calling `inner<U: Drawable>(x)` where `x: T` is rejected with
/// "type `T` does not implement trait `Drawable`" — sema's
/// trait-bound inference doesn't see that `T: Drawable` satisfies
/// `U: Drawable` at the call site.
///
/// Separate from the `UnresolvedDynAlloc` / default-argument fixes
/// landing in this change; parking as an `#[ignore]` regression so
/// it's trivially discoverable when that gap closes.
#[test]
#[ignore = "trait bounds don't propagate through nested generic calls — pre-existing sema gap"]
fn nested_generic_dyn_coercion_specializes() {
    three_way_roundtrip(
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
struct Square {
    Int s

    impl Drawable {
        function draw(self) -> String { return "square" }
    }
}
function inner<U: Drawable>(y: U) -> String {
    let d: dyn Drawable = y
    return d.draw()
}
function outer<T: Drawable>(x: T) -> String {
    return inner(x)
}
function main() {
    print(outer(Circle(1)))
    print(outer(Square(2)))
}
"#,
    );
}

/// Generic function with a defaulted concrete-typed parameter.  The
/// default expression must lower with a concrete type (Int literal
/// here) — a `TypeVar`-typed default would be rejected by sema's
/// new `has_type_vars()` check.  Pins that concrete defaults in
/// generic callees work end-to-end.
#[test]
fn generic_function_with_concrete_default_argument() {
    three_way_roundtrip(
        r#"
function identity<T>(x: T, tag: Int = 7) -> Int { return tag }
function main() {
    print(identity("hello"))
    print(identity(42, 99))
    print(identity("alice", 3))
}
"#,
    );
}

/// Named-arg override of a default: caller supplies every slot by name,
/// which should win over the registered default expressions.  Pins the
/// "named arg wins over default" branch of `merge_call_args`.
#[test]
fn named_argument_overrides_default() {
    three_way_roundtrip(
        r#"
function combine(x: Int = 1, y: Int = 2, z: Int = 3) -> Int {
    return x * 100 + y * 10 + z
}
function main() {
    print(combine())
    print(combine(y: 9))
    print(combine(z: 7, x: 4))
}
"#,
    );
}

#[test]
fn dyn_named_argument_coerces() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
function render(label: String, s: dyn Drawable) -> String {
    return label + s.draw()
}
function main() {
    print(render(s: Circle(1), label: "tag="))
}
"#,
    ));
    assert_eq!(output, vec!["tag=circle"]);
}

#[test]
fn closure_captures_dyn_value_and_dispatches() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
function main() {
    let d: dyn Drawable = Circle(1)
    let f: () -> String = function() -> String { d.draw() }
    print(f())
    // Call again to ensure the captured fat pointer is stable.
    print(f())
}
"#,
    ));
    assert_eq!(output, vec!["circle", "circle"]);
}

#[test]
fn same_concrete_two_traits_yields_two_vtables() {
    let output = compile_and_run(
        r#"
trait Speak {
    function say(self) -> String
}
trait Tag {
    function tag(self) -> String
}
struct Dog {
    impl Speak {
        function say(self) -> String { return "woof" }
    }
    impl Tag {
        function tag(self) -> String { return "dog" }
    }
}
function main() {
    let s: dyn Speak = Dog()
    let t: dyn Tag = Dog()
    print(s.say())
    print(t.tag())
}
"#,
    );
    assert_eq!(output, vec!["woof", "dog"]);
}

#[test]
fn dyn_two_methods_invoked_separately_through_same_value() {
    let output = compile_and_run(
        r#"
trait TwoOps {
    function first(self) -> String
    function second(self) -> String
}
struct Pair {
    impl TwoOps {
        function first(self) -> String { return "a" }
        function second(self) -> String { return "b" }
    }
}
function main() {
    let p: dyn TwoOps = Pair()
    print(p.first())
    print(p.second())
    // Same value through a different binding to ensure the vtable
    // pointer stays correct across moves.
    let q = p
    print(q.second())
}
"#,
    );
    assert_eq!(output, vec!["a", "b", "b"]);
}

#[test]
fn dyn_asymmetric_if_branches() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
function choose(flag: Bool, base: dyn Drawable) -> dyn Drawable {
    if flag {
        return Circle(1)
    } else {
        return base
    }
}
function main() {
    let d: dyn Drawable = Circle(99)
    print(choose(true, d).draw())
    print(choose(false, d).draw())
}
"#,
    ));
    assert_eq!(output, vec!["circle", "circle"]);
}

#[test]
fn dyn_field_reassignment_coerces_concrete() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
struct Scene { dyn Drawable hero }
function main() {
    let mut s: Scene = Scene(Circle(3))
    print(s.hero.draw())
    s.hero = Square(5)
    print(s.hero.draw())
}
"#,
    ));
    assert_eq!(output, vec!["circle", "square"]);
}

/// Method dispatch through a `dyn` struct field — pins that field
/// access followed by `.method()` routes through `DynCall`, not a
/// static method lookup keyed on a concrete type. Adjacent to
/// [`dyn_in_struct_field`], which only reads the field; this variant
/// calls a method on the read.
#[test]
fn dyn_field_method_chaining() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
struct Scene { dyn Drawable hero }
function make() -> Scene { return Scene(Circle(3)) }
function main() {
    print(make().hero.draw())
}
"#,
    ));
    assert_eq!(output, vec!["circle"]);
}

/// Recursive dyn dispatch: a trait method takes a `dyn Trait` of the
/// same trait. Exercises the coercion path on an argument that is
/// itself a dyn value and pins that nested dispatch works correctly.
#[test]
fn dyn_recursive_dispatch() {
    let output = compile_and_run(
        r#"
trait Greeter {
    function greet(self) -> String
}
struct Hello {
    Int dummy

    impl Greeter {
        function greet(self) -> String { return "hello" }
    }
}
struct World {
    Int dummy

    impl Greeter {
        function greet(self) -> String { return "world" }
    }
}
function combine(a: dyn Greeter, b: dyn Greeter) -> String {
    return a.greet()
}
function main() {
    let h: dyn Greeter = Hello(0)
    let w: dyn Greeter = World(0)
    print(combine(h, w))
    print(combine(w, h))
}
"#,
    );
    assert_eq!(output, vec!["hello", "world"]);
}

/// `dyn Trait` in an `Option` payload. Until bidirectional inference
/// lands (see docs/known-issues.md), you cannot write
/// `let x: Option<dyn Trait> = Some(Circle(1))` directly — the element
/// type doesn't propagate into the payload literal — but you *can*
/// go through an intermediate `dyn` binding. This pins that the
/// binding-then-wrap path at least works so the Option<dyn> Phase-3
/// fix has a baseline to build on.
#[test]
fn dyn_in_option_payload_via_let_binding() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
function main() {
    let d: dyn Drawable = Circle(3)
    let opt: Option<dyn Drawable> = Some(d)
    match opt {
        Some(v) -> print(v.draw())
        None -> print("none")
    }
}
"#,
    ));
    assert_eq!(output, vec!["circle"]);
}

/// Three-way agreement on the enum-variant + reassignment paths. These
/// are the two scenarios most likely for IR-interp and Cranelift to
/// diverge silently (variant payload layout vs. mutable-binding store
/// semantics), so cross-validation against the AST interpreter pins
/// both backends to the same observable behaviour.
/// IR-interp has its own `roundtrip` (AST↔IR), Cranelift tests
/// validate against IR; this widens the three-way coverage so a
/// regression in one backend fires immediately.
#[test]
fn three_way_agreement_on_enum_variant_and_reassignment() {
    three_way_roundtrip(&with_drawable_prelude(
        r#"
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
    print(describe(Held(Circle(1))))
    print(describe(Held(Square(2))))
    let mut x: dyn Drawable = Circle(3)
    print(x.draw())
    x = Square(4)
    print(x.draw())
}
"#,
    ));
}

/// Generic and dyn dispatch coexisting in the *same function signature*.
/// The driver test `static_and_dyn_dispatch_coexist` pins they can both
/// be declared at module scope; this pins they can flow through one
/// call site without interfering. The generic parameter is unused at
/// the body level (no `<T: Trait>` method call, which is documented as
/// not yet supported in compiled mode — see known-issues.md), so the
/// test isolates the *signature-level* coupling rather than the
/// monomorphize-vs-lower gap.
#[test]
fn dyn_and_generic_param_coexist_in_signature() {
    let output = compile_and_run(&with_drawable_prelude(
        r#"
// Takes both a dyn Drawable (runtime dispatch) and a generic T
// (monomorphized away). Only the dyn arg is dispatched; T just rides
// through as data. This is the smallest signature that exercises both
// metadata paths (vtable registration for the dyn arg, monomorph
// keying for the generic arg) at the same call site.
function combine<T>(d: dyn Drawable, label: T, count: Int) -> String {
    return d.draw()
}
function main() {
    print(combine(Circle(1), "ignored", 7))
    print(combine(Square(2), 99, 7))
}
"#,
    ));
    assert_eq!(output, vec!["circle", "square"]);
}
