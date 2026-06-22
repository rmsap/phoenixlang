use crate::Analysis;
use crate::checker::{DefaultValue, SymbolKind, check};
use crate::types::Type;
use phoenix_common::diagnostics::Diagnostic;
use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;

fn check_source(source: &str) -> Vec<Diagnostic> {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    check(&program).diagnostics
}

fn assert_no_errors(source: &str) {
    let errors = check_source(source);
    assert!(errors.is_empty(), "unexpected errors: {:?}", errors);
}

fn assert_has_error(source: &str, expected_msg: &str) {
    let errors = check_source(source);
    assert!(
        errors.iter().any(|e| e.message.contains(expected_msg)),
        "expected error containing '{}', got: {:?}",
        expected_msg,
        errors
    );
}

/// Assert that NO diagnostic's message contains `unexpected_msg`. Used to pin
/// that a fix removed a spurious cascade error while the primary error remains.
fn assert_no_error_containing(source: &str, unexpected_msg: &str) {
    let errors = check_source(source);
    assert!(
        !errors.iter().any(|e| e.message.contains(unexpected_msg)),
        "expected no error containing '{}', got: {:?}",
        unexpected_msg,
        errors
    );
}

/// Assert that the checker emits *exactly* `expected_count` diagnostics
/// whose message contains `expected_msg`. Stronger than [`assert_has_error`]
/// when the test name implies a specific count of violations — e.g.
/// "exactly one nested defer" — and the walker accidentally flagging the
/// surrounding legal construct would otherwise pass silently.
fn assert_error_count_for_message(source: &str, expected_msg: &str, expected_count: usize) {
    let errors = check_source(source);
    let matching: Vec<&Diagnostic> = errors
        .iter()
        .filter(|e| e.message.contains(expected_msg))
        .collect();
    assert_eq!(
        matching.len(),
        expected_count,
        "expected {} errors containing '{}', got {} matches in {:?}",
        expected_count,
        expected_msg,
        matching.len(),
        errors,
    );
}

#[test]
fn valid_simple_program() {
    assert_no_errors("function main() { let x: Int = 42\n print(x) }");
}

#[test]
fn valid_function_call() {
    assert_no_errors(
        "function add(a: Int, b: Int) -> Int { return a + b }\nfunction main() { let result: Int = add(1, 2)\n print(result) }",
    );
}

#[test]
fn type_mismatch_var_decl() {
    assert_has_error(
        "function main() { let x: Int = \"hello\" }",
        "type mismatch",
    );
}

#[test]
fn undefined_variable() {
    assert_has_error("function main() { print(x) }", "undefined variable `x`");
}

#[test]
fn duplicate_variable() {
    assert_has_error(
        "function main() { let x: Int = 1\n let x: Int = 2 }",
        "already defined",
    );
}

#[test]
fn assignment_to_immutable() {
    assert_has_error(
        "function main() { let x: Int = 1\n x = 2 }",
        "cannot assign to immutable",
    );
}

#[test]
fn assignment_to_mutable() {
    assert_no_errors("function main() { let mut x: Int = 1\n x = 2 }");
}

#[test]
fn return_type_mismatch() {
    assert_has_error(
        "function foo() -> Int { return \"hello\" }",
        "return type mismatch",
    );
}

#[test]
fn if_condition_not_bool() {
    assert_has_error(
        "function main() { if 42 { print(1) } }",
        "if condition must be Bool",
    );
}

#[test]
fn wrong_argument_count() {
    assert_has_error(
        "function foo(a: Int) -> Int { return a }\nfunction main() { foo(1, 2) }",
        "takes 1 argument",
    );
}

#[test]
fn wrong_argument_type() {
    assert_has_error(
        "function foo(a: Int) -> Int { return a }\nfunction main() { foo(\"hello\") }",
        "expected `Int` but got `String`",
    );
}

#[test]
fn while_loop_valid() {
    assert_no_errors("function main() { let mut x: Int = 0\n while x < 10 { x = x + 1 } }");
}

#[test]
fn while_condition_not_bool() {
    assert_has_error(
        "function main() { while 42 { print(1) } }",
        "while condition must be Bool",
    );
}

#[test]
fn for_loop_valid() {
    assert_no_errors("function main() { for i in 0..10 { print(i) } }");
}

#[test]
fn struct_valid() {
    assert_no_errors(
        "struct Point {\n  x: Int\n  y: Int\n}\nfunction main() { let p: Point = Point(1, 2)\n print(p.x) }",
    );
}

#[test]
fn struct_wrong_field_count() {
    assert_has_error(
        "struct Point {\n  x: Int\n  y: Int\n}\nfunction main() { let p: Point = Point(1) }",
        "has 2 field(s), got 1",
    );
}

#[test]
fn enum_and_match() {
    assert_no_errors(
        "enum Color {\n  Red\n  Green\n  Blue\n}\nfunction main() {\n  let c: Color = Red\n  match c {\n    Red -> print(\"red\")\n    Green -> print(\"green\")\n    Blue -> print(\"blue\")\n  }\n}",
    );
}

#[test]
fn for_loop_non_int_range() {
    assert_has_error(
        "function main() { for i: Float in 0..10 { print(i) } }",
        "for loop variable must be Int",
    );
}

#[test]
fn while_loop_with_return() {
    assert_no_errors(
        "function foo() -> Int { let mut x: Int = 0\n while x < 10 { x = x + 1\n return x } return 0 }",
    );
}

#[test]
fn struct_field_access_valid() {
    assert_no_errors(
        "struct Point {\n  x: Int\n  y: Int\n}\nfunction main() { let p: Point = Point(1, 2)\n print(p.x) }",
    );
}

#[test]
fn struct_field_access_invalid() {
    assert_has_error(
        "struct Point {\n  x: Int\n  y: Int\n}\nfunction main() { let p: Point = Point(1, 2)\n print(p.z) }",
        "has no field `z`",
    );
}

#[test]
fn struct_field_type_check() {
    assert_no_errors(
        "struct Point {\n  x: Int\n  y: Int\n}\nfunction main() { let p: Point = Point(1, 2)\n let val: Int = p.x }",
    );
}

#[test]
fn method_call_valid() {
    assert_no_errors(
        "struct Counter {\n  value: Int\n}\nimpl Counter {\n  function get(self) -> Int { return self.value }\n}\nfunction main() { let c: Counter = Counter(0)\n let v: Int = c.get() }",
    );
}

#[test]
fn method_call_undefined() {
    assert_has_error(
        "struct Counter {\n  value: Int\n}\nfunction main() { let c: Counter = Counter(0)\n c.reset() }",
        "no method `reset`",
    );
}

#[test]
fn method_wrong_args() {
    assert_has_error(
        "struct Counter {\n  value: Int\n}\nimpl Counter {\n  function add(self, n: Int) -> Int { return self.value + n }\n}\nfunction main() { let c: Counter = Counter(0)\n c.add(1, 2) }",
        "takes 1 argument",
    );
}

#[test]
fn enum_variant_with_wrong_field_count() {
    assert_has_error(
        "enum Shape {\n  Circle(Float)\n  Square(Float)\n}\nfunction main() { let s: Shape = Circle(1.0, 2.0) }",
        "takes 1 field(s), got 2",
    );
}

#[test]
fn enum_variant_with_wrong_field_type() {
    assert_has_error(
        "enum Shape {\n  Circle(Float)\n  Square(Float)\n}\nfunction main() { let s: Shape = Circle(\"hello\") }",
        "expected `Float` but got `String`",
    );
}

#[test]
fn match_on_enum_with_bindings() {
    assert_no_errors(
        "enum Shape {\n  Circle(Float)\n  Square(Float)\n}\nfunction main() {\n  let s: Shape = Circle(3.14)\n  match s {\n    Circle(r) -> print(r)\n    Square(side) -> print(side)\n  }\n}",
    );
}

#[test]
fn impl_on_enum_valid() {
    assert_no_errors(
        "enum Color {\n  Red\n  Green\n  Blue\n}\nimpl Color {\n  function describe(self) -> String { return \"a color\" }\n}\nfunction main() {\n  let c: Color = Red\n  let desc: String = c.describe()\n}",
    );
}

#[test]
fn else_if_chain_type_check() {
    assert_has_error(
        "function main() { let x: Int = 1\n if x == 1 { print(1) } else if 42 { print(2) } }",
        "if condition must be Bool",
    );
}

#[test]
fn break_inside_while_valid() {
    assert_no_errors("function main() { while true { break } }");
}

#[test]
fn continue_inside_for_valid() {
    assert_no_errors("function main() { for i in 0..10 { continue } }");
}

#[test]
fn break_outside_loop_error() {
    assert_has_error("function main() { break }", "`break` outside of loop");
}

#[test]
fn continue_outside_loop_error() {
    assert_has_error("function main() { continue }", "`continue` outside of loop");
}

#[test]
fn break_in_nested_if_inside_loop() {
    assert_no_errors(
        "function main() { let mut x: Int = 0\n while true { x = x + 1\n if x == 5 { break } } }",
    );
}

#[test]
fn struct_type_as_param() {
    assert_no_errors(
        "struct Point {\n  x: Int\n  y: Int\n}\nfunction show(p: Point) { print(p.x) }\nfunction main() { let p: Point = Point(1, 2)\n show(p) }",
    );
}

/// A variable with a function type assigned a compatible lambda passes type checking.
#[test]
fn lambda_type_check_valid() {
    assert_no_errors(
        "function main() {\n  let double: (Int) -> Int = function(x: Int) -> Int { return x * 2 }\n  print(double(5))\n}",
    );
}

/// Assigning a lambda with mismatched parameter types to a function-typed variable
/// produces a type mismatch error.
#[test]
fn lambda_type_mismatch() {
    assert_has_error(
        "function main() {\n  let f: (Int) -> Int = function(x: String) -> Int { return 0 }\n}",
        "type mismatch",
    );
}

/// Calling a variable that holds a function value type-checks correctly,
/// verifying argument types and returning the correct result type.
#[test]
fn call_function_variable() {
    assert_no_errors(
        "function main() {\n  let add: (Int, Int) -> Int = function(a: Int, b: Int) -> Int { return a + b }\n  let result: Int = add(1, 2)\n}",
    );
}

/// A function that takes another function as a parameter type-checks
/// when the argument is a compatible lambda.
#[test]
fn higher_order_function() {
    assert_no_errors(
        "function apply(f: (Int) -> Int, x: Int) -> Int {\n  return f(x)\n}\nfunction main() {\n  let double: (Int) -> Int = function(x: Int) -> Int { return x * 2 }\n  let result: Int = apply(double, 5)\n}",
    );
}

/// A lambda that references a variable from an outer scope type-checks
/// successfully (the outer variable is visible inside the lambda body).
#[test]
fn closure_captures_outer_variable() {
    assert_no_errors(
        "function main() {\n  let offset: Int = 10\n  let addOffset: (Int) -> Int = function(x: Int) -> Int { return x + offset }\n  let result: Int = addOffset(5)\n}",
    );
}

/// A generic identity function infers T from its argument and type-checks.
#[test]
fn generic_function_identity() {
    assert_no_errors(
        "function identity<T>(x: T) -> T { return x }\nfunction main() { let result: Int = identity(42) }",
    );
}

/// A generic struct with two type parameters type-checks when constructed
/// with matching concrete types.
#[test]
fn generic_struct_valid() {
    assert_no_errors(
        "struct Pair<A, B> {\n  first: A\n  second: B\n}\nfunction main() { let p: Pair<Int, String> = Pair(1, \"hi\") }",
    );
}

/// A generic enum with a value-carrying variant type-checks correctly.
/// Uses the builtin `Option<T>` because user code cannot redeclare it
/// (builtin names are reserved).
#[test]
fn generic_enum_option() {
    assert_no_errors("function main() { let x: Option<Int> = Some(42) }");
}

/// The `None` variant of a generic enum is compatible with any concrete
/// instantiation because its type arguments remain as type variables.
#[test]
fn generic_enum_none_compatible() {
    assert_no_errors("function main() { let x: Option<Int> = None }");
}

/// Assigning the result of a generic function to the wrong concrete type
/// produces a type mismatch error (T is inferred as Int, not String).
#[test]
fn generic_function_type_mismatch() {
    assert_has_error(
        "function identity<T>(x: T) -> T { return x }\nfunction main() { let s: String = identity(42) }",
        "type mismatch",
    );
}

#[test]
fn list_literal_valid() {
    assert_no_errors("function main() { let nums: List<Int> = [1, 2, 3] }");
}

#[test]
fn list_literal_empty() {
    assert_no_errors("function main() { let nums: List<Int> = [] }");
}

#[test]
fn list_element_type_mismatch() {
    assert_has_error(
        "function main() { let nums: List<Int> = [1, \"hello\", 3] }",
        "list element type mismatch",
    );
}

#[test]
fn list_length_method() {
    assert_no_errors(
        "function main() { let nums: List<Int> = [1, 2, 3]\n let len: Int = nums.length() }",
    );
}

#[test]
fn list_get_method() {
    assert_no_errors(
        "function main() { let nums: List<Int> = [1, 2, 3]\n let first: Int = nums.get(0) }",
    );
}

#[test]
fn list_get_wrong_arg_type() {
    assert_has_error(
        "function main() { let nums: List<Int> = [1, 2, 3]\n let first: Int = nums.get(\"zero\") }",
        "expected Int but got String",
    );
}

#[test]
fn list_push_method() {
    assert_no_errors(
        "function main() { let nums: List<Int> = [1, 2]\n let nums2: List<Int> = nums.push(3) }",
    );
}

#[test]
fn list_push_wrong_type() {
    assert_has_error(
        "function main() { let nums: List<Int> = [1, 2]\n let nums2: List<Int> = nums.push(\"hello\") }",
        "expected Int but got String",
    );
}

#[test]
fn list_unknown_method() {
    assert_has_error(
        "function main() { let nums: List<Int> = [1]\n nums.foo() }",
        "no method `foo` on type `List`",
    );
}

#[test]
fn list_type_annotation() {
    assert_no_errors("function main() { let names: List<String> = [\"alice\", \"bob\"] }");
}

#[test]
fn list_var_decl_type_mismatch() {
    assert_has_error(
        "function main() { let nums: List<Int> = [\"hello\"] }",
        "type mismatch",
    );
}

// --- Built-in Option and Result type tests ---

/// `Option<Int> x = Some(42)` passes type checking using the built-in Option enum.
#[test]
fn option_some_valid() {
    assert_no_errors("function main() { let x: Option<Int> = Some(42) }");
}

/// `Option<Int> x = None` passes because None is compatible with any Option instantiation.
#[test]
fn option_none_valid() {
    assert_no_errors("function main() { let x: Option<Int> = None }");
}

/// `Option<String> x = Some(42)` produces a type mismatch (Int vs String).
#[test]
fn option_type_mismatch() {
    assert_has_error(
        "function main() { let x: Option<String> = Some(42) }",
        "type mismatch",
    );
}

/// The return type of `unwrap()` on `Option<Int>` is `Int`.
#[test]
fn option_unwrap_type() {
    assert_no_errors("function main() { let x: Option<Int> = Some(42)\n let v: Int = x.unwrap() }");
}

