//! Verifies that compiled Phoenix programs reclaim heap memory under
//! load — the regression the three-backend matrix can't catch.
//!
//! The matrix only compares stdout. If the GC silently stopped
//! collecting (e.g. `phx_gc_enable` got dropped from the C `main`
//! wrapper, or the auto-collect threshold got infinity'd), `alloc_loop`
//! would still terminate with the right total — it would just leak
//! every iteration's allocation. This test forces that scenario to
//! fail by capping the child's virtual address space *below* what the
//! leak-everything path would need.
//!
//! Test shape: run `alloc_loop.phx` as a compiled binary with
//! `RLIMIT_AS` capped to 64 MB (set in
//! [`alloc_loop_stays_under_address_space_limit`] below — see that
//! function's comment for the per-iter accounting). The fixture
//! allocates ~80–100 bytes per iteration × 100k iterations ≈ 8–10 MB
//! cumulative. With GC working, peak live-bytes stays in the kilobyte
//! range. Without GC, every allocation is retained and the cumulative
//! 8–10 MB plus glibc + thread stacks + Cranelift-emitted code
//! exhausts the 64 MB virtual-address-space cap; the binary either
//! hits the rlimit and is killed, or `mmap`/`brk` returns an error
//! and the runtime aborts.
//!
//! Linux-only because that's where `RLIMIT_AS` and `pre_exec` give
//! us the surgical control we need without parsing platform-specific
//! `/proc` files. CI is expected to run Linux for the GC gate.

#![cfg(target_os = "linux")]

mod common;

use common::compiled_fixtures::build_fixture;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;

/// 64 MB virtual-address-space cap. A typical Linux x86_64 Rust
/// binary maps roughly 30–50 MB of virtual address space at steady
/// state (glibc + thread stacks with their guard regions + Rust
/// runtime + Cranelift-emitted code section). 64 MB sits ~14–34 MB
/// above that, leaving headroom for the GC's own arena (peak a few
/// hundred KiB on these fixtures) plus a small margin — but well
/// below the leak-everything path's cumulative footprint.
///
/// **Calibration risk.** If glibc/jemalloc starts reserving very
/// large arenas up-front (so a leak doesn't trigger fresh `mmap`
/// calls until much later), these tests could pass even on a
/// regressed GC. The 64 MB number is the most conservative cap
/// that still leaves room for the steady state on the CI image
/// we've measured against. If a future libc upgrade flakes the
/// tests, the right move is to *raise* iteration count in the
/// fixtures, not raise the cap.
///
/// If a single number stops fitting both the steady-state mappings
/// *and* the leak-detection signal across CI environments, this
/// constant should grow into a `match` over `cfg!(target_env = ...)`
/// (gnu vs. musl libc) rather than be bumped to a value that
/// accommodates the largest. Document the chosen cap per env and
/// keep the leak-path footprint comfortably above all of them.
///
/// Local debugging escape hatch: `PHOENIX_GC_RLIMIT_BYTES=128M cargo
/// test ...` overrides the cap without editing this file. Useful
/// when a CI image's steady-state mappings drift and the tests
/// flake — the override makes calibration a one-liner instead of
/// a code change. Production CI should leave it unset so the
/// documented cap is exercised.
const DEFAULT_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

fn rlimit_bytes() -> u64 {
    match std::env::var("PHOENIX_GC_RLIMIT_BYTES") {
        Ok(s) => s.parse().unwrap_or_else(|e| {
            panic!(
                "PHOENIX_GC_RLIMIT_BYTES must be a u64 byte count (no \
                 suffix); got {s:?}: {e}"
            )
        }),
        Err(_) => DEFAULT_LIMIT_BYTES,
    }
}

