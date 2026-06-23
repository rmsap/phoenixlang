//! Shared build-and-run plumbing for the `extern js` interop fixture family.
//!
//! Both the always-on Node tier ([`interop_node.rs`](../../interop_node.rs)) and
//! the five-backend matrix ([`interop_matrix.rs`](../../interop_matrix.rs)) need to
//! build `tests/fixtures/interop/<name>/main.phx` for a WASM target, load the
//! emitted glue under Node with that fixture's `host.mjs`, and read its raw
//! stdout. The Node tier asserts that stdout byte-for-byte against the baseline;
//! the matrix compares it against the *other four backends*. Keeping the build +
//! `node` invocation here means the Node driver (`DRIVER_MJS`) and the fixture
//! layout live in exactly one place, so the two tiers can't drift.

use super::compiled_fixtures::{TempDir, phoenix_bin, workspace_root};
use std::path::PathBuf;
use std::process::Command;

/// `tests/fixtures/interop/` — the directory tree holding one subdirectory per
/// interop fixture (`main.phx` + `host.mjs` + `expected.txt`). It sits below the
/// single-file fixtures so it stays outside the `fixture_inventory` claim scan.
pub fn interop_fixtures_dir() -> PathBuf {
    workspace_root().join("tests/fixtures/interop")
}

/// The fixture's `expected.txt` baseline (the exact stdout every backend must
/// reproduce), read verbatim including its trailing newline.
pub fn read_expected(fixture: &str) -> String {
    std::fs::read_to_string(interop_fixtures_dir().join(fixture).join("expected.txt"))
        .unwrap_or_else(|e| panic!("reading expected.txt for interop fixture `{fixture}`: {e}"))
}

/// The static Node driver. It lives next to the built glue + the copied host
/// stub, so every import is a sibling relative path. Phoenix `print` output
/// (routed through the glue's `writeStdout`) and any host-side `emit` both
/// accumulate into one buffer in call order, written out once at the end — so the
/// captured bytes pin the exact interleaving. `writeStdout`'s second arg is the
/// fd; it's intentionally ignored so *all* program output is captured.
pub const DRIVER_MJS: &str = r#"
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

/// Build `tests/fixtures/interop/<fixture>/main.phx` for `target` (emitting the
/// paired `.wasm` + `.js` glue), run it under Node with the fixture's `host.mjs`,
/// and return the captured raw stdout. `test` names the temp build dir so the
/// linear + gc runs of one fixture (which share a `target`-free portion) never
/// collide. Asserts a clean build + a clean Node exit; the *caller* decides what
/// the stdout must equal.
pub fn run_fixture_under_node(test: &str, fixture: &str, target: &str) -> String {
    let fdir = interop_fixtures_dir().join(fixture);
    let main = fdir.join("main.phx");
    let host = fdir.join("host.mjs");

    let dir = TempDir::new(test);
    let wasm = dir.join("app.wasm");

    let status = phoenix_bin()
        .args(["build", "--target", target])
        .arg(&main)
        .arg("-o")
        .arg(&wasm)
        .status()
        .unwrap_or_else(|e| panic!("spawning `phoenix build` for `{fixture}`: {e}"));
    assert!(
        status.success(),
        "`{target}` build of interop fixture `{fixture}` failed"
    );
    assert!(
        dir.join("app.js").exists(),
        "interop fixture `{fixture}` should produce a paired .js glue sidecar"
    );

    // The driver imports `./host.mjs` as a sibling, so copy the fixture's stub in.
    std::fs::copy(&host, dir.join("host.mjs"))
        .unwrap_or_else(|e| panic!("copying host.mjs for `{fixture}`: {e}"));
    let driver = dir.join("driver.mjs");
    std::fs::write(&driver, DRIVER_MJS).unwrap();

    let output = Command::new("node")
        .arg(&driver)
        .output()
        .unwrap_or_else(|e| panic!("running node for interop fixture `{fixture}`: {e}"));
    assert!(
        output.status.success(),
        "node run of interop fixture `{fixture}` ({target}) failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}
