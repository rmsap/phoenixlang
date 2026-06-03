//! Shared harness for the backend-agreement roundtrip matrices.
//!
//! Both `three_backend_matrix.rs` (single-file fixtures) and
//! `multi_module_matrix.rs` (multi-file projects) assert that every
//! fixture produces byte-identical stdout under `phoenix run`,
//! `phoenix run-ir`, `phoenix build` (native), and `phoenix build
//! --target wasm32-linear` executed under `wasmtime`. The only
//! per-suite differences are how a fixture *key* maps onto the
//! filesystem / into diagnostics and whether an `expected.txt` pin
//! exists — captured in [`MatrixCfg`]. Everything else (process
//! spawning, temp-bin cleanup, the wasmtime soft-skip gate, the
//! divergence message) lives here so the two suites can't drift.
//!
//! The `wasm32-linear` column is **soft-skipped** when `wasmtime`
//! isn't on `$PATH` (a visible warning is printed). Setting
//! `PHOENIX_REQUIRE_WASMTIME=1` turns the skip into a hard failure —
//! the same gating shape as the `compile_wasm_linear.rs` integration
//! tests and §2.3's `PHOENIX_REQUIRE_VALGRIND` gate. CI provisions
//! `wasmtime` and runs `phoenix-driver` with that var set, so a skip
//! there means a real regression (see `.github/workflows/ci.yml`).

use std::process::Command;
use std::sync::OnceLock;

use super::compiled_fixtures::{TempBin, phoenix_bin, workspace_root};

/// Per-suite configuration: how a fixture key maps onto the filesystem
/// and into diagnostics. The function-pointer fields keep both suites
/// driving the *same* assertion code while differing only in their
/// path/label conventions.
pub struct MatrixCfg {
    /// Source path for `key`, relative to the workspace root
    /// (e.g. `"tests/fixtures/hello.phx"` or
    /// `"tests/fixtures/multi/basic_import/main.phx"`).
    pub source_rel: fn(&str) -> String,
    /// Human-facing label for `key` in panic / warning messages
    /// (e.g. `"hello.phx"` or `"multi/basic_import"`).
    pub label: fn(&str) -> String,
    /// Temp-artifact name stem for `key`, disambiguating the two
    /// suites' build outputs in the system temp dir.
    pub bin_stem: fn(&str) -> String,
    /// Optional `expected.txt` path (relative to workspace root). The
    /// multi-file suite pins stdout against it — a coherent regression
    /// that broke every backend the same way would still agree on
    /// stdout, so equality alone is necessary but not sufficient. The
    /// single-file suite has no such file and passes `None`.
    pub expected_rel: Option<fn(&str) -> String>,
}

