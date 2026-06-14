//! `phoenix check` must pass on every fixture in the realistic gen schema
//! library (`tests/fixtures/{payments,multitenant_saas,webhooks,file_storage,
//! social,internal_admin}.phx`).
//!
//! This guards the parse/sema-clean invariant of the fixture library while the
//! full compile-and-lint wiring into `phoenix-codegen`'s `compiles_and_lints.rs`
//! is deferred (it goes RED on known generator bugs — see the "Harness wiring
//! status" note in docs/design-decisions.md). The known bugs are all in
//! *generator output*, so a check-level gate is green today and protects the
//! fixtures from parser/sema regressions and accidental edits.
//!
//! One `#[test]` per fixture so a failure names the offending schema directly
//! in `cargo test` output.
//!
//! Each `phoenix check` runs under two bounds — a wall-clock timeout (kill and
//! fail rather than hang the suite) and, on Linux, an `RLIMIT_AS` cap. These
//! guard against a parser error-recovery memory blowup that *was* live (a
//! poisoned input ballooned to 662 MB+ instead of exiting; the WSL2 failure
//! mode thrashed the host). That blowup is RESOLVED as of 2026-06-12 — the
//! bounds stay as a regression backstop: they turn any reintroduction into a
//! fast, attributable failure instead of a hung suite.
//!
//! The known repro inputs are committed as `tests/fixtures/poisoned/` and run
//! un-ignored (see [`assert_rejected_with_diagnostic`]) as the regression tests
//! that keep the parser's error recovery bounded — each must exit non-zero with
//! a diagnostic, not die by rlimit signal.

mod common;

use common::compiled_fixtures::phoenix_bin;
use std::io::Read;
use std::process::{ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Upper bound on one `phoenix check` run. A clean check finishes in well
/// under a second; the slack is for cold caches and loaded CI machines.
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

/// Linux-only address-space cap on the child. A clean `phoenix check` over a
/// fixture this size peaks around 9 MB of heap on top of the binary's
/// tens-of-MB steady-state mappings; the measured error-recovery runaway
/// passes 662 MB on its way to allocation failure. 1 GiB is far above any
/// legitimate run yet low enough that a runaway hits the rlimit and aborts
/// quickly instead of thrashing the host until [`CHECK_TIMEOUT`] fires.
#[cfg(target_os = "linux")]
const CHECK_RLIMIT_BYTES: u64 = 1024 * 1024 * 1024;

/// Drain a child output stream to completion on a background thread. The
/// child must never block on a full pipe buffer while we poll for exit —
/// a blocked child would sit until the timeout and be misreported as the
/// parser-OOM hang, with the very diagnostics that explain it discarded.
fn drain(stream: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut stream = stream;
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf);
        buf
    })
}

