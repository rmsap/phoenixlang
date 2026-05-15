//! Integration tests that verify each benchmark fixture produces the expected
//! output through both the tree-walk interpreter and the IR interpreter.
//!
//! Run with:
//! ```sh
//! cargo test -p phoenix-bench
//! ```

use phoenix_bench::{
    CompileLinkError, EMPTY, LARGE, MEDIUM, MEDIUM_LARGE, PARSE_ERROR, SMALL, TYPE_ERROR,
    assert_parse_error, assert_type_error, compile, compile_and_link, run_ir, run_native,
    run_tree_walk,
};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Compile + link a fixture, returning `Some(exe)` on success and
/// `None` when the runtime static library is missing on the current
/// machine. `PHOENIX_REQUIRE_RUNTIME_LIB=1` turns the missing-lib case
/// into a hard failure so CI catches it instead of silently skipping;
/// any other compile/link error is always a hard failure.
///
/// Centralized so every native fixture test — both positive (output
/// match) and negative (runtime abort) — applies the same skip-or-fail
/// policy. Before this helper was introduced the positive builder
/// fixtures hard-panicked on `RuntimeLibMissing` while the
/// abort-fixtures silently skipped; the inconsistency meant a sandbox
/// CI without a built runtime lib could not give a coherent
/// pass/skip/fail signal.
fn compile_and_link_or_skip(fixture: &str, source: &str) -> Option<PathBuf> {
    match compile_and_link(fixture, source) {
        Ok(p) => Some(p),
        Err(CompileLinkError::RuntimeLibMissing) => {
            if std::env::var("PHOENIX_REQUIRE_RUNTIME_LIB").as_deref() == Ok("1") {
                panic!(
                    "PHOENIX_REQUIRE_RUNTIME_LIB=1 set but the runtime static library \
                     is not on any search path — run `cargo build -p phoenix-runtime` \
                     or set $PHOENIX_RUNTIME_LIB"
                );
            }
            eprintln!(
                "warning: skipping {fixture} — runtime lib not built \
                 (set PHOENIX_REQUIRE_RUNTIME_LIB=1 to fail instead; \
                 `cargo build -p phoenix-runtime` to fix)"
            );
            None
        }
        Err(e) => panic!("{fixture} compile/link failed: {e}"),
    }
}

/// Native-binary output must match both interpreters and be non-empty.
/// IR-interp is a third witness so a regression in two backends that
/// agrees on the same wrong answer still surfaces; the non-empty
/// checks defend against `[] == [] == []` passing vacuously.
///
/// Complements (does not replace) the IR-only and tree-walk-only
/// fixture tests below — those stay as the canonical reference for
/// each fixture's expected output.
///
/// Runtime lib missing is a visible-skip environmental condition;
/// `PHOENIX_REQUIRE_RUNTIME_LIB=1` turns the skip into a hard fail.
fn assert_native_matches_interps(name: &str, source: &str) {
    let Some(exe) = compile_and_link_or_skip(name, source) else {
        return;
    };
    let native = run_native(&exe);
    let ir = run_ir(name, source);
    let tree_walk = run_tree_walk(name, source);
    assert!(!native.is_empty(), "{name} native output was empty");
    assert!(!ir.is_empty(), "{name} IR-interp output was empty");
    assert!(!tree_walk.is_empty(), "{name} tree-walk output was empty");
    assert!(
        native == tree_walk && tree_walk == ir,
        "{name}: backends diverged\n  native:    {native:?}\n  IR:        {ir:?}\n  tree-walk: {tree_walk:?}",
    );
}

// ---------------------------------------------------------------------------
// Compilation (IR well-formedness)
// ---------------------------------------------------------------------------

#[test]
fn empty_fixture_compiles() {
    let fn_count = compile("empty", EMPTY);
    assert!(fn_count >= 1, "IR module should contain at least main");
}

#[test]
fn small_fixture_compiles() {
    let fn_count = compile("small", SMALL);
    assert!(fn_count >= 2, "IR module should contain fib and main");
}

#[test]
fn medium_fixture_compiles() {
    let fn_count = compile("medium", MEDIUM);
    assert!(fn_count >= 2, "IR module should contain area and main");
}

#[test]
fn medium_large_fixture_compiles() {
    let fn_count = compile("medium_large", MEDIUM_LARGE);
    assert!(fn_count >= 1, "IR module should contain at least main");
}

#[test]
fn large_fixture_compiles() {
    let fn_count = compile("large", LARGE);
    assert!(fn_count >= 5, "IR module should contain multiple functions");
}

// ---------------------------------------------------------------------------
// Negative-path tests
// ---------------------------------------------------------------------------

