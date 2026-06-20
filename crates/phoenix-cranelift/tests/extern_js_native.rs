//! Native (Cranelift) `extern js` host-FFI binding (Phase 2.5 decision A0/E/G).
//!
//! The native backend lowers each `Op::ExternCall` to a call of the C-ABI symbol
//! `phx_extern_<module>__<name>` and emits a **weak** default body that aborts
//! via `phx_extern_unbound`. A linked **host shim** provides strong definitions
//! that override the defaults (strong-beats-weak, independent of link order). A
//! Phoenix closure handed to a host crosses as its env pointer and is invoked
//! through the exported `phx_invoke_closure_<sig>` trampoline.
//!
//! These tests compile the Phoenix program to an object, compile a small C host
//! shim, link them together with the runtime archive, and assert the program's
//! stdout — the native column of the five-backend interop matrix (PR 16). They
//! need `cc` and the built runtime static lib (the same provisioning every
//! native compile test relies on: `cargo build -p phoenix-runtime`).
//!
//! **Windows is out of scope.** The native binding relies on weak-symbol
//! override via PLT interposition — the ELF/Mach-O model. On Windows/COFF the
//! strong-beats-weak override is not guaranteed (see
//! `docs/known-issues.md#native-extern-js-interop-is-elfmach-o-only-no-windowscoff-weak-override`),
//! so the whole file is compiled out there rather than asserting behavior the
//! platform doesn't support.
#![cfg(not(target_os = "windows"))]

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use phoenix_cranelift::link_executable_with_objects;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("phoenix_extern_native_tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn unique(prefix: &str, ext: &str) -> PathBuf {
    let id = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    scratch_dir().join(format!("{prefix}_{id}_{n}.{ext}"))
}

/// Compile a C host shim to an object file with the system `cc`.
fn compile_c_shim(c_src: &str, label: &str) -> PathBuf {
    let c_path = unique(label, "c");
    let o_path = unique(label, "o");
    std::fs::write(&c_path, c_src).unwrap();
    let status = Command::new("cc")
        .arg("-c")
        .arg(&c_path)
        .arg("-o")
        .arg(&o_path)
        .status()
        .expect("failed to spawn cc to compile the host shim");
    assert!(status.success(), "cc failed to compile the host shim");
    let _ = std::fs::remove_file(&c_path);
    o_path
}

/// Compile the Phoenix program, link it with the given host-shim objects + the
/// runtime, run it, and return captured stdout lines. Asserts a clean exit.
fn run_with_shim(phoenix_src: &str, shim_objs: &[PathBuf]) -> Vec<String> {
    let obj_bytes = common::compile_to_obj(phoenix_src);
    let obj_path = unique("app", "o");
    let exe_path = unique("app", "exe");
    std::fs::write(&obj_path, &obj_bytes).unwrap();

    link_executable_with_objects(&obj_path, &exe_path, shim_objs)
        .expect("linking the program with its host shim failed");

    let output = Command::new(&exe_path)
        .output()
        .expect("could not run the linked interop binary");

    // Clean up scratch artifacts before asserting, so a failing assertion
    // doesn't leak them into the temp dir across reruns.
    let _ = std::fs::remove_file(&obj_path);
    let _ = std::fs::remove_file(&exe_path);
    for o in shim_objs {
        let _ = std::fs::remove_file(o);
    }

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "interop binary exited with {}: stderr={stderr}",
        output.status
    );

    stdout.lines().map(str::to_string).collect()
}

/// A host shim covering every native marshalling path the round-trip test
/// exercises: scalar (`Int`), `Bool` (the `i8` native code), `Float` (`f64`),
/// `String` in *and* out (out via `phx_string_alloc`, the same GC-string builder
/// the WASM glue uses), `JsValue` as an opaque `i64` host handle, and closures
/// invoked through the exported trampolines — both a no-result `(Int) -> Void`
/// callback and a value-returning `(Int) -> Int` callback (exercising the
/// trampoline's return path and proving two distinct trampolines coexist).
const ROUND_TRIP_SHIM: &str = r#"
#include <stdint.h>
#include <stddef.h>

// Runtime export: allocate a GC-managed Phoenix string payload.
extern char *phx_string_alloc(size_t n);
// Compiler-exported trampolines: a `(Int) -> Void` and a `(Int) -> Int` callback.
extern void phx_invoke_closure_i_to_v(void *env, int64_t n);
extern int64_t phx_invoke_closure_i_to_i(void *env, int64_t n);

int64_t phx_extern_js__addOne(int64_t n) { return n + 1; }

// `Bool` crosses as the native `i8` code; flip it.
int8_t phx_extern_js__negate(int8_t b) { return b ? 0 : 1; }

// `Float` crosses as `f64`; halve it.
double phx_extern_js__halve(double x) { return x / 2.0; }

// A Phoenix `String` is a `(ptr, len)` fat pointer: two i64s in / two i64s out.
struct PhxStr { const char *ptr; int64_t len; };
struct PhxStr phx_extern_js__shout(const char *ptr, int64_t len) {
  char *out = phx_string_alloc((size_t)len);
  for (int64_t i = 0; i < len; i++) {
    char c = ptr[i];
    if (c >= 'a' && c <= 'z') c = (char)(c - 'a' + 'A');
    out[i] = c;
  }
  struct PhxStr r = { out, len };
  return r;
}

