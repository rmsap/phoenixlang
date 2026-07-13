//! The five-backend `extern js` interop matrix.
//!
//! The canonical interop fixture family — scalar round-trip, `String` in + out,
//! multi-byte UTF-8 `String` round-trip, `JsValue` pass-through, and
//! closures-as-callbacks — runs the *same* Phoenix
//! program on **all five backends** and asserts every one reproduces the fixture's
//! `expected.txt` line-for-line (each column is compared via [`lines`], which
//! normalizes the trailing newline; the always-on Node tier in `interop_node.rs`
//! pins the stricter byte-exact baseline for the two WASM columns):
//!
//! | column        | host binding                                              |
//! |---------------|-----------------------------------------------------------|
//! | AST interp    | Rust host stubs on the shared `HostRegistry` (PR 3)       |
//! | IR interp     | the *same* Rust host stubs (one contract, both interps)   |
//! | native        | a linked C host shim overriding the weak defaults (PR 9)  |
//! | wasm32-linear | the generated JS glue under Node (PRs 5–7)                |
//! | wasm32-gc     | the generated JS glue under Node (PRs 12–15)              |
//!
//! This is decision A0 made concrete: the `extern js` *surface* is one uniform
//! host-FFI boundary, so a fixture whose host functions are stubbable in Rust / C
//! / JS produces identical output no matter how the backend marshals across it
//! (i32 handle table vs. externref, copy-marshalled strings vs. scratch helpers,
//! pinned closures vs. host-traced refs). Each backend asserts against the single
//! shared baseline, so cross-backend agreement is transitive.
//!
//! **The host stubs are written once.** `register_*` registers the Rust closures
//! on *both* interpreters through the [`HostRegister`] trait (their `register_host`
//! methods are signature-identical); the C shim and the fixture's `host.mjs`
//! re-express the *same* behavior for the native and WASM columns. The three
//! languages staying in lockstep is the property under test.
//!
//! **Gating.** The interp columns need no toolchain and always run, so the matrix
//! never fully skips — it always asserts AST == IR == baseline. The native column
//! is ELF/Mach-O-only (weak-symbol override) and needs `cc` + the runtime static
//! lib; the WASM columns need `node` (+ the linear runtime wasm), under the usual
//! `PHOENIX_REQUIRE_*` hard-fail gates. A column with no toolchain soft-skips with
//! a visible note rather than weakening the assertion.
//!
//! **Carve-outs (asserted, not silent).** Two interop fixtures are *not* in this
//! matrix because the effect they assert exists only in the glue tier, not in the
//! language: the `dom/*` family (real DOM mutation — browser/jsdom tier,
//! `interop_browser.rs`) and `host_effect` (the glue's `ctx.emit` output-ordering
//! channel — the interpreters' `HostContext` has no `emit`, only `call_callback`).
//! [`carve_outs_are_glue_tier_only`] pins that exclusion so it can't silently grow.

mod common;

use common::interop::{interop_fixtures_dir, read_expected, run_fixture_under_node};
// Used only by the (ELF/Mach-O-only) native column for its whole-dir temp scope.
#[cfg(not(target_os = "windows"))]
use common::compiled_fixtures::TempDir;
#[cfg(not(target_os = "windows"))]
use common::skip_if_no_runtime_lib;
use common::{skip_if_no_node, skip_if_no_runtime_wasm, skip_if_no_wasm_gc};

use phoenix_common::host::{HostFunction, HostValue};
use phoenix_common::span::SourceId;
use phoenix_interp::interpreter::Interpreter;
use phoenix_ir_interp::interpreter::IrInterpreter;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

// --- Host registration: one stub set, both interpreters ---------------------

/// A host-registration surface common to both interpreters. Their inherent
/// `register_host(module, name, HostFunction)` methods are signature-identical
/// (the shared `phoenix_common::host` contract), so a fixture registers the
/// *same* closures on the AST and IR columns through this one object-safe trait —
/// the parity the matrix exists to assert.
trait HostRegister {
    fn reg(&mut self, module: &str, name: &str, f: HostFunction);
}

