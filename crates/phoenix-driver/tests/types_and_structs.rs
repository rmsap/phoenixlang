mod common;
use common::*;

#[test]
fn struct_and_methods() {
    run_expect(
        r#"
struct Point {
  Int x
  Int y

  function sum(self) -> Int {
    return self.x + self.y
  }
}
function main() {
  let p: Point = Point(3, 4)
  print(p.sum())
}
"#,
        &["7"],
    );
}

#[test]
fn undefined_variable_caught() {
    expect_type_error(r#"function main() { print(x) }"#, "undefined variable");
}

#[test]
fn immutable_assignment_caught() {
    expect_type_error(
        r#"function main() { let x: Int = 1
x = 2 }"#,
        "cannot assign to immutable",
    );
}

/// Comprehensive integration test for generics across functions, structs,
/// enums, and pattern matching.
///
/// Exercises:
/// - Generic function declaration and call with type inference
/// - Generic struct with multiple type parameters
/// - Generic enum with value-carrying and unit variants
/// - Match on a generic enum
/// - Generic higher-order function with closure argument
#[test]
fn generics() {
    run_expect(
        r#"
enum Option<T> {
  Some(T)
  None
}

struct Pair<A, B> {
  A first
  B second
}

function identity<T>(x: T) -> T {
  return x
}

function unwrapOr<T>(opt: Option<T>, defaultVal: T) -> T {
  return match opt {
    Some(v) -> v
    None -> defaultVal
  }
}

function map<T, U>(x: T, f: (T) -> U) -> U {
  return f(x)
}

function main() {
  let a: Int = identity(42)
  print(a)

  let b: String = identity("hello")
  print(b)

  let p: Pair<Int, String> = Pair(1, "world")
  print(p.first)
  print(p.second)

  let someVal: Option<Int> = Some(10)
  let noneVal: Option<Int> = None
  let x: Int = unwrapOr(someVal, 0)
  print(x)
  let y: Int = unwrapOr(noneVal, 99)
  print(y)

  let someVal2: Option<Int> = Some(10)
  match someVal2 {
    Some(v) -> print(v)
    None -> print(0)
  }

  let mapped: String = map(123, function(n: Int) -> String { return toString(n) })
  print(mapped)
}
"#,
        &["42", "hello", "1", "world", "10", "99", "10", "123"],
    );
}

/// Nested generics: Option<List<Int>>, List of Options, etc.
#[test]
fn nested_generics() {
    run_expect(
        r#"
function main() {
  let optList: Option<List<Int>> = Some([1, 2, 3])
  match optList {
    Some(nums) -> print(nums.get(0))
    None -> print(0)
  }

  let listOpts: List<Option<Int>> = [Some(10), None, Some(30)]
  match listOpts.get(0) {
    Some(v) -> print(v)
    None -> print(0)
  }
  match listOpts.get(1) {
    Some(v) -> print(v)
    None -> print(0)
  }

  let nestedOpt: Option<Option<Int>> = Some(Some(42))
  match nestedOpt {
    Some(inner) -> {
      match inner {
        Some(v) -> print(v)
        None -> print(0)
      }
    }
    None -> print(0)
  }
}
"#,
        &["1", "10", "0", "42"],
    );
}

/// All values — including structs, enums, and lists — can be freely reused
/// after assignment.  Phoenix uses garbage collection, so there is no
/// ownership transfer.
#[test]
fn values_reusable_after_assignment() {
    // Primitives can be reused after assignment
    run_expect(
        r#"
function main() {
  let x: Int = 42
  let y: Int = x
  print(x)
  print(y)

  let s: String = "hello"
  let t: String = s
  print(s)
  print(t)

  let a: Bool = true
  let b: Bool = a
  print(a)

  let f: Float = 3.14
  let g: Float = f
  print(f)
}
"#,
        &["42", "42", "hello", "hello", "true", "3.14"],
    );

    // Structs can be reused after assignment to another variable
    run_expect(
        r#"
struct Point {
  Int x
  Int y
}
function main() {
  let p: Point = Point(1, 2)
  let q: Point = p
  print(p.x)
  print(q.x)
}
"#,
        &["1", "1"],
    );

    // Lists can be reused after assignment to another variable
    run_expect(
        r#"
function main() {
  let a: List<Int> = [1, 2, 3]
  let b: List<Int> = a
  print(a.length())
  print(b.length())
}
"#,
        &["3", "3"],
    );

    // Structs can be reused after being passed to a function
    run_expect(
        r#"
struct Point {
  Int x
  Int y
}
function take(p: Point) { print(p.x) }
function main() {
  let p: Point = Point(1, 2)
  take(p)
  print(p.x)
}
"#,
        &["1", "1"],
    );

    // Enums (Option) can be reused after assignment to another variable
    run_expect(
        r#"
function main() {
  let a: Option<Int> = Some(42)
  let b: Option<Int> = a
  print(a.isSome())
  print(b.isSome())
}
"#,
        &["true", "true"],
    );

    // Method call does not affect reusability of receiver
    run_expect(
        r#"
struct Counter {
  Int value

  function get(self) -> Int { return self.value }
}
function main() {
  let c: Counter = Counter(10)
  let v: Int = c.get()
  print(v)
  print(c.value)
}
"#,
        &["10", "10"],
    );
}

