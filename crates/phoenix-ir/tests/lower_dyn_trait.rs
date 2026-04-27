//! Positive IR-lowering tests for `dyn Trait`.
//!
//! The verifier tests in `phoenix-ir/src/verify/dyn_ops.rs` construct
//! malformed IR by hand and assert specific invariant violations are
//! flagged. They do not exercise `lower()` at all.
//!
//! These tests close the audit-flagged gap by driving the full pipeline
//! (lex → parse → check → lower) on small `dyn Trait` programs and
//! pinning the resulting IR module shape: trait registration, vtable
//! materialization, slot-index resolution, and the textual form of
//! `Op::DynAlloc`/`Op::DynCall` (Phoenix IR has no parser, so the
//! display format is the closest analogue to a round-trip).

use phoenix_common::span::SourceId;
use phoenix_ir::instruction::{Op, ValueId};
use phoenix_ir::lower;
use phoenix_ir::verify::verify;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

/// Drive lex → parse → sema → IR lowering and assert each phase passed
/// cleanly. Returns the lowered module for inspection.
fn lower_program(source: &str) -> phoenix_ir::module::IrModule {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "sema errors: {:?}",
        result.diagnostics
    );
    let module = lower(&program, &result.module);
    let verify_errors = verify(&module);
    assert!(
        verify_errors.is_empty(),
        "verifier errors: {verify_errors:?}"
    );
    module
}

const DRAWABLE_PROGRAM: &str = "
trait Drawable {
    function draw(self) -> String
}
struct Circle {
    Int radius

    impl Drawable {
        function draw(self) -> String { return \"circle\" }
    }
}
struct Square {
    Int side