#[test]
fn parse_error_fixture_has_parse_errors() {
    assert_parse_error("parse_error", PARSE_ERROR);
}

#[test]
fn type_error_fixture_has_type_errors() {
    assert_type_error("type_error", TYPE_ERROR);
}

// ---------------------------------------------------------------------------
// Tree-walk interpreter tests
// ---------------------------------------------------------------------------

#[test]
fn empty_fixture_tree_walk() {
    let output = run_tree_walk("empty", EMPTY);
    assert!(output.is_empty());
}

#[test]
fn small_fixture_tree_walk() {
    let output = run_tree_walk("small", SMALL);
    assert_eq!(output, vec!["55"]);
}

#[test]
fn medium_fixture_tree_walk() {
    let output = run_tree_walk("medium", MEDIUM);
    assert_eq!(output, vec!["3", "78.53975"]);
}

#[test]
fn medium_large_fixture_tree_walk() {
    let output = run_tree_walk("medium_large", MEDIUM_LARGE);
    assert_eq!(output, vec!["(3, 7)", "25", "120", "[4, 8, 12, 16, 20]"]);
}

#[test]
fn large_fixture_tree_walk() {
    let output = run_tree_walk("large", LARGE);
    let expected = vec![
        "(4, 6)",
        "(8, 12)",
        "circle with radius 5: area = 78.53975",
        "rectangle 3x4: area = 12",
        "triangle base=6 height=3: area = 9",
        "42",
        "Hello, Phoenix!",
        "60",
        "first: 1",
        "success: 42",
        "1",
        "2",
        "Fizz",
        "4",
        "Buzz",
        "Fizz",
        "7",
        "8",
        "Fizz",
        "Buzz",
        "11",
        "Fizz",
        "13",
        "14",
        "FizzBuzz",
        "10",
        "99",
        "45",
    ];
    assert_eq!(output, expected);
}

// ---------------------------------------------------------------------------
// IR interpreter tests
//
// These are #[ignore]-d when the IR interpreter does not yet support the
// required features.  Remove #[ignore] as the IR interpreter gains coverage;
// see the IR interpreter's known-limitations section in its crate docs for
// the current status.
// ---------------------------------------------------------------------------

#[test]
fn empty_fixture_ir_interp() {
    let output = run_ir("empty", EMPTY);
    assert!(output.is_empty());
}

#[test]
fn small_fixture_ir_interp() {
    let output = run_ir("small", SMALL);
    assert_eq!(output, vec!["55"]);
}

#[test]
fn medium_fixture_ir_interp() {
    let output = run_ir("medium", MEDIUM);
    assert_eq!(output, vec!["3", "78.53975"]);
}

#[test]
fn medium_large_fixture_ir_interp() {
    let output = run_ir("medium_large", MEDIUM_LARGE);
    assert_eq!(output, vec!["(3, 7)", "25", "120", "[4, 8, 12, 16, 20]"]);
}

#[test]
#[ignore = "IR interpreter does not yet support string methods — needed for describe()"]
fn large_fixture_ir_interp() {
    let output = run_ir("large", LARGE);
    let expected = vec![
        "(4, 6)",
        "(8, 12)",
        "circle with radius 5: area = 78.53975",
        "rectangle 3x4: area = 12",
        "triangle base=6 height=3: area = 9",
        "42",
        "Hello, Phoenix!",
        "60",
        "first: 1",
        "success: 42",
        "1",
        "2",
        "Fizz",
        "4",
        "Buzz",
        "Fizz",
        "7",
        "8",
        "Fizz",
        "Buzz",
        "11",
        "Fizz",
        "13",
        "14",
        "FizzBuzz",
        "10",
        "99",
        "45",
    ];
    assert_eq!(output, expected);
}

// ---------------------------------------------------------------------------
// Native compile-and-run tests. Same `compile_and_link` + `run_native`
// path the `compile_and_run` bench group exercises — catches
// codegen / linker / runtime regressions the interpreter tests miss.
// The tree-walk fixture tests above stay as the canonical expected
// output; equality is checked transitively through
// `assert_native_matches_interps`.
// ---------------------------------------------------------------------------

#[test]
fn medium_fixture_native() {
    assert_native_matches_interps("medium", MEDIUM);
}

// `medium_large` and `large` are blocked on outstanding Cranelift
// codegen gaps. Drop the matching `#[ignore]` once the capability
// lands; the pipeline bench's `COMPILE_AND_RUN_FIXTURES` will
// auto-enable the matching `compile_and_run` group at the same time.

