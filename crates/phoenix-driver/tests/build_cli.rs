//! CLI integration tests for `phoenix build`'s `--target` flag.
//!
//! Default-target behavior (no `--target`) is exercised by
//! `three_backend_matrix.rs` for every fixture, so these tests focus
//! on the new flag: explicit `native` parity, the not-yet-implemented
//! WASM variants (Phase 2.4 PR 1 placeholder), and the unknown-target
//! diagnostic. The implicit `.wasm` output-suffix branch in
//! `build.rs` is unreachable until WASM codegen lands in PR 2+;
//! a regression there will be caught by the first PR that produces
//! real WASM bytes.

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

/// RAII guard: removes its path on drop so a compiled binary doesn't
/// leak when a downstream assertion panics. Mirrors `TempBin` in
/// `three_backend_matrix.rs`.
struct TempBin(PathBuf);

impl Drop for TempBin {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn temp_bin(name: &str) -> TempBin {
    let path =
        std::env::temp_dir().join(format!("phoenix_build_cli_{}_{}", std::process::id(), name));
    let _ = std::fs::remove_file(&path);
    TempBin(path)
}

#[test]
fn explicit_native_target_builds_and_runs() {
    let bin = temp_bin("native");
    let build = phoenix_bin()
        .args([
            "build",
            "tests/fixtures/hello.phx",
            "--target",
            "native",
            "-o",
        ])
        .arg(&bin.0)
        .output()
        .expect("failed to spawn `phoenix build`");
    assert!(
        build.status.success(),
        "`phoenix build --target native` exited non-zero\n  stdout: {}\n  stderr: {}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );

    let run = Command::new(&bin.0)
        .output()
        .expect("failed to run compiled binary");
    assert!(run.status.success(), "compiled binary exited non-zero");
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "42");
}

#[test]
fn wasm32_linear_target_reports_not_yet_implemented() {
    let bin = temp_bin("wasm32_linear");
    let out = phoenix_bin()
        .args([
            "build",
            "tests/fixtures/hello.phx",
            "--target",
            "wasm32-linear",
            "-o",
        ])
        .arg(&bin.0)
        .output()
        .expect("failed to spawn `phoenix build`");
    assert!(
        !out.status.success(),
        "WASM target should fail in Phase 2.4 PR 1"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not yet implemented"),
        "expected not-yet-implemented diagnostic, got stderr: {stderr}",
    );
    assert!(
        stderr.contains("wasm32-linear"),
        "expected stderr to name the requested target, got: {stderr}",
    );
    assert!(
        !bin.0.exists(),
        "no artifact should be written when compile fails",
    );
}

#[test]
fn wasm32_gc_target_reports_not_yet_implemented() {
    let bin = temp_bin("wasm32_gc");
    let out = phoenix_bin()
        .args([
            "build",
            "tests/fixtures/hello.phx",
            "--target",
            "wasm32-gc",
            "-o",
        ])
        .arg(&bin.0)
        .output()
        .expect("failed to spawn `phoenix build`");
    assert!(
        !out.status.success(),
        "WASM target should fail in Phase 2.4 PR 1"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not yet implemented"),
        "expected not-yet-implemented diagnostic, got stderr: {stderr}",
    );
    assert!(
        stderr.contains("wasm32-gc"),
        "expected stderr to name the requested target, got: {stderr}",
    );
    assert!(
        !bin.0.exists(),
        "no artifact should be written when compile fails",
    );
}

#[test]
fn unknown_target_lists_every_accepted_spelling() {
    let out = phoenix_bin()
        .args(["build", "tests/fixtures/hello.phx", "--target", "bogus"])
        .output()
        .expect("failed to spawn `phoenix build`");
    assert!(
        !out.status.success(),
        "unknown --target should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown --target"),
        "expected `unknown --target` diagnostic, got stderr: {stderr}",
    );
    assert!(
        stderr.contains("`bogus`"),
        "expected stderr to echo the rejected spelling, got: {stderr}",
    );
    // The diagnostic promises every accepted spelling — guard each so a
    // future variant added without registering its CLI name is caught.
    for expected in ["native", "wasm32-linear", "wasm32-gc"] {
        assert!(
            stderr.contains(expected),
            "expected stderr to list `{expected}` as an accepted target, got: {stderr}",
        );
    }
}
