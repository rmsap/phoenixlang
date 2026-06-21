//! `phoenix build --target wasm32-linear` emits a paired `.js` glue
//! sidecar for `extern js` programs, and the generated glue actually runs the
//! module under Node when a host is supplied.
//!
//! These Node tests are gated like the other host-dependent tiers: they skip
//! with a visible note when `node` (or the wasm runtime artifact) is absent, and
//! hard-fail under `PHOENIX_REQUIRE_NODE=1` so CI can't silently bypass them. As
//! of PR 8 they also cover closures-as-callbacks (synchronous + retained-across-
//! GC); the always-on per-fixture Node harness is PR 10.

mod common;

use common::compiled_fixtures::{TempDir, phoenix_bin};
use common::{skip_if_no_node, skip_if_no_runtime_wasm};
use std::path::PathBuf;
use std::process::Command;

/// `true` iff `wasm` has an export-section entry that is a *function* named
/// `name`. Parses the export section rather than scanning the raw bytes for the
/// substring (which can't tell an export name from an incidental byte match), so
/// the assertion can't pass on a coincidence.
fn module_exports_func(wasm: &[u8], name: &str) -> bool {
    use wasmparser::{ExternalKind, Parser, Payload};
    for payload in Parser::new(0).parse_all(wasm) {
        if let Ok(Payload::ExportSection(exports)) = payload {
            for export in exports {
                if let Ok(export) = export
                    && export.kind == ExternalKind::Func
                    && export.name == name
                {
                    return true;
                }
            }
        }
    }
    false
}

