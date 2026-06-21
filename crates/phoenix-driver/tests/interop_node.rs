//! Always-on Node tier for the `extern js` interop fixture family.
//!
//! Per fixture: build the program for a WASM target (emitting the paired
//! `.wasm` and `.js` glue), load the glue under Node with the fixture's JS host
//! stub, run, and assert captured stdout byte-for-byte against a baseline. This is the
//! gated counterpart to the unit/mechanism tests in `extern_js_glue.rs`: those
//! pin specific marshalling/error paths; this pins whole-program byte-identical
//! output for the canonical interop family (scalar round-trip, string in + out,
//! `JsValue` handle, closures-as-callbacks, host-side effects).
//!
//! **Gating** reuses the shared skip plumbing in [`common`]: skip with a visible
//! warning when `node` or the built wasm runtime is absent, hard-fail under
//! `PHOENIX_REQUIRE_NODE=1` / `PHOENIX_REQUIRE_RUNTIME_WASM=1` so CI can't
//! silently bypass the tier. `skip_if_no_runtime_wasm` probes the same search
//! paths (`$PHOENIX_RUNTIME_WASM` included) the `phoenix` binary itself uses, so
//! the skip decision can't disagree with the build it then runs.
//!
//! **Target-generic.** [`run_interop_fixture`] takes the WASM target, so the
//! WASM-GC binding (in PR 15) joins by calling it again with `"wasm32-gc"` and the
//! same fixtures + baselines — no restructuring. The fixtures live in
//! `tests/fixtures/interop/<name>/` (`main.phx` + `host.mjs` + `expected.txt`), a
//! directory tree like `tests/fixtures/multi/` so it sits outside the single-file
//! `fixture_inventory` claim check.

mod common;

use common::compiled_fixtures::{TempDir, phoenix_bin, workspace_root};
use common::{skip_if_no_node, skip_if_no_runtime_wasm};
use std::path::PathBuf;
use std::process::Command;

fn interop_fixtures_dir() -> PathBuf {
    workspace_root().join("tests/fixtures/interop")
}

/// The static Node driver. It lives next to the built glue + the copied host
/// stub, so every import is a sibling relative path. Phoenix `print` output
/// (routed through the glue's `writeStdout`) and any host-side `emit` both
/// accumulate into one buffer in call order, written out once at the end — so the
/// captured bytes pin the exact interleaving. `writeStdout`'s second arg is the
/// fd; it's intentionally ignored so *all* program output is captured.
const DRIVER_MJS: &str = r#"
import { readFile } from "node:fs/promises";
import { instantiate } from "./app.js";
import { host as makeHost } from "./host.mjs";

const wasm = await readFile(new URL("./app.wasm", import.meta.url));
let out = "";
const emit = (t) => { out += t; };
const { run } = await instantiate({
  wasm,
  host: makeHost({ emit }),
  writeStdout: (t, _fd) => { out += t; },
});
run();
process.stdout.write(out);
"#;

/// Build `tests/fixtures/interop/<name>/main.phx` for `target`, run it under Node
/// with that fixture's `host.mjs`, and assert stdout equals `expected.txt`.
fn run_interop_fixture(name: &str, target: &str) {
    let fdir = interop_fixtures_dir().join(name);
    let main = fdir.join("main.phx");
    let host = fdir.join("host.mjs");
    let expected = std::fs::read_to_string(fdir.join("expected.txt"))
        .unwrap_or_else(|e| panic!("reading expected.txt for interop fixture `{name}`: {e}"));

    let dir = TempDir::new(&format!("interop_{name}_{}", target.replace(['-'], "_")));
    let wasm = dir.join("app.wasm");

    let status = phoenix_bin()
        .args(["build", "--target", target])
        .arg(&main)
        .arg("-o")
        .arg(&wasm)
        .status()
        .unwrap_or_else(|e| panic!("spawning `phoenix build` for `{name}`: {e}"));
    assert!(
        status.success(),
        "`{target}` build of interop fixture `{name}` failed"
    );
    assert!(
        dir.join("app.js").exists(),
        "interop fixture `{name}` should produce a paired .js glue sidecar"
    );

    // The driver imports `./host.mjs` as a sibling, so copy the fixture's stub in.
    std::fs::copy(&host, dir.join("host.mjs"))
        .unwrap_or_else(|e| panic!("copying host.mjs for `{name}`: {e}"));
    let driver = dir.join("driver.mjs");
    std::fs::write(&driver, DRIVER_MJS).unwrap();

    let output = Command::new("node")
        .arg(&driver)
        .output()
        .unwrap_or_else(|e| panic!("running node for interop fixture `{name}`: {e}"));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "node run of interop fixture `{name}` ({target}) failed: {stderr}"
    );
    assert_eq!(
        stdout, expected,
        "interop fixture `{name}` ({target}) stdout did not match its baseline"
    );
}

/// One always-on Node test per interop fixture. Adding a fixture is: a
/// `tests/fixtures/interop/<name>/` directory plus a line here. The WASM target
/// is a parameter so PR 15 adds the WASM-GC column without restructuring.
macro_rules! interop_node_test {
    ($test_name:ident, $fixture:literal) => {
        #[test]
        fn $test_name() {
            if skip_if_no_runtime_wasm(stringify!($test_name)) {
                return;
            }
            if skip_if_no_node(stringify!($test_name)) {
                return;
            }
            run_interop_fixture($fixture, "wasm32-linear");
        }
    };
}

interop_node_test!(interop_scalars_round_trip, "scalars");
interop_node_test!(interop_strings_in_and_out, "strings");
interop_node_test!(interop_jsvalue_handle_round_trip, "jsvalue");
interop_node_test!(interop_closures_as_callbacks, "callbacks");
interop_node_test!(interop_host_side_effect_ordering, "host_effect");