/// `Result<Int, String> x = Ok(42)` passes type checking using the built-in Result enum.
#[test]
fn result_ok_valid() {
    assert_no_errors(r#"function main() { let x: Result<Int, String> = Ok(42) }"#);
}

/// `Result<Int, String> x = Err("oops")` passes type checking.
#[test]
fn result_err_valid() {
    assert_no_errors(r#"function main() { let x: Result<Int, String> = Err("oops") }"#);
}

/// `isOk()` on a Result returns Bool.
#[test]
fn result_is_ok_returns_bool() {
    assert_no_errors(
        r#"function main() { let r: Result<Int, String> = Ok(1)
let b: Bool = r.isOk() }"#,
    );
}

/// Providing the wrong number of type arguments to a generic struct
/// produces a type mismatch error (Pair needs 2, but only 1 is given).
#[test]
fn generic_wrong_type_arg_count() {
    assert_has_error(
        "struct Pair<A, B> {\n  first: A\n  second: B\n}\nfunction main() { let p: Pair<Int> = Pair(1, \"hi\") }",
        "type mismatch",
    );
}

/// A generic higher-order function that takes a builtin `Option<T>`
/// and a default `T` value type-checks correctly. Renamed from
/// `unwrapOr` to `defaultedUnwrap` to avoid colliding with the
/// builtin's `unwrapOr` method.
#[test]
fn generic_unwrap_or() {
    assert_no_errors(
        "function defaultedUnwrap<T>(opt: Option<T>, defaultVal: T) -> T {\n  return match opt {\n    Some(v) -> v\n    None -> defaultVal\n  }\n}\nfunction main() {\n  let x: Option<Int> = Some(42)\n  let result: Int = defaultedUnwrap(x, 0)\n}",
    );
}

/// Calling a closure with the wrong number of arguments produces an error.
#[test]
fn closure_wrong_arg_count() {
    assert_has_error(
        "function main() {\n  let f: (Int) -> Int = function(x: Int) -> Int { return x * 2 }\n  f(1, 2)\n}",
        "takes 1 argument(s), got 2",
    );
}

/// A lambda whose body returns the wrong type produces a return type mismatch error.
#[test]
fn closure_return_type_mismatch() {
    assert_has_error(
        "function main() {\n  let f: (Int) -> Int = function(x: Int) -> Int { return \"hello\" }\n}",
        "return type mismatch",
    );
}

/// `Option<List<Int>>` with a nested generic type passes type checking.
#[test]
fn generic_nested_type() {
    assert_no_errors("function main() { let x: Option<List<Int>> = Some([1, 2, 3]) }");
}

/// Passing a function with the wrong type signature to a higher-order
/// function produces a type mismatch error.
#[test]
fn function_type_param_mismatch() {
    assert_has_error(
        "function apply(f: (Int) -> Int, x: Int) -> Int {\n  return f(x)\n}\nfunction main() {\n  let g: (String) -> String = function(s: String) -> String { return s }\n  apply(g, 5)\n}",
        "expected `(Int) -> Int` but got `(String) -> String`",
    );
}

/// A lambda that returns another lambda type-checks correctly with
/// nested function types.
#[test]
fn nested_closures_valid() {
    assert_no_errors(
        "function main() {\n  let makeAdder: (Int) -> (Int) -> Int = function(n: Int) -> (Int) -> Int {\n    return function(x: Int) -> Int { return x + n }\n  }\n  let add5: (Int) -> Int = makeAdder(5)\n  let result: Int = add5(10)\n}",
    );
}

/// A full trait decl + impl + method call passes type checking.
#[test]
fn trait_impl_valid() {
    assert_no_errors(
        r#"
trait Display {
  function toString(self) -> String
}
struct Point {
  x: Int
  y: Int

  impl Display {
function toString(self) -> String { return "Point" }
  }
}
function main() {
  let p: Point = Point(1, 2)
  let s: String = p.toString()
}
"#,
    );
}

/// An impl that is missing a required trait method produces an error.
#[test]
fn trait_impl_missing_method() {
    assert_has_error(
        r#"
trait Display {
  function toString(self) -> String
}
struct Point {
  x: Int
  y: Int

  impl Display {
  }
}
function main() { }
"#,
        "missing method `toString`",
    );
}

/// A generic function with a trait bound, called with a type that implements
/// the trait, passes type checking.
#[test]
fn trait_bound_satisfied() {
    assert_no_errors(
        r#"
trait Display {
  function toString(self) -> String
}
struct Point {
  x: Int
  y: Int

  impl Display {
function toString(self) -> String { return "Point" }
  }
}
function show<T: Display>(item: T) -> String {
  return item.toString()
}
function main() {
  let p: Point = Point(1, 2)
  let s: String = show(p)
}
"#,
    );
}

/// A generic function with a trait bound, called with a type that does NOT
/// implement the trait, produces an error.
#[test]
fn trait_bound_not_satisfied() {
    assert_has_error(
        r#"
trait Display {
  function toString(self) -> String
}
struct Point {
  x: Int
  y: Int
}
function show<T: Display>(item: T) -> String {
  return item.toString()
}
function main() {
  let p: Point = Point(1, 2)
  let s: String = show(p)
}
"#,
        "does not implement trait `Display`",
    );
}

/// `impl FakeTrait for X` where FakeTrait is not defined produces an error.
#[test]
fn unknown_trait_in_impl() {
    assert_has_error(
        r#"
struct Point {
  x: Int
  y: Int

  impl FakeTrait {
function foo(self) -> Int { return 0 }
  }
}
function main() { }
"#,
        "unknown trait `FakeTrait`",
    );
}

/// A trait with two methods, both implemented, passes type checking.
#[test]
fn trait_multiple_methods_valid() {
    assert_no_errors(
        r#"
trait Shape {
  function area(self) -> Float
  function name(self) -> String
}
struct Circle {
  radius: Float

  impl Shape {
function area(self) -> Float { return 3.14 }
function name(self) -> String { return "Circle" }
  }
}
function main() {
  let c: Circle = Circle(1.0)
  let a: Float = c.area()
  let n: String = c.name()
}
"#,
    );
}

/// A trait with two methods where impl only provides one should error.
#[test]
fn trait_partial_impl() {
    assert_has_error(
        r#"
trait Shape {
  function area(self) -> Float
  function name(self) -> String
}
struct Circle {
  radius: Float

  impl Shape {
function area(self) -> Float { return 3.14 }
  }
}
function main() { }
"#,
        "missing method `name`",
    );
}

// --- Type inference tests ---

/// Type inference works for literals.
#[test]
fn type_inference_literal() {
    assert_no_errors("function main() { let x = 42\n print(x) }");
}

/// Type inference works for struct constructors.
#[test]
fn type_inference_struct() {
    assert_no_errors(
        r#"
struct Point { x: Int  y: Int }
function main() {
  let p = Point(1, 2)
  print(p.x)
}
"#,
    );
}

/// Type inference rejects Void initializer.
#[test]
fn type_inference_rejects_void() {
    assert_has_error(
        "function foo() { }\nfunction main() { let x = foo() }",
        "cannot infer type for `x`: initializer has type Void",
    );
}

/// Type inference rejects ambiguous generic types (e.g. `None`).
#[test]
fn type_inference_rejects_ambiguous_generic() {
    assert_has_error(
        "function main() { let x = None }",
        "cannot infer type for `x`: initializer has ambiguous type",
    );
}

/// Explicit annotation with `None` is fine — the annotation resolves the type.
#[test]
fn type_annotation_resolves_none() {
    assert_no_errors("function main() { let x: Option<Int> = None }");
}

/// Type inference works for mutable variables.
#[test]
fn type_inference_mut() {
    assert_no_errors(
        r#"
function main() {
  let mut x = 42
  x = x + 1
  print(x)
}
"#,
    );
}

/// Type inference works for string values with mutability.
#[test]
fn type_inference_mut_string() {
    assert_no_errors(
        r#"
function main() {
  let mut s = "hello"
  s = "world"
  print(s)
}
"#,
    );
}

/// Type inference catches type mismatches on reassignment.
#[test]
fn type_inference_mut_mismatch() {
    assert_has_error(
        r#"
function main() {
  let mut x = 42
  x = "hello"
}
"#,
        "type mismatch",
    );
}

/// For-loop with inferred type (no annotation) works.
#[test]
fn for_loop_inferred_type() {
    assert_no_errors("function main() { for i in 0..10 { print(i) } }");
}

/// For-loop with explicit Int type annotation works.
#[test]
fn for_loop_explicit_int_type() {
    assert_no_errors("function main() { for i: Int in 0..10 { print(i) } }");
}

// --- GC memory model tests ---
// Phoenix uses garbage collection.  All values — including structs, enums,
// lists, and closures — can be freely shared and reused after assignment or
// being passed to functions.

/// A struct assigned to another variable remains usable.
#[test]
fn struct_reusable_after_assignment() {
    assert_no_errors(
        r#"
struct Point {
  x: Int
  y: Int
}
function main() {
  let p: Point = Point(1, 2)
  let q: Point = p
  print(p.x)
  print(q.x)
}
"#,
    );
}

/// A list assigned to another variable remains usable.
#[test]
fn list_reusable_after_assignment() {
    assert_no_errors(
        r#"
function main() {
  let a: List<Int> = [1, 2, 3]
  let b: List<Int> = a
  print(a.length())
  print(b.length())
}
"#,
    );
}

/// An enum (Option) assigned to another variable remains usable.
#[test]
fn enum_reusable_after_assignment() {
    assert_no_errors(
        r#"
function main() {
  let a: Option<Int> = Some(42)
  let b: Option<Int> = a
  print(a.isSome())
}
"#,
    );
}

/// A struct passed to a function can still be used by the caller.
#[test]
fn function_arg_does_not_consume() {
    assert_no_errors(
        r#"
struct Point {
  x: Int
  y: Int
}
function take(p: Point) { print(p.x) }
function main() {
  let p: Point = Point(1, 2)
  take(p)
  print(p.x)
}
"#,
    );
}

/// A struct referenced inside a closure can still be used outside.
#[test]
fn closure_capture_does_not_consume() {
    assert_no_errors(
        r#"
struct Point {
  x: Int
  y: Int
}
function main() {
  let p: Point = Point(1, 2)
  let q: Point = p
  let f: (Int) -> Int = function(x: Int) -> Int { return p.x }
  print(p.x)
}
"#,
    );
}

/// The same variable can be passed as multiple arguments to a function.
#[test]
fn same_var_passed_twice() {
    assert_no_errors(
        r#"
struct Point {
  x: Int
  y: Int
}
function both(a: Point, b: Point) { print(a.x) }
function main() {
  let p: Point = Point(1, 2)
  both(p, p)
}
"#,
    );
}

/// A variable used inside an if branch can still be used after the branch.
#[test]
fn use_after_assign_in_if_branch() {
    assert_no_errors(
        r#"
struct Data { value: Int }
function take(d: Data) { print(d.value) }
function main() {
  let d: Data = Data(42)
  if true {
take(d)
  }
  print(d.value)
}
"#,
    );
}

// --- Match exhaustiveness tests ---

/// A match on an enum missing a variant (without wildcard) should error.
#[test]
fn match_non_exhaustive_error() {
    assert_has_error(
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
Green -> print("green")
  }
}
"#,
        "non-exhaustive match",
    );
}

/// A match with a wildcard is always exhaustive.
#[test]
fn match_exhaustive_with_wildcard() {
    assert_no_errors(
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
    );
}

/// A match with a binding catch-all is always exhaustive.
#[test]
fn match_exhaustive_with_binding() {
    assert_no_errors(
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
other -> print("other")
  }
}
"#,
    );
}

/// A match covering all enum variants is exhaustive.
#[test]
fn match_exhaustive_all_variants() {
    assert_no_errors(
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
Green -> print("green")
Blue -> print("blue")
  }
}
"#,
    );
}

// --- Comparison operator error type tests ---

/// Comparing incompatible types returns an error (not Bool).
#[test]
fn comparison_incompatible_types_error() {
    assert_has_error(
        "function main() { let b: Bool = 42 < \"hello\" }",
        "cannot compare",
    );
}

/// Equality between incompatible types returns an error.
#[test]
fn equality_incompatible_types_error() {
    assert_has_error(
        "function main() { let b: Bool = 42 == \"hello\" }",
        "cannot compare",
    );
}

// --- Additional missing tests ---

/// Duplicate function definition produces an error.
#[test]
fn duplicate_function_error() {
    assert_has_error(
        "function foo() { }\nfunction foo() { }\nfunction main() { }",
        "already defined",
    );
}

/// A match block body with a return statement has the correct type.
#[test]
fn match_block_body_with_return() {
    assert_no_errors(
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
  let s: Shape = Rect(3.0, 3.0)
  let desc: String = s.describe()
}
"#,
    );
}

/// Empty match on an enum without arms should error for exhaustiveness.
#[test]
fn match_empty_arms_error() {
    assert_has_error(
        r#"
enum Color {
  Red
  Green
}
function main() {
  let c: Color = Red
  match c {
  }
}
"#,
        "non-exhaustive match",
    );
}

/// A generic function with a closure parameter infers type arguments correctly.
#[test]
fn generic_function_with_closure() {
    assert_no_errors(
        r#"
function map<T, U>(value: T, f: (T) -> U) -> U {
  return f(value)
}
function main() {
  let result: String = map(42, function(n: Int) -> String { return toString(n) })
}
"#,
    );
}

/// Multiple errors are accumulated and all reported.
#[test]
fn multiple_errors_accumulated() {
    let errors = check_source(
        r#"
function main() {
  let x: Int = "hello"
  let y: Bool = 42
  let z: Float = true
}
"#,
    );
    assert!(
        errors.len() >= 3,
        "expected at least 3 errors, got: {:?}",
        errors
    );
}

/// Match on Option missing Some variant (without wildcard) errors.
#[test]
fn match_option_non_exhaustive() {
    assert_has_error(
        r#"
function main() {
  let x: Option<Int> = Some(42)
  match x {
Some(v) -> print(v)
  }
}
"#,
        "non-exhaustive match",
    );
}

/// A deeply nested generic type passes type checking.
#[test]
fn deeply_nested_generic_type() {
    assert_no_errors(
        r#"
function main() {
  let items: List<Option<Int>> = [Some(1), None, Some(3)]
  let opt: Option<List<Int>> = Some([1, 2, 3])
}
"#,
    );
}

/// Closures at 3 levels of nesting type-check correctly.
#[test]
fn triple_nested_closures() {
    assert_no_errors(
        r#"
function main() {
  let a: Int = 1
  let f: (Int) -> (Int) -> (Int) -> Int = function(b: Int) -> (Int) -> (Int) -> Int {
return function(c: Int) -> (Int) -> Int {
  return function(d: Int) -> Int {
    return a + b + c + d
  }
}
  }
  let g: (Int) -> (Int) -> Int = f(2)
  let h: (Int) -> Int = g(3)
  let result: Int = h(4)
}
"#,
    );
}

/// Using the for-loop variable after the loop is fine (it's scoped).
#[test]
fn for_loop_variable_scoped() {
    assert_has_error(
        "function main() { for i in 0..10 { print(i) }\n print(i) }",
        "undefined variable `i`",
    );
}

/// Calling a method on a void expression errors.
#[test]
fn method_on_void_error() {
    assert_has_error(
        "function foo() { }\nfunction main() { foo().bar() }",
        "cannot call method on Void",
    );
}

// --- Phase 1.8 feature tests ---

/// Field assignment to an immutable variable is an error.
#[test]
fn field_assignment_immutable_error() {
    assert_has_error(
        r#"
struct Point { x: Int  y: Int }
function main() {
  let p: Point = Point(1, 2)
  p.x = 10
}
"#,
        "immutable",
    );
}

/// Field assignment with wrong type is an error.
#[test]
fn field_assignment_wrong_type_error() {
    assert_has_error(
        r#"
struct Point { x: Int  y: Int }
function main() {
  let mut p: Point = Point(1, 2)
  p.x = "hello"
}
"#,
        "type mismatch",
    );
}

/// The `?` operator on a non-Result/non-Option type is an error.
#[test]
fn try_operator_on_non_result_error() {
    assert_has_error(
        r#"
function foo() -> Result<Int, String> {
  let x: Int = 42
  let y: Int = x?
  return Ok(y)
}
function main() { }
"#,
        "?",
    );
}

/// The `?` operator in a function not returning Result/Option is an error.
#[test]
fn try_operator_wrong_return_type_error() {
    assert_has_error(
        r#"
function helper() -> Result<Int, String> { return Ok(1) }
function main() {
  let x: Int = helper()?
}
"#,
        "?",
    );
}

/// Type aliases resolve correctly so `type Id = Int; Id x = 42` passes.
#[test]
fn type_alias_resolves() {
    assert_no_errors(
        r#"
type Id = Int
function main() {
  let x: Id = 42
}
"#,
    );
}

/// String interpolation type-checks to String.
#[test]
fn string_interpolation_type_checks() {
    assert_no_errors(
        r#"
function main() {
  let name: String = "world"
  let greeting: String = "hello {name}"
}
"#,
    );
}

#[test]
fn lambda_implicit_return_type_mismatch() {
    assert_has_error(
        "function main() {\n  let f: (Int) -> String = function(x: Int) -> String { x }\n}",
        "lambda return type mismatch",
    );
}

#[test]
fn generic_type_alias_missing_args() {
    assert_has_error(
        "type StringResult<T> = Result<T, String>\nfunction main() {\n  let x: StringResult = Ok(42)\n}",
        "generic type alias `StringResult` requires type arguments",
    );
}

#[test]
fn field_assignment_type_mismatch() {
    assert_has_error(
        "struct Point {\n  x: Int\n  y: Int\n}\nfunction main() {\n  let mut p: Point = Point(1, 2)\n  p.x = \"hello\"\n}",
        "type mismatch",
    );
}

// ── Low-priority edge case tests ───────────────────────────────

#[test]
fn circular_type_alias_produces_error() {
    // type A refers to B which doesn't exist yet at registration time
    assert_has_error(
        "type A = B\ntype B = A\nfunction main() { let x: A = 42 }",
        "unknown type `B`",
    );
}

#[test]
fn trait_bound_only_valid_on_type_params() {
    // Trait bounds on concrete (non-generic) parameter types should still work
    // when the type actually implements the trait
    assert_no_errors(
        r#"
trait Display {
  function toString(self) -> String
}
struct Point {
  x: Int
  y: Int

  impl Display {
function toString(self) -> String { return "point" }
  }
}
function show<T: Display>(item: T) -> String {
  return item.toString()
}
function main() {
  let p: Point = Point(1, 2)
  print(show(p))
}
"#,
    );
}

