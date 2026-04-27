//! Round-trip tests: basic language features (arithmetic, variables, control flow, functions).

mod common;
use common::roundtrip;

// ── Fixtures ─────────────────────────────────────────────────────────

#[test]
fn fixture_hello() {
    roundtrip(&std::fs::read_to_string("../../tests/fixtures/hello.phx").unwrap());
}

#[test]
fn fixture_fibonacci() {
    roundtrip(&std::fs::read_to_string("../../tests/fixtures/fibonacci.phx").unwrap());
}

#[test]
fn fixture_fizzbuzz() {
    roundtrip(&std::fs::read_to_string("../../tests/fixtures/fizzbuzz.phx").unwrap());
}

#[test]
fn fixture_features() {
    roundtrip(&std::fs::read_to_string("../../tests/fixtures/features.phx").unwrap());
}

// ── Basic ────────────────────────────────────────────────────────────

#[test]
fn hello_world() {
    roundtrip(r#"function main() { print("Hello, World!") }"#);
}

#[test]
fn arithmetic() {
    roundtrip(
        r#"
function main() {
    print(2 + 3)
    print(10 - 4)
    print(3 * 7)
    print(15 / 4)
    print(17 % 5)
    print(-42)
}
"#,
    );
}

#[test]
fn float_arithmetic() {
    roundtrip(
        r#"
function main() {
    print(1.5 + 2.5)
    print(10.0 - 3.5)
    print(2.0 * 3.5)
    print(7.0 / 2.0)
}
"#,
    );
}

#[test]
fn boolean_logic() {
    roundtrip(
        r#"
function main() {
    print(true && false)
    print(true || false)
    print(!true)
    print(1 == 1)
    print(1 != 2)
    print(3 < 5)
    print(5 > 3)
}
"#,
    );
}

#[test]
fn string_comparison() {
    roundtrip(
        r#"
function main() {
    print("abc" == "abc")
    print("abc" != "def")
    print("abc" < "def")
}
"#,
    );
}

#[test]
fn variables_and_mutation() {
    roundtrip(
        r#"
function main() {
    let x: Int = 10
    print(x)
    let mut y: Int = 5
    y = y + 3
    print(y)
}
"#,
    );
}

#[test]
fn if_else() {
    roundtrip(
        r#"
function main() {
    let x: Int = 10
    if x > 5 {
        print("big")
    } else {
        print("small")
    }
    if x < 5 {
        print("small")
    } else {
        print("big")
    }
}
"#,
    );
}

#[test]
fn while_loop() {
    roundtrip(
        r#"
function main() {
    let mut sum: Int = 0
    let mut i: Int = 1
    while i <= 10 {
        sum = sum + i
        i = i + 1
    }
    print(sum)
}
"#,
    );
}

#[test]
fn for_loop() {
    roundtrip(
        r#"
function main() {
    let items: List<Int> = [1, 2, 3, 4, 5]
    let mut total: Int = 0
    for item in items {
        total = total + item
    }
    print(total)
}
"#,
    );
}

#[test]
fn function_call() {
    roundtrip(
        r#"
function add(a: Int, b: Int) -> Int {
    return a + b
}
function main() {
    print(add(3, 4))
}
"#,
    );
}

#[test]
fn recursion() {
    roundtrip(
        r#"
function factorial(n: Int) -> Int {
    if n <= 1 { return 1 }
    return n * factorial(n - 1)
}
function main() {
    print(factorial(5))
}
"#,
    );
}

// ── Loop control flow ───────────────────────────────────────────────

#[test]
fn loop_break() {
    roundtrip(
        r#"
function main() {
    let mut i: Int = 0
    while true {
        if i >= 5 { break }
        i = i + 1
    }
    print(i)
}
"#,
    );
}

#[test]
fn loop_continue() {
    roundtrip(
        r#"
function main() {
    let mut sum: Int = 0
    let mut i: Int = 0
    while i < 10 {
        i = i + 1
        if i % 2 == 0 { continue }
        sum = sum + i
    }
    print(sum)
}
"#,
    );
}

#[test]
fn for_loop_break() {
    roundtrip(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3, 4, 5]
    let mut total: Int = 0
    for n in nums {
        if n > 3 { break }
        total = total + n
    }
    print(total)
}
"#,
    );
}

#[test]
fn for_loop_continue() {
    roundtrip(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3, 4, 5]
    let mut total: Int = 0
    for n in nums {
        if n % 2 == 0 { continue }
        total = total + n
    }
    print(total)
}
"#,
    );
}

// ── toString ─────────────────────────────────────────────────────────

#[test]
fn to_string_builtin() {
    roundtrip(
        r#"
function main() {
    print(toString(42))
    print(toString(3.14))
    print(toString(true))
    print(toString("hello"))
}
"#,
    );
}

// ── Default arguments ────────────────────────────────────────────────
//
// The IR interpreter has no default-expression handling; it sees
// defaults already materialized by `merge_call_args` in IR lowering.
// A divergence vs. the AST interpreter here means `merge_call_args`
// regressed.

#[test]
fn default_argument_trailing_positional() {
    roundtrip(
        r#"
function add(x: Int, y: Int = 10) -> Int { return x + y }
function main() {
    print(add(1))
    print(add(1, 2))
}
"#,
    );
}

#[test]
fn default_argument_with_named_earlier_slot() {
    roundtrip(
        r#"
function greet(name: String = "world", suffix: String = "!") -> String {
    return "hello " + name + suffix
}
function main() {
    print(greet())
    print(greet(name: "phoenix"))
    print(greet(suffix: "."))
}
"#,
    );
}

#[test]
fn default_argument_calls_another_function() {
    roundtrip(
        r#"
function origin() -> Int { return 42 }
function identity(x: Int = origin()) -> Int { return x }
function main() {
    print(identity())
    print(identity(7))
}
"#,
    );
}

// ── Method-call defaults ─────────────────────────────────────────────
//
// The IR interpreter sees defaults already materialized by
// `merge_method_call_args` in IR lowering, paralleling the
// free-function pattern.  A divergence vs. the AST interpreter here
// means either the lowering or the AST-interp `call_method` path
// regressed.

#[test]
fn method_default_trailing_positional() {
    roundtrip(
        r#"
struct Counter {
    Int n

    function bump(self, by: Int = 10) -> Int { return self.n + by }
}
function main() {
    let c: Counter = Counter(1)
    print(c.bump())
    print(c.bump(2))
}
"#,
    );
}

#[test]
fn method_default_calls_another_function() {
    roundtrip(
        r#"
function origin() -> Int { return 42 }
struct Box {
    Int value

    function wrap(self, tag: Int = origin()) -> Int { return self.value + tag }
}
function main() {
    let b: Box = Box(1)
    print(b.wrap())
    print(b.wrap(7))
}
"#,
    );
}

/// Positional arg overrides the registered default — IR-interp and
/// AST-interp must agree that the default is *not* used when the
/// caller passes a value.  Mirrors the compiled-side
/// `method_default_overridden_by_positional`.
#[test]
fn method_default_overridden_by_positional() {
    roundtrip(
        r#"
struct Counter {
    Int n

    function bump(self, by: Int = 1) -> Int { return self.n + by }
}
function main() {
    let c: Counter = Counter(10)
    print(c.bump(99))
    print(c.bump())
}
"#,
    );
}
