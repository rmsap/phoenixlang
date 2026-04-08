mod common;
use common::*;

#[test]
fn enum_and_match() {
    run_expect(
        r#"
enum Shape {
  Circle(Float)
  Rect(Float, Float)
}
impl Shape {
  function area(self) -> Float {
    return match self {
      Circle(r) -> 3.14 * r * r
      Rect(w, h) -> w * h
    }
  }
}
function main() {
  let s: Shape = Circle(5.0)
  print(s.area())
}
"#,
        &["78.5"],
    );
}

#[test]
fn match_wildcard() {
    run_expect(
        r#"
enum Color { Red Green Blue }
function main() {
  let c: Color = Green
  match c {
    Red -> print("red")
    _ -> print("other")
  }
}
"#,
        &["other"],
    );
}

#[test]
fn type_mismatch_caught() {
    expect_type_error(
        r#"function main() { let x: Int = "hello" }"#,
        "type mismatch",
    );
}

/// Non-exhaustive match on an enum is caught at compile time.
#[test]
fn match_exhaustiveness_check() {
    expect_type_error(
        r#"
enum Color {
  Red
  Green
  Blue
}
function main() {
  let c: Color = Red
  match c {
    Red -> print("red")
  }
}
"#,
        "non-exhaustive match",
    );
}

/// Match with wildcard is always considered exhaustive.
#[test]
fn match_with_wildcard_is_exhaustive() {
    run_expect(
        r#"
enum Color {
  Red
  Green
  Blue
}
function main() {
  let c: Color = Red
  match c {
    Red -> print("red")
    _ -> print("other")
  }
}
"#,
        &["red"],
    );
}

#[test]
fn implicit_return_match_expression() {
    run_expect(
        r#"
enum Shape {
  Circle(Float)
  Rect(Float, Float)
}
function describe(s: Shape) -> String {
  match s {
    Circle(r) -> "circle"
    Rect(w, h) -> "rectangle"
  }
}
function main() {
  print(describe(Circle(5.0)))
  print(describe(Rect(3.0, 4.0)))
}
"#,
        &["circle", "rectangle"],
    );
}

#[test]
fn type_alias_with_match_and_implicit_return() {
    run_expect(
        r#"
type Str = String
function describe(x: Int) -> Str {
  match x {
    0 -> "zero"
    _ -> "nonzero"
  }
}
function main() {
  print(describe(0))
  print(describe(5))
}
"#,
        &["zero", "nonzero"],
    );
}

// ── 1.8 Edge cases: Implicit Return ────────────────────────────────

#[test]
fn implicit_return_match_arm_block() {
    run_expect(
        r#"
function classify(x: Int) -> String {
  match x {
    0 -> "zero"
    _ -> {
      let prefix: String = "number: "
      prefix + toString(x)
    }
  }
}
function main() {
  print(classify(0))
  print(classify(5))
}
"#,
        &["zero", "number: 5"],
    );
}

#[test]
fn implicit_return_enum_variant() {
    run_expect(
        r#"
function alwaysSome(x: Int) -> Option<Int> {
  Some(x)
}
function main() {
  let r: Option<Int> = alwaysSome(42)
  print(r.unwrap())
}
"#,
        &["42"],
    );
}

#[test]
fn empty_match_produces_exhaustiveness_warning() {
    expect_type_error(
        r#"
enum Color {
  Red
  Green
  Blue
}
function main() {
  let c: Color = Red
  match c { }
}
"#,
        "non-exhaustive match",
    );
}

#[test]
fn match_arm_type_mismatch_caught() {
    expect_type_error(
        r#"
enum Color { Red
  Blue }
function main() {
  let c: Color = Red
  let x: Int = match c {
    Red -> 1
    Blue -> "string"
  }
}
"#,
        "match arm type mismatch",
    );
}

#[test]
fn match_arm_types_consistent() {
    run_expect(
        r#"
enum Color { Red
  Blue }
function main() {
  let c: Color = Red
  let x: String = match c {
    Red -> "red"
    Blue -> "blue"
  }
  print(x)
}
"#,
        &["red"],
    );
}

// ── Second audit regression tests ───────────────────────────────────────

