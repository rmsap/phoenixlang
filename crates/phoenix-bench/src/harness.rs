//! Compile-and-link harness for the `compile_and_run` bench group and
//! the `*_native` fixture-validity tests.
//!
//! # Execution model
//!
//! Source → object → linked executable, once per fixture (cached by
//! source-hash within the process). Each timed iteration spawns the
//! cached executable as a subprocess. `cranelift-jit` was rejected
//! because the runtime's singleton GC heap needs a per-iter reset and
//! the JIT glue would re-resolve every runtime extern symbol every
//! iter.
//!
//! # Failure model
//!
//! `CompileLinkError` is for **skip-able** conditions: fixture-level
//! codegen refusal (`IrVerify` / `CraneliftCompile`) and the
//! first-run runtime-lib-not-built trip-wire (`RuntimeLibMissing`).
//! **Hard environmental failures panic** — no `cc`, write errors,
//! linker non-zero exit, unsupported host. The split exists because a
//! broken toolchain should fail a CI runner loudly rather than
//! silently producing zero measurements.
//!
//! # Measurement bias
//!
//! `time_run` reports `Instant`-bounded wall-clock that includes
//! subprocess spawn (single-digit ms on Linux, slower elsewhere) and
//! up to [`WAIT_POLL_CAP`] of poll quantization. The `compile_and_run`
//! group is designed for catching cumulative regressions across
//! codegen + runtime + execution, not sub-ms tuning wins. A
//! `pidfd_open` + `poll` design would give exact-exit-time precision
//! on Linux but is non-portable; an `Arc<Mutex<Child>>` watchdog
//! thread is portable but adds a synchronization layer. Polling with
//! exponential backoff is the simpler-and-good-enough choice.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::{Arc, LazyLock, LockResult, Mutex};
use std::time::{Duration, Instant};

use crate::check_fixture;

/// Hard wall-clock budget for a single subprocess run. ~1000× the
/// upper bound for a `medium`-class fixture, so a real bench won't
/// trip it even under load; the goal is to kill a fixture that hangs.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on `try_wait` poll spacing. Sized at 1 ms because
/// `compile_and_run`'s smallest participating fixture runs in
/// single-digit ms; a larger cap would swamp sub-ms signal.
const WAIT_POLL_CAP: Duration = Duration::from_millis(1);

/// Initial `try_wait` poll spacing. 100 µs floor: faster polling is
/// CPU-bound for long runs without buying useful precision.
const WAIT_POLL_START: Duration = Duration::from_micros(100);

/// Per-fixture cache slot. Holding the inner `Mutex` across compile +
/// link serializes concurrent callers on the *same* key while callers
/// on other keys proceed in parallel. `Some` only after a successful
/// link, so a failed attempt lets the next caller retry.
type CacheSlot = Arc<Mutex<Option<PathBuf>>>;

/// In-process cache of `(name, source)` hash → per-key compile-and-link slot.
static LINKED_CACHE: LazyLock<Mutex<HashMap<u64, CacheSlot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Per-process scratch directory. PID-stamped so concurrent
/// `cargo bench` invocations don't collide. Not cleaned up on exit —
/// leaving artifacts behind is debug-friendly, and the OS tmp reaper
/// (`tmpwatch` / `systemd-tmpfiles`) handles old PIDs.
static LINKER_TEMP_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    let dir = std::env::temp_dir().join(format!("phoenix-bench-{}", std::process::id()));
    // `LazyLock` does not re-initialize after a panic. Surface OS-level
    // cause so the operator sees what's actionable.
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| {
        panic!(
            "phoenix-bench: failed to create scratch dir {}: {e} \
             (check disk space, permissions, and that $TMPDIR is writable)",
            dir.display(),
        )
    });
    dir
});

/// Recover a poisoned lock by taking the inner value. Cache state is
/// just `Arc`-shared slots — a prior panic mid-compile leaves a slot
/// either `None` (next caller retries) or `Some(path)` (a successful
/// prior link is still usable), so either way the invariant holds.
fn recover<T>(r: LockResult<T>) -> T {
    r.unwrap_or_else(|e| e.into_inner())
}