// `JsValue` crosses as an opaque i64 handle the host owns; here the box is the
// value itself, so unbox(makeBox(n)) == n.
int64_t phx_extern_js__makeBox(int64_t n) { return n; }
int64_t phx_extern_js__unbox(int64_t handle) { return handle; }

// A drained callback host: invoke the Phoenix closure synchronously per index.
void phx_extern_js__eachUpTo(int64_t n, void *cb) {
  for (int64_t i = 0; i < n; i++) phx_invoke_closure_i_to_v(cb, i);
}

// A value-returning callback host: sum cb(i) for i in [0, n) — exercises the
// trampoline's return path (a `(Int) -> Int` closure handed across).
int64_t phx_extern_js__sumMap(int64_t n, void *cb) {
  int64_t acc = 0;
  for (int64_t i = 0; i < n; i++) acc += phx_invoke_closure_i_to_i(cb, i);
  return acc;
}
"#;

const ROUND_TRIP_PROGRAM: &str = "extern js {\n  \
       function addOne(n: Int) -> Int\n  \
       function negate(b: Bool) -> Bool\n  \
       function halve(x: Float) -> Float\n  \
       function shout(s: String) -> String\n  \
       function makeBox(n: Int) -> JsValue\n  \
       function unbox(b: JsValue) -> Int\n  \
       function eachUpTo(n: Int, cb: (Int) -> Void)\n  \
       function sumMap(n: Int, cb: (Int) -> Int) -> Int\n\
     }\n\
     function main() {\n  \
       print(addOne(41))\n  \
       print(negate(true))\n  \
       print(halve(3.0))\n  \
       print(shout(\"hi\"))\n  \
       print(unbox(makeBox(7)))\n  \
       print(sumMap(4, function(i: Int) -> Int { i * i }))\n  \
       eachUpTo(3, function(i: Int) { print(i) })\n\
     }\n";

#[test]
fn native_extern_calls_round_trip_through_a_linked_host_shim() {
    let shim = compile_c_shim(ROUND_TRIP_SHIM, "roundtrip_shim");
    let out = run_with_shim(ROUND_TRIP_PROGRAM, std::slice::from_ref(&shim));
    assert_eq!(
        out,
        vec![
            "42".to_string(),    // addOne(41): Int round-trip
            "false".to_string(), // negate(true): Bool (i8) round-trip
            "1.5".to_string(),   // halve(3.0): Float (f64) round-trip
            "HI".to_string(),    // shout("hi"): String in + out via phx_string_alloc
            "7".to_string(),     // unbox(makeBox(7)): JsValue handle round-trip
            "14".to_string(),    // sumMap(4, i*i): 0+1+4+9 — value-returning callback
            "0".to_string(),     // eachUpTo(3, cb): the callback fires per index,
            "1".to_string(),     //   invoked through phx_invoke_closure_i_to_v
            "2".to_string(),
        ],
    );
}

/// With **no** host shim linked, the weak default definitions satisfy the symbols
/// so the program links and runs, then aborts the instant it calls the first
/// extern — naming the missing `(module, name)`, never failing silently (decision
/// A0). Uses the public no-shim link path (`link_executable_with_objects` with no
/// extra objects).
#[test]
fn native_unlinked_extern_aborts_with_a_clear_message() {
    let obj_bytes = common::compile_to_obj(
        "extern js { function alert(message: String) }\n\
         function main() { alert(\"boom\") }\n",
    );
    let obj_path = unique("unlinked", "o");
    let exe_path = unique("unlinked", "exe");
    std::fs::write(&obj_path, &obj_bytes).unwrap();
    link_executable_with_objects(&obj_path, &exe_path, &[])
        .expect("a program with only weak extern defaults should still link");

    let output = Command::new(&exe_path)
        .output()
        .expect("could not run the unlinked interop binary");

    // Clean up before asserting, so a failing assertion doesn't leak scratch.
    let _ = std::fs::remove_file(&obj_path);
    let _ = std::fs::remove_file(&exe_path);

    assert!(
        !output.status.success(),
        "an unbound extern must abort, but the binary exited cleanly"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no host binding for `extern js` function `alert`")
            && stderr.contains("phx_extern_js__alert"),
        "the abort must name the missing extern and its symbol, got: {stderr}"
    );
}

/// A program that *declares* an extern but never *calls* it lowers no
/// `Op::ExternCall`, so `collect_externs` finds nothing and the compiler emits no
/// weak `phx_extern_*` shim at all. It must link and run normally with no host
/// shim — there is no unbound symbol to abort on. Guards against a regression
/// where the mere presence of an `extern js` block (rather than an actual call)
/// would start pulling in shims or the abort path.
#[test]
fn native_declared_but_uncalled_extern_runs_normally() {
    let out = run_with_shim(
        "extern js { function alert(message: String) }\n\
         function main() { print(\"ran\") }\n",
        &[],
    );
    assert_eq!(out, vec!["ran".to_string()]);
}