#[test]
fn enum_variant_equality_same_type() {
    // Enum variants of the same type with same data should be equal
    run_expect(
        r#"
enum Color {
  Red
  Blue
}
function main() {
  let a: Color = Red
  let b: Color = Red
  let c: Color = Blue
  print(a == b)
  print(a == c)
}
"#,
        &["true", "false"],
    );
}

#[test]
fn variant_pattern_binding_count_mismatch() {
    expect_type_error(
        r#"
enum Pair {
  Two(Int, Int)
}
function main() {
  let p: Pair = Two(1, 2)
  match p {
    Two(a) -> print(a)
  }
}
"#,
        "has 2 field(s) but pattern has 1 binding(s)",
    );
}

// =========================================================================
// Tests for bug fixes and previously missing coverage
// =========================================================================

// --- B1: break/continue inside match blocks (now compile errors) ---

#[test]
fn break_inside_match_in_loop_error() {
    expect_type_error(
        r#"
function main() {
    let mut x: Int = 0
    while x < 10 {
        match x {
            5 -> { break }
            _ -> {}
        }
        x = x + 1
    }
    print(x)
}
"#,
        "`break` is not allowed inside match arms",
    );
}

#[test]
fn continue_inside_match_in_loop_error() {
    expect_type_error(
        r#"
function main() {
    let mut total: Int = 0
    let mut i: Int = 0
    while i < 5 {
        i = i + 1
        match i {
            3 -> { continue }
            _ -> {}
        }
        total = total + i
    }
    print(total)
}
"#,
        "`continue` is not allowed inside match arms",
    );
}

// --- B8: enum variant wrong argument count ---

#[test]
fn enum_variant_too_many_args() {
    expect_type_error(
        r#"
enum Shape {
    Circle(Float)
    Rect(Float, Float)
}
function main() {
    let s: Shape = Circle(1.0, 2.0, 3.0)
    print(s)
}
"#,
        "takes 1 field(s), got 3",
    );
}

#[test]
fn enum_variant_too_few_args() {
    expect_type_error(
        r#"
enum Shape {
    Circle(Float)
    Rect(Float, Float)
}
function main() {
    let s: Shape = Rect(1.0)
    print(s)
}
"#,
        "takes 2 field(s), got 1",
    );
}

// --- Match expression as value ---

#[test]
fn match_expression_used_as_value() {
    run_expect(
        r#"
enum Color { Red Green Blue }
function colorCode(c: Color) -> Int {
    match c {
        Red -> 1
        Green -> 2
        Blue -> 3
    }
}
function main() {
    let code: Int = colorCode(Red)
    print(code)
}
"#,
        &["1"],
    );
}

// --- Enum equality ---

#[test]
fn enum_variant_equality() {
    run_expect(
        r#"
enum Direction { North South East West }
function main() {
    let a: Direction = North
    let b: Direction = North
    let c: Direction = South
    print(a == b)
    print(a == c)
}
"#,
        &["true", "false"],
    );
}

// --- Type alias with match exhaustiveness ---

#[test]
fn type_alias_match_exhaustiveness() {
    expect_type_error(
        r#"
type OptInt = Option<Int>
function main() {
    let x: OptInt = Some(42)
    match x {
        Some(n) -> print(n)
    }
}
"#,
        "non-exhaustive",
    );
}

// --- Multiple enum variants with data ---

#[test]
fn enum_with_mixed_data_variants() {
    run_expect(
        r#"
enum Token {
    Number(Int)
    Word(String)
    Eof
}
function describe(t: Token) -> String {
    match t {
        Number(n) -> "num"
        Word(s) -> "word"
        Eof -> "eof"
    }
}
function main() {
    print(describe(Number(42)))
    print(describe(Word("hi")))
    print(describe(Eof))
}
"#,
        &["num", "word", "eof"],
    );
}

// ── Pattern Matching ──────────────────────────────────────────────────

#[test]
fn match_on_int_literal_pattern() {
    run_expect(
        r#"
function describe(n: Int) -> String {
    match n {
        0 -> "zero"
        1 -> "one"
        _ -> "other"
    }
}
function main() {
    print(describe(0))
    print(describe(1))
    print(describe(42))
}
"#,
        &["zero", "one", "other"],
    );
}