/// Values can be shared freely across function calls, closures, and
/// conditional branches without restriction.
#[test]
fn values_shared_freely() {
    // Same struct passed as two arguments to a function
    run_expect(
        r#"
struct Point {
  Int x
  Int y
}
function both(a: Point, b: Point) { print(a.x) }
function main() {
  let p: Point = Point(1, 2)
  both(p, p)
}
"#,
        &["1"],
    );

    // Struct used in a closure and still usable afterwards
    run_expect(
        r#"
struct Point {
  Int x
  Int y
}
function main() {
  let p: Point = Point(1, 2)
  let q: Point = p
  let f: (Int) -> Int = function(x: Int) -> Int { return p.x }
  print(p.x)
}
"#,
        &["1"],
    );

    // Struct used after being passed to a function that returns it
    run_expect(
        r#"
struct Point {
  Int x
  Int y
}
function take(p: Point) -> Point { return p }
function main() {
  let p: Point = Point(1, 2)
  let q: Point = take(p)
  print(p.x)
}
"#,
        &["1"],
    );

    // Variable used after being passed to a function inside an if branch
    run_expect(
        r#"
struct Data { Int value }
function take(d: Data) { print(d.value) }
function main() {
  let d: Data = Data(42)
  if true {
    take(d)
  }
  print(d.value)
}
"#,
        &["42", "42"],
    );
}

// ── 1.8.3: Field Assignment ────────────────────────────────────────

#[test]
fn field_assignment_basic() {
    run_expect(
        r#"
struct Point {
  Int x
  Int y
}
function main() {
  let mut p: Point = Point(1, 2)
  p.x = 10
  print(p.x)
  print(p.y)
}
"#,
        &["10", "2"],
    );
}

#[test]
fn field_assignment_multiple_fields() {
    run_expect(
        r#"
struct Point {
  Int x
  Int y
}
function main() {
  let mut p: Point = Point(0, 0)
  p.x = 3
  p.y = 4
  print(p.x)
  print(p.y)
}
"#,
        &["3", "4"],
    );
}

#[test]
fn field_assignment_immutable_error() {
    expect_type_error(
        r#"
struct Point { Int x  Int y }
function main() {
  let p: Point = Point(1, 2)
  p.x = 10
}
"#,
        "cannot assign to field of immutable variable",
    );
}

#[test]
fn field_assignment_wrong_type_error() {
    expect_type_error(
        r#"
struct Point { Int x  Int y }
function main() {
  let mut p: Point = Point(1, 2)
  p.x = "hello"
}
"#,
        "type mismatch",
    );
}

#[test]
fn field_assignment_unknown_field_error() {
    expect_type_error(
        r#"
struct Point { Int x  Int y }
function main() {
  let mut p: Point = Point(1, 2)
  p.z = 10
}
"#,
        "has no field `z`",
    );
}

#[test]
fn field_assignment_nested() {
    run_expect(
        r#"
struct Inner {
  Int value
}
struct Outer {
  Inner inner
}
function main() {
  let mut o: Outer = Outer(Inner(1))
  o.inner.value = 42
  print(o.inner.value)
}
"#,
        &["42"],
    );
}

#[test]
fn field_assignment_with_expression() {
    run_expect(
        r#"
struct Counter {
  Int count
}
function main() {
  let mut c: Counter = Counter(0)
  c.count = c.count + 1
  c.count = c.count + 1
  c.count = c.count + 1
  print(c.count)
}
"#,
        &["3"],
    );
}

// ── 1.8.4: Type Alias ─────────────────────────────────────────────

#[test]
fn type_alias_simple() {
    run_expect(
        r#"
type Id = Int
function main() {
  let x: Id = 42
  print(x)
}
"#,
        &["42"],
    );
}

#[test]
fn type_alias_function_type() {
    run_expect(
        r#"
type IntTransform = (Int) -> Int
function apply(f: IntTransform, x: Int) -> Int {
  return f(x)
}
function main() {
  let double: IntTransform = function(x: Int) -> Int { return x * 2 }
  print(apply(double, 5))
}
"#,
        &["10"],
    );
}

#[test]
fn type_alias_generic() {
    run_expect(
        r#"
type StringResult<T> = Result<T, String>
function parseInt() -> StringResult<Int> {
  return Ok(42)
}
function main() {
  let r: StringResult<Int> = parseInt()
  print(r.unwrap())
}
"#,
        &["42"],
    );
}

#[test]
fn type_alias_generic_with_error() {
    run_expect(
        r#"
type StringResult<T> = Result<T, String>
function parseInt() -> StringResult<Int> {
  return Err("not a number")
}
function main() {
  let r: StringResult<Int> = parseInt()
  print(r.isErr())
}
"#,
        &["true"],
    );
}

