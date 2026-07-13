//! Five-backend roundtrip matrix for multi-file Phoenix projects.
//!
//! Walks every `tests/fixtures/multi/<name>/main.phx` and asserts
//! that `phoenix run`, `phoenix run-ir`, `phoenix build` (native),
//! `phoenix build --target wasm32-linear` + execute under `wasmtime`,
//! and `phoenix build --target wasm32-gc` + execute under
//! `wasmtime -W gc=y` all produce byte-identical stdout, plus a check
//! against the project's `expected.txt`. The harness is shared with
//! the single-file `backend_matrix.rs` via
//! [`common::backend_matrix`]; this file only supplies the multi-file
//! [`MatrixCfg`] conventions (including the `expected.txt` pin) and
//! the project list.
//!
//! wasm32-gc skips were derived empirically (2026-06-11); each
//! annotation names the missing feature and is deleted by the slice
//! that lands it.
//!
//! Both wasm columns soft-skip when `wasmtime` isn't on
//! `$PATH` (with a visible warning); `PHOENIX_REQUIRE_WASMTIME=1`
//! turns the skip into a hard failure. CI sets it (see ci.yml).
//!
//! One `#[test]` per fixture so a divergence names the offending
//! project in `cargo test` output. Stdout-only comparison (same as
//! the single-file matrix); stderr is intentionally not gated.

mod common;

use common::matrix_harness::MatrixCfg;

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
    // All five columns.
    ($name:ident, $project:literal) => {
        #[test]
        fn $name() {
            common::matrix_harness::assert_backend_agreement(&CFG, $project, None);
        }
    };
    // Skip only the wasm32-gc column — for features wasm32-linear
    // already lowers but the PR 6 wasm32-gc slices haven't reached.
    // (A both-columns `skip_wasm:` arm can be re-added if a project
    // ever needs an op wasm32-linear doesn't lower; no project does
    // today.)
    ($name:ident, $project:literal, skip_wasm_gc: $reason:literal) => {
        #[test]
        fn $name() {
            common::matrix_harness::assert_backend_agreement(&CFG, $project, Some($reason));
        }
    };
}

multi_matrix_test!(matrix_basic_import, "basic_import");
multi_matrix_test!(matrix_import_alias, "import_alias");
// Namespace imports (`import helpers` → `helpers.add(...)`, plus an
// aliased `import helpers as h`). Covers the namespace-call path on
// every backend, including a *generic* function (`helpers.identity(42)`)
// to exercise type-arg threading through monomorphization, and a call
// that relies on a callee default argument (`helpers.scaled(5)`) to
// exercise default-fill on that path.
multi_matrix_test!(matrix_namespace_import, "namespace_import");
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
// `json.decode<User>` where `User` (and its nested `Point` field) are
// imported from another module. Struct decode resolves the struct's
// identity in three separate places — sema's decodability gate
// (`lookup_struct` canonicalization), IR decoder synthesis
// (`struct_info_by_name`), and the AST interpreter's field-type seeding
// (`json_struct_fields`, keyed by qualified name) — and only a
// cross-module target makes the qualified spellings diverge from the
// bare ones. Doubles as the golden for the decode error taxonomy
// (missing vs wrong-kind vs non-object vs `null` field): the
// single-file matrix checks cross-backend agreement only, so a
// misunderstanding shared by every backend would pass there but fail
// this fixture's `expected.txt`. wasm32-gc skipped (json.decode DOM
// deferral, same as the single-file json_decode fixtures).
multi_matrix_test!(
    matrix_json_decode_import,
    "json_decode_import",
    skip_wasm_gc: "json.decode DOM (serde_json) not yet ported to wasm32-gc (Phase 4.6 follow-up)"
);

// TODO(2.7): once imported `dyn Trait` is supported (see
// `check_modules_callable.rs::imported_dyn_trait_in_function_signature_is_a_known_limitation`),
// add a `multi/dyn_trait_imported` fixture and matrix entry here so
// the dyn-dispatch path's qualified-trait keying gets the same
// three-backend roundtrip coverage as the generic-bound form above.

/// Tripwire: every `tests/fixtures/multi/<name>/` project must have a
/// `multi_matrix_test!` entry above. The registered set is checked by
/// scanning this file's own source for the quoted project name, so a
/// project dropped into the directory without an entry fails here
/// instead of silently getting zero matrix coverage.
#[test]
fn every_project_has_a_matrix_entry() {
    let src = include_str!("multi_module_matrix.rs");
    let dir = common::compiled_fixtures::workspace_root().join("tests/fixtures/multi");
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_dir() {
            continue;
        }
        let name = entry.file_name().into_string().unwrap();
        if !src.contains(&format!("\"{name}\"")) {
            missing.push(name);
        }
    }
    missing.sort();
    assert!(
        missing.is_empty(),
        "multi-file projects without a multi_matrix_test! entry: {missing:?}"
    );
}