impl HostRegister for Interpreter {
    fn reg(&mut self, module: &str, name: &str, f: HostFunction) {
        self.register_host(module, name, f);
    }
}

impl HostRegister for IrInterpreter<'_> {
    fn reg(&mut self, module: &str, name: &str, f: HostFunction) {
        self.register_host(module, name, f);
    }
}

/// `scalars`: Int / Bool / Float round-trips (mirrors `scalars/host.mjs`).
fn register_scalars(r: &mut dyn HostRegister) {
    r.reg(
        "js",
        "addOne",
        Box::new(|_c, a| match a.into_iter().next() {
            Some(HostValue::Int(n)) => Ok(HostValue::Int(n + 1)),
            _ => Err("addOne expects an Int".into()),
        }),
    );
    r.reg(
        "js",
        "negate",
        Box::new(|_c, a| match a.into_iter().next() {
            Some(HostValue::Bool(b)) => Ok(HostValue::Bool(!b)),
            _ => Err("negate expects a Bool".into()),
        }),
    );
    r.reg(
        "js",
        "halve",
        Box::new(|_c, a| match a.into_iter().next() {
            Some(HostValue::Float(x)) => Ok(HostValue::Float(x / 2.0)),
            _ => Err("halve expects a Float".into()),
        }),
    );
    r.reg(
        "js",
        "floorToInt",
        Box::new(|_c, a| match a.into_iter().next() {
            Some(HostValue::Float(x)) => Ok(HostValue::Int(x.floor() as i64)),
            _ => Err("floorToInt expects a Float".into()),
        }),
    );
}

/// `strings`: a `String` crosses into the host and one crosses back out. The
/// fixture inputs are ASCII, where Rust `char` count, UTF-8 byte length, and JS
/// `.length` (UTF-16 units) all agree — so `lengthOf` matches the JS host.
fn register_strings(r: &mut dyn HostRegister) {
    r.reg(
        "js",
        "shout",
        Box::new(|_c, a| match a.into_iter().next() {
            Some(HostValue::Str(s)) => Ok(HostValue::Str(s.to_uppercase())),
            _ => Err("shout expects a String".into()),
        }),
    );
    r.reg(
        "js",
        "lengthOf",
        Box::new(|_c, a| match a.into_iter().next() {
            Some(HostValue::Str(s)) => Ok(HostValue::Int(s.chars().count() as i64)),
            _ => Err("lengthOf expects a String".into()),
        }),
    );
}

/// `npm_module`: an `extern js "left-pad" { ... }` extern
/// registered under the *package* module, next to an ambient binding — dispatch
/// must route by `(module, name)`, never flatten the package into the ambient
/// host. `leftPad` pads with spaces to `width`, matching JS `padStart`.
fn register_npm_module(r: &mut dyn HostRegister) {
    r.reg(
        "left-pad",
        "leftPad",
        Box::new(|_c, a| {
            let mut it = a.into_iter();
            match (it.next(), it.next()) {
                (Some(HostValue::Str(s)), Some(HostValue::Int(w))) => {
                    Ok(HostValue::Str(format!("{s:>width$}", width = w as usize)))
                }
                _ => Err("leftPad expects (String, Int)".into()),
            }
        }),
    );
    r.reg(
        "js",
        "shout",
        Box::new(|_c, a| match a.into_iter().next() {
            Some(HostValue::Str(s)) => Ok(HostValue::Str(s.to_uppercase())),
            _ => Err("shout expects a String".into()),
        }),
    );
}

/// `strings_unicode`: a multi-byte UTF-8 `String` round-trips intact. `echo`
/// hands back the exact same bytes (in + out fidelity); `byteLen` reports the
/// UTF-8 byte length — the one length measure all three host languages compute
/// identically, because UTF-8 bytes are exactly what crossed the wire (unlike
/// Rust `char` count or JS's UTF-16 `.length`, which diverge on non-ASCII).
fn register_strings_unicode(r: &mut dyn HostRegister) {
    r.reg(
        "js",
        "echo",
        Box::new(|_c, a| match a.into_iter().next() {
            Some(HostValue::Str(s)) => Ok(HostValue::Str(s)),
            _ => Err("echo expects a String".into()),
        }),
    );
    r.reg(
        "js",
        "byteLen",
        Box::new(|_c, a| match a.into_iter().next() {
            Some(HostValue::Str(s)) => Ok(HostValue::Int(s.len() as i64)),
            _ => Err("byteLen expects a String".into()),
        }),
    );
}