#[test]
#[ignore = "blocked on phoenix-cranelift: print() of list<i64> not yet lowered"]
fn medium_large_fixture_native() {
    assert_native_matches_interps("medium_large", MEDIUM_LARGE);
}

#[test]
#[ignore = "blocked on phoenix-cranelift: string methods used by describe() not yet lowered"]
fn large_fixture_native() {
    assert_native_matches_interps("large", LARGE);
}

// ---------------------------------------------------------------------------
// Phase 2.7 decision F: `ListBuilder<T>` / `MapBuilder<K, V>` end-to-end
//
// These confirm the builder API compiles + runs through Cranelift; tree-walk
// and IR-interp are intentionally NOT exercised — neither backend has builder
// support today, so a program using builders only runs under `phoenix build`.
// The bench-corpus rewrite for `sort_ints` and `hash_map_churn` uses native
// compilation, so this coverage is what unblocks the published `phoenix-vs-go.md`
// ratio improvement.
// ---------------------------------------------------------------------------

#[test]
fn list_builder_native() {
    let src = r#"
function main() {
    let b: ListBuilder<Int> = List.builder()
    let mut i: Int = 0
    while i < 5 {
        b.push(i * 10)
        i = i + 1
    }
    let xs: List<Int> = b.freeze()
    print(xs.length())
    print(xs.get(0))
    print(xs.get(4))
}
"#;
    let Some(exe) = compile_and_link_or_skip("list_builder", src) else {
        return;
    };
    let output = run_native(&exe);
    assert_eq!(output, vec!["5", "0", "40"]);
}

#[test]
fn map_builder_native() {
    let src = r#"
function main() {
    let mb: MapBuilder<Int, Int> = Map.builder()
    let mut i: Int = 0
    while i < 5 {
        mb.set(i, i * 7)
        i = i + 1
    }
    let m: Map<Int, Int> = mb.freeze()
    print(m.length())
    print(m.get(0).unwrapOr(-1))
    print(m.get(4).unwrapOr(-1))
}
"#;
    let Some(exe) = compile_and_link_or_skip("map_builder", src) else {
        return;
    };
    let output = run_native(&exe);
    assert_eq!(output, vec!["5", "0", "28"]);
}

/// Empty-builder freeze: `.freeze()` with no pushes produces a
/// length-0 `List<T>`. Exercises the `if len > 0 && es > 0` guard in
/// `phx_list_builder_freeze` end-to-end.
#[test]
fn list_builder_empty_freeze_native() {
    let src = r#"
function main() {
    let b: ListBuilder<Int> = List.builder()
    let xs: List<Int> = b.freeze()
    print(xs.length())
}
"#;
    let Some(exe) = compile_and_link_or_skip("list_builder_empty", src) else {
        return;
    };
    let output = run_native(&exe);
    assert_eq!(output, vec!["0"]);
}

/// Empty-map-builder freeze produces a length-0 map.
#[test]
fn map_builder_empty_freeze_native() {
    let src = r#"
function main() {
    let mb: MapBuilder<Int, Int> = Map.builder()
    let m: Map<Int, Int> = mb.freeze()
    print(m.length())
}
"#;
    let Some(exe) = compile_and_link_or_skip("map_builder_empty", src) else {
        return;
    };
    let output = run_native(&exe);
    assert_eq!(output, vec!["0"]);
}

/// End-to-end coverage of `MapBuilder`'s last-wins dedup at freeze.
/// Sets the same key twice with different values; the frozen map
/// must show length 1 and the *latest* value. Runtime-unit-tested
/// at `map_builder_methods::tests::builder_set_last_wins_after_freeze`;
/// this test locks in the sema + codegen pathway as well.
#[test]
fn map_builder_last_wins_native() {
    let src = r#"
function main() {
    let mb: MapBuilder<Int, Int> = Map.builder()
    mb.set(42, 1)
    mb.set(42, 2)
    mb.set(7, 99)
    mb.set(42, 3)
    let m: Map<Int, Int> = mb.freeze()
    print(m.length())
    print(m.get(42).unwrapOr(-1))
    print(m.get(7).unwrapOr(-1))
}
"#;
    let Some(exe) = compile_and_link_or_skip("map_builder_last_wins", src) else {
        return;
    };
    let output = run_native(&exe);
    assert_eq!(output, vec!["2", "3", "99"]);
}