#[test]
fn type_alias_option_shorthand() {
    run_expect(
        r#"
type MaybeInt = Option<Int>
function find(present: Bool) -> MaybeInt {
  if present { return Some(42) }
  return None
}
function main() {
  let a: MaybeInt = find(true)
  let b: MaybeInt = find(false)
  print(a.unwrap())
  print(b.isNone())
}
"#,
        &["42", "true"],
    );
}

#[test]
fn type_alias_in_struct() {
    run_expect(
        r#"
type Name = String
struct User {
  Name name
  Int age
}
function main() {
  let u: User = User("Alice", 30)
  print(u.name)
}
"#,
        &["Alice"],
    );
}

#[test]
fn field_assignment_with_interpolation() {
    run_expect(
        r#"
struct Person {
  String name
  Int age
}
function main() {
  let mut p: Person = Person("Alice", 25)
  p.age = 26
  print("{p.name} is now {p.age}")
}
"#,
        &["Alice is now 26"],
    );
}

// ── 1.8 Edge cases: Field Assignment ───────────────────────────────

#[test]
fn field_assignment_deeply_nested() {
    run_expect(
        r#"
struct D { Int val }
struct C { D d }
struct B { C c }
function main() {
  let mut b: B = B(C(D(0)))
  b.c.d.val = 42
  print(b.c.d.val)
}
"#,
        &["42"],
    );
}

#[test]
fn field_assignment_on_non_struct_type() {
    expect_type_error(
        r#"
function main() {
  let mut x: Int = 42
  x.foo = 10
}
"#,
        "cannot assign to field on non-struct type",
    );
}

/// Field assignment on a variable that was also assigned to another variable
/// is valid under GC — no ownership restrictions.
#[test]
fn field_assignment_after_reassignment() {
    run_expect(
        r#"
struct Point { Int x  Int y }
function main() {
  let mut p: Point = Point(1, 2)
  let q: Point = p
  p.x = 10
  print(p.x)
}
"#,
        &["10"],
    );
}

// ── 1.8 Edge cases: Type Alias ─────────────────────────────────────

#[test]
fn type_alias_of_alias() {
    run_expect(
        r#"
type A = Int
type B = A
function main() {
  let x: B = 42
  print(x)
}
"#,
        &["42"],
    );
}

#[test]
fn type_alias_in_function_parameter() {
    run_expect(
        r#"
type Id = Int
function double(x: Id) -> Id {
  return x * 2
}
function main() {
  print(double(5))
}
"#,
        &["10"],
    );
}

#[test]
fn deeply_nested_field_assignment() {
    run_expect(
        r#"
struct Inner { Int value }
struct Middle { Inner inner }
struct Outer { Middle middle }
function main() {
  let mut o: Outer = Outer(Middle(Inner(1)))
  o.middle.inner.value = 42
  print(o.middle.inner.value)
}
"#,
        &["42"],
    );
}

#[test]
fn generic_type_alias_requires_args() {
    expect_type_error(
        r#"
type StringResult<T> = Result<T, String>
function main() {
  let x: StringResult = Ok(42)
}
"#,
        "generic type alias `StringResult` requires type arguments",
    );
}

#[test]
fn field_assignment_wrong_type() {
    expect_type_error(
        r#"
struct Point {
  Int x
  Int y
}
function main() {
  let mut p: Point = Point(1, 2)
  p.x = "hello"
}
"#,
        "type mismatch",
    );
}

#[test]
fn circular_type_alias_error() {
    expect_type_error(
        r#"
type A = B
type B = A
function main() {
  let x: A = 42
}
"#,
        "unknown type",
    );
}

#[test]
fn chained_type_alias() {
    run_expect(
        r#"
type A = Int
type B = A
function main() {
  let x: B = 42
  print(x)
}
"#,
        &["42"],
    );
}

#[test]
fn struct_field_order_deterministic() {
    // After switching to BTreeMap, struct display order should be alphabetical.
    // This test verifies the fix for non-deterministic HashMap ordering.
    run_expect(
        r#"
struct Info {
  Int age
  String name
}
function main() {
  let i: Info = Info(30, "Alice")
  print(i)
}
"#,
        &["Info(age: 30, name: Alice)"],
    );
}

#[test]
fn type_alias_self_reference_error() {
    expect_type_error(
        r#"
type A = A
function main() { }
"#,
        "type alias `A` refers to itself",
    );
}

#[test]
fn generic_type_arg_count_mismatch() {
    expect_type_error(
        r#"
struct Pair<A, B> {
  A first
  B second
}
function main() {
  let p: Pair<Int> = Pair<Int>(1, 2)
}
"#,
        "expects 2 type argument(s), got 1",
    );
}