/// Run a compiled fixture under `RLIMIT_AS`, panic on non-zero exit
/// (with disambiguated failure-mode reporting), and assert stdout
/// equals `expected_stdout` after trimming.
fn run_under_rlimit(fixture: &str, expected_stdout: &str) {
    let bin = build_fixture(fixture, "phoenix_gc_mem");
    let limit_bytes = rlimit_bytes();

    let mut cmd = Command::new(&bin.0);
    unsafe {
        cmd.pre_exec(move || {
            let rlim = libc::rlimit {
                rlim_cur: limit_bytes,
                rlim_max: limit_bytes,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &rlim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn rlimit'd {fixture}: {e}"));

    if !output.status.success() {
        // Disambiguate failure modes so a regression report points at
        // the right layer. SIGKILL → kernel OOM-killer (cgroup or
        // overcommit limit hit); SIGSEGV → likely a GC bug
        // (use-after-free/dangling root); exit(1) → runtime
        // `runtime_abort` path (e.g. allocator returned null after the
        // rlimit pinched). The 64-MB cap is only the most likely cause
        // for the first; the message should not lock the reader into
        // assuming "GC not reclaiming".
        let signal = output.status.signal();
        let code = output.status.code();
        let likely_cause: String = match (signal, code) {
            (Some(libc::SIGKILL), _) => "kernel SIGKILL — likely RLIMIT_AS / OOM-killer".into(),
            (Some(sig), _) => format!("killed by signal {sig}"),
            (None, Some(1)) => {
                "exit 1 — likely `runtime_abort` (allocator failure or panic)".into()
            }
            (None, Some(c)) => format!("non-zero exit code {c}"),
            (None, None) => "no exit code and no signal (unexpected)".into(),
        };
        panic!(
            "{fixture} failed under the {limit_bytes}-byte virtual-memory cap.\n  \
             likely cause: {likely_cause}\n  \
             status: {:?}\n  \
             stdout: {}\n  \
             stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim() == expected_stdout,
        "{fixture} output drifted from {expected_stdout:?}; stdout = {stdout:?}"
    );
}

#[test]
fn alloc_loop_stays_under_address_space_limit() {
    // Per-iteration, the fixture allocates roughly:
    //   - list `[i, i+1, i+2]`: 24-byte header + 24-byte payload +
    //     8-byte GC header = 56 bytes
    //   - `toString(i)` string: up to ~13 bytes (5-byte content + 8
    //     header) for i near 100k
    //   - `"iter " + toString(i)` concat: ~18 bytes (10-byte content
    //     + 8 header)
    // → ~87 bytes/iter × 100k iters ≈ 8.7 MB cumulative without GC.
    // Combined with the steady-state mappings, the leak path drives
    // virtual usage past 64 MB; the kernel rejects further `mmap`/`brk`
    // and either SIGKILLs the process or `runtime_abort` fires when
    // the allocator returns null. Either way the exit status is
    // non-zero and the harness reports informatively.
    //
    // Stdout sanity: 3 elements per iter × 100k iters = 300000.
    run_under_rlimit("alloc_loop.phx", "300000");
}

#[test]
fn gc_keeps_alive_stays_under_address_space_limit() {
    // Sibling regression to `alloc_loop_stays_under_address_space_limit`.
    // The matrix already pins stdout (`5`) for `gc_keeps_alive.phx`, but
    // a regression that swept `keep` between auto-collects and produced
    // an allocation-coincidence reread of `5` would slip past stdout
    // alone. Capping virtual address space below the leak-everything
    // footprint forces a real signal: with the GC working, the loop's
    // throwaway allocations are reclaimed each cycle and `keep` survives
    // via the entry-block shadow-stack frame; without it, the cumulative
    // 200k iterations of two allocations each (~80 bytes/iter ≈ 16 MB)
    // plus steady-state mappings exceed the cap.
    //
    // Higher iteration count than `alloc_loop` widens the leak signal,
    // and the long-lived `keep` reference doubles as a check that the
    // mark phase actually walks the entry-block frame across many
    // collection cycles (not just allocates and immediately sweeps).
    run_under_rlimit("gc_keeps_alive.phx", "5");
}