/// Wait for `child` with a hard `timeout`; SIGKILL and reap on either
/// timeout or a `try_wait` error so reader threads see EOF.
///
/// Poll spacing doubles from `WAIT_POLL_START` up to `WAIT_POLL_CAP`.
/// Measurement bias: a process that exits mid-sleep is observed on
/// the next poll, biasing the duration upward by at most one cap.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    let mut interval = WAIT_POLL_START;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => {
                kill_and_reap(child);
                return None;
            }
        }
        if Instant::now() >= deadline {
            kill_and_reap(child);
            return None;
        }
        std::thread::sleep(interval);
        interval = (interval * 2).min(WAIT_POLL_CAP);
    }
}

/// Best-effort SIGKILL + reap. Errors are swallowed: the child may
/// already have exited; either way the reaping side just needs pipe
/// write-ends closed so reader threads can drain.
fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Stable in-process hash of `(name, source)`. `DefaultHasher` is
/// `SipHash` today and intentionally not promised stable across Rust
/// versions — fine for an in-memory cache, deliberately not persisted.
/// `name` is in the key so byte-identical sources with different
/// names don't share an on-disk artifact stamped with the wrong name.
fn hash_fixture(name: &str, source: &str) -> u64 {
    let mut h = DefaultHasher::new();
    name.hash(&mut h);
    source.hash(&mut h);
    h.finish()
}

/// Skip-able failure modes for `compile_and_link`. Hard environmental
/// failures panic instead — see the module docs.
#[derive(Debug)]
#[non_exhaustive]
pub enum CompileLinkError {
    /// IR verifier rejected the lowered module.
    IrVerify(String),
    /// The Cranelift backend could not lower this fixture.
    CraneliftCompile(String),
    /// `libphoenix_runtime.a` was not on any search path. Most common
    /// cause: forgetting `cargo build -p phoenix-runtime` in a fresh
    /// tree.
    RuntimeLibMissing,
}

impl std::fmt::Display for CompileLinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IrVerify(s) => write!(f, "IR verify failed: {s}"),
            Self::CraneliftCompile(s) => write!(f, "cranelift compile failed: {s}"),
            Self::RuntimeLibMissing => write!(
                f,
                "{} not found; run `cargo build -p phoenix-runtime` \
                 or set $PHOENIX_RUNTIME_LIB",
                phoenix_cranelift::RUNTIME_LIB_NAME,
            ),
        }
    }
}

impl std::error::Error for CompileLinkError {}

/// Compile and link a Phoenix source program into a native
/// executable. Caches by source hash within the current process; see
/// the module docs for the failure model.
pub fn compile_and_link(name: &str, source: &str) -> Result<PathBuf, CompileLinkError> {
    // Fixture name is embedded in on-disk paths — a separator would
    // escape `LINKER_TEMP_DIR`. Hard-assert (not debug_assert) so
    // release-mode bench builds can't silently produce a bad path.
    assert!(
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "fixture name {name:?} must be ASCII alphanumeric + `_` / `-` — \
         it is embedded in on-disk paths",
    );

    let key = hash_fixture(name, source);

    // Outer lock: grab the slot, then drop the outer guard immediately.
    let slot: CacheSlot = recover(LINKED_CACHE.lock())
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(None)))
        .clone();

    // Per-key lock: serializes only same-key callers.
    let mut guard = recover(slot.lock());
    if let Some(p) = guard.as_ref() {
        return Ok(p.clone());
    }

    let (program, check_result) = check_fixture(name, source);
    let ir_module = phoenix_ir::lower(&program, &check_result.module);

    let verify_errors = phoenix_ir::verify::verify(&ir_module);
    if !verify_errors.is_empty() {
        let formatted = verify_errors
            .iter()
            .map(|e| format!("  - {}: {}", e.function, e.message))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(CompileLinkError::IrVerify(format!(
            "{} error(s)\n{formatted}",
            verify_errors.len(),
        )));
    }

    let obj_bytes = phoenix_cranelift::compile(&ir_module)
        .map_err(|e| CompileLinkError::CraneliftCompile(format!("{e}")))?;

    let stem = format!("{name}-{key:016x}");
    let obj_path = LINKER_TEMP_DIR.join(format!("{stem}.o"));
    let exe_path = LINKER_TEMP_DIR.join(&stem);
    std::fs::write(&obj_path, &obj_bytes)
        .unwrap_or_else(|e| panic!("write {} failed: {e}", obj_path.display()));

    // Runtime-lib missing → skip-able; everything else panics so a
    // broken host fails loudly. New `LinkError` variants land in the
    // panic arm by default — opt them into skip-treatment explicitly.
    if let Err(e) = phoenix_cranelift::link_executable(&obj_path, &exe_path) {
        if matches!(&e, phoenix_cranelift::LinkError::RuntimeLibNotFound) {
            return Err(CompileLinkError::RuntimeLibMissing);
        }
        panic!(
            "phoenix-bench: link failed for {name}: {e} (object kept at {})",
            obj_path.display(),
        );
    }

    // Populate only after `Ok`: panic upstream leaves the slot `None`
    // so the next caller retries cleanly.
    *guard = Some(exe_path.clone());
    Ok(exe_path)
}