#[test]
fn missing_return_not_triggered_with_explicit_return() {
    run_expect(
        r#"
function classify(n: Int) -> String {
  if n < 0 {
    return "negative"
  } else {
    return "non-negative"
  }
}
function main() {
  print(classify(5))
}
"#,
        &["non-negative"],
    );
}

#[test]
fn transitive_type_alias_cycle() {
    // C -> B -> A: when C is registered, B already exists and points to A,
    // and A already exists. The cycle is C -> B -> A, and the checker should
    // detect that C transitively reaches itself (or that A references something
    // in the chain). Since aliases are registered sequentially and A=Int, B=A
    // resolves to Int, C=B resolves to Int — no cycle here.
    // The real cycle case (A=B, B=A) requires forward references which are
    // already caught as "unknown type". This test validates the direct case.
    expect_type_error(
        r#"
type A = A
function main() { }
"#,
        "refers to itself",
    );
}

// --- B7: struct literal wrong argument count ---

#[test]
fn struct_construction_too_many_args() {
    expect_type_error(
        r#"
struct Point { Int x Int y }
function main() {
    let p: Point = Point(1, 2, 3)
    print(p.x)
}
"#,
        "has 2 field(s), got 3",
    );
}

#[test]
fn struct_construction_too_few_args() {
    expect_type_error(
        r#"
struct Point { Int x Int y }
function main() {
    let p: Point = Point(1)
    print(p.x)
}
"#,
        "has 2 field(s), got 1",
    );
}

// --- Nested field assignment ---

#[test]
fn nested_field_assignment() {
    run_expect(
        r#"
struct Point { Int x Int y }
struct Rect { Point origin Point size }
function main() {
    let mut r: Rect = Rect(Point(0, 0), Point(100, 50))
    r.origin.x = 10
    r.origin.y = 20
    print(r.origin.x)
    print(r.origin.y)
}
"#,
        &["10", "20"],
    );
}

// --- Generic edge cases ---

#[test]
fn generic_function_multiple_type_params() {
    run_expect(
        r#"
function swap<A, B>(a: A, b: B) -> B {
    return b
}
function main() {
    print(swap(1, "hello"))
    print(swap("world", 42))
}
"#,
        &["hello", "42"],
    );
}

#[test]
fn nested_generics_in_annotations() {
    run_expect(
        r#"
function main() {
    let x: Option<List<Int>> = Some([1, 2, 3])
    match x {
        Some(list) -> print(list.length())
        None -> print(0)
    }
}
"#,
        &["3"],
    );
}

// --- Struct with method chaining ---

#[test]
fn struct_method_chaining() {
    run_expect(
        r#"
struct Counter {
    Int value

    function get(self) -> Int { return self.value }
}
function main() {
    let c: Counter = Counter(42)
    print(c.get())
}
"#,
        &["42"],
    );
}

// --- Lexer: angle brackets for generics ---

#[test]
fn lexer_generic_type_angle_brackets() {
    let tokens = phoenix_lexer::tokenize("List<Int>", phoenix_common::span::SourceId(0));
    let kinds: Vec<_> = tokens.iter().map(|t| t.kind).collect();
    assert_eq!(
        kinds,
        vec![
            phoenix_lexer::TokenKind::Ident,
            phoenix_lexer::TokenKind::Lt,
            phoenix_lexer::TokenKind::IntType,
            phoenix_lexer::TokenKind::Gt,
            phoenix_lexer::TokenKind::Eof,
        ]
    );
}

// --- Field assignment immutability checks ---

#[test]
fn field_assignment_on_immutable_struct_error() {
    expect_type_error(
        r#"
struct Point { Int x Int y }
function main() {
    let p: Point = Point(1, 2)
    p.x = 10
}
"#,
        "immutable variable",
    );
}

#[test]
fn variable_type_inference() {
    run_expect(
        r#"
function main() {
    let x = 42
    print(x)
    let s = "hello"
    print(s)
    let b = true
    print(b)
    let f = 3.14
    print(f)
}
"#,
        &["42", "hello", "true", "3.14"],
    );
}

#[test]
fn mutable_variable_reassignment_multiple() {
    run_expect(
        r#"
function main() {
    let mut x: Int = 1
    x = 2
    x = 3
    x = 4
    print(x)
}
"#,
        &["4"],
    );
}

#[test]
fn variable_already_defined_error() {
    expect_type_error(
        r#"
function main() {
    let x: Int = 1
    let x: Int = 2
}
"#,
        "already defined",
    );
}

// ── Struct Edge Cases ─────────────────────────────────────────────────

#[test]
fn multiple_impl_blocks_for_same_struct() {
    run_expect(
        r#"
struct Point { Int x  Int y }
impl Point {
    function getX(self) -> Int { return self.x }
}
impl Point {
    function getY(self) -> Int { return self.y }
}
function main() {
    let p: Point = Point(3, 4)
    print(p.getX())
    print(p.getY())
}
"#,
        &["3", "4"],
    );
}

