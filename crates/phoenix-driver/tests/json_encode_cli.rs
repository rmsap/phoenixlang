//! Golden-output test for `json.encode`.
//!
//! `backend_matrix.rs::matrix_json_encode_struct` only asserts the
//! backends *agree* — it would pass even if every backend shared the same
//! wrong output (e.g. fields in the wrong order, or a dropped separator).
//! This test pins the *literal* expected JSON for the same fixture so a bug
//! present in both the tree-walk encoder and the synthesized IR encoder is
//! still caught. We assert via `run` (AST interpreter) and `run-ir` (IR
//! interpreter); since the matrix proves all four backends agree
//! byte-for-byte, pinning these two to the golden pins them all.

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

/// The exact stdout of `tests/fixtures/json_encode_struct.phx`. Mirrors
/// the fixture's six `print(json.encode(...))` lines in order: a nested
/// struct (fields in declaration order, a field-less struct as `{}`), an
/// empty struct, two bare scalars, an escaped string (`\"`/`\t`/`\n`), and
/// a bool.
const EXPECTED: &str = concat!(
    r#"{"name":"Ada","age":36,"active":true,"balance":3.14,"home":{"city":"London","zip":12345},"tags":{}}"#,
    "\n",
    "{}\n",
    "42\n",
    "3.14\n",
    r#""he\"llo\tworld\n""#,
    "\n",
    "true\n",
);

fn assert_fixture_stdout(subcommand: &str) {
    let output = phoenix_bin()
        .args([subcommand, "tests/fixtures/json_encode_struct.phx"])
        .output()
        .expect("failed to run phoenix");
    assert!(
        output.status.success(),
        "phoenix {subcommand} failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        EXPECTED,
        "phoenix {subcommand}: json.encode output does not match the pinned golden",
    );
}

#[test]
fn json_encode_struct_golden_ast() {
    assert_fixture_stdout("run");
}

#[test]
fn json_encode_struct_golden_ir() {
    assert_fixture_stdout("run-ir");
}
