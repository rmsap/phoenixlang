//! Every runnable `tests/fixtures/*.phx`
//! must produce byte-identical stdout under all three execution modes
//! (`phoenix run`, `phoenix run-ir`, `phoenix build` + execute). A
//! divergence here indicates a real backend bug, not a test issue.
//!
//! `gen_*.phx` fixtures are excluded — they exist as inputs to
//! `phoenix gen` and aren't worth exercising through the matrix.
//!
//! One `#[test]` per fixture, generated via `backend_matrix_test!`,
//! so a failure names the diverging fixture in `cargo test` output
//! without parsing assertion text.
//!
//! Scope: only stdout is compared. Stderr divergence and warning
//! output are intentionally out of scope for this gate — different
//! backends legitimately log different progress information. Don't
//! add stderr comparison here without first confirming the goal.

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

/// Run `phoenix <subcommand> tests/fixtures/<fixture>` (no extra args)
/// and return its stdout. Panics with stderr included on non-zero
/// exit; callers should treat a return as success.
fn run_subcommand(subcommand: &str, fixture: &str) -> Vec<u8> {
    let path = format!("tests/fixtures/{fixture}");
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

/// RAII guard: removes its path on drop so the compiled binary is
/// cleaned up even if a downstream assertion panics. Without this,
/// repeated failures fill `/tmp` with stale phoenix_matrix_* binaries.
struct TempBin(PathBuf);

impl Drop for TempBin {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Compile via `phoenix build`, run the resulting binary, and return
/// its stdout. The binary lands in the system temp dir under a
/// PID-and-fixture-keyed name so concurrent test runs don't clobber
/// each other; the `TempBin` guard removes it on every exit path.
fn build_and_execute(fixture: &str) -> Vec<u8> {
    let path = format!("tests/fixtures/{fixture}");
    let bin_name = format!(
        "phoenix_matrix_{}_{}",
        std::process::id(),
        fixture.trim_end_matches(".phx")
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

fn assert_three_backend_agreement(fixture: &str) {
    let run_stdout = run_subcommand("run", fixture);
    let run_ir_stdout = run_subcommand("run-ir", fixture);
    let build_stdout = build_and_execute(fixture);

    // Show all three stdouts in the panic regardless of which pair
    // diverged — when triaging, you almost always want to see what
    // the canonical AST interpreter produced as the reference point.
    if run_stdout != run_ir_stdout || run_ir_stdout != build_stdout {
        panic!(
            "{fixture}: backends disagree on stdout\n  run:    {:?}\n  run-ir: {:?}\n  build:  {:?}",
            String::from_utf8_lossy(&run_stdout),
            String::from_utf8_lossy(&run_ir_stdout),
            String::from_utf8_lossy(&build_stdout)
        );
    }
}

macro_rules! backend_matrix_test {
    ($name:ident, $fixture:literal) => {
        #[test]
        fn $name() {
            assert_three_backend_agreement($fixture);
        }
    };
}

backend_matrix_test!(matrix_hello, "hello.phx");
backend_matrix_test!(matrix_fibonacci, "fibonacci.phx");
backend_matrix_test!(matrix_fizzbuzz, "fizzbuzz.phx");
backend_matrix_test!(matrix_features, "features.phx");
backend_matrix_test!(matrix_generics, "generics.phx");
backend_matrix_test!(matrix_traits_static, "traits_static.phx");
backend_matrix_test!(matrix_traits_dyn, "traits_dyn.phx");
backend_matrix_test!(matrix_collections, "collections.phx");
backend_matrix_test!(matrix_option_result, "option_result.phx");
backend_matrix_test!(matrix_defaults, "defaults.phx");
backend_matrix_test!(matrix_closures, "closures.phx");
backend_matrix_test!(
    matrix_closures_ambiguous_captures,
    "closures_ambiguous_captures.phx"
);
backend_matrix_test!(matrix_closures_over_generic, "closures_over_generic.phx");

/// Regression marker for closures returned from generic functions at
/// *cross-width* instantiations (Int + String). Currently fails in
/// `phoenix build` because the inner closure function is shared across
/// specializations rather than cloned, and pass D erases its
/// TypeVar-bearing `capture_types` to the `__generic` placeholder.
/// Cranelift then mis-sizes the closure heap object for the wider
/// instantiation. See the fixture's header comment and
/// `docs/known-issues.md` for the full diagnosis.
///
/// Flip this to a `backend_matrix_test!` invocation when
/// monomorphization clones closure functions per enclosing-generic
/// substitution. `phoenix run` / `phoenix run-ir` already agree on
/// the expected output (`15\nhi:there\n`).
#[test]
#[ignore = "closures-over-generic at cross-width instantiations: \
            inner closure shared across specializations, capture_types \
            erased to __generic, Cranelift mis-sizes heap layout. See \
            docs/known-issues.md."]
fn matrix_closures_over_generic_cross_width() {
    assert_three_backend_agreement("closures_over_generic_cross_width.phx");
}