#[test]
fn method_arg_type_compat_with_generics_regression() {
    // Method argument checking should use types_compatible()
    // not strict equality, so type variables work correctly
    assert_no_errors(
        r#"
function main() {
  let x: Option<Int> = Some(42)
  let val: Int = x.unwrapOr(0)
  print(val)
}
"#,
    );
}

#[test]
fn empty_match_exhaustiveness_error() {
    assert_has_error(
        "enum Color {\n  Red\n  Green\n}\nfunction main() {\n  let c: Color = Red\n  match c { }\n}",
        "non-exhaustive match",
    );
}

#[test]
fn unknown_escape_sequence_passthrough() {
    // Unknown escape sequences like \x should pass through as literal characters
    assert_no_errors(
        r#"function main() { let s: String = "hello\x41"
  print(s) }"#,
    );
}

#[test]
fn and_or_with_error_operand_no_cascade() {
    // When one operand has a prior error, And/Or should not report
    // an additional "must be Bool" error about the error type
    let errors = check_source("function main() { let b: Bool = undefinedVar && true }");
    // Should have "undefined variable" but NOT "must be Bool"
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("undefined variable"))
    );
    assert!(!errors.iter().any(|e| e.message.contains("must be Bool")));
}

#[test]
fn trait_impl_wrong_param_count() {
    assert_has_error(
        r#"
trait Greet {
  function hello(self) -> String
}
struct Person {
  name: String

  impl Greet {
function hello(self, extra: Int) -> String { return "hi" }
  }
}
"#,
        "parameter(s) but trait",
    );
}

#[test]
fn trait_impl_wrong_return_type() {
    assert_has_error(
        r#"
trait Greet {
  function hello(self) -> String
}
struct Person {
  name: String

  impl Greet {
function hello(self) -> Int { return 42 }
  }
}
"#,
        "returns `Int` but trait",
    );
}

#[test]
fn trait_impl_wrong_parameter_type() {
    assert_has_error(
        r#"
trait Adder {
  function add(self, x: Int) -> Int
}
struct Foo {
  val: Int

  impl Adder {
function add(self, x: String) -> Int { return 42 }
  }
}
"#,
        "parameter `x` has type `String` but trait `Adder` expects `Int`",
    );
}

#[test]
fn named_arguments_duplicate() {
    assert_has_error(
        r#"
function foo(a: Int, b: Int) -> Int { return a + b }
function main() { print(foo(a: 1, a: 2)) }
"#,
        "duplicate",
    );
}

#[test]
fn named_arguments_unknown_parameter() {
    let diags = check_source(
        r#"
function foo(a: Int) -> Int { return a }
function main() { print(foo(z: 1)) }
"#,
    );
    let has_relevant_error = diags.iter().any(|d| {
        let msg = d.message.to_lowercase();
        msg.contains("unknown") || msg.contains("no parameter")
    });
    assert!(
        has_relevant_error,
        "expected error about unknown/no parameter, got: {:?}",
        diags
    );
}

#[test]
fn default_parameters_valid() {
    assert_no_errors(
        r#"
function greet(name: String, prefix: String = "Hello") -> String {
  return prefix + " " + name
}
function main() { print(greet("Alice")) }
"#,
    );
}

/// Default expressions are evaluated at the caller's call site (see
/// [design-decisions.md: *Default-argument lowering strategy*]) —
/// any reference to an earlier parameter would resolve against the
/// caller's scope rather than the callee's, producing a runtime /
/// lowering failure.  Sema rejects the shape up front.
#[test]
fn default_value_cannot_reference_earlier_parameter() {
    assert_has_error(
        r#"
function f(x: Int, y: Int = x + 1) -> Int { return x + y }
function main() { print(f(10)) }
"#,
        "undefined",
    );
}

// Forward-reference case (`function f(x: Int = y, y: Int)`) is
// rejected at parse time by the "non-default parameter cannot follow
// a default parameter" rule — no sema test needed.

/// Sema must reject over-long positional calls before IR lowering
/// sees them.  IR lowering's `merge_call_args` carries a defensive
/// `assert!(positional.len() <= total)` that would fire only on sema
/// bugs — this test protects that contract from the sema side.
#[test]
fn call_with_too_many_positional_args_rejected_by_sema() {
    assert_has_error(
        r#"
function add(x: Int, y: Int = 10) -> Int { return x + y }
function main() { print(add(1, 2, 3)) }
"#,
        "takes",
    );
}

/// Sema must reject named-arg + positional-arg overlap for the same
/// slot before IR lowering sees them.  `merge_call_args` currently
/// makes named args win over positional on overlap; that policy is
/// only reachable via a sema bug, so pinning sema's rejection here
/// protects the merge step from ever needing to resolve the ambiguity.
#[test]
fn call_with_named_and_positional_overlap_rejected_by_sema() {
    assert_has_error(
        r#"
function add(x: Int, y: Int) -> Int { return x + y }
function main() { print(add(1, x: 2)) }
"#,
        "already provided as positional",
    );
}

/// A default expression whose inferred type still contains free type
/// variables cannot be lowered at the caller's call site — the
/// caller's type-arg substitution binds the caller's parameters, not
/// the callee's.  Sema rejects at declaration time; the tripwire is
/// the `has_type_vars()` branch in
/// `phoenix-sema/src/checker.rs::check_function` pass 1.
///
/// Minimal repro: a bare `None` as a default.  Without an expected
/// type driving inference, sema infers `Option<T>` for the expression,
/// which still carries the fresh `T`.
#[test]
fn default_value_rejected_when_inferred_type_has_free_type_vars() {
    assert_has_error(
        r#"
function f<T>(x: Option<T> = None) -> Option<T> { return x }
function main() { }
"#,
        "generic parameters",
    );
}

#[test]
fn struct_destructuring_valid() {
    assert_no_errors(
        r#"
struct Point {
  x: Int
  y: Int
}
function main() {
  let p: Point = Point(3, 4)
  let Point { x, y } = p
  print(x)
}
"#,
    );
}

#[test]
fn struct_destructuring_unknown_field() {
    let diags = check_source(
        r#"
struct Point {
  x: Int
  y: Int
}
function main() {
  let p: Point = Point(3, 4)
  let Point { x, z } = p
}
"#,
    );
    let has_relevant_error = diags.iter().any(|d| {
        let msg = d.message.to_lowercase();
        msg.contains("z")
            && (msg.contains("not found") || msg.contains("no field") || msg.contains("unknown"))
    });
    assert!(
        has_relevant_error,
        "expected error about unknown field `z`, got: {:?}",
        diags
    );
}

#[test]
fn closure_captures_outer_mutable_variable() {
    assert_no_errors(
        r#"
function main() {
  let mut count: Int = 0
  let inc: () -> Void = function() { count = count + 1 }
  inc()
  print(count)
}
"#,
    );
}

#[test]
fn generic_function_conflicting_types() {
    assert_has_error(
        r#"
function same<T>(a: T, b: T) -> T { return a }
function main() { same(1, "hello") }
"#,
        "expected `Int` but got `String`",
    );
}

#[test]
fn match_on_int_literal_patterns() {
    assert_no_errors(
        r#"
function main() {
  let x: Int = 42
  match x {
1 -> print("one")
_ -> print("other")
  }
}
"#,
    );
}

#[test]
fn trait_impl_correct_parameter_types() {
    assert_no_errors(
        r#"
trait Converter {
  function convert(self, x: Int) -> String
}
struct MyConv {
  impl Converter {
function convert(self, x: Int) -> String { return toString(x) }
  }
}
function main() { }
"#,
    );
}

// ── Bug fix: match arm type mismatch with break/continue/return ──

/// A match arm with `break` should not cause a type mismatch error when
/// another arm evaluates to a non-Void type.
#[test]
fn match_arm_break_error() {
    assert_has_error(
        r#"
enum Action { Go  Stop }
function main() {
  let actions: List<Action> = [Go, Stop]
  let mut count: Int = 0
  for a in actions {
match a {
  Go -> { count = count + 1 }
  Stop -> { break }
}
  }
}
"#,
        "`break` is not allowed inside match arms",
    );
}

#[test]
fn match_arm_continue_error() {
    assert_has_error(
        r#"
function main() {
  let mut sum: Int = 0
  for i in 0..10 {
match i % 2 {
  0 -> { sum = sum + i }
  _ -> { continue }
}
  }
}
"#,
        "`continue` is not allowed inside match arms",
    );
}

/// `break` in a match arm outside of any loop still produces the match-arm error,
/// not the "break outside of loop" error.
#[test]
fn match_arm_break_outside_loop_error() {
    assert_has_error(
        r#"
function main() {
  let x: Int = 1
  match x {
1 -> { break }
_ -> {}
  }
}
"#,
        "`break` is not allowed inside match arms",
    );
}

/// `continue` in a match arm outside of any loop still produces the match-arm error.
#[test]
fn match_arm_continue_outside_loop_error() {
    assert_has_error(
        r#"
function main() {
  let x: Int = 1
  match x {
1 -> { continue }
_ -> {}
  }
}
"#,
        "`continue` is not allowed inside match arms",
    );
}

/// `return` in a match arm is still allowed (it exits the enclosing function).
#[test]
fn match_arm_return_still_allowed() {
    assert_no_errors(
        r#"
function foo(x: Int) -> Int {
  match x {
1 -> { return 42 }
_ -> { return 0 }
  }
  return -1
}
function main() { print(foo(1)) }
"#,
    );
}

/// A match arm with `return` should not cause a type mismatch error.
#[test]
fn match_arm_return_no_type_mismatch() {
    assert_no_errors(
        r#"
function find(nums: List<Int>) -> Int {
  for n in nums {
match n % 2 {
  0 -> { return n }
  _ -> { let x: Int = 0 }
}
  }
  return -1
}
function main() { print(find([1, 3, 4])) }
"#,
    );
}

// ── ResolvedModule exposes type registries ─────────────────

fn check_full(source: &str) -> Analysis {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let result = check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "type errors: {:?}",
        result.diagnostics
    );
    result
}

#[test]
fn check_result_contains_functions() {
    let result = check_full(
        r#"
function add(a: Int, b: Int) -> Int { return a + b }
function greet(name: String) -> String { return name }
function main() { }
"#,
    );
    assert!(result.module.function_by_name.contains_key("add"));
    assert!(result.module.function_by_name.contains_key("greet"));
    assert!(result.module.function_by_name.contains_key("main"));
    let add_info = result.module.function_info_by_name("add").unwrap();
    assert_eq!(add_info.params.len(), 2);
    assert_eq!(add_info.params[0], Type::Int);
    assert_eq!(add_info.params[1], Type::Int);
    assert_eq!(add_info.return_type, Type::Int);
    let greet_info = result.module.function_info_by_name("greet").unwrap();
    assert_eq!(greet_info.params, vec![Type::String]);
    assert_eq!(greet_info.return_type, Type::String);
}

#[test]
fn check_result_contains_structs() {
    let result = check_full(
        r#"
struct Point { x: Int  y: Int }
struct Named { name: String }
function main() { }
"#,
    );
    assert!(result.module.struct_by_name.contains_key("Point"));
    let point = result.module.struct_info_by_name("Point").unwrap();
    assert_eq!(point.fields.len(), 2);
    assert_eq!(point.fields[0].name, "x");
    assert_eq!(point.fields[0].ty, Type::Int);
    assert_eq!(point.fields[1].name, "y");
    assert_eq!(point.fields[1].ty, Type::Int);
    assert!(point.type_params.is_empty());

    assert!(result.module.struct_by_name.contains_key("Named"));
    assert_eq!(
        result.module.struct_info_by_name("Named").unwrap().fields[0].ty,
        Type::String
    );
}

#[test]
fn check_result_contains_generic_struct() {
    let result = check_full(
        r#"
struct Wrapper<T> { value: T }
function main() { }
"#,
    );
    let wrapper = result.module.struct_info_by_name("Wrapper").unwrap();
    assert_eq!(wrapper.type_params, vec!["T".to_string()]);
    assert_eq!(wrapper.fields.len(), 1);
}

#[test]
fn check_result_contains_enums() {
    let result = check_full(
        r#"
enum Color { Red  Green  Blue }
enum Shape { Circle(Float)  Rect(Float, Float) }
function main() { }
"#,
    );
    assert!(result.module.enum_by_name.contains_key("Color"));
    let color = result.module.enum_info_by_name("Color").unwrap();
    assert_eq!(color.variants.len(), 3);
    assert_eq!(color.variants[0].0, "Red");
    assert!(color.variants[0].1.is_empty()); // unit variant

    assert!(result.module.enum_by_name.contains_key("Shape"));
    let shape = result.module.enum_info_by_name("Shape").unwrap();
    assert_eq!(shape.variants.len(), 2);
    assert_eq!(shape.variants[0].0, "Circle");
    assert_eq!(shape.variants[0].1.len(), 1); // one Float field
    assert_eq!(shape.variants[1].0, "Rect");
    assert_eq!(shape.variants[1].1.len(), 2); // two Float fields
}

#[test]
fn check_result_contains_builtin_enums() {
    let result = check_full("function main() { }");
    // Option and Result are pre-registered builtins
    assert!(result.module.enum_by_name.contains_key("Option"));
    assert!(result.module.enum_by_name.contains_key("Result"));
    let option = result.module.enum_info_by_name("Option").unwrap();
    assert_eq!(option.type_params, vec!["T".to_string()]);
    assert_eq!(option.variants.len(), 2); // Some, None
}

#[test]
fn check_result_contains_methods() {
    let result = check_full(
        r#"
struct Counter { val: Int }
impl Counter {
function get(self) -> Int { return self.val }
function inc(self) -> Counter { return Counter(self.val + 1) }
}
function main() { }
"#,
    );
    assert!(result.module.method_index.contains_key("Counter"));
    assert!(
        result
            .module
            .method_info_by_name("Counter", "get")
            .is_some()
    );
    assert!(
        result
            .module
            .method_info_by_name("Counter", "inc")
            .is_some()
    );
    let get = result.module.method_info_by_name("Counter", "get").unwrap();
    assert_eq!(get.return_type, Type::Int);
    assert!(get.params.is_empty()); // excludes self
}

#[test]
fn check_result_contains_traits_and_impls() {
    let result = check_full(
        r#"
trait Display {
function toString(self) -> String
}
struct Point {
x: Int
y: Int

impl Display {
    function toString(self) -> String { return "point" }
}
}
function main() { }
"#,
    );
    assert!(result.module.trait_by_name.contains_key("Display"));
    let display = result.module.trait_info_by_name("Display").unwrap();
    assert_eq!(display.methods.len(), 1);
    assert_eq!(display.methods[0].name, "toString");
    assert_eq!(display.methods[0].return_type, Type::String);

    assert!(
        result
            .trait_impls
            .contains(&("Point".to_string(), "Display".to_string()))
    );
}

#[test]
fn check_result_contains_type_aliases() {
    let result = check_full(
        r#"
type UserId = Int
type StringResult<T> = Result<T, String>
function main() { }
"#,
    );
    assert!(result.type_aliases.contains_key("UserId"));
    assert_eq!(result.type_aliases["UserId"].target, Type::Int);
    assert!(result.type_aliases["UserId"].type_params.is_empty());

    assert!(result.type_aliases.contains_key("StringResult"));
    assert_eq!(
        result.type_aliases["StringResult"].type_params,
        vec!["T".to_string()]
    );
}

#[test]
fn check_result_function_with_defaults() {
    let result = check_full(
        r#"
function greet(name: String, greeting: String = "Hello") -> String {
return greeting + " " + name
}
function main() { }
"#,
    );
    let info = result.module.function_info_by_name("greet").unwrap();
    assert_eq!(info.params.len(), 2);
    assert_eq!(info.param_names, vec!["name", "greeting"]);
    let default_indices: Vec<usize> = {
        let mut v: Vec<usize> = info.default_param_exprs.keys().copied().collect();
        v.sort();
        v
    };
    assert_eq!(default_indices, vec![1]);
}

/// Method-side parallel of the FunctionInfo shape test: pin that
/// `MethodInfo.default_param_exprs` captures defaults with indices
/// relative to non-self params.
#[test]
fn check_result_method_with_defaults() {
    let result = check_full(
        r#"
struct Counter { n: Int }
impl Counter {
    function bump(self, by: Int = 1, factor: Int = 2) -> Int {
        return self.n * factor + by
    }
}
function main() { }
"#,
    );
    let info = result
        .module
        .method_info_by_name("Counter", "bump")
        .unwrap();
    assert_eq!(info.params.len(), 2, "non-self params count");
    let default_indices: Vec<usize> = {
        let mut v: Vec<usize> = info.default_param_exprs.keys().copied().collect();
        v.sort();
        v
    };
    assert_eq!(
        default_indices,
        vec![0, 1],
        "both non-self params carry defaults; indices are 0-based over non-self slots",
    );
}

