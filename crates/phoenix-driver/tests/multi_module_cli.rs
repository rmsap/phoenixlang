//! CLI integration tests for multi-module projects.
//!
//! Exercises the resolver-driven driver path end-to-end by invoking the
//! compiled `phoenix` binary against fixture projects on disk.

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

#[test]
fn check_succeeds_with_sibling_import() {
    let output = phoenix_bin()
        .args(["check", "tests/fixtures/multi_module_ok/main.phx"])
        .output()
        .expect("failed to run phoenix");
    assert!(
        output.status.success(),
        "expected `check` to succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn check_reports_missing_module() {
    let output = phoenix_bin()
        .args(["check", "tests/fixtures/multi_module_missing/main.phx"])
        .output()
        .expect("failed to run phoenix");
    assert!(
        !output.status.success(),
        "expected `check` to fail on missing module"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot find module"),
        "stderr should mention the missing module; got:\n{}",
        stderr
    );
}

#[test]
fn entry_parse_error_uses_caller_supplied_path_prefix() {
    // Diagnostics from a parse error in the entry must be prefixed with the
    // exact path the user passed on the command line, not a root-relative
    // form. This guards against the resolver swap accidentally reformatting
    // the diagnostic file label.
    let path = "tests/fixtures/multi_module_broken_entry/main.phx";
    let output = phoenix_bin()
        .args(["check", path])
        .output()
        .expect("failed to run phoenix");
    assert!(
        !output.status.success(),
        "expected `check` to fail on parse error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("{}:", path)),
        "stderr should prefix diagnostics with `{}:`; got:\n{}",
        path,
        stderr
    );
}