#[test]
fn struct_method_calls_another_method() {
    run_expect(
        r#"
struct Point {
    Int x
    Int y

    function sum(self) -> Int { return self.x + self.y }
    function describe(self) -> String {
        return "sum=" + toString(self.sum())
    }
}
function main() {
    let p: Point = Point(3, 4)
    print(p.describe())
}
"#,
        &["sum=7"],
    );
}

#[test]
fn struct_as_return_type() {
    run_expect(
        r#"
struct Point { Int x  Int y }
function makePoint(x: Int, y: Int) -> Point {
    return Point(x, y)
}
function main() {
    let p: Point = makePoint(10, 20)
    print(p.x)
    print(p.y)
}
"#,
        &["10", "20"],
    );
}

#[test]
fn struct_passed_to_function() {
    run_expect(
        r#"
struct Point { Int x  Int y }
function sumPoint(p: Point) -> Int {
    return p.x + p.y
}
function main() {
    let p: Point = Point(3, 4)
    print(sumPoint(p))
}
"#,
        &["7"],
    );
}

#[test]
fn generic_struct_field_access() {
    run_expect(
        r#"
struct Wrapper<T> { T value }
function main() {
    let w: Wrapper<Int> = Wrapper(42)
    print(w.value)
    let s: Wrapper<String> = Wrapper("hello")
    print(s.value)
}
"#,
        &["42", "hello"],
    );
}

#[test]
fn return_type_mismatch() {
    expect_type_error(
        r#"
function foo() -> Int {
    return "hello"
}
function main() { }
"#,
        "return type mismatch",
    );
}

// ── Generic Edge Cases ────────────────────────────────────────────────

#[test]
fn generic_function_called_with_different_types() {
    run_expect(
        r#"
function identity<T>(x: T) -> T {
    return x
}
function main() {
    let a: Int = identity(42)
    let b: String = identity("hello")
    let c: Bool = identity(true)
    let d: Float = identity(3.14)
    print(a)
    print(b)
    print(c)
    print(d)
}
"#,
        &["42", "hello", "true", "3.14"],
    );
}

#[test]
fn generic_struct_same_type_both_params() {
    run_expect(
        r#"
struct Pair<A, B> {
    A first
    B second
}
function main() {
    let p: Pair<Int, Int> = Pair(10, 20)
    print(p.first)
    print(p.second)
}
"#,
        &["10", "20"],
    );
}

#[test]
fn generic_type_arg_count_too_many() {
    expect_type_error(
        r#"
struct Box<T> { T value }
function main() {
    let b: Box<Int, String> = Box(42)
}
"#,
        "expects 1 type argument(s), got 2",
    );
}

// ── Type Alias Advanced ───────────────────────────────────────────────

#[test]
fn type_alias_in_return_type() {
    run_expect(
        r#"
type Id = Int
function getId() -> Id {
    return 42
}
function main() {
    let x: Id = getId()
    print(x)
}
"#,
        &["42"],
    );
}

#[test]
fn type_alias_for_list() {
    run_expect(
        r#"
type IntList = List<Int>
function main() {
    let nums: IntList = [1, 2, 3]
    print(nums.length())
    print(nums.get(0))
}
"#,
        &["3", "1"],
    );
}

#[test]
fn type_alias_for_function_type_in_return() {
    run_expect(
        r#"
type Transform = (Int) -> Int
function makeDoubler() -> Transform {
    return function(x: Int) -> Int { return x * 2 }
}
function main() {
    let f: Transform = makeDoubler()
    print(f(5))
}
"#,
        &["10"],
    );
}

// ── Cross-feature Interactions ────────────────────────────────────────

#[test]
fn for_loop_with_struct_mutation() {
    run_expect(
        r#"
struct Counter { Int value }
function main() {
    let mut c: Counter = Counter(0)
    for i in 0..5 {
        c.value = c.value + i
    }
    print(c.value)
}
"#,
        &["10"],
    );
}

#[test]
fn field_assignment_in_while_loop() {
    run_expect(
        r#"
struct Counter { Int value }
function main() {
    let mut c: Counter = Counter(0)
    while c.value < 5 {
        c.value = c.value + 1
    }
    print(c.value)
}
"#,
        &["5"],
    );
}

#[test]
fn no_main_function_error() {
    expect_runtime_error(
        r#"
function foo() {
    print("hello")
}
"#,
        "no main()",
    );
}

// ── Misc edge cases ───────────────────────────────────────────────────

#[test]
fn negative_integer_literal() {
    run_expect(
        r#"
function main() {
    let x: Int = -42
    print(x)
}
"#,
        &["-42"],
    );
}

#[test]
fn method_on_field_access_result() {
    run_expect(
        r#"
struct Wrapper { List<Int> items }
function main() {
    let w: Wrapper = Wrapper([1, 2, 3])
    print(w.items.length())
    print(w.items.get(2))
}
"#,
        &["3", "3"],
    );
}

// ── Type inference edge cases ─────────────────────────────────────────

