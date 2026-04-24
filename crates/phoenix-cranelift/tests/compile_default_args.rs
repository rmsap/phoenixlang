//! Tests for default-argument lowering in compiled mode.
//!
//! Sema records default-value expressions on `FunctionInfo`; IR
//! lowering's `merge_call_args` materializes each missing positional
//! slot by lowering the default expression at the call site.  These
//! tests pin that pipeline end-to-end by going through the compiled
//! Cranelift path (the AST interpreter handled defaults separately
//! before this feature; the `three_way_roundtrip` cases ensure all
//! three backends now agree).

mod common;

use common::{compile_and_run, three_way_roundtrip};

#[test]
fn default_on_trailing_positional_used() {
    let output = compile_and_run(
        r#"
function add(x: Int, y: Int = 10) -> Int {
    return x + y
}
function main() {
    print(add(1))
}
"#,
    );
    assert_eq!(output, vec!["11"]);
}

#[test]
fn default_overridden_by_positional() {
    let output = compile_and_run(
        r#"
function add(x: Int, y: Int = 10) -> Int {
    return x + y
}
function main() {
    print(add(1, 2))
}
"#,
    );
    assert_eq!(output, vec!["3"]);
}

#[test]
fn default_used_when_only_named_earlier_slot_provided() {
    let output = compile_and_run(
        r#"
function greet(name: String = "world", suffix: String = "!") -> String {
    return "hello " + name + suffix
}
function main() {
    print(greet(name: "phoenix"))
}
"#,
    );
    assert_eq!(output, vec!["hello phoenix!"]);
}

#[test]
fn default_expression_calls_another_function() {
    let output = compile_and_run(
        r#"
function origin() -> Int { return 42 }
function identity(x: Int = origin()) -> Int { return x }
function main() {
    print(identity())
    print(identity(7))
}
"#,
    );
    assert_eq!(output, vec!["42", "7"]);
}

#[test]
fn three_way_agreement_on_default_args() {
    three_way_roundtrip(
        r#"
function pad(label: String, width: Int = 5) -> String {
    return label + toString(width)
}
function main() {
    print(pad("a: "))
    print(pad("b: ", 12))
}
"#,
    );
}

/// Pins the caller-site-re-evaluation contract: each call site that
/// omits a defaulted slot must evaluate the default expression
/// *afresh*, not hoist it to a per-function initializer that runs
/// once.  Observable side effect: the default calls a helper that
/// `print`s, so the output line count is the number of times the
/// default was evaluated.
///
/// Regression site: a future "hoist the default inline lowering into
/// the callee's entry block, invoked via a fill-mask" optimization
/// would change the output from three `"tick"` lines to one and
/// break this test.  Any such change is an ABI / semantics shift
/// (see
/// [design-decisions.md: *Default-argument lowering strategy*](../../../docs/design-decisions.md#default-argument-lowering-strategy))
/// and should be weighed against this contract first.
#[test]
fn default_expression_evaluates_at_each_call_site() {
    let output = compile_and_run(
        r#"
function tick() -> Int {
    print("tick")
    return 1
}
function bump(x: Int = tick()) -> Int { return x }
function main() {
    print(bump())
    print(bump())
    print(bump())
}
"#,
    );
    assert_eq!(
        output,
        vec!["tick", "1", "tick", "1", "tick", "1"],
        "each `bump()` call must re-evaluate `tick()`; seeing fewer `tick` lines \
         means the default was hoisted into a once-per-function initializer"
    );
}

#[test]
fn default_with_named_and_positional_mix() {
    // Exercises the named-arg + default path: `x` is filled positionally,
    // `y` by name, `z` falls back to its default.
    let output = compile_and_run(
        r#"
function combine(x: Int, y: Int, z: Int = 100) -> Int {
    return x + y + z
}
function main() {
    print(combine(1, y: 2))
}
"#,
    );
    assert_eq!(output, vec!["103"]);
}

/// Pins the known gap: `lower_method_call` in
/// `crates/phoenix-ir/src/lower_expr.rs` does not route through
/// `merge_call_args`, so method calls that omit a defaulted trailing
/// positional argument never synthesize the default.  The fix is the
/// same shape as the free-function version shipped on 2026-04-24 —
/// hoist the `merge_call_args` path into the method branch too — but
/// it's a separate site and lands separately.  Tracked in
/// `docs/known-issues.md` under "Default arguments are not supported
/// on method calls".
#[test]
#[ignore = "method-call site does not route through merge_call_args — see known-issues.md"]
fn default_on_method_parameter() {
    three_way_roundtrip(
        r#"
struct Counter { Int n }
impl Counter {
    function bump(self, by: Int = 1) -> Int {
        return self.n + by
    }
}
function main() {
    let c: Counter = Counter(10)
    print(c.bump())
    print(c.bump(5))
}
"#,
    );
}
