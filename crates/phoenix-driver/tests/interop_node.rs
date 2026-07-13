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

use common::interop::{read_expected, run_fixture_under_node};
use common::{skip_if_no_node, skip_if_no_runtime_wasm, skip_if_no_wasm_gc};

/// Build `tests/fixtures/interop/<name>/main.phx` for `target`, run it under Node
/// with that fixture's `host.mjs`, and assert stdout equals `expected.txt`. The
/// build + `node` invocation is shared with the five-backend matrix via
/// [`common::interop`]; this tier owns the byte-exact baseline assertion.
fn run_interop_fixture(name: &str, target: &str) {
    let test = format!("interop_{name}_{}", target.replace(['-'], "_"));
    let stdout = run_fixture_under_node(&test, name, target);
    assert_eq!(
        stdout,
        read_expected(name),
        "interop fixture `{name}` ({target}) stdout did not match its baseline"
    );
}

/// One always-on Node test per interop fixture **per WASM target**: the *same*
/// fixture + host stub + baseline runs on both `wasm32-linear` and `wasm32-gc`,
/// asserting byte-identical output (the two glues differ only in marshalling).
/// Adding a fixture is a `tests/fixtures/interop/<name>/` directory plus a line
/// here. wasm32-gc embeds no Phoenix runtime, so its build needs only `node`
/// (not the linear runtime wasm).
macro_rules! interop_node_test {
    ($linear:ident, $gc:ident, $fixture:literal) => {
        #[test]
        fn $linear() {
            if skip_if_no_runtime_wasm(stringify!($linear)) {
                return;
            }
            if skip_if_no_node(stringify!($linear)) {
                return;
            }
            run_interop_fixture($fixture, "wasm32-linear");
        }

        #[test]
        fn $gc() {
            if skip_if_no_node(stringify!($gc)) || skip_if_no_wasm_gc(stringify!($gc)) {
                return;
            }
            run_interop_fixture($fixture, "wasm32-gc");
        }
    };
}

interop_node_test!(
    interop_scalars_round_trip,
    interop_scalars_round_trip_gc,
    "scalars"
);
interop_node_test!(
    interop_strings_in_and_out,
    interop_strings_in_and_out_gc,
    "strings"
);
interop_node_test!(
    interop_strings_unicode_round_trip,
    interop_strings_unicode_round_trip_gc,
    "strings_unicode"
);
interop_node_test!(
    interop_jsvalue_handle_round_trip,
    interop_jsvalue_handle_round_trip_gc,
    "jsvalue"
);
interop_node_test!(
    interop_closures_as_callbacks,
    interop_closures_as_callbacks_gc,
    "callbacks"
);
interop_node_test!(
    interop_host_side_effect_ordering,
    interop_host_side_effect_ordering_gc,
    "host_effect"
);
interop_node_test!(
    interop_npm_module_namespaced_binding,
    interop_npm_module_namespaced_binding_gc,
    "npm_module"
);
// Two Phoenix modules each binding the same `("left-pad", "leftPad")` pair —
// the expected BYO pattern. The pair dedupes into one import + one thunk
// (`collect_externs`), which only a compiled multi-file build exercises; this
// tier runs it through the real driver's module resolution on both wasm
// targets. (Its interpreter coverage lives in phoenix-interp's multi-module
// unit tests; the five-backend matrix front-end is single-file, so the fixture
// is exempted there — see `carve_outs_are_glue_tier_only`.)
interop_node_test!(
    interop_npm_module_multi_module,
    interop_npm_module_multi_module_gc,
    "npm_module_multi"
);