#[test]
fn match_on_string_literal_pattern() {
    run_expect(
        r#"
function greet(name: String) -> String {
    match name {
        "Alice" -> "Hi Alice!"
        "Bob" -> "Hey Bob!"
        _ -> "Hello stranger"
    }
}
function main() {
    print(greet("Alice"))
    print(greet("Bob"))
    print(greet("Charlie"))
}
"#,
        &["Hi Alice!", "Hey Bob!", "Hello stranger"],
    );
}

#[test]
fn match_on_bool_literal_pattern() {
    run_expect(
        r#"
function describe(b: Bool) -> String {
    match b {
        true -> "yes"
        false -> "no"
    }
}
function main() {
    print(describe(true))
    print(describe(false))
}
"#,
        &["yes", "no"],
    );
}

#[test]
fn match_binding_pattern() {
    run_expect(
        r#"
function doubleOrZero(opt: Option<Int>) -> Int {
    match opt {
        Some(x) -> x * 2
        None -> 0
    }
}
function main() {
    print(doubleOrZero(Some(5)))
    print(doubleOrZero(None))
}
"#,
        &["10", "0"],
    );
}

#[test]
fn match_variant_with_multiple_fields() {
    run_expect(
        r#"
enum Expr {
    Add(Int, Int)
    Lit(Int)
}
function eval(e: Expr) -> Int {
    match e {
        Add(a, b) -> a + b
        Lit(n) -> n
    }
}
function main() {
    print(eval(Add(3, 4)))
    print(eval(Lit(42)))
}
"#,
        &["7", "42"],
    );
}

#[test]
fn match_expression_assigned_to_variable() {
    run_expect(
        r#"
function main() {
    let x: Int = 5
    let result: String = match x {
        0 -> "zero"
        _ -> "nonzero"
    }
    print(result)
}
"#,
        &["nonzero"],
    );
}

// ── Enum Edge Cases ───────────────────────────────────────────────────

#[test]
fn enum_single_variant() {
    run_expect(
        r#"
enum Wrapper {
    Value(Int)
}
function main() {
    let w: Wrapper = Value(42)
    match w {
        Value(n) -> print(n)
    }
}
"#,
        &["42"],
    );
}

#[test]
fn enum_variant_equality_with_data() {
    run_expect(
        r#"
function main() {
    let a: Option<Int> = Some(42)
    let b: Option<Int> = Some(42)
    let c: Option<Int> = Some(99)
    let d: Option<Int> = None
    print(a == b)
    print(a == c)
    print(a == d)
    print(d == d)
}
"#,
        &["true", "false", "false", "true"],
    );
}

#[test]
fn enum_with_impl_methods() {
    run_expect(
        r#"
enum Color { Red Green Blue }
impl Color {
    function name(self) -> String {
        return match self {
            Red -> "red"
            Green -> "green"
            Blue -> "blue"
        }
    }
}
function main() {
    let c: Color = Green
    print(c.name())
}
"#,
        &["green"],
    );
}

#[test]
fn to_string_on_enum_variant() {
    run_expect(
        r#"
function main() {
    let s: Option<Int> = Some(42)
    let n: Option<Int> = None
    print(toString(s))
    print(toString(n))
}
"#,
        &["Some(42)", "None"],
    );
}

#[test]
fn nested_match_expressions() {
    run_expect(
        r#"
function main() {
    let outer: Option<Option<Int>> = Some(Some(42))
    let result: Int = match outer {
        Some(inner) -> match inner {
            Some(val) -> val
            None -> -1
        }
        None -> -2
    }
    print(result)
}
"#,
        &["42"],
    );
}

// ── Struct/enum construction type mismatches ──────────────────────────

#[test]
fn struct_field_type_mismatch_in_construction() {
    expect_type_error(
        r#"
struct Point { Int x  Int y }
function main() {
    let p: Point = Point("hello", 2)
}
"#,
        "field `x`: expected `Int` but got `String`",
    );
}

#[test]
fn enum_variant_field_type_mismatch() {
    expect_type_error(
        r#"
enum Wrapper { Value(Int) }
function main() {
    let w: Wrapper = Value("hello")
}
"#,
        "variant `Value` field 1: expected `Int` but got `String`",
    );
}

// ── Non-exhaustive match at runtime ───────────────────────────────────

#[test]
fn runtime_nonexhaustive_match_on_int() {
    expect_runtime_error(
        r#"
function describe(n: Int) -> String {
    return match n {
        0 -> "zero"
        1 -> "one"
    }
}
function main() {
    print(describe(99))
}
"#,
        "no pattern matched",
    );
}