#[test]
fn type_inference_void_initializer_error() {
    expect_type_error(
        r#"
function doNothing() { }
function main() {
    let x = doNothing()
}
"#,
        "cannot infer type",
    );
}

#[test]
fn type_inference_ambiguous_generic_error() {
    expect_type_error(
        r#"
function main() {
    let x = None
}
"#,
        "cannot infer type",
    );
}

// ── Field access on non-struct ────────────────────────────────────────

#[test]
fn field_access_unknown_field_error() {
    expect_type_error(
        r#"
struct Point { Int x  Int y }
function main() {
    let p: Point = Point(1, 2)
    print(p.z)
}
"#,
        "has no field `z`",
    );
}

// ── Struct/enum display format ────────────────────────────────────────

#[test]
fn struct_display_format() {
    run_expect(
        r#"
struct Point { Int x  Int y }
function main() {
    let p: Point = Point(3, 4)
    print(p)
}
"#,
        &["Point(x: 3, y: 4)"],
    );
}

// ── Type alias indirect cycle ─────────────────────────────────────────

#[test]
fn type_alias_indirect_cycle_error() {
    expect_type_error(
        r#"
type A = B
type B = C
type C = A
function main() { }
"#,
        "unknown type",
    );
}

// ── Multiple errors accumulated (checker doesn't stop at first) ───────

#[test]
fn multiple_type_errors_reported() {
    let source = r#"
function main() {
    let x: Int = "hello"
    let y: String = 42
}
"#;
    let tokens = phoenix_lexer::lexer::tokenize(source, phoenix_common::span::SourceId(0));
    let (program, parse_errors) = phoenix_parser::parser::parse(&tokens);
    assert!(parse_errors.is_empty());
    let check_result = phoenix_sema::checker::check(&program);
    assert!(
        check_result.diagnostics.len() >= 2,
        "expected at least 2 type errors, got {}",
        check_result.diagnostics.len()
    );
}

// ── Struct with many fields ───────────────────────────────────────────

#[test]
fn struct_with_many_fields() {
    run_expect(
        r#"
struct Config {
    String host
    Int port
    Bool debug
    Float timeout
}
function main() {
    let c: Config = Config("localhost", 8080, true, 30.0)
    print(c.host)
    print(c.port)
    print(c.debug)
    print(c.timeout)
}
"#,
        &["localhost", "8080", "true", "30"],
    );
}

// ── Recursive data via enum ───────────────────────────────────────────

// NOTE: Recursive enum types (e.g. Cons(Int, IntList)) are a known
// limitation — the checker doesn't resolve self-referential type names
// in variant fields. See PLAN.md "Limitation: Recursive struct/enum
// definitions not detected" and Phase 1.10.

// ── Method with multiple parameters ───────────────────────────────────

#[test]
fn method_with_multiple_params() {
    run_expect(
        r#"
struct Rect {
    Float width
    Float height

    function scale(self, factor: Float) -> Rect {
        return Rect(self.width * factor, self.height * factor)
    }
}
function main() {
    let r: Rect = Rect(10.0, 5.0)
    let scaled: Rect = r.scale(2.0)
    print(scaled.width)
    print(scaled.height)
}
"#,
        &["20", "10"],
    );
}

// ── Empty struct ──────────────────────────────────────────────────────

#[test]
fn empty_struct() {
    run_expect(
        r#"
struct Unit { }
function main() {
    let u: Unit = Unit()
    print(u)
}
"#,
        &["Unit()"],
    );
}

// ── Chained method calls ──────────────────────────────────────────────

#[test]
fn chained_method_calls() {
    run_expect(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3]
    let extended: List<Int> = nums.push(4).push(5)
    print(extended.length())
    print(extended.get(3))
    print(extended.get(4))
}
"#,
        &["5", "4", "5"],
    );
}

// ── Deeply nested struct field access ─────────────────────────────────

#[test]
fn deeply_nested_field_access() {
    run_expect(
        r#"
struct Inner { Int value }
struct Middle { Inner inner }
struct Outer { Middle middle }
function main() {
    let o: Outer = Outer(Middle(Inner(42)))
    print(o.middle.inner.value)
}
"#,
        &["42"],
    );
}

// ── Reassign mutable variable to different value of same type ─────────

#[test]
fn mutable_struct_reassignment() {
    run_expect(
        r#"
struct Point { Int x  Int y }
function main() {
    let mut p: Point = Point(1, 2)
    print(p.x)
    p = Point(10, 20)
    print(p.x)
}
"#,
        &["1", "10"],
    );
}

// ── Undefined variable in expression ──────────────────────────────────

#[test]
fn undefined_variable_in_expression() {
    expect_type_error(
        r#"
function main() {
    let x: Int = y + 1
}
"#,
        "undefined variable",
    );
}

#[test]
fn output_struct_fields() {
    run_expect(
        r#"
struct Point { Int x  Int y }
function main() {
  let p: Point = Point(3, 7)
  print(p.x)
  print(p.y)
}
"#,
        &["3", "7"],
    );
}

