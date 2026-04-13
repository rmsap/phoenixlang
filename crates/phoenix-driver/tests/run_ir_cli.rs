//! CLI integration tests for `phoenix run-ir`.
//!
//! These tests invoke the compiled `phoenix` binary as a subprocess to verify
//! end-to-end execution via the IR interpreter.

use std::path::PathBuf;
use std::process::Command;

/// Returns the workspace root directory.
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

#[test]
fn run_ir_hello_fixture() {
    let output = phoenix_bin()
        .args(["run-ir", "tests/fixtures/hello.phx"])
        .output()
        .expect("failed to run phoenix");
    assert!(
        output.status.success(),
        "phoenix run-ir failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "42");
}

#[test]
fn run_ir_fibonacci_fixture() {
    let output = phoenix_bin()
        .args(["run-ir", "tests/fixtures/fibonacci.phx"])
        .output()
        .expect("failed to run phoenix");
    assert!(
        output.status.success(),
        "phoenix run-ir failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.is_empty(), "fibonacci should produce output");
}

#[test]
fn run_ir_matches_tree_walk_interpreter() {
    let run_output = phoenix_bin()
        .args(["run", "tests/fixtures/fizzbuzz.phx"])
        .output()
        .expect("failed to run phoenix run");
    let run_ir_output = phoenix_bin()
        .args(["run-ir", "tests/fixtures/fizzbuzz.phx"])
        .output()
        .expect("failed to run phoenix run-ir");

    assert!(run_output.status.success(), "phoenix run failed");
    assert!(run_ir_output.status.success(), "phoenix run-ir failed");

    assert_eq!(
        run_output.stdout, run_ir_output.stdout,
        "run and run-ir should produce identical output",
    );
}

#[test]
fn run_ir_nonexistent_file_fails() {
    let output = phoenix_bin()
        .args(["run-ir", "does_not_exist.phx"])
        .output()
        .expect("failed to run phoenix");
    assert!(
        !output.status.success(),
        "phoenix run-ir should fail on nonexistent file",
    );
}
