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

/// `$VAR` is set to exactly `1`. The opt-in shape shared by every
/// `PHOENIX_REQUIRE_*` gate in the repo.
fn require(var: &str) -> bool {
    std::env::var(var).as_deref() == Ok("1")
}

/// Whether the `phoenix` binary under test will find the runtime static
/// library. Probes `$PHOENIX_RUNTIME_LIB` (which the binary honors) and
/// otherwise the binary's *own* exe-relative search — NOT this test
/// binary's. The test binary lives in `target/<profile>/deps/`, so a
/// `find_runtime_lib()` here would see a `deps/libphoenix_runtime.a` that
/// the spawned binary (in `target/<profile>/`) never searches — making the
/// skip decision disagree with the build the test then runs.
fn runtime_lib_visible_to_phoenix() -> bool {
    if std::env::var_os("PHOENIX_RUNTIME_LIB").is_some() {
        return true;
    }
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_phoenix"));
    phoenix_cranelift::find_runtime_lib_near(&bin).is_some()
}

/// Soft-skip the native build tier when `libphoenix_runtime.a` isn't
/// on any search path: the CLI link step would hard-error, but linking
/// is profile-independent and is already gated in the debug `check` job
/// (which builds the lib and sets `PHOENIX_REQUIRE_RUNTIME_LIB=1`) and
/// proven end-to-end by release.yml's install smoke test. Here the
/// `release-test` / release `test` jobs run `cargo test --release`
/// without building the lib, so without this gate they fail spuriously.
/// `PHOENIX_REQUIRE_RUNTIME_LIB=1` turns the skip into a hard failure —
/// same shape as `link.rs`'s in-crate `precheck` gate. Returns `true`
/// when the caller should early-return (skip).
#[must_use]
fn skip_if_no_runtime_lib(label: &str) -> bool {
    if runtime_lib_visible_to_phoenix() {
        return false;
    }
    assert!(
        !require("PHOENIX_REQUIRE_RUNTIME_LIB"),
        "PHOENIX_REQUIRE_RUNTIME_LIB=1 set but libphoenix_runtime.a is not on any \
         search path — run `cargo build -p phoenix-runtime` or set $PHOENIX_RUNTIME_LIB"
    );
    eprintln!(
        "warning: skipping {label} — libphoenix_runtime.a not built \
         (set PHOENIX_REQUIRE_RUNTIME_LIB=1 to fail instead; \
         `cargo build -p phoenix-runtime` to fix)"
    );
    true
}

/// Wasm-tier counterpart of [`skip_if_no_runtime_lib`], gated by
/// `PHOENIX_REQUIRE_RUNTIME_WASM=1`. Mirrors the skip plumbing in
/// `phoenix-cranelift/tests/compile_wasm_linear.rs`. Returns `true`
/// when the caller should early-return (skip).
#[must_use]
fn skip_if_no_runtime_wasm(label: &str) -> bool {
    if phoenix_cranelift::runtime_wasm_available() {
        return false;
    }
    assert!(
        !require("PHOENIX_REQUIRE_RUNTIME_WASM"),
        "PHOENIX_REQUIRE_RUNTIME_WASM=1 set but phoenix_runtime.wasm is not on any \
         search path — run `cargo build -p phoenix-runtime --target wasm32-wasip1 --release` first"
    );
    eprintln!(
        "warning: skipping {label} — phoenix_runtime.wasm not built \
         (set PHOENIX_REQUIRE_RUNTIME_WASM=1 to fail instead; \
         `cargo build -p phoenix-runtime --target wasm32-wasip1 --release` to fix)"
    );
    true
}

#[test]
fn explicit_native_target_builds_and_runs() {
    if skip_if_no_runtime_lib("explicit_native_target_builds_and_runs") {
        return;
    }
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
fn native_target_appends_exe_suffix_when_output_omitted() {
    // Implicit native output: with no `-o`, `build.rs` derives the
    // filename from the input stem and appends `std::env::consts::EXE_SUFFIX`
    // (`.exe` on Windows, empty elsewhere) so the default-named artifact is
    // directly runnable. On Unix the suffix is empty so this pins the
    // implicit-native branch; on Windows it specifically proves the `.exe`
    // append — the one path the smoke-test's explicit `-o ...exe` can't
    // exercise. Runs in a fresh tempdir so the inferred path is contained.
    if skip_if_no_runtime_lib("native_target_appends_exe_suffix_when_output_omitted") {
        return;
    }
    let dir = tempfile::tempdir().expect("create tempdir");
    let fixture_src = workspace_root().join("tests/fixtures/hello.phx");
    let fixture_dst = dir.path().join("hello.phx");
    std::fs::copy(&fixture_src, &fixture_dst).expect("copy hello.phx fixture into the tempdir");

    let out = Command::new(env!("CARGO_BIN_EXE_phoenix"))
        .current_dir(dir.path())
        .args(["build", "hello.phx", "--target", "native"])
        .output()
        .expect("failed to spawn `phoenix build`");
    assert!(
        out.status.success(),
        "`phoenix build --target native` exited non-zero\n  stdout: {}\n  stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let inferred = dir
        .path()
        .join(format!("hello{}", std::env::consts::EXE_SUFFIX));
    assert!(
        inferred.exists(),
        "expected inferred native artifact at {} (EXE_SUFFIX = {:?})",
        inferred.display(),
        std::env::consts::EXE_SUFFIX,
    );

    let run = Command::new(&inferred)
        .output()
        .expect("failed to run inferred native binary");
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
    if skip_if_no_runtime_wasm("wasm32_linear_target_emits_wasm_module_with_explicit_output") {
        return;
    }
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
    if skip_if_no_runtime_wasm("wasm32_linear_target_appends_wasm_suffix_when_output_omitted") {
        return;
    }
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
fn wasm32_gc_target_emits_a_wasm_artifact() {
    // PR 5 slice 1 lifted the "not yet implemented" stub:
    // `--target wasm32-gc` now produces a `.wasm` for `hello.phx`
    // (per design-decisions.md §Phase 2.4 decision J's MVP scope).
    // Op-surface beyond the slice-1 minimum (closures / lists /
    // maps / strings / etc.) still errors, but `hello.phx` is on
    // the supported path.
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
        out.status.success(),
        "wasm32-gc build of hello.phx should now succeed; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        bin.0.exists(),
        "`phoenix build --target wasm32-gc -o {}` should have written the artifact",
        bin.0.display(),
    );
    // The output is a real WASM module — at minimum it starts with
    // the WASM magic bytes. Full structural validation lives in
    // `crates/phoenix-cranelift/tests/compile_wasm_gc.rs`.
    let bytes = std::fs::read(&bin.0).expect("read wasm artifact");
    assert!(
        bytes.starts_with(b"\0asm"),
        "expected WASM magic bytes at the start of the artifact"
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
