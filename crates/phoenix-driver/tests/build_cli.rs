//! CLI integration tests for `phoenix build`'s `--target` flag.
//!
//! Default-target behavior (no `--target`) is exercised by
//! `three_backend_matrix.rs` for every fixture, so these tests focus
//! on the new flag: explicit `native` parity, the `wasm32-linear`
//! success path (PR 2 of Phase 2.4) under both explicit `-o` and the
//! implicit `.wasm` suffix branch, the still-not-implemented
//! `wasm32-gc` variant, and the unknown-target diagnostic. Deeper
//! WASM correctness coverage lives in
//! `phoenix-cranelift/tests/compile_wasm_linear.rs`.

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

/// Smoke-check the emitted bytes look like a WASM module. Header is
/// `\0asm` (4 bytes) followed by a 4-byte little-endian version word.
/// Reads exactly the 8-byte header so the assertion proves both halves
/// are present and well-formed without slurping the whole module.
fn assert_is_wasm_module(path: &std::path::Path) {
    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .unwrap_or_else(|err| panic!("open emitted .wasm at {}: {err}", path.display()));
    let mut header = [0u8; 8];
    file.read_exact(&mut header).unwrap_or_else(|err| {
        panic!(
            "emitted file at {} is shorter than 8 bytes (read_exact: {err})",
            path.display()
        )
    });
    assert_eq!(
        &header[..4],
        b"\0asm",
        "emitted file at {} is not a WASM module (missing \\0asm magic)",
        path.display(),
    );
    // MVP / current core spec version word is `0x00000001` little-endian.
    // The wasm-encoder we ship emits this; pinning it here catches a
    // future bump to a non-MVP version (e.g. a draft extension) that
    // would slip past a magic-only check.
    assert_eq!(
        &header[4..8],
        &[0x01, 0x00, 0x00, 0x00],
        "emitted file at {} has unexpected WASM version word {:02x?}",
        path.display(),
        &header[4..8],
    );
}

#[test]
fn wasm32_linear_target_emits_wasm_module_with_explicit_output() {
    // Explicit `-o` path: caller's choice wins verbatim, no `.wasm`
    // suffix inference. Pinning this branch separately from the
    // implicit-suffix branch below catches regressions in either
    // side of `build.rs`'s output-path logic.
    let bin = temp_bin("wasm32_linear_explicit.wasm");
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
        out.status.success(),
        "`phoenix build --target wasm32-linear` exited non-zero\n  stdout: {}\n  stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        bin.0.exists(),
        "expected .wasm artifact at {}",
        bin.0.display()
    );
    assert_is_wasm_module(&bin.0);
}

#[test]
fn wasm32_linear_target_appends_wasm_suffix_when_output_omitted() {
    // Implicit `.wasm` suffix: with no `-o` flag, `build.rs` derives
    // the output filename from the input stem and appends `.wasm` for
    // WASM targets. Run inside a fresh tempdir so the inferred path
    // doesn't collide with other tests' output and so the RAII
    // cleanup is contained.
    let dir = tempfile::tempdir().expect("create tempdir");
    let fixture_src = workspace_root().join("tests/fixtures/hello.phx");
    let fixture_dst = dir.path().join("hello.phx");
    std::fs::copy(&fixture_src, &fixture_dst).expect("copy hello.phx fixture into the tempdir");

    let out = Command::new(env!("CARGO_BIN_EXE_phoenix"))
        .current_dir(dir.path())
        .args(["build", "hello.phx", "--target", "wasm32-linear"])
        .output()
        .expect("failed to spawn `phoenix build`");
    assert!(
        out.status.success(),
        "`phoenix build --target wasm32-linear` exited non-zero\n  stdout: {}\n  stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let inferred = dir.path().join("hello.wasm");
    assert!(
        inferred.exists(),
        "expected inferred .wasm artifact at {}",
        inferred.display()
    );
    assert_is_wasm_module(&inferred);

    // Lock the tempdir to exactly the input fixture + emitted module.
    // Anything else (a leftover .o, a stray .wat dump, a half-finished
    // temp file) indicates `build.rs` is writing artifacts where it
    // shouldn't — easier to catch here than after PR 3+ accumulates
    // additional emitted outputs.
    let owned_entries: Vec<std::ffi::OsString> = std::fs::read_dir(dir.path())
        .expect("read tempdir")
        .map(|e| e.expect("dir entry").file_name())
        .collect();
    let mut entries: Vec<&str> = owned_entries
        .iter()
        .map(|s| s.to_str().expect("non-UTF-8 tempdir entry"))
        .collect();
    entries.sort_unstable();
    assert_eq!(
        entries,
        ["hello.phx", "hello.wasm"],
        "tempdir contents drifted; `phoenix build` is writing artifacts \
         outside the expected (input, emitted .wasm) pair"
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
        "wasm32-gc should still fail until Phase 2.4 PR 5+ lands GC codegen"
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
    // The diagnostic promises every accepted spelling — guard each by
    // walking `Target::all_cli_names()` itself so a future variant
    // registered there (but accidentally dropped from the diagnostic
    // formatter) trips this test rather than silently going missing.
    for expected in phoenix_cranelift::Target::all_cli_names() {
        assert!(
            stderr.contains(expected),
            "expected stderr to list `{expected}` as an accepted target, got: {stderr}",
        );
    }
}
