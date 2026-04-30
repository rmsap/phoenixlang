//! Three-backend roundtrip matrix for multi-file Phoenix projects.
//!
//! Walks every `tests/fixtures/multi/<name>/main.phx` and asserts
//! that `phoenix run`, `phoenix run-ir`, and `phoenix build` + execute
//! all produce byte-identical stdout. Mirrors the single-file
//! `three_backend_matrix.rs`.
//!
//! One `#[test]` per fixture so a divergence names the offending
//! project in `cargo test` output. Stdout-only comparison (same as
//! the single-file matrix); stderr is intentionally not gated.

use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn phoenix_bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_phoenix"));
    cmd.current_dir(workspace_root());
    cmd
}

/// Run `phoenix <subcommand> tests/fixtures/multi/<project>/main.phx`
/// and return its stdout. Panics on non-zero exit.
fn run_subcommand(subcommand: &str, project: &str) -> Vec<u8> {
    let path = format!("tests/fixtures/multi/{project}/main.phx");
    let output = phoenix_bin()
        .args([subcommand, &path])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `phoenix {subcommand} {path}`: {e}"));
    if !output.status.success() {
        panic!(
            "`phoenix {subcommand} {path}` exited non-zero\n  stdout: {}\n  stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output.stdout
}

/// RAII guard for the temporary build output. See the matching guard
/// in `three_backend_matrix.rs` for the "fill /tmp on failure"
/// rationale.
struct TempBin(PathBuf);

impl Drop for TempBin {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Compile via `phoenix build` and run the resulting binary, returning
/// its stdout.
fn build_and_execute(project: &str) -> Vec<u8> {
    let path = format!("tests/fixtures/multi/{project}/main.phx");
    // Bin name is qualified by `(pid, thread id, project)` so two
    // simultaneous `cargo test` invocations against the same workspace
    // can't collide (different pids), AND a future change that fans
    // the same project across multiple harness threads can't collide
    // either (different thread ids). `ThreadId`'s `Debug` impl is the
    // documented way to obtain a usable identifier.
    let bin_name = format!(
        "phoenix_multi_matrix_{}_{:?}_{}",
        std::process::id(),
        std::thread::current().id(),
        project,
    );
    let bin = TempBin(std::env::temp_dir().join(&bin_name));
    let _ = std::fs::remove_file(&bin.0);

    let build = phoenix_bin()
        .args(["build", &path, "-o"])
        .arg(&bin.0)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `phoenix build {path}`: {e}"));
    if !build.status.success() {
        panic!(
            "`phoenix build {path}` exited non-zero\n  stdout: {}\n  stderr: {}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );
    }

    let run = Command::new(&bin.0)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn compiled `{}`: {e}", bin.0.display()));
    if !run.status.success() {
        panic!(
            "compiled `{}` exited non-zero\n  stdout: {}\n  stderr: {}",
            bin.0.display(),
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
    }
    run.stdout
}

/// Read the fixture's `expected.txt` (which pins what stdout *should*
/// be, not just what all three backends agree on). A coherent
/// regression that broke all three backends the same way would still
/// produce identical stdout across them, so stdout-equality alone is
/// necessary but not sufficient — comparing against the fixture's
/// `expected.txt` closes that gap.
fn read_expected(project: &str) -> Vec<u8> {
    let path = workspace_root().join(format!("tests/fixtures/multi/{project}/expected.txt"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

fn assert_three_backend_agreement(project: &str) {
    let run_stdout = run_subcommand("run", project);
    let run_ir_stdout = run_subcommand("run-ir", project);
    let build_stdout = build_and_execute(project);
    if run_stdout != run_ir_stdout || run_ir_stdout != build_stdout {
        panic!(
            "multi/{project}: backends disagree on stdout\n  run:    {:?}\n  run-ir: {:?}\n  build:  {:?}",
            String::from_utf8_lossy(&run_stdout),
            String::from_utf8_lossy(&run_ir_stdout),
            String::from_utf8_lossy(&build_stdout)
        );
    }
    let expected = read_expected(project);
    if run_stdout != expected {
        panic!(
            "multi/{project}: stdout does not match expected.txt\n  expected: {:?}\n  got:      {:?}",
            String::from_utf8_lossy(&expected),
            String::from_utf8_lossy(&run_stdout),
        );
    }
}

macro_rules! multi_matrix_test {
    ($name:ident, $project:literal) => {
        #[test]
        fn $name() {
            assert_three_backend_agreement($project);
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
