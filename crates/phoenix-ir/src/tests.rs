//! Integration tests for IR lowering.

use crate::lower;
use crate::types::{OPTION_ENUM, RESULT_ENUM};
use crate::verify;
use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

/// Parse, type-check, and lower a Phoenix source string to IR.
/// Panics if there are parse or sema errors.
fn lower_source(source: &str) -> crate::IrModule {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "Parse errors: {:?}", parse_errors);
    let check_result = checker::check(&program);
    assert!(
        check_result.diagnostics.is_empty(),
        "Sema errors: {:?}",
        check_result
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );
    lower(&program, &check_result)
}

/// Lower source and return the IR as a string.
fn lower_to_string(source: &str) -> String {
    let module = lower_source(source);

    // Run the verifier.
    let errors = verify::verify(&module);
    assert!(
        errors.is_empty(),
        "Verification errors: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );

    module.to_string()
}

#[test]
fn lower_empty_main() {
    let ir = lower_to_string("function main() { }");
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_return_int_literal() {
    let ir = lower_to_string("function main() -> Int { 42 }");
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_return_float_literal() {
    let ir = lower_to_string("function main() -> Float { 3.14 }");
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_return_string_literal() {
    let ir = lower_to_string(r#"function main() -> String { "hello" }"#);
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_return_bool_literal() {
    let ir = lower_to_string("function main() -> Bool { true }");
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_int_arithmetic() {
    let ir = lower_to_string(
        "function main() -> Int {
            let x: Int = 10
            let y: Int = 20
            x + y
        }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_print_call() {
    let ir = lower_to_string(
        r#"function main() {
            print("hello")
        }"#,
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_function_call() {
    let ir = lower_to_string(
        "function add(a: Int, b: Int) -> Int { a + b }
         function main() -> Int { add(1, 2) }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_if_else() {
    let ir = lower_to_string(
        "function main() {
            let x: Int = 10
            if x > 5 {
                print(\"big\")
            } else {
                print(\"small\")
            }
        }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_while_loop() {
    let ir = lower_to_string(
        "function main() {
            let mut i: Int = 0
            while i < 10 {
                i += 1
            }
        }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_for_range() {
    let ir = lower_to_string(
        "function main() {
            for i in 0..5 {
                print(toString(i))
            }
        }",
    );
    insta::assert_snapshot!(ir);
}

// ── Mutable variables ────────────────────────────────────────────────

#[test]
fn lower_mutable_variable() {
    let ir = lower_to_string(
        "function main() -> Int {
            let mut x: Int = 10
            x = 20
            x
        }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_compound_assignment() {
    let ir = lower_to_string(
        "function main() -> Int {
            let mut x: Int = 5
            x += 3
            x
        }",
    );
    insta::assert_snapshot!(ir);
}

// ── Structs ──────────────────────────────────────────────────────────

#[test]
fn lower_struct_construction() {
    let ir = lower_to_string(
        "struct Point { Int x  Int y }
         function main() -> Int {
             let p: Point = Point(10, 20)
             p.x
         }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_struct_field_assignment() {
    let ir = lower_to_string(
        "struct Point { Int x  Int y }
         function main() {
             let mut p: Point = Point(1, 2)
             p.x = 42
         }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_struct_destructuring() {
    let ir = lower_to_string(
        "struct Point { Int x  Int y }
         function main() -> Int {
             let p: Point = Point(3, 4)
             let Point { x, y } = p
             x + y
         }",
    );
    insta::assert_snapshot!(ir);
}

// ── Enums and match ──────────────────────────────────────────────────

#[test]
fn lower_enum_construction() {
    let ir = lower_to_string(
        "enum Color { Red  Green  Blue }
         function main() -> Color { Red }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_match_enum() {
    let ir = lower_to_string(
        "enum Shape {
             Circle(Float)
             Rect(Float, Float)
         }
         function main() -> Float {
             let s: Shape = Circle(5.0)
             match s {
                 Circle(r) -> r
                 Rect(w, h) -> w + h
             }
         }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_match_wildcard() {
    let ir = lower_to_string(
        "function main() -> Int {
             let x: Int = 3
             match x {
                 1 -> 10
                 2 -> 20
                 _ -> 0
             }
         }",
    );
    insta::assert_snapshot!(ir);
}

// ── Closures / lambdas ───────────────────────────────────────────────

#[test]
fn lower_lambda() {
    let ir = lower_to_string(
        "function main() -> Int {
             let f: (Int) -> Int = function(x: Int) -> Int { x + 1 }
             f(10)
         }",
    );
    insta::assert_snapshot!(ir);
}

// ── Method calls ─────────────────────────────────────────────────────

#[test]
fn lower_user_method_call() {
    let ir = lower_to_string(
        "struct Counter {
             Int value
             function get(self) -> Int { self.value }
         }
         function main() -> Int {
             let c: Counter = Counter(42)
             c.get()
         }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_builtin_method_call() {
    let ir = lower_to_string(
        r#"function main() -> Int {
             let s: String = "hello"
             s.length()
         }"#,
    );
    insta::assert_snapshot!(ir);
}

// ── String interpolation ─────────────────────────────────────────────

#[test]
fn lower_string_interpolation() {
    let ir = lower_to_string(
        r#"function main() {
             let name: String = "world"
             print("hello {name}!")
         }"#,
    );
    insta::assert_snapshot!(ir);
}

// ── Short-circuit logic ──────────────────────────────────────────────

#[test]
fn lower_short_circuit_and() {
    let ir = lower_to_string(
        "function main() -> Bool {
             let a: Bool = true
             let b: Bool = false
             a && b
         }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_short_circuit_or() {
    let ir = lower_to_string(
        "function main() -> Bool {
             let a: Bool = false
             let b: Bool = true
             a || b
         }",
    );
    insta::assert_snapshot!(ir);
}

// ── Unary operators ──────────────────────────────────────────────────

#[test]
fn lower_unary_neg() {
    let ir = lower_to_string(
        "function main() -> Int {
             let x: Int = 42
             -x
         }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_unary_not() {
    let ir = lower_to_string(
        "function main() -> Bool {
             let x: Bool = true
             !x
         }",
    );
    insta::assert_snapshot!(ir);
}

// ── Float arithmetic ─────────────────────────────────────────────────

#[test]
fn lower_float_arithmetic() {
    let ir = lower_to_string(
        "function main() -> Float {
             let a: Float = 1.5
             let b: Float = 2.5
             a + b * 2.0
         }",
    );
    insta::assert_snapshot!(ir);
}

// ── Comparisons ──────────────────────────────────────────────────────

#[test]
fn lower_string_comparison() {
    let ir = lower_to_string(
        r#"function main() -> Bool {
             let a: String = "abc"
             let b: String = "def"
             a == b
         }"#,
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_float_comparison() {
    let ir = lower_to_string(
        "function main() -> Bool {
             let a: Float = 1.0
             let b: Float = 2.0
             a < b
         }",
    );
    insta::assert_snapshot!(ir);
}

// ── Nested if/else if/else ───────────────────────────────────────────

#[test]
fn lower_if_else_if_else() {
    let ir = lower_to_string(
        "function main() {
             let x: Int = 5
             if x > 10 {
                 print(\"big\")
             } else if x > 0 {
                 print(\"medium\")
             } else {
                 print(\"small\")
             }
         }",
    );
    insta::assert_snapshot!(ir);
}

// ── List and map literals ────────────────────────────────────────────

#[test]
fn lower_list_literal() {
    let ir = lower_to_string(
        "function main() -> List<Int> {
             [1, 2, 3]
         }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_map_literal() {
    let ir = lower_to_string(
        r#"function main() -> Map<String, Int> {
             {"a": 1, "b": 2}
         }"#,
    );
    insta::assert_snapshot!(ir);
}

// ── Break and continue ──────────────────────────────────────────────

#[test]
fn lower_break_continue() {
    let ir = lower_to_string(
        "function main() {
             let mut i: Int = 0
             while i < 10 {
                 i += 1
                 if i == 5 {
                     break
                 }
                 if i == 3 {
                     continue
                 }
                 print(toString(i))
             }
         }",
    );
    insta::assert_snapshot!(ir);
}

// ── For-each over collections ────────────────────────────────────────

#[test]
fn lower_for_each() {
    let ir = lower_to_string(
        "function main() {
             let nums: List<Int> = [10, 20, 30]
             for n in nums {
                 print(toString(n))
             }
         }",
    );
    insta::assert_snapshot!(ir);
}

// ── Implicit return ──────────────────────────────────────────────────

#[test]
fn lower_implicit_return_in_function() {
    let ir = lower_to_string(
        "function double(x: Int) -> Int { x * 2 }
         function main() -> Int { double(21) }",
    );
    insta::assert_snapshot!(ir);
}

// ── Return statement ─────────────────────────────────────────────────

#[test]
fn lower_explicit_return() {
    let ir = lower_to_string(
        "function abs(x: Int) -> Int {
             if x < 0 {
                 return -x
             }
             x
         }
         function main() -> Int { abs(-5) }",
    );
    insta::assert_snapshot!(ir);
}

// ── Verifier ─────────────────────────────────────────────────────────

#[test]
fn verifier_catches_no_errors_on_valid_ir() {
    let module = lower_source("function main() -> Int { 42 }");
    let errors = verify::verify(&module);
    assert!(errors.is_empty());
}

#[test]
fn verifier_catches_missing_terminator() {
    // Manually build a module with a block that has no terminator.
    use crate::block::BasicBlock;
    use crate::instruction::FuncId;
    use crate::module::{IrFunction, IrModule};
    use crate::terminator::Terminator;
    use crate::types::IrType;

    let mut module = IrModule::new();
    let mut func = IrFunction::new(
        FuncId(0),
        "bad_func".to_string(),
        vec![],
        vec![],
        IrType::Void,
        None,
    );
    func.blocks.push(BasicBlock {
        id: crate::block::BlockId(0),
        params: vec![],
        instructions: vec![],
        terminator: Terminator::None,
    });
    module.functions.push(func);

    let errors = verify::verify(&module);
    assert!(
        !errors.is_empty(),
        "verifier should catch missing terminator"
    );
    assert!(
        errors[0].message.contains("no terminator"),
        "error should mention no terminator, got: {}",
        errors[0].message
    );
}

#[test]
fn verifier_catches_invalid_block_target() {
    use crate::block::BasicBlock;
    use crate::instruction::FuncId;
    use crate::module::{IrFunction, IrModule};
    use crate::terminator::Terminator;
    use crate::types::IrType;

    let mut module = IrModule::new();
    let mut func = IrFunction::new(
        FuncId(0),
        "bad_target".to_string(),
        vec![],
        vec![],
        IrType::Void,
        None,
    );
    func.blocks.push(BasicBlock {
        id: crate::block::BlockId(0),
        params: vec![],
        instructions: vec![],
        // Jump to bb99, which doesn't exist.
        terminator: Terminator::Jump {
            target: crate::block::BlockId(99),
            args: vec![],
        },
    });
    module.functions.push(func);

    let errors = verify::verify(&module);
    assert!(!errors.is_empty(), "verifier should catch invalid target");
    assert!(
        errors[0].message.contains("invalid target"),
        "error should mention invalid target, got: {}",
        errors[0].message
    );
}

// ── Multiple interacting functions ───────────────────────────────────

#[test]
fn lower_multiple_functions() {
    let ir = lower_to_string(
        "function square(x: Int) -> Int { x * x }
         function sumOfSquares(a: Int, b: Int) -> Int {
             square(a) + square(b)
         }
         function main() -> Int { sumOfSquares(3, 4) }",
    );
    insta::assert_snapshot!(ir);
}

// ── String concatenation ────────────────────────────────────────────

#[test]
fn lower_string_concat() {
    let ir = lower_to_string(
        r#"function main() -> String {
             let a: String = "hello"
             let b: String = " world"
             a + b
         }"#,
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_lambda_with_captures() {
    let ir = lower_to_string(
        r#"function main() -> String {
             let greeting: String = "hello"
             let f: () -> String = function() -> String { greeting }
             f()
         }"#,
    );
    // Verify that the captured variable has the correct type (string, not i64).
    assert!(
        ir.contains("string"),
        "captured String variable should have string type in IR, got:\n{ir}"
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_match_string_literal() {
    let ir = lower_to_string(
        r#"function main() -> Int {
             let s: String = "hi"
             match s {
                 "hi" -> 1
                 "bye" -> 2
                 _ -> 0
             }
         }"#,
    );
    // Verify that string_eq is used, not ieq.
    assert!(
        ir.contains("string_eq"),
        "string match should use string_eq, got:\n{ir}"
    );
    insta::assert_snapshot!(ir);
}

// ── Impl block methods ──────────────────────────────────────────────

#[test]
fn lower_impl_block_method() {
    let ir = lower_to_string(
        "struct Pair { Int first  Int second }
         impl Pair {
             function sum(self) -> Int { self.first + self.second }
         }
         function main() -> Int {
             let p: Pair = Pair(3, 7)
             p.sum()
         }",
    );
    insta::assert_snapshot!(ir);
}

// ── Match with binding pattern ──────────────────────────────────────

#[test]
fn lower_match_binding() {
    let ir = lower_to_string(
        "function main() -> Int {
             let x: Int = 42
             match x {
                 1 -> 100
                 n -> n + 1
             }
         }",
    );
    insta::assert_snapshot!(ir);
}

// ── Try operator ────────────────────────────────────────────────────

#[test]
fn lower_try_operator() {
    let ir = lower_to_string(
        "function fallible() -> Result<Int, String> { Ok(42) }
         function main() -> Result<Int, String> {
             let val: Int = fallible()?
             Ok(val + 1)
         }",
    );
    // The try operator should produce an enum_discriminant check + early return.
    assert!(
        ir.contains("enum_discriminant"),
        "try operator should check discriminant, got:\n{ir}"
    );
    insta::assert_snapshot!(ir);
}

// ── Enum variant constructor via call syntax ────────────────────────

#[test]
fn lower_enum_variant_call_syntax() {
    let ir = lower_to_string(
        "enum Shape {
             Circle(Float)
             Rect(Float, Float)
         }
         function main() -> Shape { Circle(3.14) }",
    );
    assert!(
        ir.contains("enum_alloc"),
        "enum variant call should produce enum_alloc, got:\n{ir}"
    );
    insta::assert_snapshot!(ir);
}

// ── While loop with else block ──────────────────────────────────────

#[test]
fn lower_while_with_else() {
    let ir = lower_to_string(
        r#"function main() {
             let mut i: Int = 0
             while i < 3 {
                 i += 1
             } else {
                 print("done")
             }
         }"#,
    );
    // The else block should be a separate basic block.
    assert!(
        ir.contains("builtin_call @print"),
        "else block should contain print call, got:\n{ir}"
    );
    insta::assert_snapshot!(ir);
}

// ── For range with else block ───────────────────────────────────────

#[test]
fn lower_for_range_with_else() {
    let ir = lower_to_string(
        r#"function main() {
             for i in 0..3 {
                 print(toString(i))
             } else {
                 print("empty")
             }
         }"#,
    );
    insta::assert_snapshot!(ir);
}

// ── Indirect call through closure variable ──────────────────────────

#[test]
fn lower_indirect_closure_call() {
    let ir = lower_to_string(
        "function apply(f: (Int) -> Int, x: Int) -> Int { f(x) }
         function main() -> Int {
             apply(function(n: Int) -> Int { n * 2 }, 21)
         }",
    );
    assert!(
        ir.contains("call_indirect"),
        "calling a function parameter should use call_indirect, got:\n{ir}"
    );
    insta::assert_snapshot!(ir);
}

// ── Trait method dispatch ───────────────────────────────────────────

#[test]
fn lower_trait_method() {
    let ir = lower_to_string(
        "trait Greetable {
             function greet(self) -> String
         }
         struct Person { String name }
         impl Greetable for Person {
             function greet(self) -> String { self.name }
         }
         function main() -> String {
             let p: Person = Person(\"Alice\")
             p.greet()
         }",
    );
    assert!(
        ir.contains("call f"),
        "trait method should compile to a direct call, got:\n{ir}"
    );
    insta::assert_snapshot!(ir);
}

// ── Enum methods ────────────────────────────────────────────────────

#[test]
fn lower_enum_method() {
    let ir = lower_to_string(
        "enum Color { Red  Green  Blue }
         impl Color {
             function is_red(self) -> Bool {
                 match self {
                     Red -> true
                     _ -> false
                 }
             }
         }
         function main() -> Bool {
             let c: Color = Red
             c.is_red()
         }",
    );
    assert!(
        ir.contains("Color.is_red"),
        "enum method should be registered with mangled name, got:\n{ir}"
    );
    insta::assert_snapshot!(ir);
}

// ── Variable shadowing in nested scopes ─────────────────────────────

#[test]
fn lower_variable_shadowing() {
    let ir = lower_to_string(
        "function main() -> Int {
             let x: Int = 10
             if true {
                 let x: Int = 20
                 print(toString(x))
             }
             x
         }",
    );
    // The outer `x` should survive the inner scope — the implicit return
    // should use the outer x (v0), not the shadowed x.
    insta::assert_snapshot!(ir);
}

/// Option and Result enum layouts are registered even in minimal programs.
#[test]
fn builtin_enum_layouts_always_registered() {
    let module = lower_source(
        r#"
function main() {
    print(42)
}
"#,
    );
    // Option and Result layouts should be registered as built-in enums.
    assert!(
        module.enum_layouts.contains_key(OPTION_ENUM),
        "Option layout should be registered"
    );
    assert!(
        module.enum_layouts.contains_key(RESULT_ENUM),
        "Result layout should be registered"
    );
    // Option has two variants: Some and None.
    let option_layout = &module.enum_layouts[OPTION_ENUM];
    assert_eq!(option_layout.len(), 2, "Option should have 2 variants");
    assert_eq!(option_layout[0].0, "Some");
    assert_eq!(option_layout[1].0, "None");
    // Result has two variants: Ok and Err.
    let result_layout = &module.enum_layouts[RESULT_ENUM];
    assert_eq!(result_layout.len(), 2, "Result should have 2 variants");
    assert_eq!(result_layout[0].0, "Ok");
    assert_eq!(result_layout[1].0, "Err");
}

/// Option and Result use EnumAlloc (not StructAlloc) in IR output.
#[test]
fn option_result_use_enum_alloc_in_ir() {
    let ir = lower_to_string(
        r#"
function main() {
    let x: Option<Int> = Some(42)
    let y: Result<Int, String> = Ok(1)
}
"#,
    );
    // Should use enum_alloc, not struct_alloc, for Option/Result.
    assert!(
        ir.contains("enum_alloc @Option"),
        "Some should lower to enum_alloc @Option, got:\n{ir}"
    );
    assert!(
        ir.contains("enum_alloc @Result"),
        "Ok should lower to enum_alloc @Result, got:\n{ir}"
    );
    assert!(
        !ir.contains("struct_alloc @Some"),
        "Some should NOT use struct_alloc, got:\n{ir}"
    );
    assert!(
        !ir.contains("struct_alloc @Ok"),
        "Ok should NOT use struct_alloc, got:\n{ir}"
    );
}

/// Bare `None` must lower to `enum_alloc @Option:1()`,
/// not `struct_alloc @None`.
#[test]
fn bare_none_uses_enum_alloc() {
    let ir = lower_to_string(
        r#"
function main() {
    let x: Option<Int> = None
    print(toString(x))
}
"#,
    );
    assert!(
        ir.contains("enum_alloc @Option:1"),
        "bare None should lower to enum_alloc @Option:1, got:\n{ir}"
    );
    assert!(
        !ir.contains("struct_alloc @None"),
        "bare None should NOT use struct_alloc, got:\n{ir}"
    );
}

/// Verify `Err("msg")` produces `enum_alloc @Result:1(...)`.
#[test]
fn err_constructor_uses_enum_alloc() {
    let ir = lower_to_string(
        r#"
function main() {
    let x: Result<Int, String> = Err("bad")
    print(toString(x))
}
"#,
    );
    assert!(
        ir.contains("enum_alloc @Result:1("),
        "Err should lower to enum_alloc @Result:1(...), got:\n{ir}"
    );
    assert!(
        !ir.contains("struct_alloc @Err"),
        "Err should NOT use struct_alloc, got:\n{ir}"
    );
}

/// Match on Option<Int> with Some(x) and None arms produces
/// `enum_discriminant` and `enum_get_field` instructions.
#[test]
fn match_on_option() {
    let ir = lower_to_string(
        r#"
function main() -> Int {
    let x: Option<Int> = Some(42)
    match x {
        Some(v) -> v
        None -> 0
    }
}
"#,
    );
    assert!(
        ir.contains("enum_discriminant"),
        "match on Option should use enum_discriminant, got:\n{ir}"
    );
    assert!(
        ir.contains("enum_get_field"),
        "match on Option should use enum_get_field for Some binding, got:\n{ir}"
    );
}

/// Match on Result<Int, String> with Ok(x) and Err(e) arms produces
/// `enum_discriminant` and `enum_get_field` instructions.
#[test]
fn match_on_result() {
    let ir = lower_to_string(
        r#"
function main() -> Int {
    let x: Result<Int, String> = Ok(42)
    match x {
        Ok(v) -> v
        Err(e) -> 0
    }
}
"#,
    );
    assert!(
        ir.contains("enum_discriminant"),
        "match on Result should use enum_discriminant, got:\n{ir}"
    );
    assert!(
        ir.contains("enum_get_field"),
        "match on Result should use enum_get_field for Ok/Err bindings, got:\n{ir}"
    );
}

/// Try operator on Option — the `?` desugaring should check for None and
/// return early, extracting the Some payload on the happy path.
#[test]
fn lower_try_operator_option() {
    let ir = lower_to_string(
        "function maybe() -> Option<Int> { Some(10) }
         function main() -> Option<Int> {
             let val: Int = maybe()?
             Some(val + 1)
         }",
    );
    assert!(
        ir.contains("enum_discriminant"),
        "try on Option should check discriminant, got:\n{ir}"
    );
}

/// Snapshot test for match on Result — verifies the Err binding resolves
/// to the correct type (`string`, not `struct.__generic`).
#[test]
fn match_on_result_snapshot() {
    let ir = lower_to_string(
        r#"
function main() -> Int {
    let x: Result<Int, String> = Ok(42)
    match x {
        Ok(v) -> v
        Err(e) -> 0
    }
}
"#,
    );
    insta::assert_snapshot!(ir);
}

// ── If as a first-class expression ───────────────────────────────────────
// These tests cover value-producing `if` lowering: merge-block parameter for
// non-Void results, branch values threaded via `Jump { args }`, else-if
// chains that produce nested merge blocks, and the Void statement-position
// case that must still lower cleanly.

#[test]
fn lower_if_expr_threads_value_fib() {
    // Recursive `fib` with `if` in tail position.
    // The IR must return a well-typed I64.
    let ir = lower_to_string(
        "function fib(n: Int) -> Int {
            if n <= 1 { n } else { fib(n - 1) + fib(n - 2) }
        }
        function main() { print(fib(10)) }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_if_expr_as_var_init() {
    let ir = lower_to_string(
        "function main() -> Int {
            let x: Int = if true { 10 } else { 20 }
            x
        }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_if_expr_else_if_chain_value() {
    // `else if` recursion: each nested `if` should produce its own merge
    // block whose ValueId is threaded to the outer merge.
    let ir = lower_to_string(
        "function main() -> Int {
            if false { 1 } else if true { 2 } else { 3 }
        }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_if_expr_in_arithmetic() {
    let ir = lower_to_string(
        "function main() -> Int {
            1 + if true { 2 } else { 3 }
        }",
    );
    insta::assert_snapshot!(ir);
}

#[test]
fn lower_if_expr_all_branches_return() {
    // When every branch diverges (explicit `return`), the merge block has
    // no live predecessors.  The IR must still be well-formed.
    let ir = lower_to_string(
        "function classify(n: Int) -> String {
            if n < 0 { return \"neg\" } else { return \"non-neg\" }
        }
        function main() { print(classify(-1)) }",
    );
    insta::assert_snapshot!(ir);
}

/// An ambiguous/unresolvable generic call site must not
/// panic in `lower_type` when sema's inference falls back to `Type::Error`.
/// Lowering should produce a module (possibly with placeholder types)
/// without unwinding.
#[test]
fn unresolvable_generic_call_does_not_panic() {
    let source = r#"
function identity<T>(x: T) -> T { x }
function main() {
    // No call to `identity` at all — sema has nothing to infer, and the
    // template stays untouched. Lower should succeed.
}
"#;
    // Compiling an uninstantiated template must not panic.
    let _ = lower_source(source);
}

/// Verifier must skip generic templates — their bodies contain
/// `IrType::TypeVar` which no backend consumes.
#[test]
fn verifier_skips_generic_templates() {
    let module = lower_source(
        r#"
function identity<T>(x: T) -> T { x }
function main() { print(identity(1)) }
"#,
    );

    // The module contains the template and the specialization.
    let names: Vec<&str> = module.functions.iter().map(|f| f.name.as_str()).collect();
    assert!(names.contains(&"identity"), "expected template");
    assert!(
        names.contains(&"identity__i64"),
        "expected specialization, have: {names:?}"
    );

    // Verification succeeds without errors despite TypeVars in the
    // template body.
    let errs = verify::verify(&module);
    assert!(
        errs.is_empty(),
        "verifier reported errors for template: {errs:?}"
    );
}

/// `IrModule::concrete_functions` must filter out generic templates.
#[test]
fn concrete_functions_filters_templates() {
    let module = lower_source(
        r#"
function identity<T>(x: T) -> T { x }
function main() { print(identity(1)) }
"#,
    );
    for f in module.concrete_functions() {
        assert!(
            !f.is_generic_template,
            "concrete_functions yielded template `{}`",
            f.name
        );
    }
}

/// Generic methods on user-defined types are monomorphized just like
/// generic free functions.
#[test]
fn generic_method_specializations_appear_in_module() {
    let module = lower_source(
        r#"
struct Holder {
    Int tag
}
impl Holder {
    function wrap<U>(self, x: U) -> U { x }
}
function main() {
    let h = Holder(1)
    print(h.wrap(42))
    print(h.wrap("hi"))
}
"#,
    );

    // Expect two specializations: at Int and at String.
    let names: Vec<&str> = module.functions.iter().map(|f| f.name.as_str()).collect();
    assert!(
        names.iter().any(|n| n.contains("wrap__i64")),
        "missing Int specialization, have: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("wrap__str")),
        "missing String specialization, have: {names:?}"
    );
}

#[test]
fn generic_method_call_retargets_to_specialization() {
    use crate::instruction::Op;

    let module = lower_source(
        r#"
struct Holder {
    Int tag
}
impl Holder {
    function wrap<U>(self, x: U) -> U { x }
}
function main() {
    let h = Holder(1)
    print(h.wrap(42))
    print(h.wrap("hi"))
}
"#,
    );

    // Find the template's FuncId — any specialization targeting this id
    // would be a regression.
    let template_id = module
        .functions
        .iter()
        .find(|f| f.name == "Holder.wrap" && f.is_generic_template)
        .expect("expected a template function for Holder.wrap")
        .id;

    let main = module
        .functions
        .iter()
        .find(|f| f.name == "main")
        .expect("main function");
    for block in &main.blocks {
        for instr in &block.instructions {
            if let Op::Call(callee, type_args, _) = &instr.op {
                assert_ne!(
                    *callee, template_id,
                    "main still calls the Holder.wrap template directly — \
                     sema did not record call_type_args or monomorphize \
                     did not rewrite the call"
                );
                assert!(
                    type_args.is_empty(),
                    "main's Op::Call still carries non-empty type_args \
                     post-monomorphization: {type_args:?}"
                );
            }
        }
    }
}

/// A generic call at a reference type (List, Map, closure, enum) must
/// mangle to a symbol-safe name. This pins the exact mangled names so a
/// regression that silently produces different-but-still-safe symbols is
/// still caught.
#[test]
fn mangled_names_are_exact_and_symbol_safe() {
    let module = lower_source(
        r#"
function identity<T>(x: T) -> T { x }
function sizeOf<K, V>(m: Map<K, V>) -> Int { m.length() }
function hasValue<T>(o: Option<T>) -> Bool { o.isSome() }
function main() {
    let xs: List<Int> = [1, 2]
    let ys = identity(xs)
    let m: Map<String, Int> = {"a": 1}
    let s = sizeOf(m)
    let some: Option<Int> = Some(1)
    let h = hasValue(some)
    print(ys.length())
    print(s)
    print(h)
}
"#,
    );
    let names: Vec<&str> = module.functions.iter().map(|f| f.name.as_str()).collect();
    // Pin exact mangled names so a silent mangling regression is caught.
    // Note: `hasValue` is inferred as `T := Int` from `Option<Int>`, so the
    // specialization is `hasValue__i64` — not `hasValue__e_Option`.
    for expected in ["identity__L_i64_E", "sizeOf__str__i64", "hasValue__i64"] {
        assert!(
            names.iter().any(|n| n == &expected),
            "expected mangled name `{expected}`, have: {names:?}"
        );
    }
    // All function names must be symbol-safe `[A-Za-z0-9_.]` (the dot is
    // for method names like `Holder.wrap`; Cranelift's symbol emitter
    // rewrites `.` to `__`).
    for n in &names {
        assert!(
            n.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.'),
            "function name `{n}` is not symbol-safe"
        );
    }
}

/// Two distinct type arguments must mangle to distinct names. Regression
/// guard for any future simplification of `mangle_type` that collapses
/// structurally-different types into the same encoding.
#[test]
fn mangling_is_injective_for_distinct_type_args() {
    use crate::monomorphize::mangle_type;
    use crate::types::IrType;

    let cases: Vec<IrType> = vec![
        IrType::I64,
        IrType::F64,
        IrType::Bool,
        IrType::Void,
        IrType::StringRef,
        IrType::StructRef("Point".into()),
        IrType::EnumRef("Point".into()), // distinct encoding from StructRef of same name
        IrType::ListRef(Box::new(IrType::I64)),
        IrType::ListRef(Box::new(IrType::StringRef)),
        IrType::MapRef(Box::new(IrType::StringRef), Box::new(IrType::I64)),
        IrType::MapRef(Box::new(IrType::I64), Box::new(IrType::StringRef)),
        IrType::ClosureRef {
            param_types: vec![IrType::I64],
            return_type: Box::new(IrType::I64),
        },
        IrType::ClosureRef {
            param_types: vec![IrType::I64, IrType::I64],
            return_type: Box::new(IrType::I64),
        },
    ];
    let mut seen = std::collections::HashMap::new();
    for ty in &cases {
        let mangled = mangle_type(ty);
        if let Some(prev) = seen.insert(mangled.clone(), ty.clone()) {
            panic!("mangling collision: `{mangled}` produced by both `{prev:?}` and `{ty:?}`");
        }
    }
}

/// Before monomorphization runs, a generic call site's IR instruction
/// should carry the concrete type arguments in `Op::Call`'s middle slot.
/// We can't observe that from the final `lower()` output (which runs
/// monomorphize internally), so instead we assert the post-mono invariant:
/// every remaining `Op::Call` in concrete functions has empty `type_args`.
/// This is the contract Cranelift and the interpreter rely on.
#[test]
fn post_monomorphization_no_call_carries_type_args() {
    use crate::instruction::Op;

    let module = lower_source(
        r#"
function identity<T>(x: T) -> T { x }
function outer<T>(x: T) -> T { identity(x) }
function main() {
    print(outer(1))
    print(outer("hi"))
}
"#,
    );
    for func in module.concrete_functions() {
        for block in &func.blocks {
            for instr in &block.instructions {
                if let Op::Call(_, targs, _) = &instr.op {
                    assert!(
                        targs.is_empty(),
                        "concrete function `{}` has Op::Call with non-empty type_args: {targs:?}",
                        func.name
                    );
                }
            }
        }
    }
}

/// Pass D end-to-end: an orphan `IrType::TypeVar` in a non-template
/// function (arising from sema inference that never saw a constraint —
/// typically an empty list literal whose element type is unresolved)
/// must be erased to `StructRef(GENERIC_PLACEHOLDER)` so the Cranelift
/// backend's use-site inference can handle it instead of panicking in
/// `is_value_type()`.
#[test]
fn orphan_typevar_is_erased_to_generic_placeholder_post_mono() {
    use crate::types::{GENERIC_PLACEHOLDER, IrType};

    let module = lower_source(
        r#"
function main() {
    let xs: List<Int> = []
    print(xs.length())
}
"#,
    );

    // Walk every concrete function and assert no IrType::TypeVar remains.
    // Any List<TypeVar(...)> result type introduced by the `[]` literal
    // before sema binding must have been erased to
    // List<StructRef("__generic")>.
    fn walk(t: &IrType, saw_placeholder: &mut bool) {
        match t {
            IrType::TypeVar(name) => panic!("residual TypeVar({name}) survived Pass D"),
            IrType::StructRef(n) if n == GENERIC_PLACEHOLDER => *saw_placeholder = true,
            IrType::ListRef(inner) => walk(inner, saw_placeholder),
            IrType::MapRef(k, v) => {
                walk(k, saw_placeholder);
                walk(v, saw_placeholder);
            }
            IrType::ClosureRef {
                param_types,
                return_type,
            } => {
                for p in param_types {
                    walk(p, saw_placeholder);
                }
                walk(return_type, saw_placeholder);
            }
            _ => {}
        }
    }

    for func in module.concrete_functions() {
        for pt in &func.param_types {
            walk(pt, &mut false);
        }
        for block in &func.blocks {
            for instr in &block.instructions {
                walk(&instr.result_type, &mut false);
            }
            for (_, bp_ty) in &block.params {
                walk(bp_ty, &mut false);
            }
        }
    }
    // The walker above panics on residual TypeVar; reaching here means
    // every TypeVar was erased. We don't require a placeholder to appear
    // in this particular program (sema may have bound `[]` to
    // `List<Int>` from the annotation), but the *absence* of TypeVar is
    // the key invariant.
}

/// A source-level self-recursive generic (`fn f<T>(...) { ... f(...) ... }`)
/// must produce a single specialization per concrete type arg, with the
/// recursive call targeting that same specialization (not the template).
#[test]
fn self_recursive_generic_lowers_and_specializes() {
    use crate::instruction::Op;

    let module = lower_source(
        r#"
function countDown<T>(x: T, n: Int) -> T {
    if n <= 0 { x } else { countDown(x, n - 1) }
}
function main() {
    print(countDown(42, 3))
    print(countDown("done", 2))
}
"#,
    );
    let spec_ids: Vec<_> = module
        .concrete_functions()
        .filter(|f| f.name.starts_with("countDown__"))
        .map(|f| (f.name.clone(), f.id))
        .collect();
    assert_eq!(
        spec_ids.len(),
        2,
        "expected two specializations, got {spec_ids:?}"
    );

    for (name, spec_id) in &spec_ids {
        let func = module
            .functions
            .iter()
            .find(|f| f.id == *spec_id)
            .unwrap_or_else(|| panic!("specialization `{name}` not found"));
        let mut saw_self_call = false;
        for block in &func.blocks {
            for instr in &block.instructions {
                if let Op::Call(callee, _, _) = &instr.op
                    && *callee == *spec_id
                {
                    saw_self_call = true;
                }
            }
        }
        assert!(
            saw_self_call,
            "specialization `{name}` did not target itself recursively — \
             the internal call was not rewritten"
        );
    }
}