#[test]
fn enum_variant_display_with_fields() {
    run_expect(
        r#"
enum Shape {
    Circle(Float)
    Rect(Float, Float)
}
function main() {
    let s: Shape = Circle(5.0)
    print(s)
    let r: Shape = Rect(3.0, 4.0)
    print(r)
}
"#,
        &["Circle(5)", "Rect(3, 4)"],
    );
}

#[test]
fn enum_unit_variant_display() {
    run_expect(
        r#"
enum Color { Red  Green  Blue }
function main() {
    let c: Color = Red
    print(c)
}
"#,
        &["Red"],
    );
}

// ── Match arm with block body multiple statements ─────────────────────

#[test]
fn match_block_body_with_statements() {
    run_expect(
        r#"
function describe(n: Int) -> String {
    match n {
        0 -> "zero"
        _ -> {
            let doubled: Int = n * 2
            let msg: String = "value {n} doubled is {doubled}"
            msg
        }
    }
}
function main() {
    print(describe(0))
    print(describe(5))
}
"#,
        &["zero", "value 5 doubled is 10"],
    );
}

// ── Wildcard in variant pattern ───────────────────────────────────────

#[test]
fn wildcard_binding_in_variant_pattern() {
    run_expect(
        r#"
enum Pair { Both(Int, Int) }
function first(p: Pair) -> Int {
    match p {
        Both(a, _) -> a
    }
}
function main() {
    print(first(Both(10, 20)))
}
"#,
        &["10"],
    );
}

// ── Enum variant as function argument ─────────────────────────────────

#[test]
fn enum_variant_as_function_argument() {
    run_expect(
        r#"
function describe(opt: Option<Int>) -> String {
    match opt {
        Some(n) -> "has value: {n}"
        None -> "empty"
    }
}
function main() {
    print(describe(Some(42)))
    print(describe(None))
}
"#,
        &["has value: 42", "empty"],
    );
}

// ── Match with all literal types ──────────────────────────────────────

#[test]
fn match_on_float_literal() {
    run_expect(
        r#"
function describe(f: Float) -> String {
    match f {
        0.0 -> "zero"
        _ -> "nonzero"
    }
}
function main() {
    print(describe(0.0))
    print(describe(3.14))
}
"#,
        &["zero", "nonzero"],
    );
}

// ── Assignment type mismatch ──────────────────────────────────────────

#[test]
fn assignment_type_mismatch_error() {
    expect_type_error(
        r#"
function main() {
    let mut x: Int = 42
    x = "hello"
}
"#,
        "cannot assign",
    );
}

#[test]
fn recursive_enum_linked_list() {
    run_expect(
        r#"
enum IntList { Cons(Int, IntList)  Nil }
function sum(list: IntList) -> Int {
    match list {
        Cons(head, tail) -> head + sum(tail)
        Nil -> 0
    }
}
function main() {
    let list: IntList = Cons(1, Cons(2, Cons(3, Nil)))
    print(sum(list))
}
"#,
        &["6"],
    );
}

#[test]
fn recursive_enum_binary_tree() {
    run_expect(
        r#"
enum Tree { Leaf(Int)  Branch(Tree, Tree) }
function total(t: Tree) -> Int {
    match t {
        Leaf(n) -> n
        Branch(left, right) -> total(left) + total(right)
    }
}
function main() {
    let t: Tree = Branch(Leaf(1), Branch(Leaf(2), Leaf(3)))
    print(total(t))
}
"#,
        &["6"],
    );
}

#[test]
fn match_variant_with_fewer_bindings_than_fields() {
    run_expect(
        r#"
enum Pair {
    Two(Int, Int)
}
function main() {
    let p: Pair = Two(10, 20)
    match p {
        Two(a, _) -> print(a)
    }
}
"#,
        &["10"],
    );
}

/// Variable shadowing inside match arms.
#[test]
fn variable_shadowing_in_match_arm() {
    run_expect(
        r#"
enum Shape {
  Circle(Float)
  Rect(Float, Float)
}
function main() {
  let x: Int = 99
  let s: Shape = Circle(5.0)
  let area: Float = match s {
    Circle(x) -> 3.14 * x * x
    Rect(x, y) -> x * y
  }
  print(x)
  print(area)
}
"#,
        &["99", "78.5"],
    );
}

