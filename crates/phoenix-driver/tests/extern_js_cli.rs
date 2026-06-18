//! CLI integration tests: `extern js` is rejected on every execution path
//! until host-FFI call lowering lands (Phase 2.5 PR 3).
//!
//! Sema accepts `extern js` as a valid executable-language construct (decision
//! A0), but no backend can lower an extern call yet — the IR paths would ICE in
//! `lower_ident` and the AST interpreter would mis-report it as an `undefined
//! function`. `reject_extern_js_for_execution` in `phoenix-driver` fails fast
//! with one clear diagnostic across `run` / `run-ir` / `ir` / `build`. The
//! guard runs *before* any IR lowering or linking, so the `build` case needs no
//! runtime/linker provisioning. (`gen`'s separate, schema-specific rejection is
//! covered by `gen_cli.rs`.)

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
/// path. The program type-checks cleanly, so any failure is the execution-path
/// guard firing — not a parse or sema error.
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

/// Assert the given subcommand rejects an `extern js` program with the
/// execution-path diagnostic and writes no artifact.
fn assert_rejects(subcommand: &str, name: &str) {
    let src = extern_js_source(name);
    let output = phoenix_bin()
        .arg(subcommand)
        .arg(&src)
        .output()
        .expect("failed to run phoenix");
    let _ = std::fs::remove_file(&src);

    assert!(
        !output.status.success(),
        "`phoenix {subcommand}` should reject an extern-js program"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("extern js") && stderr.contains("not yet supported"),
        "expected the execution-path extern-js diagnostic from `{subcommand}`, \
         got stderr: {stderr}"
    );
}

#[test]
fn run_rejects_extern_js() {
    assert_rejects("run", "run");
}

#[test]
fn run_ir_rejects_extern_js() {
    assert_rejects("run-ir", "run_ir");
}

#[test]
fn ir_rejects_extern_js() {
    assert_rejects("ir", "ir");
}

#[test]
fn build_rejects_extern_js() {
    // `build` rejects before lowering/linking, so no runtime lib is needed and
    // no executable is produced.
    assert_rejects("build", "build");
}
