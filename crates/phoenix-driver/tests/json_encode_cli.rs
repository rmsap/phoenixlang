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
const EXPECTED_STRUCT: &str = concat!(
    r#"{"name":"Ada","age":36,"active":true,"balance":3.14,"home":{"city":"London","zip":12345},"tags":{}}"#,
    "\n",
    "{}\n",
    "42\n",
    "3.14\n",
    r#""he\"llo\tworld\n""#,
    "\n",
    "true\n",
);

/// The exact stdout of `tests/fixtures/json_encode_enum.phx`. Mirrors the
/// fixture's `print(json.encode(...))` lines in order: a unit enum variant, a
/// single-field variant, a two-field variant, a variant carrying a nested
/// struct plus an `Option<Int>` field, `Some(scalar)`/`Some(struct)`/`None`
/// (passthrough / `null`), and a
/// struct mixing an enum field with two `Option` fields (one `Some`, one
/// `None`). Pins the
/// adjacently-tagged wire form (`{"type":..,"value":[..]}`) so a shared bug in
/// both encoders — which the cross-backend matrix would not catch — is caught.
const EXPECTED_ENUM: &str = concat!(
    r#"{"type":"Active"}"#,
    "\n",
    r#"{"type":"Pending","value":[3]}"#,
    "\n",
    r#"{"type":"Closed","value":["done",7]}"#,
    "\n",
    r#"{"type":"Located","value":[{"x":1,"y":2},7]}"#,
    "\n",
    "5\n",
    r#"{"x":8,"y":9}"#,
    "\n",
    "null\n",
    r#"{"id":1,"status":{"type":"Pending","value":[99]},"nickname":"vip","balance":null}"#,
    "\n",
);

/// The exact stdout of `tests/fixtures/json_encode_collections.phx`. Mirrors
/// the fixture's `print(json.encode(...))` lines in order: a scalar list, a
/// list of escaped strings, an empty list (`[]`), a list of structs, a nested
/// list, a `Map<String, Int>` object (insertion order preserved), an empty map
/// (`{}`), a map with a struct value, and a map with `List` values. Pins the
/// array/object wire form so a shared bug in both encoders — which the
/// cross-backend matrix would not catch — is caught.
const EXPECTED_COLLECTIONS: &str = concat!(
    "[1,2,3]\n",
    r#"["a","b\"c"]"#,
    "\n",
    "[]\n",
    r#"[{"x":1,"y":2},{"x":3,"y":4}]"#,
    "\n",
    "[[1,2],[3]]\n",
    r#"{"x":1,"y":2}"#,
    "\n",
    "{}\n",
    r#"{"origin":{"x":0,"y":0}}"#,
    "\n",
    r#"{"evens":[2,4],"odds":[1,3]}"#,
    "\n",
);

fn assert_fixture_stdout(subcommand: &str, fixture: &str, expected: &str) {
    let output = phoenix_bin()
        .args([subcommand, fixture])
        .output()
        .expect("failed to run phoenix");
    assert!(
        output.status.success(),
        "phoenix {subcommand} {fixture} failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        expected,
        "phoenix {subcommand} {fixture}: json.encode output does not match the pinned golden",
    );
}

#[test]
fn json_encode_struct_golden_ast() {
    assert_fixture_stdout(
        "run",
        "tests/fixtures/json_encode_struct.phx",
        EXPECTED_STRUCT,
    );
}

#[test]
fn json_encode_struct_golden_ir() {
    assert_fixture_stdout(
        "run-ir",
        "tests/fixtures/json_encode_struct.phx",
        EXPECTED_STRUCT,
    );
}

#[test]
fn json_encode_enum_golden_ast() {
    assert_fixture_stdout("run", "tests/fixtures/json_encode_enum.phx", EXPECTED_ENUM);
}

#[test]
fn json_encode_enum_golden_ir() {
    assert_fixture_stdout(
        "run-ir",
        "tests/fixtures/json_encode_enum.phx",
        EXPECTED_ENUM,
    );
}

#[test]
fn json_encode_collections_golden_ast() {
    assert_fixture_stdout(
        "run",
        "tests/fixtures/json_encode_collections.phx",
        EXPECTED_COLLECTIONS,
    );
}

#[test]
fn json_encode_collections_golden_ir() {
    assert_fixture_stdout(
        "run-ir",
        "tests/fixtures/json_encode_collections.phx",
        EXPECTED_COLLECTIONS,
    );
}