/// Enum with mixed unit and data variants.
#[test]
fn enum_mixed_unit_and_data_variants() {
    run_expect(
        r#"
enum Token {
  Eof
  Number(Int)
  Text(String, Int)
}
function describe(t: Token) -> String {
  return match t {
    Eof -> "eof"
    Number(n) -> "num:" + toString(n)
    Text(s, len) -> s + "(" + toString(len) + ")"
  }
}
function main() {
  print(describe(Eof))
  print(describe(Number(42)))
  print(describe(Text("hi", 2)))
}
"#,
        &["eof", "num:42", "hi(2)"],
    );
}

/// Break inside match arm is now a compile error.
#[test]
fn break_inside_match_in_loop_e2e_error() {
    expect_type_error(
        r#"
function main() {
  let mut sum: Int = 0
  for i in 0..10 {
    match i {
      5 -> { break }
      _ -> {}
    }
    sum = sum + i
  }
  print(sum)
}
"#,
        "`break` is not allowed inside match arms",
    );
}

/// Continue inside match arm is now a compile error.
#[test]
fn continue_inside_match_in_loop_e2e_error() {
    expect_type_error(
        r#"
function main() {
  let mut sum: Int = 0
  for i in 0..5 {
    match i {
      2 -> { continue }
      _ -> {}
    }
    sum = sum + i
  }
  print(sum)
}
"#,
        "`continue` is not allowed inside match arms",
    );
}

// ── Bug fix: match arm type mismatch with break/continue/return ────

/// A match arm with `break` is now a compile error.
#[test]
fn break_in_match_arm_compile_error() {
    expect_type_error(
        r#"
enum Action { Go  Stop }
function main() {
  let actions: List<Action> = [Go, Go, Stop, Go]
  let mut count: Int = 0
  for a in actions {
    match a {
      Go -> { count = count + 1 }
      Stop -> { break }
    }
  }
  print(count)
}
"#,
        "`break` is not allowed inside match arms",
    );
}

/// A match arm with `continue` is now a compile error.
#[test]
fn continue_in_match_arm_compile_error() {
    expect_type_error(
        r#"
function main() {
  let mut sum: Int = 0
  for i in 0..10 {
    match i % 2 {
      0 -> { sum = sum + i }
      _ -> { continue }
    }
  }
  print(sum)
}
"#,
        "`continue` is not allowed inside match arms",
    );
}

/// An explicit `return` inside a match arm block should exit the enclosing
/// function, not just produce the match arm's value.
#[test]
fn return_in_match_arm_exits_function() {
    run_expect(
        r#"
function findFirstEven(nums: List<Int>) -> Int {
  for n in nums {
    match n % 2 {
      0 -> { return n }
      _ -> { let x: Int = 0 }
    }
  }
  return -1
}
function main() {
  print(findFirstEven([1, 3, 4, 6]))
}
"#,
        &["4"],
    );
}

/// Explicit `return` inside nested if inside match arm should exit the function.
#[test]
fn return_in_nested_if_inside_match_arm() {
    run_expect(
        r#"
enum Shape {
  Circle(Float)
  Rect(Float, Float)
}
impl Shape {
  function describe(self) -> String {
    return match self {
      Circle(_) -> "circle"
      Rect(w, h) -> {
        if w == h { return "square" }
        return "rectangle"
      }
    }
  }
}
function main() {
  let s1: Shape = Rect(3.0, 3.0)
  print(s1.describe())
  let s2: Shape = Rect(3.0, 4.0)
  print(s2.describe())
  let s3: Shape = Circle(1.0)
  print(s3.describe())
}
"#,
        &["square", "rectangle", "circle"],
    );
}

// ── Bug fix: negative integer/float patterns ───────────────────────

/// Match on negative integer literals.
#[test]
fn match_negative_int_pattern() {
    run_expect(
        r#"
function main() {
  let x: Int = -1
  match x {
    -1 -> print("negative one")
    0 -> print("zero")
    1 -> print("one")
    _ -> print("other")
  }
}
"#,
        &["negative one"],
    );
}

/// Match on negative float literals.
#[test]
fn match_negative_float_pattern() {
    run_expect(
        r#"
function main() {
  let x: Float = -3.14
  match x {
    -3.14 -> print("neg pi-ish")
    0.0 -> print("zero")
    _ -> print("other")
  }
}
"#,
        &["neg pi-ish"],
    );
}

