//! Four-backend roundtrip matrix for multi-file Phoenix projects.
//!
//! Walks every `tests/fixtures/multi/<name>/main.phx` and asserts
//! that `phoenix run`, `phoenix run-ir`, `phoenix build` (native), and
//! `phoenix build --target wasm32-linear` + execute under `wasmtime`
//! all produce byte-identical stdout, plus a check against the
//! project's `expected.txt`. The harness is shared with the single-file
//! `three_backend_matrix.rs` via [`common::backend_matrix`]; this file
//! only supplies the multi-file [`MatrixCfg`] conventions (including
//! the `expected.txt` pin) and the project list.
//!
//! The `wasm32-linear` column soft-skips when `wasmtime` isn't on
//! `$PATH` (with a visible warning); `PHOENIX_REQUIRE_WASMTIME=1`
//! turns the skip into a hard failure. CI sets it (see ci.yml).
//!
//! One `#[test]` per fixture so a divergence names the offending
//! project in `cargo test` output. Stdout-only comparison (same as
//! the single-file matrix); stderr is intentionally not gated.

mod common;

use common::backend_matrix::MatrixCfg;

fn source_rel(project: &str) -> String {
    format!("tests/fixtures/multi/{project}/main.phx")
}

fn label(project: &str) -> String {
    format!("multi/{project}")
}

fn bin_stem(project: &str) -> String {
    format!("phoenix_multi_matrix_{project}")
}

fn expected_rel(project: &str) -> String {
    format!("tests/fixtures/multi/{project}/expected.txt")
}

static CFG: MatrixCfg = MatrixCfg {
    source_rel,
    label,
    bin_stem,
    expected_rel: Some(expected_rel),
};

macro_rules! multi_matrix_test {
    ($name:ident, $project:literal) => {
        #[test]
        fn $name() {
            common::backend_matrix::assert_backend_agreement(&CFG, $project);
        }
    };
    // No multi-file fixture needs this arm yet — every project lowers
    // to wasm. It's the ready-to-use escape hatch (mirroring the
    // single-file matrix's `traits_dyn` carve-out) for the first
    // project that reaches for an op the wasm32-linear backend doesn't
    // lower. Referenced by full path so this unexpanded arm doesn't
    // force an otherwise-unused import.
    ($name:ident, $project:literal, skip_wasm: $reason:literal) => {
        #[test]
        fn $name() {
            common::backend_matrix::assert_backend_agreement_skip_wasm(&CFG, $project, $reason);
        }
    };
}

multi_matrix_test!(matrix_basic_import, "basic_import");
multi_matrix_test!(matrix_import_alias, "import_alias");
multi_matrix_test!(matrix_import_wildcard, "import_wildcard");
multi_matrix_test!(matrix_nested_modules, "nested_modules");
// The §2.6 tripwire: a public function whose default arg references a
// private symbol in its own module. Without wrapper synthesis the
// caller would inline the private symbol directly into the entry's
// IR; with wrapper synthesis (Task #7) the call site emits a zero-arg
// `Op::Call(__default_*)` instead.
multi_matrix_test!(matrix_default_wrapper, "default_wrapper");
multi_matrix_test!(matrix_visibility_struct_pub, "visibility_struct_pub");
multi_matrix_test!(matrix_visibility_enum_pub, "visibility_enum_pub");
// A method invocation on an imported struct: catches regressions
// where the value's runtime type tag drifts from the methods table's
// receiver key (the AST interpreter previously stored `Value::Struct`
// with the bare name `User` while methods were registered under the
// qualified key `models::User`, so dispatch missed).
multi_matrix_test!(matrix_struct_methods, "struct_methods");
// A method whose default-arg expression calls a *private* helper in
// the method's own module: validates that the callee's module is
// pushed before evaluating defaults, so the private helper resolves
// through the callee's scope rather than the caller's.
multi_matrix_test!(matrix_method_default_helper, "method_default_helper");
// A cross-module enum whose variants carry payload fields, both
// constructed and pattern-matched in the entry. Catches
// regressions where the enum's qualified key (`lib::Outcome`)
// drifts between construction (`enum_layouts` keying), `EnumAlloc`
// op naming, and runtime value tag — a silent failure mode that
// fieldless variants don't exercise because no payload coercion
// runs.
multi_matrix_test!(matrix_enum_with_fields, "enum_with_fields");
// A trait imported from a sibling module and used as a generic
// bound (`<T: Drawable>`) on a function in the entry. Two structs
// in the sibling module each `impl Drawable`. Exercises the
// qualified `Type::Generic(trait_name, …)` payload shape that
// sema's `check_types.rs` now produces — a regression where the
// trait-impl table was keyed under a bare `Drawable` while the
// bound carried `shapes::Drawable` (or vice-versa) would surface
// as an "unsatisfied trait bound" sema error here. Note: imported
// `dyn ImportedTrait` is a known limitation (see
// `check_modules_callable.rs::imported_dyn_trait_in_function_signature_is_a_known_limitation`),
// so this test deliberately uses the generic-bound form.
multi_matrix_test!(matrix_trait_bound, "trait_bound");

// TODO(2.7): once imported `dyn Trait` is supported (see
// `check_modules_callable.rs::imported_dyn_trait_in_function_signature_is_a_known_limitation`),
// add a `multi/dyn_trait_imported` fixture and matrix entry here so
// the dyn-dispatch path's qualified-trait keying gets the same
// three-backend roundtrip coverage as the generic-bound form above.