/// `jsvalue`: `JsValue` is opaque. The interpreters own the handle space, so the
/// stub mints `1` for `"x"` (tag `DIV`) and `2` for `"y"` (tag `SPAN`); identity
/// is by handle. The JS host uses real objects and the C shim an `i64` handle —
/// different representations, identical observable output (decision A0/D).
fn register_jsvalue(r: &mut dyn HostRegister) {
    r.reg(
        "js",
        "getEl",
        Box::new(|_c, a| match a.into_iter().next() {
            Some(HostValue::Str(s)) => Ok(HostValue::JsValue(if s == "y" { 2 } else { 1 })),
            _ => Err("getEl expects a String".into()),
        }),
    );
    r.reg(
        "js",
        "tagOf",
        Box::new(|_c, a| match a.into_iter().next() {
            Some(HostValue::JsValue(2)) => Ok(HostValue::Str("SPAN".into())),
            Some(HostValue::JsValue(_)) => Ok(HostValue::Str("DIV".into())),
            _ => Err("tagOf expects a JsValue".into()),
        }),
    );
    r.reg(
        "js",
        "sameNode",
        Box::new(|_c, a| {
            let mut it = a.into_iter();
            match (it.next(), it.next()) {
                (Some(HostValue::JsValue(x)), Some(HostValue::JsValue(y))) => {
                    Ok(HostValue::Bool(x == y))
                }
                _ => Err("sameNode expects two JsValues".into()),
            }
        }),
    );
}

/// `callbacks`: Phoenix closures handed across as callbacks, invoked synchronously
/// (the drained-`setTimeout` / callbacks-only model, decision H). Covers a no-arg
/// `() -> Void`, an `(Int) -> Void`, and a value-returning `(Int) -> Int`.
fn register_callbacks(r: &mut dyn HostRegister) {
    r.reg(
        "js",
        "setTimeout",
        Box::new(|ctx, a| match a.into_iter().next() {
            Some(HostValue::Callback(h)) => {
                ctx.call_callback(h, vec![])?;
                Ok(HostValue::Void)
            }
            _ => Err("setTimeout expects a callback".into()),
        }),
    );
    r.reg(
        "js",
        "eachUpTo",
        Box::new(|ctx, a| {
            let mut it = a.into_iter();
            match (it.next(), it.next()) {
                (Some(HostValue::Int(n)), Some(HostValue::Callback(h))) => {
                    for i in 0..n {
                        ctx.call_callback(h, vec![HostValue::Int(i)])?;
                    }
                    Ok(HostValue::Void)
                }
                _ => Err("eachUpTo expects (Int, callback)".into()),
            }
        }),
    );
    r.reg(
        "js",
        "sumMap",
        Box::new(|ctx, a| {
            let mut it = a.into_iter();
            match (it.next(), it.next()) {
                (Some(HostValue::Int(n)), Some(HostValue::Callback(h))) => {
                    let mut acc = 0i64;
                    for i in 0..n {
                        match ctx.call_callback(h, vec![HostValue::Int(i)])? {
                            HostValue::Int(v) => acc += v,
                            other => {
                                return Err(format!("sumMap callback returned non-Int: {other:?}"));
                            }
                        }
                    }
                    Ok(HostValue::Int(acc))
                }
                _ => Err("sumMap expects (Int, callback)".into()),
            }
        }),
    );
}

// --- C host shims (native column) -------------------------------------------
//
// Each shim provides strong `phx_extern_js__<name>` definitions that override the
// weak defaults the native backend emits, plus calls into the exported
// `phx_invoke_closure_<sig>` trampolines for callbacks. The ABI: Int→i64, Bool→i8,
// Float→f64, String→`(const char*, i64)` fat pointer (out via `phx_string_alloc`),
// JsValue→opaque i64 handle, closure→`void*` env pointer.
//
// This ABI is re-expressed here (rather than shared) because test helpers don't
// cross crate boundaries cleanly and these shims are keyed to the shared interop
// fixtures, whereas `crates/phoenix-cranelift/tests/extern_js_native.rs` drives
// inline source. The duplication is deliberate: if the `extern js` C ABI above
// changes, BOTH files must be updated in lockstep.