/// Method defaults cannot reference `self`: the default expression
/// evaluates at the caller's call site where `self` is not yet bound.
/// Pass-1 of `check_impl` (checker.rs) checks defaults in a scope
/// with neither `self` nor sibling params bound, so the identifier
/// falls through as unknown.
#[test]
fn method_default_cannot_reference_self() {
    assert_has_error(
        r#"
struct Counter { n: Int }
impl Counter {
    function bump(self, by: Int = self.n) -> Int {
        return self.n + by
    }
}
function main() { }
"#,
        "undefined",
    );
}

/// Method defaults cannot reference sibling parameters either — same
/// caller-site-evaluation rule as free functions.
#[test]
fn method_default_cannot_reference_earlier_parameter() {
    assert_has_error(
        r#"
struct Counter { n: Int }
impl Counter {
    function bump(self, x: Int, y: Int = x + 1) -> Int {
        return self.n + x + y
    }
}
function main() { }
"#,
        "undefined",
    );
}

/// Over-long arg lists on a defaulted method call still error, and
/// the message is the arity form (distinct from the by-name
/// "missing argument(s)" form for under-fill).
#[test]
fn method_too_many_args_on_defaulted_method() {
    assert_has_error(
        r#"
struct Counter { n: Int }
impl Counter {
    function bump(self, by: Int = 1) -> Int {
        return self.n + by
    }
}
function main() {
    let c: Counter = Counter(0)
    c.bump(1, 2)
}
"#,
        "takes 1 argument(s), got 2",
    );
}

/// Under-fill names the missing parameter (not the confusing
/// "takes N, got M" arity message).  The required-before-defaulted
/// layout is forced by the parser's "non-default parameter cannot
/// follow a default parameter" rule, so "user passes zero, the
/// required slot is missing by name" is the minimal shape that
/// exercises the named-missing diagnostic.
#[test]
fn method_missing_required_arg_names_the_parameter() {
    assert_has_error(
        r#"
struct Counter { n: Int }
impl Counter {
    function bump(self, factor: Int, by: Int = 1) -> Int {
        return self.n * factor + by
    }
}
function main() {
    let c: Counter = Counter(0)
    c.bump()
}
"#,
        "missing argument(s): factor",
    );
}

// ── Expression-level type annotations ───────────────────

#[test]
fn expr_types_populated_for_literals() {
    let result = check_full(
        r#"
function main() {
let x: Int = 42
let y: Float = 3.14
let s: String = "hello"
let b: Bool = true
}
"#,
    );
    // expr_types should be non-empty — every expression gets recorded
    assert!(
        !result.module.expr_types.is_empty(),
        "expr_types should be populated"
    );
    // Check that all basic types appear in the values
    let types: Vec<&Type> = result.module.expr_types.values().collect();
    assert!(types.contains(&&Type::Int));
    assert!(types.contains(&&Type::Float));
    assert!(types.contains(&&Type::String));
    assert!(types.contains(&&Type::Bool));
}

#[test]
fn expr_types_populated_for_binary_ops() {
    let result = check_full(
        r#"
function main() {
let x: Int = 1 + 2
let y: Bool = 1 < 2
let z: Float = 1.0 + 2.0
}
"#,
    );
    let types: Vec<&Type> = result.module.expr_types.values().collect();
    assert!(types.contains(&&Type::Int));
    assert!(types.contains(&&Type::Bool));
    assert!(types.contains(&&Type::Float));
}

#[test]
fn expr_types_populated_for_function_calls() {
    let result = check_full(
        r#"
function add(a: Int, b: Int) -> Int { return a + b }
function main() {
let x: Int = add(1, 2)
}
"#,
    );
    // The call expression `add(1, 2)` should be recorded as Type::Int
    let has_int_call = result.module.expr_types.values().any(|t| *t == Type::Int);
    assert!(has_int_call, "call to add() should produce Type::Int");
}

#[test]
fn expr_types_populated_for_method_calls() {
    let result = check_full(
        r#"
struct Counter { val: Int }
impl Counter {
function get(self) -> Int { return self.val }
}
function main() {
let c: Counter = Counter(5)
let v: Int = c.get()
}
"#,
    );
    let has_int = result.module.expr_types.values().any(|t| *t == Type::Int);
    assert!(has_int, "method call should produce Type::Int");
}

#[test]
fn expr_types_populated_for_string_interpolation() {
    let result = check_full(
        r#"
function main() {
let name: String = "world"
let msg: String = "hello {name}"
}
"#,
    );
    let string_count = result
        .module
        .expr_types
        .values()
        .filter(|t| **t == Type::String)
        .count();
    // At least: the "world" literal, the "hello {name}" interpolation, and the `name` ident
    assert!(
        string_count >= 3,
        "should have at least 3 String-typed expressions, got {}",
        string_count
    );
}

// ── Snapshot tests for error messages ──────────────────────────────

#[test]
fn snapshot_error_type_mismatch() {
    let diags = check_source(r#"function main() { let x: Int = "hello" }"#);
    let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
    insta::assert_debug_snapshot!(messages);
}

#[test]
fn snapshot_error_undefined_variable() {
    let diags = check_source("function main() { print(x) }");
    let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
    insta::assert_debug_snapshot!(messages);
}

#[test]
fn snapshot_error_immutable_assignment() {
    let diags = check_source("function main() { let x: Int = 1\n x = 2 }");
    let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
    insta::assert_debug_snapshot!(messages);
}

#[test]
fn snapshot_error_wrong_arg_count() {
    let diags = check_source(
        "function add(a: Int, b: Int) -> Int { return a + b }\nfunction main() { add(1) }",
    );
    let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
    insta::assert_debug_snapshot!(messages);
}

#[test]
fn snapshot_error_trait_not_implemented() {
    let diags = check_source(
        "trait Display {\n  function toString(self) -> String\n}\nstruct Point { x: Int  y: Int }\nfunction show<T: Display>(item: T) -> String { return item.toString() }\nfunction main() { show(Point(1, 2)) }",
    );
    let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
    insta::assert_debug_snapshot!(messages);
}

// ── Endpoint checking tests ─────────────────────────────────────

/// A valid endpoint with body, response, and errors produces no diagnostics.
#[test]
fn endpoint_valid_no_errors() {
    assert_no_errors(
        r#"
struct User { id: Int  name: String  email: String }
endpoint createUser: POST "/api/users" {
body User
response User
error {
    Conflict(409)
    BadRequest(400)
}
}
"#,
    );
}

/// `body` on a GET endpoint produces an error.
#[test]
fn endpoint_body_on_get() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
endpoint getUser: GET "/api/users/{id}" {
body User
response User
}
"#,
        "`body` is not allowed on Get endpoints",
    );
}

/// `body` on a DELETE endpoint produces an error.
#[test]
fn endpoint_body_on_delete() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
endpoint deleteUser: DELETE "/api/users/{id}" {
body User
}
"#,
        "`body` is not allowed on Delete endpoints",
    );
}

/// An unknown response type produces an error.
#[test]
fn endpoint_unknown_response_type() {
    assert_has_error(
        r#"
endpoint getWidget: GET "/api/widgets/{id}" {
response Widget
}
"#,
        "unknown response type",
    );
}

/// An unknown body struct produces an error.
#[test]
fn endpoint_unknown_body_type() {
    assert_has_error(
        r#"
endpoint createWidget: POST "/api/widgets" {
body Widget
}
"#,
        "unknown struct `Widget` in body type",
    );
}

/// `omit { nonexistent }` on a struct without that field produces an error.
#[test]
fn endpoint_omit_nonexistent_field() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
body User omit { nonexistent }
}
"#,
        "field `nonexistent` does not exist on struct `User`",
    );
}

/// `pick { nonexistent }` on a struct without that field produces an error.
#[test]
fn endpoint_pick_nonexistent_field() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
body User pick { nonexistent }
}
"#,
        "field `nonexistent` does not exist on struct `User`",
    );
}

/// `partial { nonexistent }` on a struct without that field produces an error.
#[test]
fn endpoint_partial_nonexistent_field() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
endpoint updateUser: PATCH "/api/users/{id}" {
body User partial { nonexistent }
}
"#,
        "field `nonexistent` does not exist on struct `User`",
    );
}

// ── Inline response projection (`response Struct pick/omit/partial`) ──────────

/// A bare response projection (`response User pick { … }`) resolves: the endpoint
/// gains a `response_projection` field set and the resolved response `Type` points
/// at the generated `<Endpoint>Response` struct. No errors.
#[test]
fn endpoint_response_projection_bare() {
    let analysis = check_full(
        r#"
struct User { id: Int  name: String  secret: String }
endpoint getUser: GET "/api/users/{id}" {
response User pick { id, name }
}
"#,
    );
    let ep = analysis
        .endpoints
        .iter()
        .find(|e| e.name == "getUser")
        .expect("getUser endpoint");
    let proj = ep
        .response_projection
        .as_ref()
        .expect("response_projection should be populated");
    let fields: Vec<&str> = proj.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(fields, vec!["id", "name"], "projected field set");
    assert_eq!(
        ep.response,
        Some(Type::Named("GetUserResponse".to_string())),
        "response type should reference the generated struct",
    );
}

/// A `List<Struct pick { … }>` response projection resolves to a
/// `List<GetUserResponse>` and still records the projected field set.
#[test]
fn endpoint_response_projection_list() {
    let analysis = check_full(
        r#"
struct User { id: Int  name: String  secret: String }
endpoint listUsers: GET "/api/users" {
response List<User pick { id, name }>
}
"#,
    );
    let ep = analysis
        .endpoints
        .iter()
        .find(|e| e.name == "listUsers")
        .expect("listUsers endpoint");
    assert!(
        ep.response_projection.is_some(),
        "response_projection should be populated for a List<…> projection"
    );
    assert_eq!(
        ep.response,
        Some(Type::Generic(
            "List".to_string(),
            vec![Type::Named("ListUsersResponse".to_string())]
        )),
        "response type should be a List of the generated struct",
    );
}

/// A projection on a `response` whose base struct is unknown reports the error in
/// the `response` context (not the `body` context) — and ONLY that error: the
/// recognized-but-failed projection must not fall through to the normal response
/// path, which would re-resolve the modifier-bearing type and emit a spurious
/// "projection misplaced" + "unknown response type" cascade.
#[test]
fn endpoint_response_projection_unknown_struct() {
    let src = r#"
endpoint getUser: GET "/api/users/{id}" {
response Nope pick { id }
}
"#;
    assert_has_error(src, "unknown struct `Nope` in response type");
    assert_no_error_containing(src, "projection is only allowed");
    assert_no_error_containing(src, "unknown response type");
}

/// A projection picking a field the base struct lacks reports the field error
/// (the resolver is shared with the `body` path; this exercises it from a
/// `response`).
#[test]
fn endpoint_response_projection_nonexistent_field() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
endpoint getUser: GET "/api/users/{id}" {
response User pick { nonexistent }
}
"#,
        "field `nonexistent` does not exist on struct `User`",
    );
}

/// A projection in a position that is neither a `body` base nor a `response`
/// type — here a struct field — is rejected as misplaced.
#[test]
fn projection_misplaced_in_struct_field() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
struct Wrapper { u: User pick { id } }
"#,
        "projection is only allowed directly on a `body` base type or a `response` type",
    );
}

/// A projection on a query parameter type is rejected as misplaced.
#[test]
fn projection_misplaced_in_query_param() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
endpoint listUsers: GET "/api/users" {
query { u: User pick { id } }
response User
}
"#,
        "projection is only allowed directly on a `body` base type or a `response` type",
    );
}

/// A projection nested one level too deep in a response (`Option<Struct pick …>`)
/// is rejected: only the bare and `List<…>` shapes are wired. The diagnostic names
/// the supported shapes rather than claiming the position isn't a response.
#[test]
fn projection_misplaced_nested_in_response_option() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
endpoint getUser: GET "/api/users/{id}" {
response Option<User pick { id }>
}
"#,
        "projection is only allowed directly on a `body` base type or a `response` type",
    );
}

/// A projection nested as a `Map<_, Struct pick …>` value type is rejected for the
/// same reason as the `Option<…>` case: only the bare and `List<…>` response shapes
/// are wired, so the `Map` value reaches the misplaced-projection error.
#[test]
fn projection_misplaced_nested_in_response_map() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
endpoint getUsers: GET "/api/users" {
response Map<String, User pick { id }>
}
"#,
        "projection is only allowed directly on a `body` base type or a `response` type",
    );
}

/// A projection that picks a `File`-typed field is rejected: the projection path
/// bypasses the binary-download / multipart response validation, so a `File` field
/// would silently miscompile into a `File`-bearing `<Endpoint>Response` with no
/// multipart handling. The guard reports the field (not a cascade) and suppresses
/// the normal-path "unknown response type" fallthrough.
#[test]
fn projection_rejects_picked_file_field() {
    let src = r#"
struct Doc { id: Int  blob: File }
endpoint getDoc: GET "/api/docs/{id}" {
response Doc pick { blob }
}
"#;
    assert_has_error(src, "cannot pick a `File`-typed field");
    assert_no_error_containing(src, "unknown response type");
}

/// The same guard applies when the picked field's type is `Option<File>` (an
/// optional file field on the base struct) — exercising the recursive branch of
/// `field_carries_file`.
#[test]
fn projection_rejects_picked_optional_file_field() {
    assert_has_error(
        r#"
struct Doc { id: Int  blob: Option<File> }
endpoint getDoc: GET "/api/docs/{id}" {
response Doc pick { blob }
}
"#,
        "cannot pick a `File`-typed field",
    );
}

/// Duplicate error variant names within an endpoint produce an error.
#[test]
fn endpoint_duplicate_error_variant() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
body User
error {
    Conflict(409)
    Conflict(409)
}
}
"#,
        "duplicate error variant `Conflict`",
    );
}

/// A status code outside the 400-599 range produces an error.
#[test]
fn endpoint_status_code_out_of_range() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
body User
error {
    Success(200)
}
}
"#,
        "status code 200 is not a client/server error",
    );
}

/// An endpoint with query parameters produces no errors.
#[test]
fn endpoint_valid_with_query_params() {
    assert_no_errors(
        r#"
struct User { id: Int  name: String }
endpoint listUsers: GET "/api/users" {
query {
    page: Int = 1
    limit: Int = 20
    search: String
}
response User
}
"#,
    );
}

/// An endpoint with chained omit, pick, and partial modifiers on the body
/// produces no errors when all referenced fields exist.
#[test]
fn endpoint_valid_omit_pick_partial() {
    assert_no_errors(
        r#"
struct User { id: Int  name: String  email: String  bio: String }
endpoint updateUser: PATCH "/api/users/{id}" {
body User omit { id } partial
response User
}
"#,
    );
}

/// An endpoint with no response is valid.
#[test]
fn endpoint_no_response() {
    assert_no_errors(
        r#"
endpoint deleteUser: DELETE "/api/users/{id}" {
}
"#,
    );
}

/// An endpoint with an enum as the response type is valid.
#[test]
fn endpoint_enum_response() {
    assert_no_errors(
        r#"
enum Status { Active  Inactive  Banned }
endpoint getStatus: GET "/api/status" {
response Status
}
"#,
    );
}

/// An endpoint with `List<User>` as the response type is valid.
#[test]
fn endpoint_list_response() {
    assert_no_errors(
        r#"
struct User { id: Int  name: String }
endpoint listUsers: GET "/api/users" {
response List<User>
}
"#,
    );
}

/// An endpoint with `Option<User>` as the response type is valid.
#[test]
fn endpoint_option_response() {
    assert_no_errors(
        r#"
struct User { id: Int  name: String }
endpoint findUser: GET "/api/users/{id}" {
response Option<User>
}
"#,
    );
}

/// Error status code at boundary 400 (valid).
#[test]
fn endpoint_error_status_code_400() {
    assert_no_errors(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
body User
error { BadRequest(400) }
}
"#,
    );
}

/// Error status code at boundary 599 (valid).
#[test]
fn endpoint_error_status_code_599() {
    assert_no_errors(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
body User
error { InternalError(599) }
}
"#,
    );
}

/// Error status code 399 is out of range.
#[test]
fn endpoint_error_status_code_399() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
body User
error { Redirect(399) }
}
"#,
        "status code 399 is not a client/server error",
    );
}

/// Error status code 600 is out of range.
#[test]
fn endpoint_error_status_code_600() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
body User
error { TooHigh(600) }
}
"#,
        "status code 600 is not a client/server error",
    );
}

