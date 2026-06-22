//! Browser / DOM verification tier for `extern js` interop.
//!
//! Verifies that DOM-mutating `extern js` host functions actually mutate the DOM
//! and that a Phoenix closure registered as a DOM event handler — retained by the
//! host past the call, pinned across the GC (decision G) — fires later and mutates
//! the DOM from inside the wasm callback. Two tiers run the *same* fixtures and
//! baselines (`tests/fixtures/interop/dom/<name>/`), over the *same* glue + host:
//!
//! - **jsdom smoke (always-on):** loads the glue against a jsdom `document` under
//!   Node. No real browser, so it runs wherever the npm deps are installed —
//!   `tests/interop-browser/` (`npm ci`). Covers the DOM-host marshalling and the
//!   retained-event-handler path at the API level.
//! - **Playwright tier (gated):** loads the page in real headless Chromium and
//!   dispatches a real click. Gated by `PHOENIX_REQUIRE_BROWSER=1`; soft-skips
//!   when no browser is launchable (the runner exits with a distinct marker), the
//!   wasmtime-gate shape.
//!
//! Both tiers reuse `phoenix build --target wasm32-linear` and are target-generic
//! in spirit (PR 15 adds the WASM-GC column). The Node interop fixtures live in
//! `tests/fixtures/interop/<name>/`; the DOM-only family lives under `.../dom/`
//! because its effects (DOM mutation, real clicks) only exist in a browser/jsdom
//! host — so it stays browser-tier rather than joining the five-backend matrix
//! (decision A0/I). CI provisioning (`npm ci` + `playwright install chromium`)
//! lands in PR 17.
//!
//! Build pipeline + temp-dir plumbing and the runtime/node skip gates are shared
//! with the other interop tiers via `common` — this file adds only the
//! browser-specific bits (DOM fixtures, npm-dep gate, the two `node` runners).

mod common;

use common::compiled_fixtures::{TempDir, phoenix_bin, workspace_root};
use common::{require, skip_if_no_node, skip_if_no_runtime_wasm};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn dom_fixtures_dir() -> PathBuf {
    workspace_root().join("tests/fixtures/interop/dom")
}

fn browser_harness_dir() -> PathBuf {
    workspace_root().join("tests/interop-browser")
}

// --- Gating ----------------------------------------------------------------

/// Soft-skip a browser tier when its npm dep isn't installed: `jsdom` for the
/// jsdom tier, `playwright-core` for the browser tier. They are not bundled, so a
/// fresh checkout must `npm ci` in `tests/interop-browser/` first. The gate is
/// per-package so each tier checks exactly what its runner imports — the always-on
/// jsdom smoke shouldn't skip just because `playwright-core` is absent, and vice
/// versa. Hard-fail under `PHOENIX_REQUIRE_BROWSER_DEPS=1` (which CI sets once PR 17
/// runs `npm ci`), so landing this PR doesn't break a CI that hasn't provisioned
/// the deps yet. Returns `true` when the caller should skip (the `skip_if_no_*`
/// convention shared with `common`).
#[must_use]
fn skip_if_no_browser_dep(test: &str, pkg: &str) -> bool {
    if browser_harness_dir()
        .join("node_modules")
        .join(pkg)
        .is_dir()
    {
        return false;
    }
    assert!(
        !require("PHOENIX_REQUIRE_BROWSER_DEPS"),
        "{test}: PHOENIX_REQUIRE_BROWSER_DEPS=1 but `{pkg}` is not installed — \
         run `npm ci` in tests/interop-browser"
    );
    eprintln!("skipping {test}: `{pkg}` not installed (run `npm ci` in tests/interop-browser)");
    true
}

/// Build `tests/fixtures/interop/dom/<fixture>/main.phx` for `target` into a temp
/// build dir and copy the fixture's `host.mjs` + `page.html` beside the emitted
/// `app.wasm` + `app.js`, so a runner can load everything as siblings. The build
/// dir is named after the (unique) test so the jsdom + browser tests for one
/// fixture — which run in parallel and share the same `target` — never collide.
/// Returns the build dir (kept alive by the caller) and the fixture's expected
/// output.
fn build_dom_fixture(test: &str, fixture: &str, target: &str) -> (TempDir, String) {
    let fdir = dom_fixtures_dir().join(fixture);
    let expected = std::fs::read_to_string(fdir.join("expected.txt"))
        .unwrap_or_else(|e| panic!("reading expected.txt for DOM fixture `{fixture}`: {e}"));

    let dir = TempDir::new(test);
    let wasm = dir.join("app.wasm");
    let status = phoenix_bin()
        .args(["build", "--target", target])
        .arg(fdir.join("main.phx"))
        .arg("-o")
        .arg(&wasm)
        .status()
        .unwrap_or_else(|e| panic!("spawning `phoenix build` for DOM fixture `{fixture}`: {e}"));
    assert!(
        status.success(),
        "`{target}` build of DOM fixture `{fixture}` failed"
    );
    assert!(
        dir.join("app.js").exists(),
        "DOM fixture `{fixture}` should produce a paired .js glue sidecar"
    );

    for f in ["host.mjs", "page.html"] {
        std::fs::copy(fdir.join(f), dir.join(f))
            .unwrap_or_else(|e| panic!("copying {f} for DOM fixture `{fixture}`: {e}"));
    }
    (dir, expected)
}