/// Compile, link, and run a Phoenix program that is expected to
/// abort at runtime via `runtime_abort`. Asserts:
///   - non-zero exit status,
///   - stderr begins with the `runtime error:` prefix used by every
///     `runtime_abort` site (pinning this prefix means a future
///     reformat of unrelated panic output cannot silently satisfy
///     the substring matches below),
///   - stderr contains every fragment in `needles`.
///
/// Uses [`compile_and_link_or_skip`] for the shared runtime-lib-missing
/// policy.
fn assert_runtime_abort(fixture: &str, src: &str, needles: &[&str]) {
    let Some(exe) = compile_and_link_or_skip(fixture, src) else {
        return;
    };
    let output = Command::new(&exe)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn failed");
    assert!(
        !output.status.success(),
        "expected non-zero exit for {fixture}, got {} (stdout: {}, stderr: {})",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("runtime error:"),
        "{fixture} stderr missing `runtime error:` prefix from runtime_abort: {stderr}",
    );
    for needle in needles {
        assert!(
            stderr.contains(needle),
            "{fixture} stderr missing fragment `{needle}`: {stderr}",
        );
    }
}

/// Use-after-freeze must abort the process with a "ListBuilder.push:
/// builder was already frozen" runtime error. The check lives in
/// `phx_list_builder_push::assert_unfrozen` — sema does not enforce
/// linearity (decision G defers it), so this runtime trip is the only
/// safety net.
#[test]
fn list_builder_use_after_freeze_aborts() {
    let src = r#"
function main() {
    let b: ListBuilder<Int> = List.builder()
    b.push(1)
    let xs: List<Int> = b.freeze()
    print(xs.length())
    b.push(2)
}
"#;
    assert_runtime_abort(
        "list_builder_uaf",
        src,
        &["ListBuilder.push", "already frozen"],
    );
}

/// Mirror of [`list_builder_use_after_freeze_aborts`] for `MapBuilder`.
#[test]
fn map_builder_use_after_freeze_aborts() {
    let src = r#"
function main() {
    let mb: MapBuilder<Int, Int> = Map.builder()
    mb.set(1, 10)
    let m: Map<Int, Int> = mb.freeze()
    print(m.length())
    mb.set(2, 20)
}
"#;
    assert_runtime_abort(
        "map_builder_uaf",
        src,
        &["MapBuilder.set", "already frozen"],
    );
}

/// Calling `.freeze()` twice on the same `ListBuilder` must abort —
/// `assert_unfrozen` runs at the top of every builder method, including
/// `freeze` itself. Both freeze results are bound to a `let` and then
/// observed via `print`, so the second freeze is guaranteed to run.
#[test]
fn list_builder_double_freeze_aborts() {
    let src = r#"
function main() {
    let b: ListBuilder<Int> = List.builder()
    b.push(1)
    let xs1: List<Int> = b.freeze()
    print(xs1.length())
    let xs2: List<Int> = b.freeze()
    print(xs2.length())
}
"#;
    assert_runtime_abort(
        "list_builder_double_freeze",
        src,
        &["ListBuilder.freeze", "already frozen"],
    );
}

/// Mirror of [`list_builder_double_freeze_aborts`] for `MapBuilder`.
#[test]
fn map_builder_double_freeze_aborts() {
    let src = r#"
function main() {
    let mb: MapBuilder<Int, Int> = Map.builder()
    mb.set(1, 10)
    let m1: Map<Int, Int> = mb.freeze()
    print(m1.length())
    let m2: Map<Int, Int> = mb.freeze()
    print(m2.length())
}
"#;
    assert_runtime_abort(
        "map_builder_double_freeze",
        src,
        &["MapBuilder.freeze", "already frozen"],
    );
}

/// Push past the initial capacity (8 slots) so the runtime's grow
/// path in `phx_list_builder_push` runs end-to-end. 20 elements
/// cross at least two doublings (8 → 16 → 32). Exercises the
/// shadow-stack rooting added around the freshly-allocated buffer
/// during the grow.
#[test]
fn list_builder_grow_native() {
    let src = r#"
function main() {
    let b: ListBuilder<Int> = List.builder()
    let mut i: Int = 0
    while i < 20 {
        b.push(i)
        i = i + 1
    }
    let xs: List<Int> = b.freeze()
    print(xs.length())
    print(xs.get(0))
    print(xs.get(7))
    print(xs.get(8))
    print(xs.get(19))
}
"#;
    let Some(exe) = compile_and_link_or_skip("list_builder_grow", src) else {
        return;
    };
    let output = run_native(&exe);
    assert_eq!(output, vec!["20", "0", "7", "8", "19"]);
}