/// `body` is allowed on POST, PUT, and PATCH endpoints.
#[test]
fn endpoint_body_on_post_put_patch() {
    for method in ["POST", "PUT", "PATCH"] {
        assert_no_errors(&format!(
            r#"
struct User {{ id: Int  name: String }}
endpoint testEndpoint: {method} "/api/users" {{
body User
response User
}}
"#
        ));
    }
}

/// `pick` followed by `partial` on selected fields is valid.
#[test]
fn endpoint_pick_then_selective_partial() {
    assert_no_errors(
        r#"
struct User { id: Int  name: String  email: String  age: Int }
endpoint patchUser: PATCH "/api/users/{id}" {
body User pick { name, email, age } partial { age }
response User
}
"#,
    );
}

/// Selective `partial` with a nonexistent field produces an error.
#[test]
fn endpoint_selective_partial_nonexistent_after_omit() {
    assert_has_error(
        r#"
struct User { id: Int  name: String  email: String }
endpoint updateUser: PATCH "/api/users/{id}" {
body User omit { id } partial { name, nonexistent }
}
"#,
        "field `nonexistent` does not exist",
    );
}

/// An endpoint with only query params and no body/response is valid.
#[test]
fn endpoint_query_only() {
    assert_no_errors(
        r#"
endpoint search: GET "/api/search" {
query {
    term: String
    page: Int = 1
}
}
"#,
    );
}

/// An endpoint with multiple path parameters is valid.
#[test]
fn endpoint_multiple_path_params() {
    assert_no_errors(
        r#"
struct Comment { id: Int  text: String }
endpoint getComment: GET "/api/users/{userId}/posts/{postId}" {
response Comment
}
"#,
    );
}

// ── DefaultValue extraction tests ───────────────────────────────

/// Integer default value is extracted into `DefaultValue::Int`.
#[test]
fn endpoint_default_value_int() {
    let result = check_full(
        r#"
endpoint list: GET "/api/items" {
query { page: Int = 1 }
}
"#,
    );
    let ep = &result.endpoints[0];
    assert!(matches!(
        ep.query_params[0].default_value,
        Some(DefaultValue::Int(1))
    ));
}

/// String default value is extracted into `DefaultValue::String`.
#[test]
fn endpoint_default_value_string() {
    let result = check_full(
        r#"
endpoint list: GET "/api/items" {
query { sort: String = "name" }
}
"#,
    );
    let ep = &result.endpoints[0];
    assert!(matches!(
        &ep.query_params[0].default_value,
        Some(DefaultValue::String(s)) if s == "name"
    ));
}

/// Boolean default value is extracted into `DefaultValue::Bool`.
#[test]
fn endpoint_default_value_bool() {
    let result = check_full(
        r#"
endpoint list: GET "/api/items" {
query { verbose: Bool = false }
}
"#,
    );
    let ep = &result.endpoints[0];
    assert!(matches!(
        ep.query_params[0].default_value,
        Some(DefaultValue::Bool(false))
    ));
}

/// Query param without a default has `default_value: None`.
#[test]
fn endpoint_no_default_value() {
    let result = check_full(
        r#"
endpoint search: GET "/api/search" {
query { term: String }
}
"#,
    );
    let ep = &result.endpoints[0];
    assert!(ep.query_params[0].default_value.is_none());
    assert!(!ep.query_params[0].has_default);
}

/// Multiple query params with mixed defaults are resolved correctly.
#[test]
fn endpoint_mixed_defaults() {
    let result = check_full(
        r#"
endpoint list: GET "/api/items" {
query {
    page: Int = 1
    term: String
    verbose: Bool = true
}
}
"#,
    );
    let ep = &result.endpoints[0];
    assert!(matches!(
        ep.query_params[0].default_value,
        Some(DefaultValue::Int(1))
    ));
    assert!(ep.query_params[1].default_value.is_none());
    assert!(matches!(
        ep.query_params[2].default_value,
        Some(DefaultValue::Bool(true))
    ));
}

// ── Where constraint validation tests ───────────────────────────

/// A struct with valid `where` constraints on Int and String fields
/// produces no errors.
#[test]
fn constraint_valid_numeric_and_string() {
    assert_no_errors(
        r#"
struct User {
age: Int where self >= 0 && self <= 150
name: String where self.length > 0 && self.length <= 100
}
"#,
    );
}

/// A `where` constraint that uses `self.contains` on a String is valid.
#[test]
fn constraint_valid_string_contains() {
    assert_no_errors(
        r#"
struct User {
email: String where self.contains("@") && self.length > 3
}
"#,
    );
}

/// A `where` constraint that evaluates to Int (not Bool) produces an error.
#[test]
fn constraint_must_be_bool() {
    assert_has_error(
        r#"
struct User {
age: Int where self + 1
}
"#,
        "constraint on field `age` must evaluate to Bool",
    );
}

/// Constraints are inherited by derived endpoint body types.
#[test]
fn constraint_inherited_by_derived_type() {
    let result = check_full(
        r#"
struct User {
id: Int
name: String where self.length > 0
age: Int where self >= 0
}
endpoint createUser: POST "/api/users" {
body User omit { id }
response User
}
"#,
    );
    let ep = &result.endpoints[0];
    let body = ep.body.as_ref().unwrap();
    // name field should have constraint
    let name_field = body.fields.iter().find(|f| f.name == "name").unwrap();
    assert!(name_field.constraint.is_some());
    // age field should have constraint
    let age_field = body.fields.iter().find(|f| f.name == "age").unwrap();
    assert!(age_field.constraint.is_some());
}

/// Omitted fields don't appear in derived type (constraints irrelevant).
#[test]
fn constraint_omitted_fields_removed() {
    let result = check_full(
        r#"
struct User {
id: Int
name: String where self.length > 0
age: Int where self >= 0
}
endpoint createUser: POST "/api/users" {
body User omit { id, age }
response User
}
"#,
    );
    let ep = &result.endpoints[0];
    let body = ep.body.as_ref().unwrap();
    assert_eq!(body.fields.len(), 1);
    assert_eq!(body.fields[0].name, "name");
    assert!(body.fields[0].constraint.is_some());
}

/// Struct with constraint is stored in StructInfo.fields as FieldInfo.
#[test]
fn constraint_stored_in_struct_info() {
    let result = check_full(
        r#"
struct Item {
price: Int where self > 0
name: String
}
function main() { }
"#,
    );
    let item = result.module.struct_info_by_name("Item").unwrap();
    assert!(item.fields[0].constraint.is_some());
    assert!(item.fields[1].constraint.is_none());
}

/// `or` constraint is valid.
#[test]
fn constraint_valid_or() {
    assert_no_errors(
        r#"
struct Range { x: Int where self < 0 || self > 100 }
"#,
    );
}

/// Float field with constraint is valid.
#[test]
fn constraint_valid_float() {
    assert_no_errors(
        r#"
struct Item { price: Float where self > 0.0 && self < 1000.0 }
"#,
    );
}

/// Constraint inheritance through `pick` modifier.
#[test]
fn constraint_inherited_through_pick() {
    let result = check_full(
        r#"
struct User {
id: Int
name: String where self.length > 0
email: String where self.contains("@")
age: Int where self >= 0
}
endpoint updateEmail: PATCH "/api/users/{id}" {
body User pick { email }
response User
}
"#,
    );
    let ep = &result.endpoints[0];
    let body = ep.body.as_ref().unwrap();
    assert_eq!(body.fields.len(), 1);
    assert_eq!(body.fields[0].name, "email");
    assert!(body.fields[0].constraint.is_some());
}

/// Constraint on partial field: constraint is preserved, field is optional.
#[test]
fn constraint_preserved_through_partial() {
    let result = check_full(
        r#"
struct User {
id: Int
name: String where self.length > 0
age: Int where self >= 0
}
endpoint updateUser: PATCH "/api/users/{id}" {
body User omit { id } partial
response User
}
"#,
    );
    let ep = &result.endpoints[0];
    let body = ep.body.as_ref().unwrap();
    let name_field = body.fields.iter().find(|f| f.name == "name").unwrap();
    assert!(name_field.optional, "field should be optional from partial");
    assert!(
        name_field.constraint.is_some(),
        "constraint should be preserved"
    );
}

// ── Schema declaration tests (no-op in sema) ────────────────────

/// Schema declarations produce no type-check errors.
#[test]
fn schema_no_errors() {
    assert_no_errors(
        r#"
struct User { id: Int  name: String }
schema db {
table users from User {
    primary key id
    unique name
}
}
"#,
    );
}

/// Schema with struct, endpoints, and function — all coexist cleanly.
#[test]
fn schema_coexists_with_other_decls() {
    assert_no_errors(
        r#"
struct User { id: Int  name: String }
endpoint getUser: GET "/api/users/{id}" {
response User
}
schema db {
table users from User {
    primary key id
}
}
function main() { let x: Int = 1 }
"#,
    );
}

/// Schema with `from NonexistentType` produces no errors (parse-only).
#[test]
fn schema_nonexistent_type_no_error() {
    assert_no_errors(
        r#"
schema db {
table widgets from Widget {
    primary key id
}
}
"#,
    );
}

/// Multiple schema declarations in one program produce no errors.
#[test]
fn schema_multiple_declarations() {
    assert_no_errors(
        r#"
schema main_db {
table users from User { primary key id }
}
schema analytics_db {
table events { String eventType  Int timestamp }
}
"#,
    );
}

// ── LSP data: definition_span and symbol_references ─────────────

/// definition_span is populated for structs.
#[test]
fn definition_span_on_struct() {
    let result = check_full(
        r#"
struct User { id: Int  name: String }
function main() { }
"#,
    );
    let user = result.module.struct_info_by_name("User").unwrap();
    assert!(
        user.definition_span.start < user.definition_span.end,
        "struct should have a non-empty definition span"
    );
}

/// definition_span is populated for functions.
#[test]
fn definition_span_on_function() {
    let result = check_full(
        r#"
function add(a: Int, b: Int) -> Int { a + b }
function main() { }
"#,
    );
    let add = result.module.function_info_by_name("add").unwrap();
    assert!(
        add.definition_span.start < add.definition_span.end,
        "function should have a non-empty definition span"
    );
}

/// definition_span is populated for enums.
#[test]
fn definition_span_on_enum() {
    let result = check_full(
        r#"
enum Color { Red  Green  Blue }
function main() { }
"#,
    );
    let color = result.module.enum_info_by_name("Color").unwrap();
    assert!(
        color.definition_span.start < color.definition_span.end,
        "enum should have a non-empty definition span"
    );
}

/// symbol_references tracks variable references.
#[test]
fn symbol_references_tracks_variables() {
    let result = check_full(
        r#"
function main() {
let x: Int = 42
print(x)
}
"#,
    );
    let var_refs: Vec<_> = result
        .symbol_references
        .values()
        .filter(|r| r.name == "x" && r.kind == SymbolKind::Variable)
        .collect();
    assert!(
        !var_refs.is_empty(),
        "should have at least one reference to variable x"
    );
}

/// symbol_references tracks function calls.
#[test]
fn symbol_references_tracks_function_calls() {
    let result = check_full(
        r#"
function add(a: Int, b: Int) -> Int { a + b }
function main() { let r: Int = add(1, 2) }
"#,
    );
    let fn_refs: Vec<_> = result
        .symbol_references
        .values()
        .filter(|r| r.name == "add" && r.kind == SymbolKind::Function)
        .collect();
    assert!(
        !fn_refs.is_empty(),
        "should have a reference to function add"
    );
}

/// symbol_references tracks field accesses.
#[test]
fn symbol_references_tracks_field_access() {
    let result = check_full(
        r#"
struct Point { x: Int  y: Int }
function main() {
let p: Point = Point(1, 2)
print(p.x)
}
"#,
    );
    let field_refs: Vec<_> = result
        .symbol_references
        .values()
        .filter(|r| {
            r.name == "x"
                && matches!(&r.kind, SymbolKind::Field { struct_name } if struct_name == "Point")
        })
        .collect();
    assert!(
        !field_refs.is_empty(),
        "should have a reference to field Point.x"
    );
}

// ── Duplicate endpoint name ───────────────────────────────────────

/// Two endpoints with the same name produce an error.
#[test]
fn endpoint_duplicate_name() {
    assert_has_error(
        r#"
struct User { id: Int  name: String }
endpoint getUser: GET "/api/users/{id}" {
response User
}
endpoint getUser: GET "/api/users/{id}/v2" {
response User
}
"#,
        "duplicate endpoint name `getUser`",
    );
}

// ── Empty omit/pick blocks ────────────────────────────────────────

/// `omit { }` — omitting nothing — is valid (body equals the full struct).
#[test]
fn endpoint_empty_omit() {
    let result = check_full(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
body User omit { }
response User
}
"#,
    );
    let ep = &result.endpoints[0];
    let body = ep.body.as_ref().unwrap();
    assert_eq!(body.fields.len(), 2);
}

/// `pick { }` — picking nothing — is valid (body has zero fields).
#[test]
fn endpoint_empty_pick() {
    let result = check_full(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
body User pick { }
response User
}
"#,
    );
    let ep = &result.endpoints[0];
    let body = ep.body.as_ref().unwrap();
    assert_eq!(body.fields.len(), 0);
}

// ── Omit all fields ──────────────────────────────────────────────

/// `omit` removing all fields results in an empty derived type.
#[test]
fn endpoint_omit_all_fields() {
    let result = check_full(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
body User omit { id, name }
response User
}
"#,
    );
    let ep = &result.endpoints[0];
    let body = ep.body.as_ref().unwrap();
    assert_eq!(body.fields.len(), 0);
}

// ── Where constraint on complex types ────────────────────────────

/// A `where` constraint on a `List<Int>` field using `.length` is valid.
#[test]
fn constraint_on_list_type() {
    assert_no_errors(
        r#"
struct Config {
tags: List<Int> where self.length > 0
}
"#,
    );
}

/// A `where` constraint on an `Option<Int>` field is valid — `self`
/// binds to the `Option<Int>` type.
#[test]
fn constraint_on_option_type() {
    assert_no_errors(
        r#"
struct Config {
maxRetries: Option<Int> where self.isSome()
}
"#,
    );
}

/// A field name starting with `_` is REJECTED — Python's pydantic model drops it
/// (private attribute) and Go leaves it unexported, both a silent data loss.
#[test]
fn field_leading_underscore_rejected() {
    assert_has_error(
        r#"
struct Thing {
_hidden: Int
}
"#,
        "starts with `_`",
    );
}

/// Two field names that collapse to the same snake_case are REJECTED — the Python
/// model would declare the attribute twice and silently drop one.
#[test]
fn fields_snake_case_collision_rejected() {
    assert_has_error(
        r#"
struct Thing {
fooBar: Int
foo_bar: String
}
"#,
        "snake_case collision",
    );
}

/// Distinct field names that snake-case to distinct attributes are accepted (no
/// false positive: `avatarUrl` and `name` don't collide).
#[test]
fn fields_distinct_snake_case_ok() {
    assert_no_errors(
        r#"
struct Author {
name: String
avatarUrl: String
}
"#,
    );
}

/// The collision check fires on a struct used as a request body (the
/// codegen-relevant path, not just a free-standing struct) — the body model is the
/// surface that actually crosses the wire, so the rejection must reach it.
#[test]
fn body_struct_snake_case_collision_rejected() {
    assert_has_error(
        r#"
struct CreateThing {
fooBar: Int
foo_bar: String
}
endpoint createThing: POST "/things" {
body CreateThing
response CreateThing
}
"#,
        "snake_case collision",
    );
}

/// A typo'd constraint property (`self.lenght`) is REJECTED, not silently
/// swallowed as an error type. Regression for the field-access-on-builtin hole.
#[test]
fn constraint_typo_length_property_rejected() {
    assert_has_error(
        r#"
struct User {
name: String where self.lenght > 0
}
"#,
        "has no property `lenght`",
    );
}

/// `self.length` is a real property of a `String` field (an `Int`); the
/// constraint checks clean.
#[test]
fn constraint_length_on_string_valid() {
    assert_no_errors(
        r#"
struct User {
name: String where self.length > 0 && self.length <= 100
}
"#,
    );
}

/// `self.length` on an `Option<String>` field unwraps the `Option` to the inner
/// `String` (the constraint describes the present value), so it checks clean.
#[test]
fn constraint_length_on_optional_string_valid() {
    assert_no_errors(
        r#"
struct User {
bio: Option<String> where self.length > 0
}
"#,
    );
}

/// `self.length` is meaningful for a `List` field too (every target renders a
/// length check).
#[test]
fn constraint_length_on_list_valid() {
    assert_no_errors(
        r#"
struct Cart {
items: List<String> where self.length > 0
}
"#,
    );
}

