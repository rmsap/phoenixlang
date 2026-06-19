//! `phoenix build --target wasm32-linear` emits a paired `.js` glue
//! sidecar for `extern js` programs, and the generated glue actually runs the
//! module under Node when a host is supplied.
//!
//! The Node smoke test is gated like the other host-dependent tiers: it skips
//! with a visible note when `node` (or the wasm runtime artifact) is absent, and
//! hard-fails under `PHOENIX_REQUIRE_NODE=1` so CI can't silently bypass it. The
//! full per-fixture Node harness is PR 8.

use std::path::{Path, PathBuf};
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

/// The wasm32-linear backend embeds-and-merges this artifact; without it a build
/// can't produce a `.wasm`, so the glue tests have nothing to exercise.
fn runtime_wasm_present() -> bool {
    workspace_root()
        .join("target/wasm32-wasip1/release/phoenix_runtime.wasm")
        .exists()
}

fn require_runtime_wasm_or_skip(test: &str) -> bool {
    if runtime_wasm_present() {
        return true;
    }
    if std::env::var("PHOENIX_REQUIRE_RUNTIME_WASM").is_ok() {
        panic!("{test}: PHOENIX_REQUIRE_RUNTIME_WASM=1 but phoenix_runtime.wasm is not built");
    }
    eprintln!(
        "skipping {test}: phoenix_runtime.wasm not built (cargo build -p phoenix-runtime --target wasm32-wasip1 --release)"
    );
    false
}

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn require_node_or_skip(test: &str) -> bool {
    if node_available() {
        return true;
    }
    if std::env::var("PHOENIX_REQUIRE_NODE").is_ok() {
        panic!("{test}: PHOENIX_REQUIRE_NODE=1 but `node` is not on PATH");
    }
    eprintln!("skipping {test}: `node` not on PATH");
    false
}