/// Run `phoenix check <path>` (workspace-relative) under [`CHECK_TIMEOUT`]
/// and — on Linux — the [`CHECK_RLIMIT_BYTES`] address-space cap, returning
/// the exit status plus captured stdout/stderr. Kills the child and panics
/// (with whatever output it produced) if it outlives the timeout.
fn run_check(path: &str) -> (ExitStatus, Vec<u8>, Vec<u8>) {
    let mut cmd = phoenix_bin();
    cmd.args(["check", path])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "linux")]
    common::rlimit::cap_address_space(&mut cmd, CHECK_RLIMIT_BYTES);

    let mut child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn `phoenix check {path}`: {e}"));
    let stdout = drain(child.stdout.take().expect("stdout was piped"));
    let stderr = drain(child.stderr.take().expect("stderr was piped"));

    let deadline = Instant::now() + CHECK_TIMEOUT;
    let status = loop {
        let status = child
            .try_wait()
            .unwrap_or_else(|e| panic!("failed to poll `phoenix check {path}`: {e}"));
        if let Some(status) = status {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // Reaping the child closes its end of the pipes, so the drain
            // threads see EOF and finish with whatever was written.
            let stdout = stdout.join().expect("stdout drain panicked");
            let stderr = stderr.join().expect("stderr drain panicked");
            panic!(
                "`phoenix check {path}` exceeded {CHECK_TIMEOUT:?} and was killed — \
                 likely the parser error-recovery OOM; see that entry in \
                 docs/known-issues.md for the currently known triggers\n  \
                 stdout so far: {}\n  stderr so far: {}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let stdout = stdout.join().expect("stdout drain panicked");
    let stderr = stderr.join().expect("stderr drain panicked");
    (status, stdout, stderr)
}

/// On Linux the rlimit cap turns the parser error-recovery OOM into a fast
/// allocation-failure abort, so an exit **by signal** is the likelier face of
/// that bug here (the timeout is only the fallback); a plain non-zero exit
/// code is an ordinary diagnostic failure. Returns a message suffix pointing
/// at the known-issues entry when the status says signal.
#[cfg(target_os = "linux")]
fn oom_hint(status: &ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status.signal() {
        Some(sig) => format!(
            "\n  killed by signal {sig} under the {CHECK_RLIMIT_BYTES}-byte RLIMIT_AS \
             cap — likely the parser error-recovery OOM; see that entry in \
             docs/known-issues.md for the currently known triggers"
        ),
        None => String::new(),
    }
}

#[cfg(not(target_os = "linux"))]
fn oom_hint(_status: &ExitStatus) -> String {
    String::new()
}

/// Run `phoenix check tests/fixtures/<fixture>` and panic with the full
/// diagnostic output on a non-zero exit.
fn assert_checks_clean(fixture: &str) {
    let path = format!("tests/fixtures/{fixture}");
    let (status, stdout, stderr) = run_check(&path);
    if !status.success() {
        panic!(
            "`phoenix check {path}` exited non-zero{}\n  stdout: {}\n  stderr: {}",
            oom_hint(&status),
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
    }
}

/// Run `phoenix check tests/fixtures/poisoned/<fixture>` and assert it fails
/// the way a *fixed* parser must: an ordinary non-zero exit carrying a
/// diagnostic — not a pass (the fixtures are malformed on purpose), not death
/// by signal (the rlimit catching a memory runaway), and not the wall-clock
/// timeout ([`run_check`] panics on that itself).
fn assert_rejected_with_diagnostic(fixture: &str) {
    let path = format!("tests/fixtures/poisoned/{fixture}");
    let (status, stdout, stderr) = run_check(&path);
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(
        !status.success(),
        "`phoenix check {path}` passed, but the fixture is malformed on purpose — \
         either the poisoned fixture lost its trigger or the parser now silently \
         accepts it\n  stdout: {stdout}\n  stderr: {stderr}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert!(
            status.signal().is_none(),
            "`phoenix check {path}` was killed by signal {:?} — the parser \
             error-recovery OOM (docs/known-issues.md) is still live\n  \
             stdout: {stdout}\n  stderr: {stderr}",
            status.signal().unwrap()
        );
    }
    assert!(
        !stdout.trim().is_empty() || !stderr.trim().is_empty(),
        "`phoenix check {path}` exited non-zero without emitting any diagnostic"
    );
}

macro_rules! schema_fixture_checks {
    ($($test_name:ident => $fixture:literal),+ $(,)?) => {
        $(
            #[test]
            fn $test_name() {
                assert_checks_clean($fixture);
            }
        )+
    };
}

// Kept in lockstep with `FILE_FIXTURES` in `phoenix-codegen`'s
// `compiles_and_lints.rs` — `gen_schema_library_lists_match` in
// `fixture_inventory.rs` fails if the two lists diverge.
schema_fixture_checks! {
    check_payments => "payments.phx",
    check_multitenant_saas => "multitenant_saas.phx",
    check_webhooks => "webhooks.phx",
    check_file_storage => "file_storage.phx",
    check_social => "social.phx",
    check_internal_admin => "internal_admin.phx",
}

// ── Parser error-recovery OOM repros ──────────────────────────────────────
//
// `tests/fixtures/poisoned/` preserves the known triggers of the parser
// error-recovery OOM as minimal committed inputs, so the fix can't silently
// regress. The OOM is resolved (see the note on `poisoned_fixture_checks!`
// below); these run un-ignored as the regression tests that keep the parser's
// error recovery bounded — each only spawns an independent `phoenix check`.

macro_rules! poisoned_fixture_checks {
    ($($test_name:ident => $fixture:literal),+ $(,)?) => {
        $(
            #[test]
            fn $test_name() {
                assert_rejected_with_diagnostic($fixture);
            }
        )+
    };
}

// All three parser error-recovery OOM triggers are RESOLVED as of 2026-06-12:
// each poisoned input now emits a bounded run of diagnostics and exits non-zero
// (~7 MB RSS, no rlimit-signal death), rather than ballooning to 662 MB+.
// Verified against the committed `tests/fixtures/poisoned/` inputs and against
// re-introducing each trigger into its full source fixture. Likely closed by the
// recent endpoint-parser error-recovery changes. These run un-ignored as the
// regression tests that keep the parser bounded.
poisoned_fixture_checks! {
    poisoned_keyword_field_rejected => "keyword_field.phx",
    poisoned_doc_comment_in_query_rejected => "doc_comment_in_query.phx",
    poisoned_response_projection_rejected => "response_projection.phx",
}