#[test]
fn extern_js_emits_paired_glue_and_runs_under_node() {
    if skip_if_no_runtime_wasm("extern_js_emits_paired_glue_and_runs_under_node") {
        return;
    }
    if skip_if_no_node("extern_js_emits_paired_glue_and_runs_under_node") {
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
    if skip_if_no_runtime_wasm("jsvalue_bool_and_float_round_trip_through_the_glue") {
        return;
    }
    if skip_if_no_node("jsvalue_bool_and_float_round_trip_through_the_glue") {
        return;
    }

    let dir = TempDir::new("roundtrip");
    let src = dir.join("app.phx");
    // Exercise the marshalling paths the Node smoke test above doesn't:
    // `JsValue` (opaque handle table — put on return, get on the next arg),
    // `Bool` (i32 0/1, both directions), and `Float` (f64, both directions).
    // `print` on wasm32-linear handles Int/Bool/String but not Float, so the
    // Float value is funneled back through an Int-returning extern before
    // printing; the Bool likewise round-trips through `boolToInt` here to keep
    // the baseline purely numeric. Each still drives its type across the
    // boundary both ways.
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
    if skip_if_no_runtime_wasm("instantiate_rejects_a_host_missing_a_required_binding") {
        return;
    }
    if skip_if_no_node("instantiate_rejects_a_host_missing_a_required_binding") {
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

/// A host function *returning* a `String` builds a GC-managed Phoenix string via
/// the exported `phx_string_alloc`. This pins the full
/// round-trip across three shapes: a *multi-byte* host string is printed (so a
/// byte-vs-char length mistake in the allocation would corrupt it), a host
/// string is *passed back* into a second extern (`lengthOf`) that reads it, and
/// an *empty* return exercises `phx_string_alloc(0)`.
#[test]
fn string_returning_extern_round_trips_through_the_glue() {
    if skip_if_no_runtime_wasm("string_returning_extern_round_trips_through_the_glue") {
        return;
    }
    if skip_if_no_node("string_returning_extern_round_trips_through_the_glue") {
        return;
    }

    let dir = TempDir::new("strret");
    let src = dir.join("app.phx");
    std::fs::write(
        &src,
        "extern js {\n  \
           function greet(name: String) -> String\n  \
           function lengthOf(s: String) -> Int\n  \
           function echo(s: String) -> String\n\
         }\n\
         function main() {\n  \
           print(greet(\"world\"))\n  \
           print(lengthOf(greet(\"hi\")))\n  \
           print(lengthOf(echo(\"\")))\n\
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

    // The module exports `phx_string_alloc` as a function (the glue's string-in
    // path calls it). Checked against the parsed export section, not a raw-byte
    // substring scan, so the assertion is exact.
    let wasm_bytes = std::fs::read(&wasm).expect("read built wasm");
    assert!(
        module_exports_func(&wasm_bytes, "phx_string_alloc"),
        "the module must export phx_string_alloc for the glue's string-in path"
    );

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
    greet: (name) => "Héllo, " + name,   // multi-byte return -> GC string
    lengthOf: (s) => s.length,           // reads a host-allocated string back
    echo: (s) => s,                      // identity: echo("") -> empty GC string
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
        stdout, "Héllo, world\n9\n0\n",
        "string-in round-trip: greet(\"world\")=\"Héllo, world\" (multi-byte, \
         13 UTF-8 bytes, printed back intact); lengthOf(greet(\"hi\"))=\
         len(\"Héllo, hi\")=9; lengthOf(echo(\"\"))=0 (empty return via \
         phx_string_alloc(0))"
    );
}

/// A Phoenix closure handed to a host as a callback (Phase 2.5 decision G)
/// crosses as its env pointer, is wrapped in a JS callable, and round-trips
/// through the exported `__phoenix_invoke_closure_*` trampoline. This is the
/// synchronous (drained-`setTimeout`) shape the always-on Node tier gates on:
/// the host invokes the callback *during* the extern call, so the closure is
/// also shadow-stack rooted — no GC subtlety. Exercises a no-arg callback and an
/// `(Int) -> Void` callback (the arg marshals host→wasm).
#[test]
fn callbacks_round_trip_synchronously_through_the_glue() {
    if skip_if_no_runtime_wasm("callbacks_round_trip_synchronously_through_the_glue") {
        return;
    }
    if skip_if_no_node("callbacks_round_trip_synchronously_through_the_glue") {
        return;
    }

    let dir = TempDir::new("callbacks_sync");
    let src = dir.join("app.phx");
    std::fs::write(
        &src,
        "extern js {\n  \
           function setTimeout(cb: () -> Void, ms: Int)\n  \
           function eachUpTo(n: Int, cb: (Int) -> Void)\n\
         }\n\
         function main() {\n  \
           setTimeout(function() { print(42) }, 0)\n  \
           eachUpTo(3, function(i: Int) { print(i) })\n\
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

    let bytes = std::fs::read(&wasm).unwrap();
    // The two distinct callback signatures each get an exported trampoline, and
    // the GC pin/unpin hooks are exported so the glue can root a retained
    // callback (here exercised only synchronously, but the export surface must
    // be present).
    assert!(
        module_exports_func(&bytes, "__phoenix_invoke_closure__to_v"),
        "the `() -> Void` callback trampoline should be exported"
    );
    assert!(
        module_exports_func(&bytes, "__phoenix_invoke_closure_i_to_v"),
        "the `(Int) -> Void` callback trampoline should be exported"
    );
    assert!(
        module_exports_func(&bytes, "phx_gc_pin") && module_exports_func(&bytes, "phx_gc_unpin"),
        "a callback-passing module must export the GC pin/unpin hooks"
    );

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
    // Drained setTimeout: invoke the Phoenix callback synchronously.
    setTimeout: (cb, ms) => { cb(); },
    eachUpTo: (n, cb) => { for (let i = 0; i < n; i++) cb(i); },
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
        stdout, "42\n0\n1\n2\n",
        "the no-arg callback fires once (42) and the (Int)->Void callback fires \
         per index (0,1,2 marshalled host→wasm)"
    );
}

/// A callback that *returns* a value to the host, exercising the wasm→host
/// result marshalling the synchronous test above doesn't reach. Two shapes:
///   * `(String) -> String` — the host's string argument is copied in
///     (host→wasm), and the closure's freshly-allocated string result crosses
///     back as a multi-value `[ptr, len]` the glue reads with `readString`
///     (`__r[0]`/`__r[1]`). This multi-value return path is the part most prone
///     to an off-by-one, so it's the highest-value coverage here.
///   * `(Int) -> Bool` — the closure's `Bool` result crosses back as an `i32`
///     the glue turns into a JS boolean (`__r !== 0`).
///
/// The Phoenix callbacks don't print; the driver collects what the host receives
/// from each invocation and appends it to stdout, so the assertion checks the
/// *returned* values, not just that the callback fired.
#[test]
fn callbacks_marshal_string_and_bool_results_back_to_the_host() {
    if skip_if_no_runtime_wasm("callbacks_marshal_string_and_bool_results_back_to_the_host") {
        return;
    }
    if skip_if_no_node("callbacks_marshal_string_and_bool_results_back_to_the_host") {
        return;
    }

    let dir = TempDir::new("callbacks_results");
    let src = dir.join("app.phx");
    std::fs::write(
        &src,
        "extern js {\n  \
           function mapEach(cb: (String) -> String)\n  \
           function keepBig(cb: (Int) -> Bool)\n\
         }\n\
         function main() {\n  \
           mapEach(function(s: String) -> String { return s + \"!\" })\n  \
           keepBig(function(n: Int) -> Bool { return n > 1 })\n\
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

    let bytes = std::fs::read(&wasm).unwrap();
    assert!(
        module_exports_func(&bytes, "__phoenix_invoke_closure_s_to_s"),
        "the `(String) -> String` callback trampoline should be exported"
    );
    assert!(
        module_exports_func(&bytes, "__phoenix_invoke_closure_i_to_b"),
        "the `(Int) -> Bool` callback trampoline should be exported"
    );

    let driver = dir.join("driver.mjs");
    std::fs::write(
        &driver,
        r#"
import { readFile } from "node:fs/promises";
import { instantiate } from "./app.js";
const wasm = await readFile(new URL("./app.wasm", import.meta.url));
let out = "";
const strings = [];
const bools = [];
const { run } = await instantiate({
  wasm,
  host: {
    // String arg copied in, string result read back out of linear memory.
    mapEach: (cb) => { strings.push(cb("a"), cb("bb")); },
    // Int arg marshalled in, Bool result marshalled back.
    keepBig: (cb) => { bools.push(cb(0), cb(1), cb(2)); },
  },
  writeStdout: (t) => { out += t; },
});
run();
// The callbacks print nothing; report what the host received from each.
process.stdout.write(out + strings.join(",") + "|" + bools.join(","));
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
        stdout, "a!,bb!|false,false,true",
        "the (String)->String callback appends `!` and the result crosses back \
         via the multi-value (ptr,len) return; the (Int)->Bool callback returns \
         `n > 1` marshalled to a JS boolean"
    );
}

/// The retained-callback path of decision G: a closure created inside a function
/// that *returns* (so its shadow-stack frame is gone) is kept alive across a
/// real GC purely by the host pin, then fires correctly, then is released.
///
/// **What this proves and what it doesn't.** `churn` allocates on the order of
/// tens of MiB of dead intermediate strings (O(n²) concatenation, ~80 MiB at
/// 4000 iterations) — many multiples of the 1 MiB `DEFAULT_COLLECTION_THRESHOLD`
/// — so the allocator runs many collection cycles between `register` and `fire`;
/// a GC firing is guaranteed by allocation volume, not merely likely. A broken
/// pin would let one of those cycles sweep the closure and recycle its slot, so
/// `fire()`'s dispatch through the closure's `env[0]` would read reused memory
/// and print garbage / a wrong value / trap — caught by the exact-stdout
/// assertion below. What this test deliberately does *not* do is assert the
/// firing count directly (no in-wasm collection counter is exported, by design);
/// the **mechanism** — a pin roots an object across a forced collection with no
/// frame — is proven deterministically by `gc_collects.rs`'s
/// `pinned_allocation_survives_collection_without_a_frame`. This test is the
/// end-to-end `phx_gc_pin` boundary check layered on top of that proof.
#[test]
fn a_retained_callback_survives_gc_and_is_released() {
    if skip_if_no_runtime_wasm("a_retained_callback_survives_gc_and_is_released") {
        return;
    }
    if skip_if_no_node("a_retained_callback_survives_gc_and_is_released") {
        return;
    }

    let dir = TempDir::new("callbacks_retained");
    let src = dir.join("app.phx");
    // `setup` creates the closure and hands it to the host, then returns —
    // popping the only frame that rooted it. `churn` then allocates ~80 MiB of
    // dead intermediate strings (O(n²) concatenation) — ~80× the 1 MiB
    // collection threshold, so many GC cycles run while the closure is reachable
    // *only* through the host pin. `fire` invokes the retained callback
    // afterwards; if the pin had failed, its slot would have been swept/reused
    // and `fire` would not print a clean `99`.
    std::fs::write(
        &src,
        "extern js {\n  \
           function register(cb: () -> Void)\n  \
           function fire()\n  \
           function release()\n\
         }\n\
         function setup() {\n  \
           register(function() { print(99) })\n\
         }\n\
         function churn() -> Int {\n  \
           let mut s = \"seed\"\n  \
           let mut i = 0\n  \
           while i < 4000 {\n    \
             s = s + \"abcdefghij\"\n    \
             i = i + 1\n  \
           }\n  \
           return i\n\
         }\n\
         function main() {\n  \
           setup()\n  \
           print(churn())\n  \
           fire()\n  \
           release()\n\
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

    let driver = dir.join("driver.mjs");
    std::fs::write(
        &driver,
        r#"
import { readFile } from "node:fs/promises";
import { instantiate } from "./app.js";
const wasm = await readFile(new URL("./app.wasm", import.meta.url));
let out = "";
let stored = null;
let released = false;
const { run, retainedCallbackCount } = await instantiate({
  wasm,
  host: {
    register: (cb) => { stored = cb; },        // retain the callback past the call
    fire: () => { stored(); },                  // invoke it after the GC
    release: () => { stored.release(); stored = null; released = true; },
  },
  writeStdout: (t) => { out += t; },
});
run();
// After main() runs release(), the explicit unpin must have dropped the entry.
process.stdout.write(out + "released=" + released + " held=" + retainedCallbackCount());
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
        stdout, "4000\n99\nreleased=true held=0",
        "the retained callback survives the forced GC (fires 99 after churn \
         prints 4000) and the host's explicit release() unpins it (held=0)"
    );
}

/// The `FinalizationRegistry` reclamation path of decision G: a host that drops
/// a retained callback **without** calling `release()` must still reclaim it
/// cleanly — the wrapper's finalizer fires, calls `__releaseCallback`, and that
/// unregisters the (now-firing) finalizer and unpins. This is the *dominant*
/// reclamation case (hosts rarely call `release()` explicitly), and it exercises
/// the unregister-on-finalize wiring that prevents a stale finalizer from later
/// clobbering a recycled env pointer.
///
/// The test *observes* reclamation rather than merely a clean exit: the glue's
/// `retainedCallbackCount()` is `1` while the host holds the wrapper and must
/// drop back to `0` after the wrapper is dropped and GC is driven. A broken
/// retention path (e.g. the map holding the wrapper strongly, starving the
/// `FinalizationRegistry`) would leave the count at `1` and fail the assertion —
/// a clean exit alone could not catch that. Runs Node with `--expose-gc`.
#[test]
fn a_dropped_callback_is_reclaimed_through_the_finalizer_without_release() {
    if skip_if_no_runtime_wasm(
        "a_dropped_callback_is_reclaimed_through_the_finalizer_without_release",
    ) {
        return;
    }
    if skip_if_no_node("a_dropped_callback_is_reclaimed_through_the_finalizer_without_release") {
        return;
    }

    let dir = TempDir::new("callbacks_finalizer");
    let src = dir.join("app.phx");
    std::fs::write(
        &src,
        "extern js {\n  \
           function register(cb: () -> Void)\n\
         }\n\
         function main() {\n  \
           register(function() { print(7) })\n\
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

    let driver = dir.join("driver.mjs");
    std::fs::write(
        &driver,
        r#"
import { readFile } from "node:fs/promises";
import { instantiate } from "./app.js";
const wasm = await readFile(new URL("./app.wasm", import.meta.url));
let out = "";
let stored = null;
const { run, retainedCallbackCount } = await instantiate({
  wasm,
  host: {
    register: (cb) => { cb(); stored = cb; },   // invoke, then retain the wrapper
  },
  writeStdout: (t) => { out += t; },
});
run();
// One callback is retained (pinned) while the host holds the wrapper.
const heldBefore = retainedCallbackCount();
// Drop the wrapper WITHOUT calling release(): reclamation must flow through the
// FinalizationRegistry. Force GC and drain tasks so the finalizer runs here.
stored = null;
if (typeof global.gc === "function") {
  for (let i = 0; i < 20; i++) {
    global.gc();
    await new Promise((r) => setTimeout(r, 0));
  }
}
// After the finalizer runs the retained-callback count must return to zero —
// this observes the unpin, not merely a clean exit.
const heldAfter = retainedCallbackCount();
process.stdout.write(out + "held=" + heldBefore + "," + heldAfter);
"#,
    )
    .unwrap();

    let output = Command::new("node")
        .arg("--expose-gc")
        .arg(&driver)
        .output()
        .expect("failed to run node");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(output.status.success(), "node run failed: {stderr}");
    assert_eq!(
        stdout, "7\nheld=1,0",
        "the callback fires during the call (7), is retained while the host holds \
         the wrapper (held=1), and dropping the wrapper drives the finalizer to \
         unpin it (held=0) — a strong-map regression would leave held at 1"
    );
}

/// An `Int`-returning host function that hands back a non-numeric value
/// (`undefined`, an object, `NaN`) must surface a clear glue error rather than an
/// opaque `RangeError` from `BigInt(NaN)`. Pins the `Number.isFinite` guard in
/// `outbound_return`'s `I64` arm end-to-end.
#[test]
fn int_returning_host_returning_non_numeric_throws_a_clear_error() {
    if skip_if_no_runtime_wasm("int_returning_host_returning_non_numeric_throws_a_clear_error") {
        return;
    }
    if skip_if_no_node("int_returning_host_returning_non_numeric_throws_a_clear_error") {
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
    if skip_if_no_runtime_wasm("extern_free_program_emits_no_js_sidecar") {
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
    if skip_if_no_runtime_wasm("rebuild_without_externs_removes_a_stale_generated_sidecar") {
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
    if skip_if_no_runtime_wasm("a_hand_written_js_beside_the_wasm_is_never_clobbered") {
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
    if skip_if_no_runtime_wasm("extern_build_overwrites_an_unmarked_js_with_a_warning") {
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
    if skip_if_no_runtime_wasm("non_wasm_output_extension_appends_js_for_the_sidecar") {
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
