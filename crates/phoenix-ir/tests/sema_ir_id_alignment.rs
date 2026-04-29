//! Sema↔IR id-alignment tests.
//!
//! The contract we pin: IR's `register_declarations` adopts every
//! `FuncId`, `StructId`, `EnumId`, and `TraitId` that
//! `phoenix_sema::ResolvedModule` produces, so
//! `IrModule.functions[id.index()]` always refers to the same
//! callable as `ResolvedModule.functions[id.index()]` (free function)
//! or `ResolvedModule.user_methods[id.index() - user_method_offset]`
//! (user method).
//!
//! Closures and monomorphized specializations are appended past
//! `IrModule.synthesized_start`; everything below that boundary must
//! be 1:1 with sema's tables.

use phoenix_common::span::SourceId;
use phoenix_ir::lower;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

fn lower_program(source: &str) -> (phoenix_sema::Analysis, phoenix_ir::module::IrModule) {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let analysis = checker::check(&program);
    assert!(
        analysis.diagnostics.is_empty(),
        "sema errors: {:?}",
        analysis.diagnostics
    );
    let module = lower(&program, &analysis.module);
    (analysis, module)
}

#[test]
fn ir_function_ids_match_resolved_function_ids() {
    let (analysis, ir) = lower_program(
        r#"
function alpha() { }
function beta(x: Int) -> Int { return x + 1 }
function gamma() { }
"#,
    );
    let resolved = &analysis.module;

    // Every free function in ResolvedModule has an IR function at the
    // same FuncId.
    for (name, fid, _info) in resolved.functions_with_names() {
        let ir_func = ir.lookup(fid).expect("FuncId in range");
        assert_eq!(ir_func.id, fid, "FuncId mismatch at sema name `{name}`");
        assert_eq!(
            ir_func.name, name,
            "name mismatch at FuncId({}); IR has `{}`, sema has `{}`",
            fid.0, ir_func.name, name
        );
    }

    // user_method_offset and synthesized_start must agree with sema's
    // table sizes.
    assert_eq!(
        ir.user_method_offset, resolved.user_method_offset,
        "IR user_method_offset disagrees with ResolvedModule"
    );
    assert!(
        ir.synthesized_start as usize >= resolved.functions.len() + resolved.user_methods.len(),
        "synthesized_start must be past the user-method range"
    );
}

#[test]
fn ir_user_method_ids_match_resolved_user_method_ids() {
    let (analysis, ir) = lower_program(
        r#"
struct Counter {
    Int v

    function bump(self) -> Int { return self.v + 1 }
    function get(self) -> Int { return self.v }
}
function main() { }
"#,
    );
    let resolved = &analysis.module;

    // Every user method in ResolvedModule has an IR function at the
    // same FuncId, with the canonical mangled name.
    for ((tn, mn), fid, _info) in resolved.user_methods_with_names() {
        let ir_func = ir.lookup(fid).expect("FuncId in range");
        assert_eq!(ir_func.id, fid);
        let expected = format!("{tn}.{mn}");
        assert_eq!(
            ir_func.name, expected,
            "method name mismatch at FuncId({})",
            fid.0
        );
        // method_index in IR should round-trip the same id.
        let by_index = ir
            .method_index
            .get(&(tn.to_string(), mn.to_string()))
            .copied();
        assert_eq!(by_index, Some(fid), "method_index disagrees with sema");
    }
}

#[test]
fn ir_function_count_meets_or_exceeds_sema_count() {
    // After monomorphization IR may have *more* functions (closure
    // bodies, generic specializations) but never fewer than sema's
    // pre-allocated set.
    let (analysis, ir) = lower_program(
        r#"
function ident<T>(x: T) -> T { return x }
function main() {
    let a: Int = ident(1)
    let b: Bool = ident(true)
}
"#,
    );
    let sema_count = analysis.module.functions.len() + analysis.module.user_methods.len();
    assert!(
        ir.function_count() >= sema_count,
        "IR must have at least {sema_count} functions, has {}",
        ir.function_count()
    );
}

#[test]
fn closures_are_appended_past_synthesized_start() {
    let (analysis, ir) = lower_program(
        r#"
function main() {
    let f: (Int) -> Int = function(n: Int) -> Int { return n + 1 }
    let y: Int = f(2)
}
"#,
    );
    let sema_count = analysis.module.functions.len() + analysis.module.user_methods.len();
    assert_eq!(ir.synthesized_start as usize, sema_count);

    // If any function exists past synthesized_start, it must be a
    // closure / specialization (`is_synthesized` must agree) and its
    // FuncId must match its position.
    for (fid, slot) in ir.iter_slots() {
        let i = fid.index();
        assert_eq!(slot.func().id, fid, "FuncId disagrees with vector position");
        if i >= sema_count {
            assert!(
                ir.is_synthesized(fid),
                "function at index {i} (past synthesized_start={}) should be flagged synthesized",
                ir.synthesized_start
            );
        }
    }
}

#[test]
fn no_placeholder_funcs_after_lowering() {
    // The post-lowering sentinel check (debug_assert in `lower()`)
    // panics if any IrFunction retains FuncId(u32::MAX). We exercise
    // a representative program that touches free functions, methods,
    // and closures; if the contract held, none should leak.
    let (_analysis, ir) = lower_program(
        r#"
struct Counter {
    Int n

    function bump(self) -> Int { return self.n + 1 }
}
function apply(f: (Int) -> Int, x: Int) -> Int { return f(x) }
function main() {
    let c: Counter = Counter(1)
    let n: Int = c.bump()
    let r: Int = apply(function(k: Int) -> Int { return k * 2 }, n)
}
"#,
    );
    for (fid, slot) in ir.iter_slots() {
        assert_ne!(
            slot.func().id.0,
            u32::MAX,
            "IrModule.functions[{}] still has FuncId(u32::MAX) sentinel",
            fid.index()
        );
    }
}

#[test]
fn orphan_methods_get_filled_placeholder_slots() {
    // Sema produces orphan-method slots when registration rejects the
    // parent decl (within-module duplicate type, coherence-violating
    // impl). Their FuncIds were pre-allocated and `user_methods.len()`
    // includes them, so IR must size and fill `module.functions`
    // matching that count — even though `user_methods_with_names()`
    // skips them. The orphan-fill pass installs unreachable
    // placeholders at those slots; the post-registration debug_assert
    // would panic without them.
    //
    // This test bypasses the diagnostic gate in `lower_program` because
    // this scenario *is* a sema error in the user's program — the
    // assertion we're pinning is "IR doesn't crash on it", not "this
    // input is well-formed".
    let source = r#"
struct Foo { Int x }
struct Foo {
    String y
    function bar(self) -> Int { return 1 }
}
function main() {}
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let analysis = checker::check(&program);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("`Foo` is already defined")),
        "expected duplicate-struct diagnostic, got: {:?}",
        analysis.diagnostics
    );
    assert!(
        analysis.module.orphan_method_count > 0,
        "expected at least one orphan method from the duplicate's `bar`, got 0"
    );
    // The orphan-fill pass + debug_asserts in IR's `register_declarations`
    // and `lower()` exercise the contract; constructing the module
    // without panicking is the assertion.
    let ir = lower(&program, &analysis.module);
    for (fid, slot) in ir.iter_slots() {
        assert_ne!(
            slot.func().id.0,
            u32::MAX,
            "IrModule.functions[{}] still has FuncId(u32::MAX) sentinel after orphan fill",
            fid.index()
        );
    }
}