// NOTE: Strings nested inside interpolation expressions (e.g. `"outer {func("inner")}"`)
// are not supported because the lexer terminates the outer string at the inner quote.
// The parser-level brace-counting fix handles braces from non-string contexts
// (e.g. nested braces in expressions). Full nested-string support would require
// an interpolation-aware lexer.

#[test]
fn generic_impl_method_returns_type_param() {
    run_expect(
        r#"
struct Wrapper<T> {
    T value

    function get(self) -> T { self.value }
}
function main() {
    let w: Wrapper<Int> = Wrapper(42)
    print(w.get())
}
"#,
        &["42"],
    );
}

#[test]
fn generic_impl_method_uses_type_param_in_param() {
    run_expect(
        r#"
struct Box<T> {
    T value

    function set(self, newVal: T) -> Box<T> {
        Box(newVal)
    }
}
function main() {
    let b: Box<Int> = Box(1)
    let b2: Box<Int> = b.set(42)
    print(b2.value)
}
"#,
        &["42"],
    );
}

#[test]
fn generic_impl_two_type_params() {
    run_expect(
        r#"
struct Pair<A, B> {
    A first
    B second

    function getFirst(self) -> A { self.first }
    function getSecond(self) -> B { self.second }
}
function main() {
    let p: Pair<Int, String> = Pair(1, "hello")
    print(p.getFirst())
    print(p.getSecond())
}
"#,
        &["1", "hello"],
    );
}

// ── Destructuring bindings ──────────────────────────────────────────

#[test]
fn destructuring_struct_basic() {
    run_expect(
        r#"
struct Point { Int x  Int y }
function main() {
    let p: Point = Point(3, 7)
    let Point { x, y } = p
    print(x)
    print(y)
}
"#,
        &["3", "7"],
    );
}

#[test]
fn destructuring_struct_from_function() {
    run_expect(
        r#"
struct Pair { Int a  Int b }
function makePair() -> Pair { Pair(10, 20) }
function main() {
    let Pair { a, b } = makePair()
    print(a)
    print(b)
}
"#,
        &["10", "20"],
    );
}

#[test]
fn inline_method_on_struct() {
    run_expect(
        r#"
struct Point {
    Int x
    Int y

    function magnitude(self) -> Int {
        self.x * self.x + self.y * self.y
    }
}
function main() {
    let p: Point = Point(3, 4)
    print(p.magnitude())
}
"#,
        &["25"],
    );
}

#[test]
fn inline_method_on_enum() {
    run_expect(
        r#"
enum Shape {
    Circle(Float)
    Rect(Float, Float)

    function describe(self) -> String {
        match self {
            Circle(r) -> "circle"
            Rect(w, h) -> "rectangle"
        }
    }
}
function main() {
    let s: Shape = Circle(5.0)
    print(s.describe())
}
"#,
        &["circle"],
    );
}

#[test]
fn standalone_impl_still_works() {
    run_expect(
        r#"
struct Counter { Int value }
impl Counter {
    function increment(self) -> Counter {
        Counter(self.value + 1)
    }
}
function main() {
    let c: Counter = Counter(0)
    let c2: Counter = c.increment()
    print(c2.value)
}
"#,
        &["1"],
    );
}

// ── Inline methods edge cases ───────────────────────────────────────────

#[test]
fn inline_method_generic_struct() {
    run_expect(
        r#"
struct Box<T> {
    T value

    function unwrap(self) -> T {
        self.value
    }
}
function main() {
    let b: Box<Int> = Box(42)
    print(b.unwrap())
    let s: Box<String> = Box("hello")
    print(s.unwrap())
}
"#,
        &["42", "hello"],
    );
}

#[test]
fn inline_method_and_standalone_impl_together() {
    run_expect(
        r#"
struct Num {
    Int value

    function doubled(self) -> Int {
        self.value * 2
    }
}
impl Num {
    function tripled(self) -> Int {
        self.value * 3
    }
}
function main() {
    let n: Num = Num(5)
    print(n.doubled())
    print(n.tripled())
}
"#,
        &["10", "15"],
    );
}

// ── Recursive types edge cases ──────────────────────────────────────────

#[test]
fn recursive_generic_type() {
    // Generic recursive types work when constructed one level at a time
    // (nested construction like GCons("a", GCons("b", GNil)) has a
    // type inference limitation with multi-level generic variant nesting)
    run_expect(
        r#"
enum GList<T> { GCons(T, GList<T>)  GNil }
function glength<T>(list: GList<T>) -> Int {
    match list {
        GCons(head, tail) -> 1 + glength(tail)
        GNil -> 0
    }
}
function main() {
    let nil: GList<String> = GNil
    let c: GList<String> = GCons("c", nil)
    let b: GList<String> = GCons("b", c)
    let a: GList<String> = GCons("a", b)
    print(glength(a))
}
"#,
        &["3"],
    );
}

// ── Generic impl edge cases ─────────────────────────────────────────────

