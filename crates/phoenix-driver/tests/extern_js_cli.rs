//! CLI integration tests for `extern js` across the execution subcommands
//! (Phase 2.5 PR 3).
//!
//! After PR 3, `extern js` calls **lower** to the generic `Op::ExternCall`
//! host-call node, so `phoenix ir` prints the lowered IR. No execution backend
//! *binds* that op yet, though, so the run/build paths reject it with a clean,
//! backend-specific diagnostic until their host bindings land:
//!
//! - `run` (AST tree-walk interpreter) and `run-ir` (IR interpreter) — the
//!   interpreter host-function table lands in PR 4.
//! - `build` (native Cranelift) — the C-ABI host shim lands in PR 9. The
//!   rejection happens during IR→Cranelift translation, before linking, so this
//!   case needs no runtime/linker provisioning.
//!
//! (`gen`'s separate, schema-specific rejection is covered by `gen_cli.rs`.)

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

/// Write a minimal `extern js` program to a unique temp file and return its
/// path. The program type-checks cleanly, so any failure is an execution-path
/// binding gap firing — not a parse or sema error.
fn extern_js_source(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "phoenix_extern_js_{}_{}.phx",
        std::process::id(),
        name
    ));
    std::fs::write(
        &path,
        "extern js { function alert(message: String) }\n\
         function main() { alert(\"hi\") }\n",
    )
    .unwrap();
    path
}

/// Write an `extern js` program that *declares* an extern but never *calls* it,
/// and return its path. No call site means no `Op::ExternCall` is lowered, so
/// no backend ever hits its "not supported yet" reject — the program must
/// execute normally. `main` prints a sentinel so the caller can confirm the
/// body actually ran (not that execution was silently skipped).
fn extern_js_declared_but_uncalled_source(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "phoenix_extern_js_uncalled_{}_{}.phx",
        std::process::id(),
        name
    ));
    std::fs::write(
        &path,
        "extern js { function alert(message: String) }\n\
         function main() { print(\"ran\") }\n",
    )
    .unwrap();
    path
}

/// Assert the given execution subcommand runs a declare-but-don't-call program
/// to completion, printing the `main` body's sentinel. Removing the old up-front
/// `extern js` guard means merely *declaring* an extern is no longer an error;
/// only a *call* to one is rejected (and only by the backend that would execute
/// it). Covers the interpreter paths (`run` / `run-ir`), which need no
/// runtime/linker provisioning.
fn assert_uncalled_extern_runs(subcommand: &str, name: &str) {
    let src = extern_js_declared_but_uncalled_source(name);
    let output = phoenix_bin()
        .arg(subcommand)
        .arg(&src)
        .output()
        .expect("failed to run phoenix");
    let _ = std::fs::remove_file(&src);

    assert!(
        output.status.success(),
        "`phoenix {subcommand}` should run a program that declares but never calls \
         an extern; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ran"),
        "expected `main` to run and print its sentinel under `{subcommand}`, got stdout: {stdout}"
    );
}

/// Assert the given subcommand rejects an `extern js` program with a clean,
/// backend-specific diagnostic that names the feature and the phase/PR where its
/// binding lands.
fn assert_execution_rejects(subcommand: &str, name: &str) {
    let src = extern_js_source(name);
    let output = phoenix_bin()
        .arg(subcommand)
        .arg(&src)
        .output()
        .expect("failed to run phoenix");
    let _ = std::fs::remove_file(&src);

    assert!(
        !output.status.success(),
        "`phoenix {subcommand}` should reject an extern-js program (no host binding yet)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("extern js") && stderr.contains("Phase 2.5 PR"),
        "expected a clean extern-js binding diagnostic from `{subcommand}`, got stderr: {stderr}"
    );
}

#[test]
fn run_rejects_extern_js() {
    assert_execution_rejects("run", "run");
}

#[test]
fn run_ir_rejects_extern_js() {
    assert_execution_rejects("run-ir", "run_ir");
}

#[test]
fn build_rejects_extern_js() {
    // `build` rejects during IR→Cranelift translation, before linking, so no
    // runtime lib is needed and no executable is produced.
    assert_execution_rejects("build", "build");
}

#[test]
fn run_executes_program_that_declares_but_never_calls_extern() {
    assert_uncalled_extern_runs("run", "run");
}

#[test]
fn run_ir_executes_program_that_declares_but_never_calls_extern() {
    assert_uncalled_extern_runs("run-ir", "run_ir");
}

#[test]
fn ir_lowers_extern_call_to_op() {
    // `phoenix ir` does not execute — it lowers and prints. After PR 3 an
    // extern call lowers to `Op::ExternCall`, so this succeeds and the printed
    // IR shows the `extern_call <module>.<name>(...)` node.
    let src = extern_js_source("ir");
    let output = phoenix_bin()
        .arg("ir")
        .arg(&src)
        .output()
        .expect("failed to run phoenix");
    let _ = std::fs::remove_file(&src);

    assert!(
        output.status.success(),
        "`phoenix ir` should lower (not reject) an extern-js program; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("extern_call js.alert"),
        "expected the lowered IR to contain `extern_call js.alert`, got:\n{stdout}"
    );
}