const SCALARS_SHIM: &str = r#"
#include <stdint.h>
int64_t phx_extern_js__addOne(int64_t n) { return n + 1; }
int8_t phx_extern_js__negate(int8_t b) { return b ? 0 : 1; }
double phx_extern_js__halve(double x) { return x / 2.0; }
int64_t phx_extern_js__floorToInt(double x) {
  int64_t t = (int64_t)x;          // truncation toward zero
  if ((double)t > x) t -= 1;       // adjust down for negatives → floor (no libm)
  return t;
}
"#;

const STRINGS_SHIM: &str = r#"
#include <stdint.h>
#include <stddef.h>
extern char *phx_string_alloc(size_t n);
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
int64_t phx_extern_js__lengthOf(const char *ptr, int64_t len) {
  (void)ptr;
  return len;  // ASCII fixture: byte length == char count == JS .length
}
"#;

const STRINGS_UNICODE_SHIM: &str = r#"
#include <stdint.h>
#include <stddef.h>
#include <string.h>
extern char *phx_string_alloc(size_t n);
struct PhxStr { const char *ptr; int64_t len; };
// `echo` copies the UTF-8 bytes straight back out unchanged; `byteLen` returns
// the fat-pointer length — i.e. the UTF-8 byte count, byte-for-byte identical to
// what the Rust stub and JS host compute.
struct PhxStr phx_extern_js__echo(const char *ptr, int64_t len) {
  char *out = phx_string_alloc((size_t)len);
  memcpy(out, ptr, (size_t)len);
  struct PhxStr r = { out, len };
  return r;
}
int64_t phx_extern_js__byteLen(const char *ptr, int64_t len) {
  (void)ptr;
  return len;
}
"#;

const JSVALUE_SHIM: &str = r#"
#include <stdint.h>
#include <stddef.h>
#include <string.h>
extern char *phx_string_alloc(size_t n);
struct PhxStr { const char *ptr; int64_t len; };
// JsValue is an opaque i64 handle the host owns: "x" -> 1 (DIV), "y" -> 2 (SPAN).
int64_t phx_extern_js__getEl(const char *ptr, int64_t len) {
  if (len == 1 && ptr[0] == 'y') return 2;
  return 1;
}
struct PhxStr phx_extern_js__tagOf(int64_t handle) {
  const char *tag = (handle == 2) ? "SPAN" : "DIV";
  int64_t n = (int64_t)strlen(tag);
  char *out = phx_string_alloc((size_t)n);
  memcpy(out, tag, (size_t)n);
  struct PhxStr r = { out, n };
  return r;
}
int8_t phx_extern_js__sameNode(int64_t a, int64_t b) { return a == b ? 1 : 0; }
"#;

const NPM_MODULE_SHIM: &str = r#"
#include <stdint.h>
#include <stddef.h>
extern char *phx_string_alloc(size_t n);
struct PhxStr { const char *ptr; int64_t len; };
// The escaped symbol for ("left-pad", "leftPad"): the native mangling
// hex-escapes non-alphanumerics in the module half (`-` -> `_2d`), so an npm
// package specifier still yields a symbol definable from plain C.
struct PhxStr phx_extern_left_2dpad__leftPad(const char *ptr, int64_t len, int64_t width) {
  int64_t out_len = len < width ? width : len;
  char *out = phx_string_alloc((size_t)out_len);
  int64_t pad = out_len - len;
  for (int64_t i = 0; i < pad; i++) out[i] = ' ';
  for (int64_t i = 0; i < len; i++) out[pad + i] = ptr[i];
  struct PhxStr r = { out, out_len };
  return r;
}
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
"#;