/// Mirror for `MapBuilder`: 20 pairs exercise the grow path in
/// `phx_map_builder_set` past the initial 8-pair capacity.
#[test]
fn map_builder_grow_native() {
    let src = r#"
function main() {
    let mb: MapBuilder<Int, Int> = Map.builder()
    let mut i: Int = 0
    while i < 20 {
        mb.set(i, i * 11)
        i = i + 1
    }
    let m: Map<Int, Int> = mb.freeze()
    print(m.length())
    print(m.get(0).unwrapOr(-1))
    print(m.get(7).unwrapOr(-1))
    print(m.get(8).unwrapOr(-1))
    print(m.get(19).unwrapOr(-1))
}
"#;
    let Some(exe) = compile_and_link_or_skip("map_builder_grow", src) else {
        return;
    };
    let output = run_native(&exe);
    assert_eq!(output, vec!["20", "0", "77", "88", "209"]);
}

/// MapBuilder with a non-Int key type. Phoenix `String` is a 16-byte
/// fat pointer (ptr + len), which exercises `store_to_temp`'s
/// multi-slot path in the codegen as well as the
/// `phx_map_from_pairs` string-rooting note in the runtime. Without
/// this fixture the new builder code was only validated for 8-byte
/// scalar keys/values.
#[test]
fn map_builder_string_keys_native() {
    let src = r#"
function main() {
    let mb: MapBuilder<String, Int> = Map.builder()
    mb.set("alpha", 1)
    mb.set("beta", 2)
    mb.set("gamma", 3)
    let m: Map<String, Int> = mb.freeze()
    print(m.length())
    print(m.get("alpha").unwrapOr(-1))
    print(m.get("beta").unwrapOr(-1))
    print(m.get("gamma").unwrapOr(-1))
    print(m.get("missing").unwrapOr(-1))
}
"#;
    let Some(exe) = compile_and_link_or_skip("map_builder_string_keys", src) else {
        return;
    };
    let output = run_native(&exe);
    assert_eq!(output, vec!["3", "1", "2", "3", "-1"]);
}

/// Symmetric to `map_builder_string_keys_native`: fat-pointer values
/// instead of fat-pointer keys. Covers the value-side of
/// `store_to_temp`'s multi-slot path in `phx_map_builder_set` and
/// the value-rooting concern in `phx_map_from_pairs`.
#[test]
fn map_builder_string_values_native() {
    let src = r#"
function main() {
    let mb: MapBuilder<Int, String> = Map.builder()
    mb.set(1, "alpha")
    mb.set(2, "beta")
    mb.set(3, "gamma")
    let m: Map<Int, String> = mb.freeze()
    print(m.length())
    print(m.get(1).unwrapOr("missing"))
    print(m.get(2).unwrapOr("missing"))
    print(m.get(3).unwrapOr("missing"))
    print(m.get(99).unwrapOr("missing"))
}
"#;
    let Some(exe) = compile_and_link_or_skip("map_builder_string_values", src) else {
        return;
    };
    let output = run_native(&exe);
    assert_eq!(output, vec!["3", "alpha", "beta", "gamma", "missing"]);
}

/// Sema must reject pushing a wrong-typed element. `ListBuilder<Int>`
/// + `b.push("hello")` should produce a type error well before codegen.
#[test]
fn list_builder_wrong_elem_type_is_sema_error() {
    let src = r#"
function main() {
    let b: ListBuilder<Int> = List.builder()
    b.push("hello")
}
"#;
    assert_type_error("list_builder_wrong_elem", src);
}

/// Sema must reject set with a wrong-typed key.
#[test]
fn map_builder_wrong_key_type_is_sema_error() {
    let src = r#"
function main() {
    let mb: MapBuilder<Int, Int> = Map.builder()
    mb.set("oops", 1)
}
"#;
    assert_type_error("map_builder_wrong_key", src);
}

/// No-annotation construction is a **sema error**: an unannotated
/// `let b = List.builder()` leaves `T` as a typevar and sema rejects
/// it. Loosening this rule has codegen-side consequences documented
/// in [`docs/known-issues.md` § "Builder construction requires a
/// let-annotation"][1].
///
/// [1]: ../../../../docs/known-issues.md#builder-construction-requires-a-let-annotation
#[test]
fn list_builder_no_annotation_is_sema_error() {
    let src = r#"
function main() {
    let b = List.builder()
    b.push(1)
}
"#;
    assert_type_error("list_builder_no_annot", src);
}

/// Same contract for `MapBuilder` — `K` / `V` typevars without an
/// annotation. See [`list_builder_no_annotation_is_sema_error`].
#[test]
fn map_builder_no_annotation_is_sema_error() {
    let src = r#"
function main() {
    let mb = Map.builder()
    mb.set(1, 2)
}
"#;
    assert_type_error("map_builder_no_annot", src);
}