/// Run a harness runner (`jsdom-runner.mjs` / `playwright-runner.mjs`) against a
/// build dir. `node` resolves the runner's imports from the harness dir's
/// `node_modules`, so the runner path must point into `tests/interop-browser/`.
fn run_runner(runner: &str, build_dir: &Path) -> Output {
    Command::new("node")
        .arg(browser_harness_dir().join(runner))
        .arg(build_dir)
        .output()
        .unwrap_or_else(|e| panic!("spawning node for `{runner}`: {e}"))
}

/// jsdom tier: build the fixture, run it under jsdom, assert observed DOM output.
fn run_jsdom_dom_fixture(test: &str, fixture: &str) {
    if skip_if_no_runtime_wasm(test)
        || skip_if_no_node(test)
        || skip_if_no_browser_dep(test, "jsdom")
    {
        return;
    }
    let (dir, expected) = build_dom_fixture(test, fixture, "wasm32-linear");
    let out = run_runner("jsdom-runner.mjs", &dir);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "jsdom runner failed for DOM fixture `{fixture}`: {stderr}"
    );
    assert_eq!(
        stdout, expected,
        "DOM fixture `{fixture}` (jsdom) observed DOM did not match its baseline"
    );
}

/// `true` if the Playwright runner reported no launchable browser (its exit code 3
/// + `PHOENIX_BROWSER_UNAVAILABLE` marker), as opposed to a real failure.
fn browser_unavailable(out: &Output) -> bool {
    out.status.code() == Some(3)
        && String::from_utf8_lossy(&out.stderr).contains("PHOENIX_BROWSER_UNAVAILABLE")
}

/// Playwright tier: build the fixture, run it in real headless Chromium, assert
/// observed DOM output. Soft-skips when no browser is launchable unless
/// `PHOENIX_REQUIRE_BROWSER=1`.
fn run_browser_dom_fixture(test: &str, fixture: &str) {
    if skip_if_no_runtime_wasm(test)
        || skip_if_no_node(test)
        || skip_if_no_browser_dep(test, "playwright-core")
    {
        return;
    }
    let (dir, expected) = build_dom_fixture(test, fixture, "wasm32-linear");
    let out = run_runner("playwright-runner.mjs", &dir);

    if browser_unavailable(&out) {
        assert!(
            !require("PHOENIX_REQUIRE_BROWSER"),
            "{test}: PHOENIX_REQUIRE_BROWSER=1 but no headless browser is launchable — \
             run `npx playwright install chromium` (stderr: {})",
            String::from_utf8_lossy(&out.stderr)
        );
        eprintln!(
            "skipping {test}: no headless browser launchable (npx playwright install chromium)"
        );
        return;
    }

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "playwright runner failed for DOM fixture `{fixture}`: {stderr}"
    );
    assert_eq!(
        stdout, expected,
        "DOM fixture `{fixture}` (browser) observed DOM did not match its baseline"
    );
}

/// One jsdom test + one Playwright test per DOM fixture. Adding a fixture is a
/// `tests/fixtures/interop/dom/<name>/` directory plus a line here.
macro_rules! dom_fixture_tests {
    ($jsdom_fn:ident, $browser_fn:ident, $fixture:literal) => {
        #[test]
        fn $jsdom_fn() {
            run_jsdom_dom_fixture(stringify!($jsdom_fn), $fixture);
        }

        #[test]
        fn $browser_fn() {
            run_browser_dom_fixture(stringify!($browser_fn), $fixture);
        }
    };
}

dom_fixture_tests!(
    interop_dom_jsdom_set_text,
    interop_dom_browser_set_text,
    "set_text"
);
dom_fixture_tests!(
    interop_dom_jsdom_click_handler,
    interop_dom_browser_click_handler,
    "click_handler"
);