/// Run `phoenix <subcommand> <source>` (no extra args) and return its
/// stdout. Panics with stderr included on non-zero exit.
fn run_subcommand(cfg: &MatrixCfg, subcommand: &str, key: &str) -> Vec<u8> {
    let path = (cfg.source_rel)(key);
    let output = phoenix_bin()
        .args([subcommand, &path])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `phoenix {subcommand} {path}`: {e}"));
    if !output.status.success() {
        panic!(
            "`phoenix {subcommand} {path}` exited non-zero\n  stdout: {}\n  stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output.stdout
}

/// Wrap `name` as a `TempBin` in the system temp dir, clearing any
/// stale file up front; the guard removes the artifact on drop. The
/// `(pid, thread id)` qualification that keeps parallel runs from
/// colliding is applied by [`native_bin`] / [`wasm_bin`] below.
fn temp_bin(name: String) -> TempBin {
    let bin = TempBin(std::env::temp_dir().join(name));
    let _ = std::fs::remove_file(&bin.0);
    bin
}

/// Native build artifact name, qualified by `(stem, pid, thread id)`:
/// the pid keeps two simultaneous `cargo test` invocations against the
/// same workspace from colliding, the thread id keeps a future change
/// that fans one suite across harness threads from colliding either.
fn native_bin(cfg: &MatrixCfg, key: &str) -> TempBin {
    temp_bin(format!(
        "{}_{}_{:?}",
        (cfg.bin_stem)(key),
        std::process::id(),
        std::thread::current().id(),
    ))
}

fn wasm_bin(cfg: &MatrixCfg, key: &str) -> TempBin {
    temp_bin(format!(
        "{}_wasm_{}_{:?}.wasm",
        (cfg.bin_stem)(key),
        std::process::id(),
        std::thread::current().id(),
    ))
}

/// Compile via `phoenix build`, run the resulting native binary, and
/// return its stdout. Panics with stderr on any non-zero exit.
fn build_and_execute(cfg: &MatrixCfg, key: &str) -> Vec<u8> {
    let path = (cfg.source_rel)(key);
    let bin = native_bin(cfg, key);

    let build = phoenix_bin()
        .args(["build", &path, "-o"])
        .arg(&bin.0)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `phoenix build {path}`: {e}"));
    if !build.status.success() {
        panic!(
            "`phoenix build {path}` exited non-zero\n  stdout: {}\n  stderr: {}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );
    }

    let run = Command::new(&bin.0)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn compiled `{}`: {e}", bin.0.display()));
    if !run.status.success() {
        panic!(
            "compiled `{}` exited non-zero\n  stdout: {}\n  stderr: {}",
            bin.0.display(),
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
    }
    run.stdout
}

/// `PHOENIX_REQUIRE_WASMTIME=1` turns the soft-skip on missing
/// `wasmtime` into a hard failure. Same shape as the §2.3 valgrind
/// gate; documented in the module header above.
fn require_wasmtime() -> bool {
    std::env::var("PHOENIX_REQUIRE_WASMTIME").as_deref() == Ok("1")
}

/// Probe whether `wasmtime` is on `$PATH`, memoized per test binary.
/// The matrix calls this once per fixture, but the answer can't change
/// within a run, so a single `wasmtime --version` spawn (which
/// short-circuits on `ENOENT`) is cached in a `OnceLock` rather than
/// re-spawned for every fixture.
fn wasmtime_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| Command::new("wasmtime").arg("--version").output().is_ok())
}

/// Compile via `phoenix build --target wasm32-linear`, run the
/// resulting `.wasm` under `wasmtime`, and return its stdout. Returns
/// `None` when `wasmtime` isn't on `$PATH` (soft skip), with a stderr
/// warning; `PHOENIX_REQUIRE_WASMTIME=1` turns the skip into a panic
/// instead. The `.wasm` is removed via the [`TempBin`] guard on every
/// exit path, matching the native `build_and_execute`.
fn build_and_execute_wasm(cfg: &MatrixCfg, key: &str) -> Option<Vec<u8>> {
    let label = (cfg.label)(key);
    if !wasmtime_available() {
        if require_wasmtime() {
            panic!(
                "PHOENIX_REQUIRE_WASMTIME=1 set but `wasmtime` is not on PATH \
                 (matrix case: {label})"
            );
        }
        eprintln!(
            "warning: skipping wasm32-linear column for {label} — `wasmtime` \
             not on PATH (set PHOENIX_REQUIRE_WASMTIME=1 to fail instead; see \
             docs/design-decisions.md §Phase 2.4 decision B)"
        );
        return None;
    }
    let path = (cfg.source_rel)(key);
    let bin = wasm_bin(cfg, key);

    let build = phoenix_bin()
        .args(["build", "--target", "wasm32-linear", &path, "-o"])
        .arg(&bin.0)
        .output()
        .unwrap_or_else(|e| {
            panic!("failed to spawn `phoenix build --target wasm32-linear {path}`: {e}")
        });
    if !build.status.success() {
        panic!(
            "`phoenix build --target wasm32-linear {path}` exited non-zero\n  stdout: {}\n  stderr: {}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );
    }

    let run = Command::new("wasmtime")
        .arg(&bin.0)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn wasmtime on `{}`: {e}", bin.0.display()));
    if !run.status.success() {
        panic!(
            "wasmtime exited non-zero on `{}`\n  stdout: {}\n  stderr: {}",
            bin.0.display(),
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
    }
    Some(run.stdout)
}

/// Assert every supplied backend produced byte-identical stdout.
/// `wasm` is `None` when the wasm32-linear column was skipped (either
/// `wasmtime` was absent or the fixture opted out). On any divergence,
/// panics showing every backend's stdout — the AST-interp `run` output
/// is the triage reference point, so it's always shown regardless of
/// which pair actually diverged. One helper so the divergence message
/// stays consistent across the skip / no-skip callers.
fn assert_stdout_agreement(
    label: &str,
    run: &[u8],
    run_ir: &[u8],
    build: &[u8],
    wasm: Option<&[u8]>,
) {
    let native_diverge = run != run_ir || run_ir != build;
    let wasm_diverge = wasm.map(|w| w != build).unwrap_or(false);
    if native_diverge || wasm_diverge {
        let wasm_repr = match wasm {
            Some(w) => format!("{:?}", String::from_utf8_lossy(w)),
            None => "(skipped)".to_string(),
        };
        panic!(
            "{label}: backends disagree on stdout\n  run:    {:?}\n  run-ir: {:?}\n  build:  {:?}\n  wasm:   {}",
            String::from_utf8_lossy(run),
            String::from_utf8_lossy(run_ir),
            String::from_utf8_lossy(build),
            wasm_repr
        );
    }
}

/// Pin `run_stdout` against the fixture's `expected.txt` (resolved via
/// `cfg.expected_rel`). No-op for suites that don't configure one.
fn assert_expected(cfg: &MatrixCfg, key: &str, run_stdout: &[u8]) {
    let Some(expected_rel) = cfg.expected_rel else {
        return;
    };
    let path = workspace_root().join((expected_rel)(key));
    let expected =
        std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    if run_stdout != expected {
        panic!(
            "{}: stdout does not match expected.txt\n  expected: {:?}\n  got:      {:?}",
            (cfg.label)(key),
            String::from_utf8_lossy(&expected),
            String::from_utf8_lossy(run_stdout),
        );
    }
}

/// Full matrix: run / run-ir / native build / wasm (when available),
/// then the optional `expected.txt` pin. The entry point for the
/// plain `..._matrix_test!($name, $key)` macro arm.
pub fn assert_backend_agreement(cfg: &MatrixCfg, key: &str) {
    let run = run_subcommand(cfg, "run", key);
    let run_ir = run_subcommand(cfg, "run-ir", key);
    let build = build_and_execute(cfg, key);
    let wasm = build_and_execute_wasm(cfg, key);
    assert_stdout_agreement(&(cfg.label)(key), &run, &run_ir, &build, wasm.as_deref());
    assert_expected(cfg, key, &run);
}

/// Like [`assert_backend_agreement`] but carves out the wasm32-linear
/// column for fixtures that depend on Phoenix features that backend
/// doesn't lower yet (e.g. `dyn Trait` — `Op::DynAlloc` / `Op::DynCall`).
/// The three-backend agreement (and any `expected.txt` pin) is still
/// asserted; `reason` is printed so a future enablement can flip the
/// macro arm back. The entry point for the `skip_wasm:` macro arm.
pub fn assert_backend_agreement_skip_wasm(cfg: &MatrixCfg, key: &str, reason: &str) {
    let label = (cfg.label)(key);
    eprintln!(
        "note: {label}: skipping wasm32-linear column — {reason} \
         (see backend matrix opt-out at this site)"
    );
    let run = run_subcommand(cfg, "run", key);
    let run_ir = run_subcommand(cfg, "run-ir", key);
    let build = build_and_execute(cfg, key);
    assert_stdout_agreement(&label, &run, &run_ir, &build, None);
    assert_expected(cfg, key, &run);
}