#[test]
fn generic_impl_method_on_enum() {
    run_expect(
        r#"
enum Wrapper<T> {
    Wrapped(T)
    Empty

    function getOr(self, default: T) -> T {
        match self {
            Wrapped(v) -> v
            Empty -> default
        }
    }
}
function main() {
    let w: Wrapper<Int> = Wrapped(42)
    print(w.getOr(0))
    let e: Wrapper<Int> = Empty
    print(e.getOr(99))
}
"#,
        &["42", "99"],
    );
}

#[test]
fn struct_equality_same_values() {
    run_expect(
        r#"
struct Point {
    Int x
    Int y
}
function main() {
    let a: Point = Point(1, 2)
    let b: Point = Point(1, 2)
    print(a == b)
}
"#,
        &["true"],
    );
}

#[test]
fn struct_equality_different_values() {
    run_expect(
        r#"
struct Point {
    Int x
    Int y
}
function main() {
    let a: Point = Point(1, 2)
    let b: Point = Point(3, 4)
    print(a == b)
}
"#,
        &["false"],
    );
}

#[test]
fn struct_inequality() {
    run_expect(
        r#"
struct Point {
    Int x
    Int y
}
function main() {
    let a: Point = Point(1, 2)
    let b: Point = Point(3, 4)
    print(a != b)
}
"#,
        &["true"],
    );
}

#[test]
fn struct_inequality_same_values() {
    run_expect(
        r#"
struct Point {
    Int x
    Int y
}
function main() {
    let a: Point = Point(1, 2)
    let b: Point = Point(1, 2)
    print(a != b)
}
"#,
        &["false"],
    );
}

#[test]
fn deeply_nested_field_assignment_3_levels() {
    run_expect(
        r#"
struct Inner {
    Int value
}
struct Middle {
    Inner inner
}
struct Outer {
    Middle middle
}
function main() {
    let mut o: Outer = Outer(Middle(Inner(1)))
    o.middle.inner.value = 99
    print(o.middle.inner.value)
}
"#,
        &["99"],
    );
}

#[test]
fn struct_equality_nested() {
    run_expect(
        r#"
struct Inner {
    Int val
}
struct Outer {
    Inner inner
    Int other
}
function main() {
    let a: Outer = Outer(Inner(1), 2)
    let b: Outer = Outer(Inner(1), 2)
    let c: Outer = Outer(Inner(99), 2)
    print(a == b)
    print(a == c)
}
"#,
        &["true", "false"],
    );
}

/// Deeply nested field assignment (3 levels).
#[test]
fn deeply_nested_field_assignment_three_levels() {
    run_expect(
        r#"
struct A {
  Int value
}
struct B {
  A a
}
struct C {
  B b
}
function main() {
  let mut c: C = C(B(A(1)))
  c.b.a.value = 42
  print(c.b.a.value)
}
"#,
        &["42"],
    );
}

/// Struct with no fields.
#[test]
fn struct_with_no_fields() {
    run_expect(
        r#"
struct Unit {
  function describe(self) -> String {
    return "unit"
  }
}
function main() {
  let u: Unit = Unit()
  print(u.describe())
}
"#,
        &["unit"],
    );
}

/// Generic type alias used in function signature.
#[test]
fn generic_type_alias_in_signature() {
    run_expect(
        r#"
type StringResult<T> = Result<T, String>
function parse(s: String) -> StringResult<Int> {
  if s == "42" { return Ok(42) }
  return Err("bad")
}
function main() {
  let r: StringResult<Int> = parse("42")
  print(r.unwrap())
  let e: StringResult<Int> = parse("nope")
  print(e.isErr())
}
"#,
        &["42", "true"],
    );
}

/// Nested field assignment three levels deep.
#[test]
fn nested_field_assignment_3_levels() {
    run_expect(
        r#"
struct Inner { Int val }
struct Middle { Inner inner }
struct Outer { Middle mid }
function main() {
  let mut o: Outer = Outer(Middle(Inner(1)))
  o.mid.inner.val = 99
  print(o.mid.inner.val)
}
"#,
        &["99"],
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Edge case: deeply nested field assignment chains
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn four_level_nested_field_assignment() {
    run_expect(
        r#"
struct A { Int val }
struct B { A a }
struct C { B b }
struct D { C c }
function main() {
  let mut d: D = D(C(B(A(0))))
  d.c.b.a.val = 999
  print(d.c.b.a.val)
}
"#,
        &["999"],
    );
}

#[test]
fn nested_field_assignment_multiple_fields() {
    run_expect(
        r#"
struct Point { Int x  Int y }
struct Rect { Point origin  Point size }
function main() {
  let mut r: Rect = Rect(Point(0, 0), Point(100, 50))
  r.origin.x = 10
  r.origin.y = 20
  r.size.x = 200
  r.size.y = 100
  print(r.origin.x)
  print(r.origin.y)
  print(r.size.x)
  print(r.size.y)
}
"#,
        &["10", "20", "200", "100"],
    );
}