    impl Drawable {
        function draw(self) -> String { return \"square\" }
    }
}
function render(s: dyn Drawable) -> String { return s.draw() }
function main() {
    print(render(Circle(3)))
    print(render(Square(5)))
}
";

#[test]
fn lowering_registers_object_safe_trait_in_module() {
    let module = lower_program(DRAWABLE_PROGRAM);
    let trait_info = module
        .traits
        .get("Drawable")
        .expect("Drawable must be mirrored into IrModule::traits");
    assert_eq!(trait_info.methods.len(), 1, "Drawable has one method");
    assert_eq!(trait_info.methods[0].name, "draw");
}

#[test]
fn lowering_materializes_vtables_for_each_concrete_implementor() {
    let module = lower_program(DRAWABLE_PROGRAM);
    let circle_vt = module
        .dyn_vtables
        .get(&("Circle".to_string(), "Drawable".to_string()))
        .expect("(Circle, Drawable) vtable should be registered");
    let square_vt = module
        .dyn_vtables
        .get(&("Square".to_string(), "Drawable".to_string()))
        .expect("(Square, Drawable) vtable should be registered");
    assert_eq!(circle_vt.len(), 1, "vtable holds one slot per trait method");
    assert_eq!(square_vt.len(), 1);
    assert_eq!(circle_vt[0].0, "draw", "slot 0 must hold Circle::draw");
    assert_eq!(square_vt[0].0, "draw", "slot 0 must hold Square::draw");
    // Slot ordering is the property the runtime depends on; the FuncId
    // values are an implementation detail, so we only pin the names.
}

#[test]
fn dyn_call_resolves_to_correct_method_slot() {
    let module = lower_program(DRAWABLE_PROGRAM);
    let render = module
        .concrete_functions()
        .find(|f| f.name == "render")
        .expect("`render` must be present in the lowered module");

    let dyn_call = render
        .blocks
        .iter()
        .flat_map(|b| b.instructions.iter())
        .find_map(|inst| match &inst.op {
            Op::DynCall(trait_name, slot, _, _) => Some((trait_name.clone(), *slot)),
            _ => None,
        })
        .expect("`render` must lower `s.draw()` to an Op::DynCall");
    assert_eq!(dyn_call.0, "Drawable");
    assert_eq!(dyn_call.1, 0, "draw is method index 0 in declaration order");
}

#[test]
fn dyn_alloc_emitted_at_call_argument_coercion_site() {
    let module = lower_program(DRAWABLE_PROGRAM);
    let main = module
        .concrete_functions()
        .find(|f| f.name == "main")
        .expect("`main` must be present");

    let alloc_pairs: Vec<(String, String)> = main
        .blocks
        .iter()
        .flat_map(|b| b.instructions.iter())
        .filter_map(|inst| match &inst.op {
            Op::DynAlloc(trait_name, concrete, _) => Some((concrete.clone(), trait_name.clone())),
            _ => None,
        })
        .collect();
    // One DynAlloc each for Circle and Square at the render() call sites.
    assert!(
        alloc_pairs.contains(&("Circle".to_string(), "Drawable".to_string())),
        "expected Op::DynAlloc(Circle, Drawable), got: {alloc_pairs:?}"
    );
    assert!(
        alloc_pairs.contains(&("Square".to_string(), "Drawable".to_string())),
        "expected Op::DynAlloc(Square, Drawable), got: {alloc_pairs:?}"
    );
}

#[test]
fn dyn_alloc_op_display_format_is_stable() {
    // Phoenix IR has no text parser, so display-only is the closest we
    // can get to a round-trip. Pin the format so a future tweak that
    // reorders or relabels fields is loud rather than silent — IR dump
    // output is consumed by `phoenix dump-ir` users and golden-file
    // snapshot tests in other crates.
    let op = Op::DynAlloc("Drawable".into(), "Circle".into(), ValueId(7));
    assert_eq!(op.to_string(), "dyn_alloc @Drawable for Circle, v7");
}

#[test]
fn dyn_call_op_display_format_is_stable() {
    let op = Op::DynCall(
        "Drawable".into(),
        0,
        ValueId(3),
        vec![ValueId(4), ValueId(5)],
    );
    assert_eq!(op.to_string(), "dyn_call @Drawable[0], v3(v4, v5)");
}

#[test]
fn dyn_call_with_zero_args_displays_without_trailing_args() {
    let op = Op::DynCall("Drawable".into(), 2, ValueId(1), Vec::new());
    assert_eq!(op.to_string(), "dyn_call @Drawable[2], v1()");
}

/// End-to-end positive test for the `coerce_struct_ctor_args` path
/// noted in the audit as "no unit test in isolation". Lowers a struct
/// constructor whose field is typed `dyn Trait` and asserts a
/// `DynAlloc` is emitted at the constructor site.
#[test]
fn struct_ctor_with_dyn_field_emits_dyn_alloc_at_ctor_site() {
    let module = lower_program(
        "
trait Drawable { function draw(self) -> String }
struct Circle {
    Int radius
    impl Drawable { function draw(self) -> String { return \"c\" } }
}
struct Scene { dyn Drawable hero }
function main() {
    let s = Scene(Circle(3))
    print(s.hero.draw())
}
",
    );
    let main = module
        .concrete_functions()
        .find(|f| f.name == "main")
        .expect("`main` must be present");
    let ctor_alloc = main
        .blocks
        .iter()
        .flat_map(|b| b.instructions.iter())
        .any(|inst| {
            matches!(
                &inst.op,
                Op::DynAlloc(t, c, _) if t == "Drawable" && c == "Circle"
            )
        });
    assert!(
        ctor_alloc,
        "Scene(Circle(...)) must wrap Circle into a DynAlloc at the ctor site"
    );
}

/// Helper: count `Op::DynAlloc` instructions in `func_name`'s body.
fn dyn_alloc_count(module: &phoenix_ir::module::IrModule, func_name: &str) -> usize {
    module
        .concrete_functions()
        .find(|f| f.name == func_name)
        .unwrap_or_else(|| panic!("function `{func_name}` not present in module"))
        .blocks
        .iter()
        .flat_map(|b| b.instructions.iter())
        .filter(|inst| matches!(inst.op, Op::DynAlloc(..)))
        .count()
}

/// Boundary 2 (let annotation): `let d: dyn Drawable = Circle(1)` must
/// emit a `DynAlloc` at the let site, not just typecheck.
#[test]
fn let_annotation_with_dyn_emits_dyn_alloc() {
    let module = lower_program(
        "
trait Drawable { function draw(self) -> String }
struct Circle {
    Int radius
    impl Drawable { function draw(self) -> String { return \"c\" } }
}
function main() {
    let d: dyn Drawable = Circle(1)
    print(d.draw())
}
",
    );
    assert!(
        dyn_alloc_count(&module, "main") >= 1,
        "let with `dyn` annotation must lower to at least one DynAlloc"
    );
}

/// Boundary 3 (reassignment): `let mut d: dyn Drawable = Circle(...); d = Square(...)`
/// must emit a `DynAlloc` at the reassignment site too, not just at the
/// initial let.
#[test]
fn reassignment_into_dyn_emits_dyn_alloc_on_each_store() {
    let module = lower_program(
        "
trait Drawable { function draw(self) -> String }
struct Circle {
    Int radius
    impl Drawable { function draw(self) -> String { return \"c\" } }
}
struct Square {
    Int side
    impl Drawable { function draw(self) -> String { return \"s\" } }
}
function main() {
    let mut d: dyn Drawable = Circle(1)
    d = Square(2)
    print(d.draw())
}
",
    );
    assert!(
        dyn_alloc_count(&module, "main") >= 2,
        "reassignment into `dyn` slot must produce a second DynAlloc"
    );
}

/// Boundary 4 (function return): a function declared `-> dyn Drawable`
/// returning a concrete value must wrap the value at the return site.
#[test]
fn function_return_into_dyn_emits_dyn_alloc() {
    let module = lower_program(
        "
trait Drawable { function draw(self) -> String }
struct Circle {
    Int radius
    impl Drawable { function draw(self) -> String { return \"c\" } }
}
function make() -> dyn Drawable { return Circle(1) }
function main() { print(make().draw()) }
",
    );
    assert!(
        dyn_alloc_count(&module, "make") >= 1,
        "function returning `dyn Trait` must wrap the concrete value at the return site"
    );
}

/// Boundary 6 (enum variant field): a variant whose payload is typed
/// `dyn Trait` must wrap the concrete value at the variant constructor
/// site, separate from the struct-ctor path covered above.
#[test]
fn enum_variant_with_dyn_field_emits_dyn_alloc() {
    let module = lower_program(
        "
trait Drawable { function draw(self) -> String }
struct Circle {
    Int radius
    impl Drawable { function draw(self) -> String { return \"c\" } }
}
enum Slot { Held(dyn Drawable)
            Empty }
function main() {
    let v = Held(Circle(1))
    match v {
        Held(d) -> print(d.draw())
        Empty -> print(\"empty\")
    }
}
",
    );
    assert!(
        dyn_alloc_count(&module, "main") >= 1,
        "enum variant whose payload is `dyn Trait` must wrap the concrete value at the ctor site"
    );
}

/// Sanity round-trip: lowering two distinct dyn-coercion sites for the
/// same `(concrete, trait)` pair must produce exactly one vtable entry
/// (idempotency contract documented in `lower_dyn::register_dyn_vtable`).
#[test]
fn repeated_dyn_alloc_for_same_pair_yields_single_vtable_entry() {
    let module = lower_program(
        "
trait Drawable { function draw(self) -> String }
struct Circle {
    Int radius
    impl Drawable { function draw(self) -> String { return \"c\" } }
}
function one(x: dyn Drawable) -> String { return x.draw() }
function two(x: dyn Drawable) -> String { return x.draw() }
function main() {
    print(one(Circle(1)))
    print(two(Circle(2)))
}
",
    );
    let count = module
        .dyn_vtables
        .keys()
        .filter(|(c, t)| c == "Circle" && t == "Drawable")
        .count();
    assert_eq!(
        count, 1,
        "(Circle, Drawable) must appear exactly once despite multiple coercion sites"
    );
}