/// A temp directory that removes itself on drop, so a failing assertion unwinds
/// without leaking it (the manual cleanup each test used to run at its end is
/// skipped on panic). `Deref<Target = Path>` lets callers keep using
/// `dir.join(...)` as if it were a `PathBuf`.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> TempDir {
        let path =
            std::env::temp_dir().join(format!("phoenix_glue_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn extern_js_emits_paired_glue_and_runs_under_node() {
    if !require_runtime_wasm_or_skip("extern_js_emits_paired_glue_and_runs_under_node") {
        return;
    }
    if !require_node_or_skip("extern_js_emits_paired_glue_and_runs_under_node") {
        return;
    }

    let dir = TempDir::new("run");
    let src = dir.join("app.phx");
    std::fs::write(
        &src,
        "extern js {\n  \
           function alert(message: String)\n  \
           function getLength(s: String) -> Int\n\
         }\n\
         function main() {\n  \
           alert(\"hi\")\n  \
           print(getLength(\"abcd\"))\n\
         }\n",
    )
    .unwrap();
    let wasm = dir.join("app.wasm");

    let status = phoenix_bin()
        .args(["build", "--target", "wasm32-linear"])
        .arg(&src)
        .arg("-o")
        .arg(&wasm)
        .status()
        .expect("failed to run phoenix build");
    assert!(status.success(), "wasm32-linear build failed");

    let js = dir.join("app.js");
    assert!(wasm.exists(), "the .wasm artifact should exist");
    assert!(
        js.exists(),
        "a paired .js glue sidecar should exist next to the .wasm"
    );

    // Node driver: import the glue (relative, so no path escaping), instantiate
    // with host stubs, run, and print what the program produced. `alert` appends
    // a sentinel; `getLength("abcd")` returns 4 (marshalled back through i64);
    // `print` routes through WASI fd_write into `writeStdout`.
    let driver = dir.join("driver.mjs");
    std::fs::write(
        &driver,
        r#"
import { readFile } from "node:fs/promises";
import { instantiate } from "./app.js";
const wasm = await readFile(new URL("./app.wasm", import.meta.url));
let out = "";
const { run } = await instantiate({
  wasm,
  host: {
    alert: (m) => { out += "A:" + m + "\n"; },
    getLength: (s) => s.length,
  },
  writeStdout: (t) => { out += t; },
});
run();
process.stdout.write(out);
"#,
    )
    .unwrap();

    let output = Command::new("node")
        .arg(&driver)
        .output()
        .expect("failed to run node");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(output.status.success(), "node run failed: {stderr}");
    assert_eq!(
        stdout, "A:hi\n4\n",
        "the glue should drive the program end-to-end (host call + Int round-trip + print)"
    );
}

#[test]
fn jsvalue_bool_and_float_round_trip_through_the_glue() {
    if !require_runtime_wasm_or_skip("jsvalue_bool_and_float_round_trip_through_the_glue") {
        return;
    }
    if !require_node_or_skip("jsvalue_bool_and_float_round_trip_through_the_glue") {
        return;
    }

    let dir = TempDir::new("roundtrip");
    let src = dir.join("app.phx");
    // Exercise the marshalling paths the Node smoke test above doesn't:
    // `JsValue` (opaque handle table — put on return, get on the next arg),
    // `Bool` (i32 0/1, both directions), and `Float` (f64, both directions).
    // `print` on wasm32-linear only handles Int/String today, so the Bool and
    // Float values are funneled back through Int-returning externs before
    // printing — that still drives a Bool/Float across the boundary each way.
    std::fs::write(
        &src,
        "extern js {\n  \
           function makeBox(n: Int) -> JsValue\n  \
           function unbox(b: JsValue) -> Int\n  \
           function negate(b: Bool) -> Bool\n  \
           function boolToInt(b: Bool) -> Int\n  \
           function halve(x: Float) -> Float\n  \
           function floorToInt(x: Float) -> Int\n\
         }\n\
         function main() {\n  \
           print(unbox(makeBox(7)))\n  \
           print(boolToInt(negate(true)))\n  \
           print(floorToInt(halve(3.0)))\n\
         }\n",
    )
    .unwrap();
    let wasm = dir.join("app.wasm");

    let status = phoenix_bin()
        .args(["build", "--target", "wasm32-linear"])
        .arg(&src)
        .arg("-o")
        .arg(&wasm)
        .status()
        .expect("failed to run phoenix build");
    assert!(status.success(), "wasm32-linear build failed");
    assert!(dir.join("app.js").exists(), "glue sidecar should exist");

    let driver = dir.join("driver.mjs");
    std::fs::write(
        &driver,
        r#"
import { readFile } from "node:fs/promises";
import { instantiate } from "./app.js";
const wasm = await readFile(new URL("./app.wasm", import.meta.url));
let out = "";
const { run } = await instantiate({
  wasm,
  host: {
    makeBox: (n) => ({ v: n }),  // returns a JS object -> opaque handle
    unbox: (b) => b.v,           // receives the same object back via the handle
    negate: (b) => !b,
    boolToInt: (b) => (b ? 1 : 0),
    halve: (x) => x / 2,
    floorToInt: (x) => Math.floor(x),
  },
  writeStdout: (t) => { out += t; },
});
run();
process.stdout.write(out);
"#,
    )
    .unwrap();

    let output = Command::new("node")
        .arg(&driver)
        .output()
        .expect("failed to run node");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(output.status.success(), "node run failed: {stderr}");
    assert_eq!(
        stdout, "7\n0\n1\n",
        "JsValue handle round-trip (7); Bool (negate(true)=false -> 0); \
         Float (halve(3.0)=1.5 -> floor = 1)"
    );
}

/// The glue fails fast at `instantiate` when the caller's `host` omits a
/// function some real thunk needs, naming the missing binding — rather than
/// deferring to a cryptic `host.<name> is not a function` partway through a run.
#[test]
fn instantiate_rejects_a_host_missing_a_required_binding() {
    if !require_runtime_wasm_or_skip("instantiate_rejects_a_host_missing_a_required_binding") {
        return;
    }
    if !require_node_or_skip("instantiate_rejects_a_host_missing_a_required_binding") {
        return;
    }

    let dir = TempDir::new("missinghost");
    let src = dir.join("app.phx");
    std::fs::write(
        &src,
        "extern js {\n  \
           function alert(message: String)\n\
         }\n\
         function main() {\n  \
           alert(\"hi\")\n\
         }\n",
    )
    .unwrap();
    let wasm = dir.join("app.wasm");

    let status = phoenix_bin()
        .args(["build", "--target", "wasm32-linear"])
        .arg(&src)
        .arg("-o")
        .arg(&wasm)
        .status()
        .expect("failed to run phoenix build");
    assert!(status.success(), "wasm32-linear build failed");
    assert!(dir.join("app.js").exists(), "glue sidecar should exist");

    // Driver instantiates with an empty `host` (no `alert`). The guard must
    // throw at instantiate time, before `run`, naming the missing binding.
    let driver = dir.join("driver.mjs");
    std::fs::write(
        &driver,
        r#"
import { readFile } from "node:fs/promises";
import { instantiate } from "./app.js";
const wasm = await readFile(new URL("./app.wasm", import.meta.url));
try {
  await instantiate({ wasm, host: {} });
  process.stdout.write("NO_THROW");
} catch (e) {
  process.stdout.write("THREW:" + e.message);
}
"#,
    )
    .unwrap();

    let output = Command::new("node")
        .arg(&driver)
        .output()
        .expect("failed to run node");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(output.status.success(), "node run failed: {stderr}");
    assert!(
        stdout.starts_with("THREW:") && stdout.contains("alert"),
        "instantiate should throw at instantiate time naming the missing `alert` \
         host binding, got: {stdout}"
    );
}

#[test]
fn string_returning_extern_throws_a_clear_error_at_runtime() {
    if !require_runtime_wasm_or_skip("string_returning_extern_throws_a_clear_error_at_runtime") {
        return;
    }
    if !require_node_or_skip("string_returning_extern_throws_a_clear_error_at_runtime") {
        return;
    }

    // A host function *returning* a String needs `phx_string_alloc`, deferred to
    // PR 7. The build must still succeed (the glue emits a throwing thunk so the
    // import stays satisfied); only an actual *call* fails, with a message that
    // names the missing piece. This pins that runtime behaviour end-to-end.
    let dir = TempDir::new("strret");
    let src = dir.join("app.phx");
    std::fs::write(
        &src,
        "extern js {\n  \
           function getText() -> String\n\
         }\n\
         function main() {\n  \
           print(getText())\n\
         }\n",
    )
    .unwrap();
    let wasm = dir.join("app.wasm");

    let status = phoenix_bin()
        .args(["build", "--target", "wasm32-linear"])
        .arg(&src)
        .arg("-o")
        .arg(&wasm)
        .status()
        .expect("failed to run phoenix build");
    assert!(
        status.success(),
        "the build must succeed; only the runtime call is deferred"
    );
    assert!(dir.join("app.js").exists(), "glue sidecar should exist");

    let driver = dir.join("driver.mjs");
    std::fs::write(
        &driver,
        r#"
import { readFile } from "node:fs/promises";
import { instantiate } from "./app.js";
const wasm = await readFile(new URL("./app.wasm", import.meta.url));
const { run } = await instantiate({ wasm, host: { getText: () => "nope" } });
try {
  run();
  process.stdout.write("NO_THROW");
} catch (e) {
  process.stdout.write("THREW:" + e.message);
}
"#,
    )
    .unwrap();

    let output = Command::new("node")
        .arg(&driver)
        .output()
        .expect("failed to run node");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(output.status.success(), "node run failed: {stderr}");
    assert!(
        stdout.starts_with("THREW:") && stdout.contains("phx_string_alloc"),
        "calling the deferred String-returning extern should throw a clear error \
         naming phx_string_alloc, got: {stdout}"
    );
}

/// An `Int`-returning host function that hands back a non-numeric value
/// (`undefined`, an object, `NaN`) must surface a clear glue error rather than an
/// opaque `RangeError` from `BigInt(NaN)`. Pins the `Number.isFinite` guard in
/// `outbound_return`'s `I64` arm end-to-end.
#[test]
fn int_returning_host_returning_non_numeric_throws_a_clear_error() {
    if !require_runtime_wasm_or_skip(
        "int_returning_host_returning_non_numeric_throws_a_clear_error",
    ) {
        return;
    }
    if !require_node_or_skip("int_returning_host_returning_non_numeric_throws_a_clear_error") {
        return;
    }

    let dir = TempDir::new("nonnumeric");
    let src = dir.join("app.phx");
    std::fs::write(
        &src,
        "extern js {\n  \
           function bad() -> Int\n\
         }\n\
         function main() {\n  \
           print(bad())\n\
         }\n",
    )
    .unwrap();
    let wasm = dir.join("app.wasm");

    let status = phoenix_bin()
        .args(["build", "--target", "wasm32-linear"])
        .arg(&src)
        .arg("-o")
        .arg(&wasm)
        .status()
        .expect("failed to run phoenix build");
    assert!(status.success(), "wasm32-linear build failed");
    assert!(dir.join("app.js").exists(), "glue sidecar should exist");

    // Host returns `undefined` for an `Int`-returning extern; the glue's
    // finiteness guard must throw a clear error naming the cause before
    // `BigInt(Math.trunc(NaN))` would throw an opaque `RangeError`.
    let driver = dir.join("driver.mjs");
    std::fs::write(
        &driver,
        r#"
import { readFile } from "node:fs/promises";
import { instantiate } from "./app.js";
const wasm = await readFile(new URL("./app.wasm", import.meta.url));
const { run } = await instantiate({ wasm, host: { bad: () => undefined } });
try {
  run();
  process.stdout.write("NO_THROW");
} catch (e) {
  process.stdout.write("THREW:" + e.message);
}
"#,
    )
    .unwrap();

    let output = Command::new("node")
        .arg(&driver)
        .output()
        .expect("failed to run node");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(output.status.success(), "node run failed: {stderr}");
    assert!(
        stdout.starts_with("THREW:") && stdout.contains("non-numeric"),
        "an Int-returning host returning a non-numeric value should throw a clear \
         glue error, got: {stdout}"
    );
}

#[test]
fn extern_free_program_emits_no_js_sidecar() {
    if !require_runtime_wasm_or_skip("extern_free_program_emits_no_js_sidecar") {
        return;
    }
    let dir = TempDir::new("nosidecar");
    let src = dir.join("app.phx");
    std::fs::write(&src, "function main() { print(42) }\n").unwrap();
    let wasm = dir.join("app.wasm");

    let status = phoenix_bin()
        .args(["build", "--target", "wasm32-linear"])
        .arg(&src)
        .arg("-o")
        .arg(&wasm)
        .status()
        .expect("failed to run phoenix build");
    assert!(status.success(), "wasm32-linear build failed");

    assert!(wasm.exists(), "the .wasm artifact should exist");
    assert!(
        !dir.join("app.js").exists(),
        "an extern-free program must not emit a .js glue sidecar"
    );
}

/// Rebuilding the same artifact from an extern-free program removes a now-stale,
/// generator-owned sidecar — otherwise a consumer could import glue for imports
/// the module no longer declares. None of this needs Node: it's pure driver
/// file management, so it only gates on the runtime artifact.
#[test]
fn rebuild_without_externs_removes_a_stale_generated_sidecar() {
    if !require_runtime_wasm_or_skip("rebuild_without_externs_removes_a_stale_generated_sidecar") {
        return;
    }
    let dir = TempDir::new("stale");
    let src = dir.join("app.phx");
    let wasm = dir.join("app.wasm");
    let js = dir.join("app.js");

    // First build: an extern-using program emits the paired glue sidecar.
    std::fs::write(
        &src,
        "extern js {\n  \
           function alert(message: String)\n\
         }\n\
         function main() {\n  \
           alert(\"hi\")\n\
         }\n",
    )
    .unwrap();
    let build = |src: &PathBuf| {
        phoenix_bin()
            .args(["build", "--target", "wasm32-linear"])
            .arg(src)
            .arg("-o")
            .arg(&wasm)
            .status()
            .expect("failed to run phoenix build")
    };
    assert!(build(&src).success(), "extern build failed");
    assert!(js.exists(), "the extern build should emit the glue sidecar");

    // Rebuild the same artifact from an extern-free program: the stale,
    // generator-owned sidecar must be removed.
    std::fs::write(&src, "function main() { print(1) }\n").unwrap();
    assert!(build(&src).success(), "extern-free rebuild failed");
    assert!(
        !js.exists(),
        "the stale generated sidecar should be removed on an extern-free rebuild"
    );
}

/// A user-authored `.js` beside the `.wasm` (one *without* the generated-code
/// marker) is never clobbered by an extern-free build — the marker guard is the
/// whole point, so pin that it survives byte-for-byte.
#[test]
fn a_hand_written_js_beside_the_wasm_is_never_clobbered() {
    if !require_runtime_wasm_or_skip("a_hand_written_js_beside_the_wasm_is_never_clobbered") {
        return;
    }
    let dir = TempDir::new("handwritten");
    let src = dir.join("app.phx");
    let wasm = dir.join("app.wasm");
    let js = dir.join("app.js");

    std::fs::write(&src, "function main() { print(1) }\n").unwrap();
    let hand_written = "// my own glue, keep me\nexport const answer = 42;\n";
    std::fs::write(&js, hand_written).unwrap();

    let status = phoenix_bin()
        .args(["build", "--target", "wasm32-linear"])
        .arg(&src)
        .arg("-o")
        .arg(&wasm)
        .status()
        .expect("failed to run phoenix build");
    assert!(status.success(), "extern-free build failed");

    assert!(js.exists(), "the hand-written sidecar must not be removed");
    assert_eq!(
        std::fs::read_to_string(&js).unwrap(),
        hand_written,
        "a hand-written sidecar (no generated marker) must survive untouched"
    );
}

/// The mirror of the no-clobber case above: an *extern-using* build has nowhere
/// else to put its glue, so it must overwrite a hand-written `.js` (one lacking
/// the generated-code marker) — but only after warning, so the user who kept a
/// `.js` and then added an `extern` block doesn't lose it silently. Pin both the
/// overwrite and the warning.
#[test]
fn extern_build_overwrites_an_unmarked_js_with_a_warning() {
    if !require_runtime_wasm_or_skip("extern_build_overwrites_an_unmarked_js_with_a_warning") {
        return;
    }
    let dir = TempDir::new("clobberwarn");
    let src = dir.join("app.phx");
    let wasm = dir.join("app.wasm");
    let js = dir.join("app.js");

    // A hand-written sidecar (no generated marker) already sits beside the
    // soon-to-be-built artifact.
    let hand_written = "// my own glue, keep me\nexport const answer = 42;\n";
    std::fs::write(&js, hand_written).unwrap();

    // The program now uses `extern js`, so the build must emit glue at this exact
    // path, replacing the hand-written file.
    std::fs::write(
        &src,
        "extern js {\n  \
           function alert(message: String)\n\
         }\n\
         function main() {\n  \
           alert(\"hi\")\n\
         }\n",
    )
    .unwrap();

    let output = phoenix_bin()
        .args(["build", "--target", "wasm32-linear"])
        .arg(&src)
        .arg("-o")
        .arg(&wasm)
        .output()
        .expect("failed to run phoenix build");
    assert!(output.status.success(), "wasm32-linear build failed");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The hand-written file is replaced by generated glue (now carrying the
    // marker), and the build warned about the clobber before doing it.
    let now = std::fs::read_to_string(&js).unwrap();
    assert_ne!(now, hand_written, "the extern build must overwrite the .js");
    assert!(
        now.starts_with("// Code-generated by Phoenix"),
        "the overwritten sidecar should be the generated glue, got: {now}"
    );
    assert!(
        stderr.contains("warning: overwriting") && stderr.contains("hand-written"),
        "the extern build should warn before clobbering an unmarked .js, got: {stderr}"
    );
}

/// When the output artifact has a non-`.wasm` extension (`-o app.foo`), the glue
/// sidecar appends `.js` (→ `app.foo.js`) rather than swapping the extension, so
/// the sidecar always sits beside the artifact with its stem intact.
#[test]
fn non_wasm_output_extension_appends_js_for_the_sidecar() {
    if !require_runtime_wasm_or_skip("non_wasm_output_extension_appends_js_for_the_sidecar") {
        return;
    }
    let dir = TempDir::new("ext");
    let src = dir.join("app.phx");
    let artifact = dir.join("app.foo");

    std::fs::write(
        &src,
        "extern js {\n  \
           function alert(message: String)\n\
         }\n\
         function main() {\n  \
           alert(\"hi\")\n\
         }\n",
    )
    .unwrap();

    let status = phoenix_bin()
        .args(["build", "--target", "wasm32-linear"])
        .arg(&src)
        .arg("-o")
        .arg(&artifact)
        .status()
        .expect("failed to run phoenix build");
    assert!(status.success(), "wasm32-linear build failed");

    assert!(artifact.exists(), "the artifact should exist");
    assert!(
        dir.join("app.foo.js").exists(),
        "a non-.wasm artifact gets `.js` appended (stem intact), not its extension swapped"
    );
    assert!(
        !dir.join("app.js").exists(),
        "the `.wasm`->`.js` swap must not apply to a non-.wasm artifact"
    );
}