const CALLBACKS_SHIM: &str = r#"
#include <stdint.h>
// Exported trampolines: () -> Void, (Int) -> Void, (Int) -> Int.
extern void phx_invoke_closure__to_v(void *env);
extern void phx_invoke_closure_i_to_v(void *env, int64_t n);
extern int64_t phx_invoke_closure_i_to_i(void *env, int64_t n);
void phx_extern_js__setTimeout(void *cb, int64_t ms) { (void)ms; phx_invoke_closure__to_v(cb); }
void phx_extern_js__eachUpTo(int64_t n, void *cb) {
  for (int64_t i = 0; i < n; i++) phx_invoke_closure_i_to_v(cb, i);
}
int64_t phx_extern_js__sumMap(int64_t n, void *cb) {
  int64_t acc = 0;
  for (int64_t i = 0; i < n; i++) acc += phx_invoke_closure_i_to_i(cb, i);
  return acc;
}
"#;

// --- The matrix -------------------------------------------------------------

/// One interop fixture and the three host re-expressions the matrix runs it
/// against (the JS host is the fixture's own `host.mjs`).
struct Case {
    /// `tests/fixtures/interop/<fixture>/` directory name.
    fixture: &'static str,
    /// Registers the Rust host stubs on either interpreter.
    register: fn(&mut dyn HostRegister),
    /// The C host shim for the native column. Read only by the non-Windows
    /// `native_column`; the Windows stub never compiles the C shim.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    c_shim: &'static str,
}

const SCALARS: Case = Case {
    fixture: "scalars",
    register: register_scalars,
    c_shim: SCALARS_SHIM,
};
const STRINGS: Case = Case {
    fixture: "strings",
    register: register_strings,
    c_shim: STRINGS_SHIM,
};
const STRINGS_UNICODE: Case = Case {
    fixture: "strings_unicode",
    register: register_strings_unicode,
    c_shim: STRINGS_UNICODE_SHIM,
};
const JSVALUE: Case = Case {
    fixture: "jsvalue",
    register: register_jsvalue,
    c_shim: JSVALUE_SHIM,
};
const CALLBACKS: Case = Case {
    fixture: "callbacks",
    register: register_callbacks,
    c_shim: CALLBACKS_SHIM,
};
const NPM_MODULE: Case = Case {
    fixture: "npm_module",
    register: register_npm_module,
    c_shim: NPM_MODULE_SHIM,
};

fn lines(s: &str) -> Vec<String> {
    s.lines().map(String::from).collect()
}

/// Run `case` on every available backend, asserting each reproduces the fixture's
/// baseline. `test` names temp build dirs uniquely per fixture.
fn run_matrix(test: &str, case: &Case) {
    let src = std::fs::read_to_string(interop_fixtures_dir().join(case.fixture).join("main.phx"))
        .unwrap_or_else(|e| panic!("reading main.phx for `{}`: {e}", case.fixture));
    let expected = lines(&read_expected(case.fixture));

    // Front-end, shared by the interp + native columns.
    let tokens = tokenize(&src, SourceId(0));
    let (program, perrs) = parser::parse(&tokens);
    assert!(
        perrs.is_empty(),
        "{}: parse errors: {perrs:?}",
        case.fixture
    );
    let checked = checker::check(&program);
    assert!(
        checked.diagnostics.is_empty(),
        "{}: type errors: {:?}",
        case.fixture,
        checked.diagnostics
    );

    // Column 1: AST interpreter (always runs — the matrix never fully skips).
    let ast = phoenix_interp::run_with_host_capture(
        &program,
        checked.module.lambda_captures.clone(),
        |i| (case.register)(i),
    )
    .unwrap_or_else(|e| panic!("{}: AST interp failed: {e:?}", case.fixture));
    assert_eq!(ast, expected, "{}: AST interp column", case.fixture);

    // Column 2: IR interpreter (the same Rust stubs).
    let module = phoenix_ir::lower(&program, &checked.module);
    let verrs = phoenix_ir::verify::verify(&module);
    assert!(
        verrs.is_empty(),
        "{}: IR verification: {verrs:?}",
        case.fixture
    );
    let ir = phoenix_ir_interp::run_with_host_capture(&module, |i| (case.register)(i))
        .unwrap_or_else(|e| panic!("{}: IR interp failed: {e:?}", case.fixture));
    assert_eq!(ir, expected, "{}: IR interp column", case.fixture);

    // Column 3: native, via a linked C host shim (ELF/Mach-O only).
    native_column(test, case, &module, &expected);

    // Columns 4 & 5: the WASM glues under Node.
    if !skip_if_no_node(test) {
        if !skip_if_no_runtime_wasm(test) {
            let out =
                run_fixture_under_node(&format!("{test}_linear"), case.fixture, "wasm32-linear");
            assert_eq!(
                lines(&out),
                expected,
                "{}: wasm32-linear column",
                case.fixture
            );
        }
        if !skip_if_no_wasm_gc(test) {
            let out = run_fixture_under_node(&format!("{test}_gc"), case.fixture, "wasm32-gc");
            assert_eq!(lines(&out), expected, "{}: wasm32-gc column", case.fixture);
        }
    }
}