/// `self.length` on an `Option<List>` field unwraps the `Option` to the inner
/// `List` (the unwrap-then-`List` arm of the lengthable carve-out), so it checks
/// clean — same as `Option<String>` but exercising the `List` branch.
#[test]
fn constraint_length_on_optional_list_valid() {
    assert_no_errors(
        r#"
struct Cart {
items: Option<List<String>> where self.length > 0
}
"#,
    );
}

/// `self.length` on a non-lengthable type (`Int`) is a misuse and is rejected
/// rather than silently accepted as nonsense.
#[test]
fn constraint_length_on_int_rejected() {
    assert_has_error(
        r#"
struct User {
age: Int where self.length > 0
}
"#,
        "has no property `length`",
    );
}

/// A numeric comparison constraint on an `Option<Int>` field checks the inner
/// `Int` (the constraint describes the present value), so it is valid — not
/// rejected as `Option<Int>`-vs-`Int`.
#[test]
fn constraint_numeric_on_optional_int_valid() {
    assert_no_errors(
        r#"
struct Config {
maxRetries: Option<Int> where self >= 0 && self <= 10
}
"#,
    );
}

/// An equality comparison constraint on an `Option<Int>` field checks the inner
/// `Int` (same `Option` unwrap as the ordering operators, but exercising the
/// `==` arm of the binary-op match), so it is valid.
#[test]
fn constraint_equality_on_optional_int_valid() {
    assert_no_errors(
        r#"
struct Config {
mode: Option<Int> where self == 0
}
"#,
    );
}

/// The realistic optional idiom: a presence check `&&` an inner-length check on
/// the same `Option<String>` field. `self.isSome()` resolves on the `Option`
/// (method dispatch keeps `self` at its full type) while `self.length` unwraps to
/// the inner `String` (field-access carve-out) — both paths in one expression.
#[test]
fn constraint_issome_and_length_on_optional_string_valid() {
    assert_no_errors(
        r#"
struct User {
bio: Option<String> where self.isSome() && self.length > 0
}
"#,
    );
}

/// `self.length` on a `Map` field is rejected — `Map` is not a lengthable base
/// for constraint purposes (only `String`/`List` are). Locks the `Map` arm of
/// the same non-lengthable path that `Int` exercises.
#[test]
fn constraint_length_on_map_rejected() {
    assert_has_error(
        r#"
struct Cache {
entries: Map<String, Int> where self.length > 0
}
"#,
        "has no property `length`",
    );
}

/// `self.length` on a `Bytes` field is rejected — `Bytes` is deliberately
/// excluded from the lengthable bases (byte-length is ambiguous and unused by
/// any fixture), so the carve-out treats it like any other non-lengthable type.
#[test]
fn constraint_length_on_bytes_rejected() {
    assert_has_error(
        r#"
struct Upload {
blob: Bytes where self.length > 0
}
"#,
        "has no property `length`",
    );
}

/// A String/List *method* call (`self.contains`) on an `Option<T>` field operates
/// on the inner value in a constraint (codegen nil-guards it), matching the
/// `self.length` / numeric-comparison forms — so it checks clean. The method
/// dispatch retries on the unwrapped inner type as a last resort, after every
/// `Option`-level path (so `self.isSome()` still resolves on the `Option`).
#[test]
fn constraint_method_call_on_optional_string_valid() {
    assert_no_errors(
        r#"
struct User {
email: Option<String> where self.contains("@")
}
"#,
    );
}

/// An *unrecognized* method on an `Option<String>` field in a constraint is still
/// rejected — and exactly once. The first dispatch on the `Option` returns a silent
/// `None`, the retry dispatches on the unwrapped `String`, and `check_string_method`
/// reports a single "no method … on type `String`" diagnostic (naming the inner
/// type) and returns `Some(Error)`, so the catch-all below never also fires.
#[test]
fn constraint_unknown_method_on_optional_string_rejected_once() {
    assert_error_count_for_message(
        r#"
struct User {
email: Option<String> where self.fooBar()
}
"#,
        "no method `fooBar` on type `String`",
        1,
    );
}

/// Documents the other known residual: a struct-field access on an
/// `Option<Struct>` field (`self.zip`) is still rejected — the `Option` unwrap in
/// `check_field_access` feeds only the `.length` carve-out, not general field
/// resolution (only the outer type is looked up as a struct). Rejection is LOUD
/// (a real diagnostic) and names the inner struct, not `Option<Address>`.
#[test]
fn constraint_struct_field_access_on_optional_rejected_loudly() {
    assert_has_error(
        r#"
struct Address {
zip: Int
}
struct User {
addr: Option<Address> where self.zip > 0
}
"#,
        "type `Address` has no property `zip`",
    );
}

/// A typo'd subfield on a *plain* (non-optional) struct field is reported through
/// the struct branch — "struct `X` has no field `Y`", worded for a struct, not the
/// built-in "type `T` has no property" carve-out. The struct branch returns early
/// so the `in_constraint` check below cannot also fire (no double-report on the
/// same span).
#[test]
fn constraint_struct_field_typo_rejected_once() {
    assert_error_count_for_message(
        r#"
struct Address {
zip: Int
}
struct User {
addr: Address where self.zpi > 0
}
"#,
        "struct `Address` has no field `zpi`",
        1,
    );
}

// ── Query param default value type mismatch ──────────────────────

/// An Int query param with a String default produces an error.
#[test]
fn endpoint_query_default_type_mismatch() {
    assert_has_error(
        r#"
endpoint search: GET "/api/search" {
query { page: Int = "hello" }
}
"#,
        "default value for query param `page` does not match type `Int`",
    );
}

/// A Bool query param with an Int default produces an error.
#[test]
fn endpoint_query_default_bool_int_mismatch() {
    assert_has_error(
        r#"
endpoint search: GET "/api/search" {
query { verbose: Bool = 42 }
}
"#,
        "default value for query param `verbose` does not match type `Bool`",
    );
}

/// A String query param with a String default is valid.
#[test]
fn endpoint_query_default_string_match() {
    assert_no_errors(
        r#"
endpoint search: GET "/api/search" {
query { sort: String = "name" }
}
"#,
    );
}

// ── If as a first-class expression ───────────────────────────────

#[test]
fn if_expr_unifies_branch_types() {
    assert_no_errors("function main() { let x: Int = if true { 1 } else { 2 }\n print(x) }");
}

#[test]
fn if_expr_without_else_is_void() {
    // `let x: Int = ...` with a Void initializer should surface as a type
    // mismatch on the variable declaration.
    assert_has_error(
        "function main() { let x: Int = if true { 1 } }",
        "type mismatch",
    );
}

#[test]
fn if_expr_incompatible_branches() {
    assert_has_error(
        "function main() { let x: Int = if true { 1 } else { \"two\" } }",
        "incompatible types",
    );
}

#[test]
fn if_expr_diverging_branch_ok() {
    // Then-branch diverges (return); else contributes Int; overall type is Int.
    assert_no_errors(
        r#"
function f(n: Int) -> Int {
    let v: Int = if n < 0 { return 0 } else { n }
    v
}
function main() { print(f(5)) }
"#,
    );
}

#[test]
fn if_expr_else_if_chain_unifies() {
    assert_no_errors(
        r#"
function main() {
    let x: Int = if false { 1 } else if true { 2 } else { 3 }
    print(x)
}
"#,
    );
}

#[test]
fn if_expr_tail_with_all_returns_ok() {
    // `if` whose every branch ends in `return` satisfies the function's
    // non-Void return type — no "implicit return type mismatch" error.
    assert_no_errors(
        r#"
function classify(n: Int) -> String {
    if n < 0 { return "neg" } else { return "non-neg" }
}
function main() { print(classify(-1)) }
"#,
    );
}

// ── Definition name_span is precise ──────────────────────────────

/// Function definition_span covers only the name, not the full declaration.
#[test]
fn definition_span_is_name_only() {
    let result = check_full(
        r#"
function add(a: Int, b: Int) -> Int { a + b }
function main() { }
"#,
    );
    let add = result.module.function_info_by_name("add").unwrap();
    // "add" is 3 bytes, so the span length should be 3
    let span_len = add.definition_span.end - add.definition_span.start;
    assert_eq!(
        span_len, 3,
        "definition_span should cover just the name 'add'"
    );
}

// ── Generic call: unification conflict across argument positions ──

/// `identity<T>(a: T, b: T)` called with `(Int, String)` must report that
/// `T` was bound to two incompatible types. Previously the unification
/// failure was silently discarded and the first binding won.
#[test]
fn generic_call_reports_unification_conflict() {
    assert_has_error(
        r#"
function pair<T>(a: T, b: T) -> T { a }
function main() {
    let _ = pair(1, "two")
    print(0)
}
"#,
        "conflicting bindings for type parameter `T`",
    );
}

// ── Generic call: unresolved type parameter diagnostic ────────────

/// A generic type parameter that is not mentioned in any parameter position
/// cannot be inferred from argument types. Sema must surface this rather
/// than silently skipping the call.
#[test]
fn generic_call_reports_unresolved_type_parameter_via_return_type() {
    // `T` appears only in the return position — no argument constrains it.
    assert_has_error(
        r#"
function default<T>() -> T { default() }
function main() {
    let _ = default()
    print(0)
}
"#,
        "cannot infer type parameter `T`",
    );
}

/// When multiple type parameters are unresolvable, the diagnostic should
/// list all of them (rather than stopping at the first).
#[test]
fn generic_call_lists_all_unresolved_type_parameters() {
    let diags = check_source(
        r#"
function twoFree<A, B>() -> Int { 0 }
function main() {
    let _ = twoFree()
    print(0)
}
"#,
    );
    let joined = diags
        .iter()
        .map(|d| d.message.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("`A`") && joined.contains("`B`"),
        "expected diagnostic listing both `A` and `B`, got: {:?}",
        diags
    );
}

/// Unresolved-type-parameter diagnostics must be suppressed when an
/// argument is already `Type::Error` (e.g., undefined identifier),
/// otherwise the user sees a cascade of misleading "cannot infer"
/// errors on top of the real error.
#[test]
fn generic_call_suppresses_unresolved_cascade_when_arg_is_error() {
    let diags = check_source(
        r#"
function identity<T>(x: T) -> T { x }
function main() {
    let _ = identity(undefined_var)
    print(0)
}
"#,
    );
    let unresolved_count = diags
        .iter()
        .filter(|d| d.message.contains("cannot infer type parameter"))
        .count();
    assert_eq!(
        unresolved_count, 0,
        "should suppress 'cannot infer' cascade when arg is Type::Error, got: {:?}",
        diags
    );
}

// ── Generic method: type parameter recording ──────────────────────

/// A generic method on a *non-generic* struct must record its method-level
/// type argument into `call_type_args`, keyed by the MethodCallExpr span.
/// Previously this path silently skipped recording (bug #3 per phase-2.md).
#[test]
fn generic_method_on_non_generic_struct_records_call_type_args() {
    let tokens = tokenize(
        r#"
struct Holder {
    tag: Int
}
impl Holder {
    function wrap<U>(self, x: U) -> U { x }
}
function main() {
    let h = Holder(7)
    let r1 = h.wrap(42)
    let r2 = h.wrap("hello")
    print(r1)
    print(r2)
}
"#,
        SourceId(0),
    );
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let result = check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    assert!(
        result
            .module
            .call_type_args
            .values()
            .any(|v| v.as_slice() == [Type::Int]),
        "expected call_type_args to contain [Int] from `h.wrap(42)`, got: {:?}",
        result.module.call_type_args
    );
    assert!(
        result
            .module
            .call_type_args
            .values()
            .any(|v| v.as_slice() == [Type::String]),
        "expected call_type_args to contain [String] from `h.wrap(\"hello\")`, got: {:?}",
        result.module.call_type_args
    );
}

// ── Generic method on a *generic* struct ──────────────────────────

/// A generic method on a generic struct must record only the method's own
/// type parameters into `call_type_args` (the struct's parent type params
/// come from the receiver type, not from inference). This exercises the
/// parent-binding merge path in `check_method_call`.
#[test]
fn generic_method_on_generic_struct_records_only_method_type_args() {
    let tokens = tokenize(
        r#"
struct Pair<A, B> {
    first: A
    second: B
}
impl Pair {
    function swap<U>(self, x: U) -> U { x }
}
function main() {
    let p: Pair<Int, String> = Pair(1, "hi")
    print(p.swap(42))
    print(p.swap("hello"))
}
"#,
        SourceId(0),
    );
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let result = check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    // The method has one type param (U); call_type_args values should
    // have length 1 for both call sites, containing the method's inferred
    // concrete type. The struct's A and B must NOT appear.
    let recorded: Vec<Vec<Type>> = result.module.call_type_args.values().cloned().collect();
    assert!(
        recorded.iter().any(|v| v.as_slice() == [Type::Int]),
        "expected [Int] from `p.swap(42)`, have: {:?}",
        recorded
    );
    assert!(
        recorded.iter().any(|v| v.as_slice() == [Type::String]),
        "expected [String] from `p.swap(\"hello\")`, have: {:?}",
        recorded
    );
    assert!(
        recorded.iter().all(|v| v.len() == 1),
        "method call_type_args must contain only the method's own type params, have: {:?}",
        recorded
    );
}

// ── Generic call: happy-path trait bound records type args ────────

/// When a generic call with trait bounds succeeds (the concrete type does
/// implement the bound), the inferred type args must still be recorded.
#[test]
fn trait_bounded_generic_call_records_type_args() {
    let tokens = tokenize(
        r#"
trait Tag {
    function tagged(self) -> Int
}
struct Point {
    x: Int

    impl Tag {
        function tagged(self) -> Int { self.x }
    }
}
function tag<T: Tag>(x: T) -> Int { x.tagged() }
function main() {
    let p = Point(7)
    let r = tag(p)
    print(r)
}
"#,
        SourceId(0),
    );
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let result = check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "unexpected errors: {:?}",
        result.diagnostics
    );
    assert!(
        result
            .module
            .call_type_args
            .values()
            .any(|v| v.as_slice() == [Type::Named("Point".to_string())]),
        "expected call_type_args to contain [Point] from `tag(p)`, got: {:?}",
        result.module.call_type_args
    );
}

// ── Generic call: error-arg path must not panic downstream ────────

/// Regression for IR-lowering panic on `Type::Error` call_type_args entries.
/// When sema cannot infer a type parameter (e.g., because an argument has
/// a type error), it must either skip recording or record a well-formed
/// binding — never `Type::Error`. The key invariant checked here is that
/// no `Type::Error` ever appears in `ResolvedModule.call_type_args`.
#[test]
fn generic_call_with_arg_error_does_not_leak_type_error_into_call_type_args() {
    let tokens = tokenize(
        r#"
function identity<T>(x: T) -> T { x }
function main() {
    // `undefined_var` is not in scope — sema emits an error but should
    // not panic or leak Type::Error into call_type_args.
    let r = identity(undefined_var)
    print(r)
}
"#,
        SourceId(0),
    );
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let result = check(&program);
    // Sema emits an error; that's expected. The fix is that IR lowering
    // handles it gracefully. We additionally assert that no `Type::Error`
    // reaches `call_type_args`, so downstream can't panic on it.
    for (span, tys) in &result.module.call_type_args {
        for t in tys {
            assert!(
                !matches!(t, Type::Error),
                "call_type_args at {span:?} contains Type::Error: {tys:?}"
            );
        }
    }
}

// ── dyn Trait negative tests ─────────────────────────────────────────

/// `dyn NonexistentTrait` in a function parameter position must be
/// rejected with the "unknown trait" diagnostic — not silently accepted.
#[test]
fn dyn_unknown_trait_in_param_position_is_rejected() {
    assert_has_error(
        "function take(x: dyn Unknown) -> Int { 0 }
         function main() { }",
        "unknown trait `Unknown`",
    );
}

#[test]
fn dyn_unknown_trait_in_return_position_is_rejected() {
    assert_has_error(
        "function make() -> dyn Unknown { }
         function main() { }",
        "unknown trait `Unknown`",
    );
}

#[test]
fn dyn_unknown_trait_in_let_annotation_is_rejected() {
    assert_has_error(
        "function main() { let x: dyn Unknown = 0 }",
        "unknown trait `Unknown`",
    );
}

#[test]
fn dyn_unknown_trait_in_struct_field_is_rejected() {
    assert_has_error(
        "struct Holder { value: dyn Unknown }
         function main() { }",
        "unknown trait `Unknown`",
    );
}

#[test]
fn dyn_unknown_trait_in_list_element_is_rejected() {
    assert_has_error(
        "function main() { let xs: List<dyn Unknown> = [] }",
        "unknown trait `Unknown`",
    );
}

/// `dyn` over a trait whose method takes `Self` as a parameter must
/// surface the object-safety error and point users at static dispatch.
#[test]
fn dyn_non_object_safe_trait_self_in_param_is_rejected() {
    assert_has_error(
        "trait Eq { function eq(self, other: Self) -> Bool }
         struct Point { x: Int }
         function main() { let e: dyn Eq = Point(0) }",
        "not object-safe",
    );
}

