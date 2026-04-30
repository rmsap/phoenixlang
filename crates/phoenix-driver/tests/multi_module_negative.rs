//! Negative-path coverage for multi-module compilation.
//!
//! Each test runs `phoenix check tests/fixtures/multi_negative/<name>/main.phx`,
//! asserts a non-zero exit, and asserts the stderr contains a specific
//! substring that pins the diagnostic shape. Covers visibility
//! violations (cross-module privacy), import-resolution failures
//! (missing items, missing modules, ambiguous modules, cyclic
//! imports), and the `main`-in-non-entry rule.
//!
//! Mirrors the matrix-test driver in `multi_module_matrix.rs` but
//! flips the success/failure expectations: every fixture here is
//! ill-formed by design.

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

/// Run `phoenix check` on the fixture's `main.phx`; assert non-zero
/// exit and that stderr contains every entry in `expected_substrings`
/// (substring match, in any order). Returns the captured stderr so
/// callers can do additional pin-asserts on diagnostic shape (notes,
/// suggestions, etc.).
fn assert_check_fails_with(fixture: &str, expected_substrings: &[&str]) -> String {
    let path = format!("tests/fixtures/multi_negative/{fixture}/main.phx");
    let output = phoenix_bin()
        .args(["check", &path])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `phoenix check {path}`: {e}"));
    assert!(
        !output.status.success(),
        "expected `phoenix check {path}` to fail, but it succeeded\n  stdout: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is not valid UTF-8");
    for expected in expected_substrings {
        assert!(
            stderr.contains(expected),
            "stderr does not contain expected substring `{}`\n  full stderr: {}",
            expected,
            stderr
        );
    }
    stderr
}

#[test]
fn negative_import_private_function() {
    // The diagnostic must use the rich shape: error message + note +
    // suggestion (Phase 2.6 §2.6 exit criterion). Pinning all three
    // here keeps the rich shape from regressing to a plain
    // single-line error.
    let stderr = assert_check_fails_with(
        "import_private_function",
        &[
            "`secretHelper` is private to module `lib`",
            "suggestion:",
            "mark `secretHelper` as `public`",
            "note:",
            "declared here",
        ],
    );
    // The note span must point at the lib.phx declaration, not at the
    // import site — verifies the note actually carries cross-file span
    // metadata rather than reusing the primary span.
    assert!(
        stderr.contains("lib.phx"),
        "expected note span to reference lib.phx; stderr: {}",
        stderr
    );
}

#[test]
fn negative_unimported_function_not_in_scope() {
    // Pin the file-name in the diagnostic so a regression that drops
    // source-context (rendering the error without a span) is caught
    // here rather than silently passing.
    assert_check_fails_with(
        "unimported_function",
        &["undefined function `add`", "main.phx"],
    );
}

#[test]
fn negative_import_nonexistent_name() {
    assert_check_fails_with(
        "import_nonexistent",
        &["`doesNotExist` is not declared in module `lib`"],
    );
}

#[test]
fn negative_missing_module() {
    let stderr = assert_check_fails_with("missing_module", &["cannot find module 'phantom'"]);
    // Resolver lists what it tried so the user can see what the
    // probe set was.
    assert!(
        stderr.contains("phantom.phx") && stderr.contains("phantom/mod.phx"),
        "expected resolver to list both probe paths; stderr: {}",
        stderr
    );
}

#[test]
fn negative_cyclic_imports() {
    assert_check_fails_with("cyclic_imports", &["cyclic module imports", "a", "b"]);
}

#[test]
fn negative_ambiguous_module() {
    let stderr = assert_check_fails_with("ambiguous_module", &["module 'things' is ambiguous"]);
    // Ambiguity must surface both candidate paths so the user knows
    // which file to delete or rename.
    assert!(
        stderr.contains("things.phx") && stderr.contains("things/mod.phx"),
        "expected both candidate paths; stderr: {}",
        stderr
    );
}

#[test]
fn negative_main_in_non_entry_module() {
    assert_check_fails_with(
        "main_in_non_entry",
        &["`main` may only be declared in the entry module"],
    );
}
