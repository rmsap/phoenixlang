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

/// Method call omitting a defaulted trailing positional argument.
/// `c.bump()` now synthesizes the default via the new
/// `merge_method_call_args` helper in
/// `crates/phoenix-ir/src/lower_expr.rs`; sema's
/// `check_method_args` accepts the elided slot when
/// `MethodInfo.default_param_exprs[by_idx]` is populated.  Pinned
/// three-way to guarantee AST / IR / compiled agree on the
/// caller-site-materialized default.
#[test]
fn default_on_method_parameter() {
    three_way_roundtrip(
        r#"
struct Counter {
    Int n

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

/// A positional arg wins over a registered default.  Redundant with
/// the above's second call but separated so the "override" case can
/// regress independently of the "elided" case.  Three-way so all
/// backends agree on the override path.
#[test]
fn method_default_overridden_by_positional() {
    three_way_roundtrip(
        r#"
struct Counter {
    Int n

    function bump(self, by: Int = 1) -> Int {
        return self.n + by
    }
}
function main() {
    let c: Counter = Counter(10)
    print(c.bump(99))
}
"#,
    );
}

/// Pins the caller-site-re-evaluation contract for method defaults:
/// each call that omits the defaulted slot must evaluate the default
/// afresh.  Observable via a `print` side effect in the default
/// helper; a hoisted-into-callee rewrite would print fewer lines.
/// See the free-function analogue
/// `default_expression_evaluates_at_each_call_site`.
#[test]
fn method_default_references_global_function_each_call() {
    let output = compile_and_run(
        r#"
function defaultBump() -> Int {
    print("default")
    return 1
}
struct Counter {
    Int n

    function bump(self, by: Int = defaultBump()) -> Int {
        return self.n + by
    }
}
function main() {
    let c: Counter = Counter(10)
    print(c.bump())
    print(c.bump())
    print(c.bump())
}
"#,
    );
    assert_eq!(
        output,
        vec!["default", "11", "default", "11", "default", "11"],
        "each `c.bump()` call must re-evaluate `defaultBump()`; fewer `default` lines \
         means the default was hoisted into a once-per-function initializer",
    );
}

/// Method default on a *generic struct*: exercises the
/// `merge_method_call_args` lookup through the base-name key that
/// struct-mono later mangles, paired with the method-mono specialization.
/// Pins that the default lookup uses the bare base name (`"Box"`),
/// not the mangled `"Box__i64"`, since sema's `methods` registry keys
/// on the unmangled type name.
#[test]
fn method_default_on_generic_struct() {
    three_way_roundtrip(
        r#"
struct Box<T> {
    T value

    function padded(self, width: Int = 5) -> Int {
        return width
    }
}
function main() {
    let b: Box<Int> = Box(7)
    print(b.padded())
    print(b.padded(12))
}
"#,
    );
}

/// Method-level generic parameters (distinct from the enclosing
/// struct's) with a defaulted slot whose type is concrete.  The
/// default must not be rejected as "references the function's own
/// generic parameters" — the default's type (`Int`) has no free type
/// vars — and the method-mono + default-synthesis interact cleanly.
#[test]
fn method_default_with_method_level_type_params() {
    three_way_roundtrip(
        r#"
struct Counter {
    Int n

    function label<U>(self, item: U, prefix: Int = 0) -> Int {
        return self.n + prefix
    }
}
function main() {
    let c: Counter = Counter(10)
    print(c.label("hello"))
    print(c.label(42, 5))
}
"#,
    );
}

/// Method default whose expression constructs the enclosing struct
/// type via a free function returning that struct.  Pins that IR
/// lowering's default synthesis does not depend on the default
/// producing a *different* type than the receiver — a prior
/// implementation might hoist the default's lowering to a context
/// where the enclosing struct type isn't registered yet.
#[test]
fn method_default_calls_function_returning_same_struct_type() {
    three_way_roundtrip(
        r#"
struct Counter {
    Int n

    function add(self, other: Counter = fresh()) -> Int {
        return self.n + other.n
    }
}
function fresh() -> Counter {
    return Counter(100)
}
function main() {
    let c: Counter = Counter(10)
    print(c.add())
    print(c.add(Counter(7)))
}
"#,
    );
}