/// `dyn` over a trait whose method returns `Self` must surface the
/// object-safety error.
#[test]
fn dyn_non_object_safe_trait_self_in_return_is_rejected() {
    assert_has_error(
        "trait Cloneable { function duplicate(self) -> Self }
         struct Point { x: Int }
         function main() { let c: dyn Cloneable = Point(0) }",
        "not object-safe",
    );
}

/// `Self` nested inside a generic arg (e.g. `Option<Self>`) must also be
/// caught by object-safety.
#[test]
fn dyn_non_object_safe_trait_self_nested_in_generic_arg() {
    assert_has_error(
        "trait MaybeSelf { function maybe(self) -> Option<Self> }
         struct Point { x: Int }
         function main() { let m: dyn MaybeSelf = Point(0) }",
        "not object-safe",
    );
}

/// Even when `dyn NonObjectSafeTrait` is rejected, the trait must still
/// be usable as a generic bound — that's the whole point of allowing
/// non-object-safe traits into the program.
#[test]
fn non_object_safe_trait_still_usable_as_generic_bound() {
    assert_no_errors(
        "trait Eq { function eq(self, other: Self) -> Bool }
         function takeEq<T: Eq>(x: T) -> Int { 0 }
         function main() { }",
    );
}

/// Coercion of a concrete type that does not implement the trait into a
/// `dyn Trait` slot should be rejected at the assignment boundary, not
/// silently accepted via a fallthrough in `types_compatible`.
#[test]
fn dyn_rejects_concrete_that_does_not_impl_trait() {
    assert_has_error(
        "trait Drawable { function draw(self) -> String }
         struct Point { x: Int }
         // Point has no `impl Drawable`, so the coercion must fail.
         function main() { let d: dyn Drawable = Point(0) }",
        "type mismatch",
    );
}

/// Empty list literal annotated as `List<dyn Trait>` — the documented
/// half-broken `push` workaround. Sema rejects with a "type mismatch"
/// because the empty `[]` types as `List<T>` and the annotation's
/// `List<dyn Drawable>` doesn't propagate. Pins the user-visible failure
/// mode until bidirectional inference lands. See
/// `docs/known-issues.md` ("`List<dyn Trait>` literal initialization in
/// compiled mode") for the full rationale.
#[test]
fn empty_list_of_dyn_literal_is_rejected() {
    assert_has_error(
        "trait Drawable { function draw(self) -> String }
         function main() {
             let xs: List<dyn Drawable> = []
         }",
        "type mismatch",
    );
}