/// The native column: compile the program to an object, compile + link the C host
/// shim over it, run, and assert. Soft-skips when `cc` or the runtime static lib
/// is absent. Compiled out on Windows, whose COFF linker has no strong-beats-weak
/// override (`docs/known-issues.md`), matching
/// `crates/phoenix-cranelift/tests/extern_js_native.rs`.
#[cfg(not(target_os = "windows"))]
fn native_column(
    test: &str,
    case: &Case,
    module: &phoenix_ir::module::IrModule,
    expected: &[String],
) {
    if skip_if_no_runtime_lib(test) {
        return;
    }
    if !cc_available() {
        eprintln!(
            "skipping native column for `{}`: `cc` not found",
            case.fixture
        );
        return;
    }
    let obj = phoenix_cranelift::compile(module, phoenix_cranelift::Target::Native)
        .unwrap_or_else(|e| panic!("{}: native compile failed: {e:?}", case.fixture));
    // A whole-dir temp scope: `TempDir`'s `Drop` removes the `.c`/`.o`/`.exe`
    // artifacts even when an assertion below panics, so a failing native column
    // leaks nothing. Named by `test` (unique per matrix test) and PID-scoped, so
    // concurrent runs don't collide.
    let dir = TempDir::new(test);
    let out = run_native_with_shim(&dir, &obj, case.c_shim, case.fixture);
    assert_eq!(out, expected, "{}: native column", case.fixture);
}

#[cfg(target_os = "windows")]
fn native_column(
    _test: &str,
    case: &Case,
    _module: &phoenix_ir::module::IrModule,
    _expected: &[String],
) {
    eprintln!(
        "skipping native column for `{}`: ELF/Mach-O-only weak-symbol override",
        case.fixture
    );
}