/// Negative pattern that doesn't match falls through to wildcard.
#[test]
fn match_negative_pattern_no_match() {
    run_expect(
        r#"
function main() {
  let x: Int = 5
  match x {
    -1 -> print("neg one")
    _ -> print("other")
  }
}
"#,
        &["other"],
    );
}

// --- Match arm implicit vs explicit return regression tests ---

/// A match arm block with an unexecuted `return` and an implicit return
/// value must use the implicit value as the match result, NOT propagate
/// it as a function-level return.
#[test]
fn match_arm_implicit_return_not_confused_with_explicit() {
    run_expect(
        r#"
function choose(x: Int) -> Int {
  let result: Int = match x {
    1 -> {
      if false {
        return 100
      }
      42
    }
    _ -> 0
  }
  return result + 1
}

function main() {
  print(toString(choose(1)))
}
"#,
        &["43"],
    );
}

/// When the explicit `return` inside a match arm IS executed, it should
/// propagate as a function-level return, skipping the rest of the function.
#[test]
fn match_arm_explicit_return_propagates_correctly() {
    run_expect(
        r#"
function choose(x: Int) -> Int {
  let result: Int = match x {
    1 -> {
      if true {
        return 100
      }
      42
    }
    _ -> 0
  }
  return result + 1
}

function main() {
  print(toString(choose(1)))
}
"#,
        &["100"],
    );
}

/// A match arm block with no explicit return should always produce its
/// implicit value as the match result.
#[test]
fn match_arm_block_implicit_return_basic() {
    run_expect(
        r#"
function choose(x: Int) -> Int {
  let result: Int = match x {
    1 -> {
      let a: Int = 10
      let b: Int = 20
      a + b
    }
    _ -> 0
  }
  return result + 5
}

function main() {
  print(toString(choose(1)))
}
"#,
        &["35"],
    );
}

/// Regression: return inside a while loop within a match arm block should
/// propagate as a function return, not a match result.
#[test]
fn match_arm_return_inside_while_propagates() {
    run_expect(
        r#"
function test(x: Int) -> Int {
  let result: Int = match x {
    1 -> {
      let mut i: Int = 0
      while i < 3 {
        if i == 1 {
          return 999
        }
        i = i + 1
      }
      0
    }
    _ -> 0
  }
  return result + 1
}

function main() {
  print(toString(test(1)))
}
"#,
        &["999"],
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Feature interaction: pattern matching on nested generic types
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn match_nested_option() {
    run_expect(
        r#"
function main() {
  let x: Option<Option<Int>> = Some(Some(42))
  match x {
    Some(inner) -> {
      match inner {
        Some(v) -> print(v)
        None -> print("inner none")
      }
    }
    None -> print("outer none")
  }
}
"#,
        &["42"],
    );
}

#[test]
fn match_nested_option_inner_none() {
    run_expect(
        r#"
function main() {
  let x: Option<Option<Int>> = Some(None)
  match x {
    Some(inner) -> {
      match inner {
        Some(v) -> print(v)
        None -> print("inner none")
      }
    }
    None -> print("outer none")
  }
}
"#,
        &["inner none"],
    );
}

#[test]
fn match_nested_option_outer_none() {
    run_expect(
        r#"
function main() {
  let x: Option<Option<Int>> = None
  match x {
    Some(inner) -> {
      match inner {
        Some(v) -> print(v)
        None -> print("inner none")
      }
    }
    None -> print("outer none")
  }
}
"#,
        &["outer none"],
    );
}

#[test]
fn match_nested_result() {
    run_expect(
        r#"
function main() {
  let x: Result<Option<Int>, String> = Ok(Some(99))
  match x {
    Ok(opt) -> {
      match opt {
        Some(v) -> print(v)
        None -> print("none inside ok")
      }
    }
    Err(e) -> print(e)
  }
}
"#,
        &["99"],
    );
}

#[test]
fn match_generic_enum_with_list() {
    run_expect(
        r#"
function main() {
  let x: Option<List<Int>> = Some([1, 2, 3])
  match x {
    Some(items) -> print(items.length())
    None -> print("none")
  }
}
"#,
        &["3"],
    );
}