/// Populated `List<dyn Trait>` literals (`[Circle(1), Circle(2)]` typed
/// `List<dyn Drawable>`) are *accepted by sema* today (the recursive
/// `types_compatible` rule applies the dyn coercion element-wise). They
/// then fail in IR lowering / Cranelift codegen because element-wise
/// `Op::DynAlloc` wraps are never materialized. This test pins the
/// current "sema accepts" behaviour so a future tightening at the sema
/// layer is a deliberate change, not a silent regression. The IR-side
/// rejection is exercised by the `#[ignore]`d compile test
/// `dyn_list_via_push_workaround`.
#[test]
fn populated_list_of_dyn_literal_is_currently_accepted_by_sema() {
    assert_no_errors(
        "trait Drawable { function draw(self) -> String }
         struct Circle { r: Int }
         impl Drawable for Circle {
             function draw(self) -> String { return \"c\" }
         }
         function takeList(xs: List<dyn Drawable>) -> Int { 0 }
         function main() {
             takeList([Circle(1), Circle(2)])
         }",
    );
}

/// Guards `Checker::pin_inferred_type_to_annotation` for the empty-map
/// case (the companion to the empty-list / zero-field-enum pinning that
/// the IR test `enum_type_at_handles_zero_field_generic_variant` and the
/// WASM `list_methods_empty_and_filter_bounds` test cover). An empty
/// `{}` infers as `Map<K, V>` with unconstrained type vars; the `let m:
/// Map<String, Int>` annotation must be pinned onto the *expression*'s
/// recorded type (not just the variable), because IR / WASM lowering
/// reads `expr_types` for the `MapAlloc` key/value type args. Without
/// the pin the recorded type keeps the type vars and lowers to the
/// `__generic` placeholder.
#[test]
fn pin_empty_map_literal_records_concrete_annotation_type() {
    let result = check_full(
        r#"
function main() {
    let m: Map<String, Int> = {}
}
"#,
    );
    // The `{}` initializer is the only recorded expression. It must now
    // read as the concrete `Map<String, Int>`, not `Map<K, V>`.
    let pinned = result.module.expr_types.values().any(|t| {
        matches!(t, Type::Generic(name, args)
            if name == "Map" && **args == [Type::String, Type::Int])
    });
    assert!(
        pinned,
        "empty map literal should be pinned to Map<String, Int>, got: {:?}",
        result.module.expr_types.values().collect::<Vec<_>>()
    );
    // And no recorded expression type may still carry a type var.
    assert!(
        !result.module.expr_types.values().any(Type::has_type_vars),
        "pinning should leave no type-var-bearing recorded types, got: {:?}",
        result.module.expr_types.values().collect::<Vec<_>>()
    );
}

/// Guards the `dyn`-annotation carve-out in
/// `Checker::pin_inferred_type_to_annotation`. Inside a generic fn, `let
/// d: dyn Drawable = s` (with `s: T`, `T: Drawable`) infers the
/// initializer as the bare type var `T` — which *does* trip the pin's
/// `init_type.has_type_vars()` precondition. But the annotation is `dyn
/// Drawable` (`Type::Dyn`), not the same generic constructor as `T`, so
/// the pin must leave `s`'s recorded type as `T`. IR lowering relies on
/// that gap to materialize the `T` → `dyn Drawable` (`DynRef`) coercion
/// at the `let`; clobbering the recorded type to `dyn Drawable` would
/// skip the wrap and leave the raw value. A future loosening of the
/// same-constructor guard that started clobbering this would fail here.
#[test]
fn dyn_annotation_does_not_clobber_generic_param_initializer() {
    let result = check_full(
        "trait Drawable { function draw(self) -> String }
         function render<T: Drawable>(s: T) {
             let d: dyn Drawable = s
         }
         function main() { }",
    );
    // The lone initializer expression `s` must stay recorded as the
    // type var `T`, never rewritten to `dyn Drawable`.
    assert!(
        result
            .module
            .expr_types
            .values()
            .any(|t| matches!(t, Type::TypeVar(_))),
        "the `s` initializer should stay a type var, got: {:?}",
        result.module.expr_types.values().collect::<Vec<_>>()
    );
    assert!(
        !result
            .module
            .expr_types
            .values()
            .any(|t| matches!(t, Type::Dyn(_))),
        "the `dyn Drawable` annotation must not be pinned onto the \
         initializer expression, got: {:?}",
        result.module.expr_types.values().collect::<Vec<_>>()
    );
}

/// Multi-bound trait objects (`dyn Foo + Bar`) are out of scope for Phase
/// 2.2. The parser must surface a clean diagnostic at the `+` rather than
/// silently accepting one bound and leaving the other dangling.
#[test]
fn dyn_multi_bound_is_rejected() {
    // The exact diagnostic depends on which layer rejects (parser vs. sema).
    // Today the parser hits the `+` after `dyn Drawable` mid-expression and
    // surfaces a parse-shaped error; either layer rejecting is fine, the
    // contract is "the program fails to compile cleanly".
    let source = "trait Drawable { function draw(self) -> String }
                  trait Sized { function size(self) -> Int }
                  function f(x: dyn Drawable + Sized) -> Int { 0 }
                  function main() { }";
    let tokens = phoenix_lexer::lexer::tokenize(source, phoenix_common::span::SourceId(0));
    let (program, parse_errors) = phoenix_parser::parser::parse(&tokens);
    let result = crate::checker::check(&program);
    assert!(
        !parse_errors.is_empty() || !result.diagnostics.is_empty(),
        "expected parser or sema to reject `dyn Drawable + Sized` — got no \
         diagnostics. parse_errors={parse_errors:?}, sema_diagnostics={:?}",
        result.diagnostics
    );
}

/// `dyn Trait` in *value* position (not type position) — `let x = dyn Foo`
/// — must be rejected. `dyn` is a type-expression keyword; using it as an
/// expression is a parse error.
#[test]
fn dyn_in_value_position_is_rejected() {
    let source = "trait Drawable { function draw(self) -> String }
                  function main() { let x = dyn Drawable }";
    let tokens = phoenix_lexer::lexer::tokenize(source, phoenix_common::span::SourceId(0));
    let (_program, parse_errors) = phoenix_parser::parser::parse(&tokens);
    assert!(
        !parse_errors.is_empty(),
        "expected parser to reject `dyn` in value position — got no parse errors"
    );
}

/// Coercion from a type variable bound by trait `Foo` into `dyn Bar` (an
/// unrelated trait) must fail. `concrete_type_impls_trait` should not
/// accept any TypeVar/dyn pair just because both sides happen to be
/// type-system-shaped.
#[test]
fn dyn_coercion_with_mismatched_trait_bound_is_rejected() {
    assert_has_error(
        "trait Foo { function f(self) -> Int }
         trait Bar { function b(self) -> Int }
         function go<T: Foo>(x: T) -> Int {
             let d: dyn Bar = x
             d.b()
         }
         function main() { }",
        "type mismatch",
    );
}

// ── dyn Trait positive coercion-boundary tests ───────────────────────
//
// The 6 coercion boundaries documented in `docs/dyn-trait.md` ("When
// the wrap happens"):
//   1. function-call argument
//   2. let binding with `dyn` annotation              (covered: dyn_unknown_trait_in_let_annotation_is_rejected exercises the path)
//   3. reassignment to a `let mut` typed `dyn`
//   4. function return
//   5. struct field typed `dyn`
//   6. enum variant field typed `dyn`
//
// These tests pin that *successful* coercion of an implementor into
// each boundary type-checks cleanly. Each is the positive companion
// to the existing rejection tests, so the next regression is loud.

const DRAWABLE_PRELUDE: &str = "trait Drawable { function draw(self) -> String }
                                struct Circle { radius: Int }
                                impl Drawable for Circle {
                                    function draw(self) -> String { return \"c\" }
                                }
                                struct Square { side: Int }
                                impl Drawable for Square {
                                    function draw(self) -> String { return \"s\" }
                                }";

fn with_drawable_prelude(body: &str) -> String {
    format!("{DRAWABLE_PRELUDE}\n{body}")
}

/// Boundary 1: a concrete implementor flowing into a `dyn Trait` *call
/// argument* must type-check.
#[test]
fn dyn_coercion_in_call_argument_typechecks() {
    assert_no_errors(&with_drawable_prelude(
        "function takeDyn(x: dyn Drawable) -> String { return x.draw() }
         function main() { takeDyn(Circle(3)) }",
    ));
}

/// Method dispatch on a `dyn Trait` receiver must resolve the trait
/// method and return its declared return type.
#[test]
fn dyn_method_call_on_dyn_receiver_typechecks() {
    assert_no_errors(&with_drawable_prelude(
        "function render(s: dyn Drawable) -> String { return s.draw() }
         function main() { render(Circle(1)) }",
    ));
}

/// Calling a method that is *not* on the trait through a `dyn Trait`
/// receiver must surface a "trait has no method" diagnostic, not panic
/// or fall through to the unknown-method path.
#[test]
fn dyn_method_call_unknown_method_is_rejected() {
    assert_has_error(
        &with_drawable_prelude(
            "function render(s: dyn Drawable) -> String { return s.area() }
             function main() { render(Circle(1)) }",
        ),
        "trait `Drawable` has no method `area`",
    );
}

/// Boundary 3: reassignment to a `let mut` slot typed `dyn Trait` must
/// re-coerce the new value. Also covers heterogeneity at the same
/// binding (Circle then Square).
#[test]
fn dyn_coercion_in_reassignment_typechecks() {
    assert_no_errors(&with_drawable_prelude(
        "function main() {
             let mut d: dyn Drawable = Circle(1)
             d = Square(2)
         }",
    ));
}

/// Boundary 4: returning a concrete implementor from a function whose
/// declared return type is `dyn Trait` must type-check.
#[test]
fn dyn_coercion_in_return_value_typechecks() {
    assert_no_errors(&with_drawable_prelude(
        "function makeDrawable() -> dyn Drawable { return Circle(1) }
         function main() { makeDrawable() }",
    ));
}

/// Boundary 5: passing a concrete implementor into a struct constructor
/// position whose field is typed `dyn Trait` must type-check.
#[test]
fn dyn_coercion_in_struct_field_typechecks() {
    assert_no_errors(&with_drawable_prelude(
        "struct Scene { hero: dyn Drawable }
         function main() { Scene(Circle(1)) }",
    ));
}

/// Boundary 6: passing a concrete implementor into an enum variant
/// constructor position whose field is typed `dyn Trait` must
/// type-check.
#[test]
fn dyn_coercion_in_enum_variant_field_typechecks() {
    assert_no_errors(&with_drawable_prelude(
        "enum Slot { Held(dyn Drawable)\n Empty }
         function main() { Held(Circle(1)) }",
    ));
}

/// Generic traits cannot be used as `dyn` (the parser form
/// `dyn Trait<Concrete>` isn't supported and bare `dyn Trait` would
/// leave method-signature type parameters unbound). Pinned by
/// `check_types.rs`. The rejection fires at the `dyn`-type site, so
/// the test does not need any concrete impl — sema rejects before
/// it reaches "what implements this trait".
#[test]
fn dyn_over_generic_trait_is_rejected() {
    assert_has_error(
        "trait Container<T> { function get(self) -> T }
         function main() {
             let c: dyn Container = 0
         }",
        "generic trait `Container` cannot be used as `dyn`",
    );
}

/// Reassigning a `dyn` slot to something that does *not* implement the
/// trait must fail at the assignment boundary (regression for
/// `types_compatible`'s `dyn` rule running before the TypeVar wildcard).
#[test]
fn dyn_reassignment_with_non_implementor_is_rejected() {
    assert_has_error(
        &with_drawable_prelude(
            "struct Plain { v: Int }
             function main() {
                 let mut d: dyn Drawable = Circle(1)
                 d = Plain(0)
             }",
        ),
        "type mismatch",
    );
}

/// `defer` of an expression that references an unbound name still
/// produces the standard undefined-variable diagnostic; the defer
/// statement does not silently swallow lookup errors.
#[test]
fn defer_reports_unbound_name_in_expression() {
    assert_has_error(
        "function main() { defer print(missing) }",
        "undefined variable `missing`",
    );
}

/// `return` inside a `defer` expression is rejected at sema. The
/// runtime semantics ("return from a function that's already
/// exiting") are unclear, and on the IR side the embedded return
/// would terminate the current block before the caller's own
/// `Terminator::Return` lands.
#[test]
fn defer_with_inner_return_is_rejected() {
    assert_has_error(
        "function f(c: Bool) -> Int {
             defer if c { return 1 } else { 0 }
             2
         }",
        "`return` is not allowed inside a `defer` expression",
    );
}

/// A function whose only "return-like" construct lives inside a
/// `defer` expression must still trip the must-return-a-value
/// diagnostic. The defer body runs at exit and does not satisfy
/// the function's return obligation.
///
/// This test specifically uses a deferred expression whose *type*
/// would otherwise be a candidate for the implicit return (`42` is
/// an `Int`, matching the declared return type). The diagnostic
/// must still fire because `defer` discards its expression's value
/// — the deferred expression is never the implicit-return value.
#[test]
fn defer_does_not_satisfy_return_obligation() {
    assert_has_error(
        "function f() -> Int { defer 42 }",
        "does not return a value",
    );
}

/// A `defer` inside another `defer`'s expression is rejected: it
/// sits inside an `if`-arm block, which is not the function's
/// outermost statement level.
#[test]
fn defer_inside_defer_if_arm_is_rejected() {
    assert_has_error(
        "function f(c: Bool) {
             defer if c { defer print(\"inner\") } else { 0 }
         }",
        "`defer` must appear at the function's outermost statement level",
    );
}

/// Sibling check: a `defer` inside a `match`-arm block of a
/// deferred expression is also rejected.
#[test]
fn defer_inside_defer_match_arm_is_rejected() {
    assert_has_error(
        "function f(n: Int) {
             defer match n {
                 0 -> { defer print(\"zero\") }
                 _ -> { 0 }
             }
         }",
        "`defer` must appear at the function's outermost statement level",
    );
}

/// A `defer` reachable through a function-call argument's `if`-arm —
/// previously slipped past the narrow nested-defer check (which only
/// recursed through `if`/`match`). The placement-rule walker
/// recurses through every expression type.
#[test]
fn defer_inside_call_arg_in_defer_is_rejected() {
    // Exactly one violation expected — only the inner `defer print(...)`
    // is illegal. The outer `defer take(...)` lives at the function's
    // outermost statement level and must NOT also be flagged.
    assert_error_count_for_message(
        "function take(n: Int) {}
         function f(c: Bool) {
             defer take(if c {
                 defer print(\"inner\")
                 1
             } else { 0 })
         }",
        "`defer` must appear at the function's outermost statement level",
        1,
    );
}

/// `defer` inside an `if`-block at the function's top level (not
/// inside another defer) is also rejected. The IR side has no
/// active flag for defers — an untaken branch's defer would still
/// fire on later exits. Restricting `defer` to the outermost block
/// avoids the divergence.
#[test]
fn defer_inside_top_level_if_is_rejected() {
    assert_has_error(
        "function f(c: Bool) {
             if c {
                 defer print(\"only sometimes\")
             }
         }",
        "`defer` must appear at the function's outermost statement level",
    );
}

/// `defer` inside a loop body is rejected. AST interp would
/// register N times per N iterations; IR side would register once.
/// The static/dynamic split is a known divergence — we reject at
/// sema rather than letting users hit it silently.
#[test]
fn defer_inside_loop_body_is_rejected() {
    assert_has_error(
        "function f() {
             for i in 0..3 {
                 defer print(\"each\")
             }
         }",
        "`defer` must appear at the function's outermost statement level",
    );
}

/// A `defer` inside a *lambda* body is fine — the lambda has its
/// own outermost level (and its own defer frame on both
/// interpreters). Pins that the rule does not over-trigger across
/// the lambda boundary.
#[test]
fn defer_inside_lambda_inside_defer_is_allowed() {
    let src = "function main() {
        defer (function() { defer print(\"inner\") })()
    }";
    assert_no_errors(src);
}

/// A `defer` deep inside a lambda body — even one level of nesting
/// inside the lambda itself — is rejected, by the lambda's own
/// placement check.
#[test]
fn defer_nested_inside_lambda_body_is_rejected() {
    assert_has_error(
        "function main() {
             let f = function(c: Bool) {
                 if c { defer print(\"hi\") }
             }
             f(true)
         }",
        "`defer` must appear at the function's outermost statement level",
    );
}

/// `?` (try) inside a `defer` expression is rejected. `?` lowers to
/// a `Terminator::Return` of the operand on the error path, which
/// would (a) bypass earlier-source-order defers in LIFO and (b)
/// surface a `Result`/`Option` to the caller in place of the
/// function's intended return.
#[test]
fn defer_with_inner_try_is_rejected() {
    assert_has_error(
        "function get() -> Result<Int, String> { Ok(1) }
         function f() -> Result<Int, String> {
             defer get()?
             Ok(42)
         }",
        "`?` (try) is not allowed inside a `defer` expression",
    );
}

/// `defer` inside a `while`-loop body is rejected by the placement
/// rule (sibling to `defer_inside_loop_body_is_rejected`, which
/// covers `for`). Both walker arms share the same `walk_stmt`
/// recursion; this test pins parity.
#[test]
fn defer_inside_while_loop_body_is_rejected() {
    assert_has_error(
        "function f() {
             let mut i: Int = 0
             while i < 3 {
                 defer print(\"each\")
                 i = i + 1
             }
         }",
        "`defer` must appear at the function's outermost statement level",
    );
}

/// `defer` inside a `while` loop's `else` arm is rejected. The
/// `else` arm runs at most once when the condition is false on
/// entry — but the placement rule still applies because the IR
/// side has no active flag for defers attached to inner blocks.
#[test]
fn defer_inside_while_else_arm_is_rejected() {
    assert_has_error(
        "function f() {
             while false {
                 print(\"never\")
             } else {
                 defer print(\"else arm\")
             }
         }",
        "`defer` must appear at the function's outermost statement level",
    );
}

/// `defer` inside a `for` loop's `else` arm is rejected (parity
/// with the `while`-else case above).
#[test]
fn defer_inside_for_else_arm_is_rejected() {
    assert_has_error(
        "function f() {
             for _i in 0..0 {
                 print(\"never\")
             } else {
                 defer print(\"else arm\")
             }
         }",
        "`defer` must appear at the function's outermost statement level",
    );
}

/// A `defer` reachable through a `?` (try) operand at the top level
/// of another `defer`'s expression is also rejected — the placement
/// walker recurses through `Expr::Try` like every other expression
/// shape, and the inner `defer` is a nested defer regardless of how
/// it's embedded.
#[test]
fn defer_inside_try_operand_in_defer_is_rejected() {
    assert_has_error(
        "function inner() -> Result<Int, String> { Ok(1) }
         function f() -> Result<Int, String> {
             defer inner()
             let x = (if true {
                 defer print(\"inner\")
                 inner()
             } else {
                 inner()
             })?
             Ok(x)
         }",
        "`defer` must appear at the function's outermost statement level",
    );
}

/// A function whose only return-shaped construct lives at the top
/// level alongside a `defer` should still satisfy
/// `validate_implicit_return`. Pinned because `Self::contains_return`
/// returns `false` for `Statement::Defer(_)`; a `return` *next to* a
/// `defer` (not inside it) must still count.
#[test]
fn defer_does_not_mask_sibling_return_for_implicit_return_check() {
    assert_no_errors(
        "function f(c: Bool) -> Int {
             defer print(\"cleanup\")
             if c { return 1 } else { return 2 }
         }",
    );
}

/// `defer` inside a top-level `match`-arm block (not nested in
/// another defer) is rejected by the placement rule. Sibling to
/// `defer_inside_top_level_if_is_rejected`; covers the `match`
/// branch of the same walker that already covers `if`/`while`/`for`.
#[test]
fn defer_inside_top_level_match_arm_is_rejected() {
    assert_has_error(
        "function f(n: Int) {
             match n {
                 0 -> { defer print(\"zero arm\") }
                 _ -> { 0 }
             }
         }",
        "`defer` must appear at the function's outermost statement level",
    );
}

/// A nested defer whose deferred expression *also* contains a
/// `return` should produce exactly one diagnostic — the placement
/// violation. The return-inside-defer check is gated on top-level
/// placement so it does not pile a second redundant error onto the
/// same defer.
#[test]
fn nested_defer_with_return_in_expr_emits_only_placement_error() {
    let src = "function f(c: Bool) -> Int {
                 if c {
                     defer if c { return 1 } else { 0 }
                 }
                 0
             }";
    assert_error_count_for_message(
        src,
        "`defer` must appear at the function's outermost statement level",
        1,
    );
    assert_error_count_for_message(
        src,
        "`return` is not allowed inside a `defer` expression",
        0,
    );
}

/// Sibling check for the `?` (try) variant: a nested defer whose
/// expression contains `?` produces only the placement error.
#[test]
fn nested_defer_with_try_in_expr_emits_only_placement_error() {
    let src = "function inner() -> Result<Int, String> { Ok(1) }
               function f(c: Bool) -> Result<Int, String> {
                   if c {
                       defer inner()?
                   }
                   Ok(0)
               }";
    assert_error_count_for_message(
        src,
        "`defer` must appear at the function's outermost statement level",
        1,
    );
    assert_error_count_for_message(
        src,
        "`?` (try) is not allowed inside a `defer` expression",
        0,
    );
}

/// A top-level `defer` whose expression contains a *nested* (illegally-
/// placed) `defer`, where the nested defer's own expression contains a
/// `return`, must NOT have the `return`-not-allowed error attributed to
/// the outer top-level defer. The `return` belongs to the nested defer
/// (which is itself flagged for placement); attributing it to the
/// outer defer would produce a misleading second diagnostic pointing
/// at code that's fine in isolation.
///
/// Expected: exactly one placement error (against the nested defer)
/// and zero return-not-allowed errors (since the only `return` lives
/// inside the nested defer's expression, not directly inside the
/// outer top-level defer's).
#[test]
fn outer_defer_does_not_inherit_inner_defers_return_violation() {
    let src = "function f(c: Bool) -> Int {
                   defer if c {
                       defer if c { return 1 } else { 0 }
                       0
                   } else {
                       0
                   }
                   42
               }";
    assert_error_count_for_message(
        src,
        "`defer` must appear at the function's outermost statement level",
        1,
    );
    assert_error_count_for_message(
        src,
        "`return` is not allowed inside a `defer` expression",
        0,
    );
}

/// Sibling check: a top-level `defer` whose expression contains a
/// nested (illegally-placed) `defer` whose own expression contains a
/// `?` operator must not have the `?`-not-allowed error attributed to
/// the outer top-level defer.
#[test]
fn outer_defer_does_not_inherit_inner_defers_try_violation() {
    let src = "function inner() -> Result<Int, String> { Ok(1) }
               function f(c: Bool) -> Result<Int, String> {
                   defer if c {
                       defer inner()?
                       inner()
                   } else {
                       inner()
                   }
                   Ok(42)
               }";
    assert_error_count_for_message(
        src,
        "`defer` must appear at the function's outermost statement level",
        1,
    );
    assert_error_count_for_message(
        src,
        "`?` (try) is not allowed inside a `defer` expression",
        0,
    );
}

// ── extern js host-FFI ──────────────

#[test]
fn extern_js_valid_block_and_call_checks_clean() {
    assert_no_errors(
        "extern js {\n\
           function alert(message: String)\n\
           function setTimeout(callback: () -> Void, ms: Int)\n\
         }\n\
         function main() {\n\
           alert(\"hi\")\n\
           setTimeout(function() { alert(\"later\") }, 300)\n\
         }",
    );
}

#[test]
fn extern_js_function_registered_in_extern_table_only() {
    let tokens = tokenize("extern js { function alert(message: String) }", SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let analysis = check(&program);
    assert!(
        analysis.diagnostics.is_empty(),
        "unexpected errors: {:?}",
        analysis.diagnostics
    );
    // Registered as an extern, with `(module, name)` linkage…
    let info = analysis
        .module
        .extern_functions
        .get("alert")
        .expect("`alert` should be registered as an extern function");
    assert_eq!(
        info.extern_js,
        Some(("js".to_string(), "alert".to_string()))
    );
    assert_eq!(info.params, vec![Type::String]);
    // …and NOT in the FuncId-indexed function table (no Phoenix body).
    assert!(!analysis.module.function_by_name.contains_key("alert"));
}

#[test]
fn extern_js_return_type_flows_to_caller() {
    // An extern returning Int is usable in arithmetic — the return type flows.
    assert_no_errors(
        "extern js { function getLength(s: String) -> Int }\n\
         function main() { let n: Int = getLength(\"abc\") + 1\n print(n) }",
    );
}

#[test]
fn extern_js_jsvalue_round_trips_opaquely() {
    // A JsValue produced by one extern can be passed to another; Phoenix never
    // inspects it, only holds and forwards it.
    assert_no_errors(
        "extern js {\n\
           function getElementById(id: String) -> JsValue\n\
           function appendChild(parent: JsValue, child: JsValue)\n\
         }\n\
         function main() {\n\
           let a: JsValue = getElementById(\"a\")\n\
           let b: JsValue = getElementById(\"b\")\n\
           appendChild(a, b)\n\
         }",
    );
}

#[test]
fn extern_js_call_arg_type_mismatch_rejected() {
    assert_has_error(
        "extern js { function alert(message: String) }\n\
         function main() { alert(42) }",
        "argument 1 of `alert`: expected `String` but got `Int`",
    );
}

#[test]
fn extern_js_call_arity_mismatch_rejected() {
    assert_has_error(
        "extern js { function alert(message: String) }\n\
         function main() { alert(\"a\", \"b\") }",
        "takes 1 argument(s)",
    );
}

#[test]
fn extern_js_non_marshallable_param_rejected() {
    assert_has_error(
        "extern js { function f(xs: List<Int>) }",
        "non-marshallable type `List<Int>`",
    );
}

#[test]
fn extern_js_non_marshallable_return_rejected() {
    assert_has_error(
        "extern js { function f() -> List<Int> }",
        "non-marshallable return type `List<Int>`",
    );
}

#[test]
fn extern_js_gen_only_scalar_param_rejected() {
    // The Gen-only scalars (DateTime/Uuid/Decimal/Money) are not marshallable
    // across the executable host-FFI boundary.
    assert_has_error(
        "extern js { function at(t: DateTime) }",
        "non-marshallable type `DateTime`",
    );
}

#[test]
fn extern_js_duplicate_name_rejected() {
    assert_has_error(
        "extern js {\n  function dup(a: Int)\n  function dup(b: String)\n}",
        "`dup` is already defined",
    );
}

#[test]
fn extern_js_collides_with_ordinary_function() {
    assert_has_error(
        "function shared() {}\n\
         extern js { function shared() }",
        "`shared` is already defined",
    );
}

#[test]
fn ordinary_function_collides_with_prior_extern_js() {
    // The reverse order of `extern_js_collides_with_ordinary_function`:
    // declarations register in source order, so an `extern js` signature
    // followed by an ordinary function of the same name reaches
    // `register_function`. Without checking `extern_functions` there, the
    // function would register under the same qualified key and silently shadow
    // the extern (call resolution consults `lookup_function` first). Must be
    // rejected, symmetric with the function-first case.
    assert_has_error(
        "extern js { function shared() }\n\
         function shared() {}",
        "`shared` is already defined",
    );
}

#[test]
fn extern_js_marshallable_closure_param_ok() {
    // A function-typed param whose parts are all marshallable is allowed
    // (closures-as-callbacks).
    assert_no_errors("extern js { function onClick(handler: (Int) -> Bool) }");
}

#[test]
fn extern_js_closure_param_with_non_marshallable_part_rejected() {
    // The recursive arm of `is_js_marshallable`: a function-typed param is only
    // marshallable when every parameter and its return are. A non-marshallable
    // argument type (`List<Int>`) inside the closure must be rejected.
    assert_has_error(
        "extern js { function each(cb: (List<Int>) -> Void) }",
        "non-marshallable type `(List<Int>) -> Void`",
    );
}

#[test]
fn extern_js_closure_return_with_non_marshallable_part_rejected() {
    // Same recursion, return position: a closure returning a non-marshallable
    // type (`List<Int>`) is itself non-marshallable.
    assert_has_error(
        "extern js { function make(seed: Int) -> () -> List<Int> }",
        "non-marshallable return type `() -> List<Int>`",
    );
}

#[test]
fn extern_js_shadowing_builtin_rejected() {
    // An extern that shadows a reserved builtin name (`Option`) must be rejected,
    // not silently registered-but-unreachable. `build_one_module_scope_phase_a`
    // skips inserting builtin-shadowing names into the module scope, so without an
    // explicit rejection the extern would land in `extern_functions` yet never be
    // resolvable — a dead, undiagnosed entry.
    assert_has_error(
        "extern js { function Option(x: Int) }",
        "`Option` is already defined",
    );
}

#[test]
fn extern_js_shadowing_builtin_function_rejected() {
    // The builtin *functions* `print` / `toString` are reserved like builtin
    // types, but live outside `is_builtin_name`. Both `check_call` and IR
    // `lower_call` dispatch them by bare name before consulting the extern
    // table, so an extern named after one would register yet never be reachable
    // — the call would silently bind to the builtin. Both must be rejected.
    assert_has_error(
        "extern js { function print(message: String) }",
        "`print` is already defined",
    );
    assert_has_error(
        "extern js { function toString(value: Int) -> String }",
        "`toString` is already defined",
    );
}

#[test]
fn extern_js_non_marshallable_param_still_registered() {
    // The marshallability check emits an error but does NOT bail on the
    // signature — the extern is still registered so downstream call-checking can
    // recover (resolve the name, report arg errors) instead of also reporting
    // "undefined function".
    let tokens = tokenize("extern js { function f(xs: List<Int>) }", SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let analysis = check(&program);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("non-marshallable type `List<Int>`")),
        "expected a marshallability error, got: {:?}",
        analysis.diagnostics
    );
    assert!(
        analysis.module.extern_functions.contains_key("f"),
        "extern `f` should still be registered for call-check recovery"
    );
}

#[test]
fn extern_js_unresolved_param_type_reports_one_error() {
    // An unresolved annotation resolves to `Type::Error` after `resolve_type_expr`
    // already reported "unknown type". The marshallability check must skip
    // `Type::Error` so a single bad annotation yields exactly that one diagnostic,
    // not also a confusing non-marshallable `<error>` message.
    let tokens = tokenize("extern js { function f(x: Bogus) }", SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let analysis = check(&program);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("unknown type `Bogus`")),
        "expected an unknown-type error, got: {:?}",
        analysis.diagnostics
    );
    assert!(
        !analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("non-marshallable")),
        "unresolved type must not also produce a non-marshallable diagnostic, got: {:?}",
        analysis.diagnostics
    );
}