/// Memoized like the `node`/wasm-gc probes: the `cc --version` subprocess runs
/// once and every later native column reads the cached verdict.
#[cfg(not(target_os = "windows"))]
fn cc_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("cc")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// Compile the program object + C shim, link them with the runtime, run, and
/// return stdout lines. All artifacts go in `dir` (a [`TempDir`] the caller owns,
/// so its `Drop` removes the whole scope — even on a panic here). Mirrors the
/// harness in `crates/phoenix-cranelift/tests/extern_js_native.rs`.
#[cfg(not(target_os = "windows"))]
fn run_native_with_shim(
    dir: &std::path::Path,
    obj_bytes: &[u8],
    c_shim: &str,
    fixture: &str,
) -> Vec<String> {
    let c_path = dir.join("shim.c");
    let shim_o = dir.join("shim.o");
    let obj_path = dir.join("app.o");
    let exe_path = dir.join("app.exe");

    // Compile the C shim.
    std::fs::write(&c_path, c_shim).unwrap();
    let status = std::process::Command::new("cc")
        .arg("-c")
        .arg(&c_path)
        .arg("-o")
        .arg(&shim_o)
        .status()
        .expect("failed to spawn cc for the host shim");
    assert!(
        status.success(),
        "cc failed to compile the host shim for `{fixture}`"
    );

    // Link the program object + shim into an executable, run it.
    std::fs::write(&obj_path, obj_bytes).unwrap();
    phoenix_cranelift::link_executable_with_objects(
        &obj_path,
        &exe_path,
        std::slice::from_ref(&shim_o),
    )
    .unwrap_or_else(|e| panic!("linking `{fixture}` with its host shim failed: {e:?}"));

    let output = std::process::Command::new(&exe_path)
        .output()
        .expect("could not run the linked interop binary");

    assert!(
        output.status.success(),
        "native interop binary for `{fixture}` exited with {}: stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    lines(&String::from_utf8_lossy(&output.stdout))
}

/// Define every matrix case in exactly one place: the same invocation emits each
/// per-fixture `#[test]` *and* builds the [`CASES`] slice that
/// [`carve_outs_are_glue_tier_only`] derives its "what's covered" set from. Because
/// both come from this one list, a case can't be in `CASES` (and so satisfy the
/// carve-out guard) without also having a test that actually runs it, or vice
/// versa — the two can't drift.
macro_rules! matrix {
    ($($test:ident => $case:ident),+ $(,)?) => {
        const CASES: &[&Case] = &[$(&$case),+];
        $(
            #[test]
            fn $test() {
                run_matrix(stringify!($test), &$case);
            }
        )+
    };
}

matrix! {
    interop_matrix_scalars => SCALARS,
    interop_matrix_strings => STRINGS,
    interop_matrix_strings_unicode => STRINGS_UNICODE,
    interop_matrix_jsvalue => JSVALUE,
    interop_matrix_callbacks => CALLBACKS,
    interop_matrix_npm_module => NPM_MODULE,
}

/// Assert the matrix carve-outs stay explicit and can't silently grow: every
/// `tests/fixtures/interop/` entry must be a matrix case above unless it is on
/// the documented exemption list — so adding a stubbable fixture without wiring
/// it into all five columns fails here. Current exemptions: `dom/*`
/// (browser/jsdom tier) and `host_effect` (the glue's `ctx.emit` channel, which
/// the interpreters' `HostContext` doesn't expose) exist only in the glue tier;
/// `npm_module_multi` is a multi-file project — this matrix's front-end is
/// single-file — so it runs on the Node tier (`interop_node.rs`, both wasm
/// targets, through the real driver's module resolution), with its interpreter
/// coverage in phoenix-interp's multi-module unit tests.
#[test]
fn carve_outs_are_glue_tier_only() {
    // Derived from the cases that actually run, not a parallel hand-kept list.
    let matrixed: Vec<&str> = CASES.iter().map(|c| c.fixture).collect();
    let matrix_exempt = ["host_effect", "dom", "npm_module_multi"];

    let mut unaccounted = Vec::new();
    for entry in std::fs::read_dir(interop_fixtures_dir()).expect("reading interop fixtures dir") {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !matrixed.contains(&name.as_str()) && !matrix_exempt.contains(&name.as_str()) {
            unaccounted.push(name);
        }
    }
    assert!(
        unaccounted.is_empty(),
        "interop fixtures neither in the five-backend matrix nor on the exemption list: \
         {unaccounted:?} — add them as a matrix case in interop_matrix.rs or, if another \
         tier legitimately owns them, to the `matrix_exempt` list (and document why)"
    );
    // The matrix cases must actually exist on disk (guards against a typo'd name).
    for f in &matrixed {
        assert!(
            interop_fixtures_dir().join(f).join("main.phx").exists(),
            "matrix fixture `{f}` is missing its main.phx"
        );
    }
    // The carve-outs must exist too, so a rename/delete can't leave the exclusion
    // list pointing at nothing. `host_effect` is a single fixture (has its own
    // `main.phx`); `dom` is the family-parent dir holding sub-fixtures, so assert
    // the directory rather than a `main.phx`.
    assert!(
        interop_fixtures_dir()
            .join("host_effect")
            .join("main.phx")
            .exists(),
        "carve-out fixture `host_effect` is missing its main.phx"
    );
    assert!(
        interop_fixtures_dir().join("dom").is_dir(),
        "carve-out family `dom` is missing its directory"
    );
    assert!(
        interop_fixtures_dir()
            .join("npm_module_multi")
            .join("main.phx")
            .exists(),
        "Node-tier fixture `npm_module_multi` is missing its main.phx"
    );
}