/// Spawn `exe`, wait, return wall-clock duration. Discards stdout /
/// stderr — `iter_custom` calls this in a tight loop, and piping
/// would add per-iter buffer churn. Panics on spawn failure, non-zero
/// exit, or `RUN_TIMEOUT` exceeded. For captured-output debugging,
/// rerun the matching `*_native` test in `tests/fixture_validity.rs`.
pub fn time_run(exe: &Path) -> Duration {
    let start = Instant::now();
    let mut child = std::process::Command::new(exe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {} failed: {e}", exe.display()));
    let status = wait_with_timeout(&mut child, RUN_TIMEOUT).unwrap_or_else(|| {
        panic!(
            "{} exceeded {:?} timeout — killed by watchdog (likely an infinite \
             loop or runtime deadlock; rerun the matching *_native test in \
             phoenix-bench/tests/fixture_validity.rs to capture output)",
            exe.display(),
            RUN_TIMEOUT,
        )
    });
    let elapsed = start.elapsed();
    if !status.success() {
        panic!(
            "{} exited with {} (rerun the matching *_native test in \
             phoenix-bench/tests/fixture_validity.rs for captured output)",
            exe.display(),
            status,
        );
    }
    elapsed
}

/// Spawn `exe` (with `RUN_TIMEOUT`) and return its stdout split on
/// newlines. Output format mirrors `run_tree_walk` / `run_ir` so the
/// same expected-output assertions can drive all three backends.
/// Panics with captured streams on failure.
///
/// The timeout matters: this is the entry point for the `*_native`
/// fixture-validity tests, which run in-process under `cargo test` —
/// a fixture that hangs without a timeout would stall the whole test
/// binary.
pub fn run_native(exe: &Path) -> Vec<String> {
    let mut child = std::process::Command::new(exe)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {} failed: {e}", exe.display()));

    // Drain stdout/stderr in dedicated threads so a child that writes
    // more than the pipe buffer (64 KiB on Linux) doesn't deadlock.
    let stdout_handle = child.stdout.take().expect("stdout was piped above");
    let stderr_handle = child.stderr.take().expect("stderr was piped above");
    let stdout_thread = std::thread::spawn(move || drain_to_string(stdout_handle));
    let stderr_thread = std::thread::spawn(move || drain_to_string(stderr_handle));

    let status = wait_with_timeout(&mut child, RUN_TIMEOUT);
    // Both wait_with_timeout exits leave the child reaped, which
    // closes the pipe write-ends so the reader threads see EOF.
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    let Some(status) = status else {
        panic!(
            "{} exceeded {:?} timeout — killed by watchdog (likely an infinite \
             loop or runtime deadlock)\nstdout: {stdout}\nstderr: {stderr}",
            exe.display(),
            RUN_TIMEOUT,
        );
    };
    if !status.success() {
        panic!(
            "{} exited with {status} (stdout: {stdout}, stderr: {stderr})",
            exe.display(),
        );
    }
    stdout.lines().map(str::to_owned).collect()
}

/// Read `r` to EOF. I/O errors are swallowed — partial output is more
/// useful in a panic message than an unwrap.
fn drain_to_string<R: std::io::Read>(mut r: R) -> String {
    let mut s = String::new();
    let _ = r.read_to_string(&mut s);
    s
}

/// Non-panicking variant of `run_native`: returns whether `exe`
/// exited cleanly within `RUN_TIMEOUT`. Used to gate `compile_and_run`
/// on actual runtime success — Cranelift may compile fixtures today
/// that abort at runtime, and we'd rather skip than panic mid-run.
pub fn probe_native(exe: &Path) -> bool {
    let Ok(mut child) = std::process::Command::new(exe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    match wait_with_timeout(&mut child, RUN_TIMEOUT) {
        Some(status) => status.success(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock in the `Display` text of every `CompileLinkError` variant.
    #[test]
    fn compile_link_error_display_covers_all_variants() {
        let ir = CompileLinkError::IrVerify("bad block".into());
        assert!(
            ir.to_string().starts_with("IR verify failed:"),
            "unexpected IrVerify display: {ir}"
        );
        assert!(ir.to_string().contains("bad block"));

        let cl = CompileLinkError::CraneliftCompile("no main".into());
        assert!(
            cl.to_string().starts_with("cranelift compile failed:"),
            "unexpected CraneliftCompile display: {cl}"
        );
        assert!(cl.to_string().contains("no main"));

        let missing = CompileLinkError::RuntimeLibMissing.to_string();
        assert!(
            missing.contains(phoenix_cranelift::RUNTIME_LIB_NAME),
            "RuntimeLibMissing should name the missing lib: {missing}"
        );
        assert!(
            missing.contains("cargo build -p phoenix-runtime"),
            "RuntimeLibMissing should suggest building the runtime: {missing}"
        );
    }

    /// `probe_native` must distinguish clean exit from any other
    /// outcome. An inverted return would silently disable every
    /// `compile_and_run` registration with no measurements produced.
    #[cfg(unix)]
    #[test]
    fn probe_native_distinguishes_clean_exit_from_failure() {
        assert!(
            probe_native(Path::new("/bin/true")),
            "probe_native should return true for a clean-exit binary"
        );
        assert!(
            !probe_native(Path::new("/bin/false")),
            "probe_native should return false for a non-zero exit"
        );
        assert!(
            !probe_native(Path::new("/nonexistent/path/to/binary")),
            "probe_native should return false when spawn fails"
        );
    }

    /// `time_run` must panic (not return a duration) on non-zero exit,
    /// else `iter_custom` would silently fold a broken binary's
    /// spawn-only time into the measurement.
    #[cfg(unix)]
    #[test]
    #[should_panic(expected = "/bin/false")]
    fn time_run_panics_on_nonzero_exit() {
        time_run(Path::new("/bin/false"));
    }

    /// Spawn-failure path of `time_run`. Timeout branch is exercised
    /// by hand when `RUN_TIMEOUT` changes — a 30 s test is too slow
    /// for the regular loop.
    #[cfg(unix)]
    #[test]
    #[should_panic(expected = "spawn")]
    fn time_run_panics_on_spawn_failure() {
        time_run(Path::new("/nonexistent/path/to/binary"));
    }

    /// Two consecutive calls with the same `(name, source)` must
    /// return the same path. A regression in slot bookkeeping would
    /// silently let `iter_custom` re-pay compile + link every iter.
    #[test]
    fn compile_and_link_caches_by_fixture() {
        let source = "function main() { print(1) }";
        let first = match compile_and_link("cache_hit_probe", source) {
            Ok(p) => p,
            Err(CompileLinkError::RuntimeLibMissing) => {
                eprintln!(
                    "warning: skipping compile_and_link_caches_by_fixture — runtime lib not \
                     built; run `cargo build -p phoenix-runtime` to exercise the cache path"
                );
                return;
            }
            Err(e) => panic!("first compile_and_link failed unexpectedly: {e}"),
        };
        let second = compile_and_link("cache_hit_probe", source)
            .expect("second compile_and_link must hit the cache");
        assert_eq!(
            first, second,
            "cache hit must return the same exe path; got {first:?} then {second:?}",
        );
    }

    /// `compile_and_link` must return `Err` (not panic) on a fixture
    /// Cranelift can't lower. Uses `medium_large` which today fails
    /// on `print()` of `list<i64>`; when that gap closes, this test
    /// turns into a re-pointing reminder.
    #[test]
    fn compile_and_link_returns_err_for_unsupported_fixture() {
        match compile_and_link("medium_large", crate::MEDIUM_LARGE) {
            Err(CompileLinkError::CraneliftCompile(_)) | Err(CompileLinkError::IrVerify(_)) => {}
            Err(CompileLinkError::RuntimeLibMissing) => {
                eprintln!(
                    "warning: compile_and_link_returns_err_for_unsupported_fixture \
                     short-circuited on RuntimeLibMissing — run \
                     `cargo build -p phoenix-runtime` to exercise the intended path"
                );
            }
            Ok(p) => panic!(
                "expected compile_and_link to return Err for the medium_large fixture, \
                 but got Ok({}) — the codegen gap may have closed; re-point this test \
                 at a still-unsupported fixture or remove it",
                p.display()
            ),
        }
    }
}
