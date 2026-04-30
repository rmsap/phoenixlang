//! Integration tests for IR lowering.

use crate::instruction::Op;
use crate::lower;
use crate::lower::LoweringContext;
use crate::lower_modules;
use crate::types::{IrType, OPTION_ENUM, RESULT_ENUM};
use crate::verify;
use phoenix_common::module_path::ModulePath;
use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_modules::ResolvedSourceModule;
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
    lower(&program, &check_result.module)
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
    module
        .functions
        .push(crate::module::FunctionSlot::Concrete(func));

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
    module
        .functions
        .push(crate::module::FunctionSlot::Concrete(func));

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
    let names: Vec<&str> = module
        .functions
        .iter()
        .map(|s| s.func().name.as_str())
        .collect();
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

/// `IrModule::lookup` resolves a template's `FuncId` to its
/// underlying `IrFunction`; `IrModule::get_concrete` returns `None`
/// for the same id. Pins the typed-split contract: lookups that don't
/// care about the kind use `lookup`, lookups that do (codegen,
/// runtime call dispatch) use `get_concrete` and get `None` rather
/// than a TypeVar-bearing body.
#[test]
fn module_lookup_template_returns_some_get_concrete_returns_none() {
    let module = lower_source(
        r#"
function identity<T>(x: T) -> T { x }
function main() { print(identity(1)) }
"#,
    );
    let (template_fid, _) = module
        .templates()
        .find(|(_, f)| f.name == "identity")
        .expect("identity template should exist");
    assert!(
        module.lookup(template_fid).is_some(),
        "lookup(template) returns Some"
    );
    assert!(
        module.get_concrete(template_fid).is_none(),
        "get_concrete(template) returns None"
    );

    // Conversely, the specialization at a different FuncId is concrete.
    let spec_fid = module.function_index["identity__i64"];
    assert!(module.lookup(spec_fid).is_some());
    assert!(module.get_concrete(spec_fid).is_some());
}

/// After monomorphization, the original template stays in
/// `module.functions` at its original `FuncId` as a Template slot,
/// while the specialization is appended as a new Concrete slot at a
/// higher FuncId. `concrete_functions()` yields only the latter.
#[test]
fn concrete_functions_excludes_template_after_specialization() {
    let module = lower_source(
        r#"
function identity<T>(x: T) -> T { x }
function main() { print(identity(1)) }
"#,
    );
    // The bare `identity` name resolves to the template's FuncId
    // (sema doesn't rewrite function_index entries during mono).
    let template_fid = module.function_index["identity"];
    let concrete_fids: Vec<_> = module.concrete_functions().map(|f| f.id).collect();
    assert!(
        !concrete_fids.contains(&template_fid),
        "concrete_functions yielded the template at FuncId({})",
        template_fid.0
    );
    // ...and the specialization *is* yielded.
    let spec_fid = module.function_index["identity__i64"];
    assert!(concrete_fids.contains(&spec_fid));
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
    // `concrete_functions()` returns `&IrFunction` directly — there is
    // no `is_template`-style flag to inspect, so a non-empty iteration
    // *is* the proof that templates were filtered out.
    let mut concrete_count = 0;
    for _ in module.concrete_functions() {
        concrete_count += 1;
    }
    assert!(concrete_count > 0, "no concrete functions iterated");
    assert!(
        module.templates().count() > 0,
        "module should also contain at least one template"
    );
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
    let names: Vec<&str> = module
        .functions
        .iter()
        .map(|s| s.func().name.as_str())
        .collect();
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
        .templates()
        .find(|(_, f)| f.name == "Holder.wrap")
        .expect("expected a template function for Holder.wrap")
        .1
        .id;

    let main = module
        .concrete_functions()
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
    let names: Vec<&str> = module
        .functions
        .iter()
        .map(|s| s.func().name.as_str())
        .collect();
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
        IrType::StructRef("Point".into(), Vec::new()),
        IrType::StructRef("Container".into(), vec![IrType::I64]),
        IrType::EnumRef("Point".into(), Vec::new()), // distinct encoding from StructRef of same name
        IrType::EnumRef("Option".into(), vec![IrType::I64]),
        IrType::EnumRef("Result".into(), vec![IrType::StringRef, IrType::I64]),
        // Collision guard: single-underscore delimiter would produce
        // `e_Opt_s_foo_i64_E` for both of these distinct types.  The `__`
        // delimiter in `mangle_type` must keep them apart.
        IrType::EnumRef(
            "Opt".into(),
            vec![IrType::StructRef("foo_i64".into(), Vec::new())],
        ),
        IrType::EnumRef(
            "Opt".into(),
            vec![IrType::StructRef("foo".into(), Vec::new()), IrType::I64],
        ),
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
            IrType::StructRef(n, args) if n == GENERIC_PLACEHOLDER && args.is_empty() => {
                *saw_placeholder = true
            }
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
            .map(|s| s.func())
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

/// `enum_type_at` is a private helper on `LoweringContext` used by
/// `lower_ident`, `lower_call`, and `lower_struct_literal` to decide
/// whether an identifier / call / struct-literal expression is actually
/// an enum variant construction. Rather than testing the helper directly
/// (ResolvedModule has too many fields to mock), we drive it through
/// `lower_source` and assert on the resulting `EnumAlloc`'s result type
/// — that type is precisely the output of `enum_type_at` threaded through
/// `lower_type_args`.
///
/// Sema types the RHS expression independently of the `let` annotation,
/// so every arg must be inferable from the variant's own arguments for
/// this test to observe a concrete `EnumRef`.  `Some(42)` pins `T = Int`
/// from the literal, so the resulting `EnumAlloc` carries `[I64]`.
#[test]
fn enum_type_at_preserves_args_for_generic_constructor_call() {
    let module = lower_source(
        r#"
function main() {
    let o: Option<Int> = Some(42)
}
"#,
    );
    let main = module.functions[module.function_index["main"].index()].func();
    let alloc_inst = main.blocks[0]
        .instructions
        .iter()
        .find(|i| matches!(&i.op, crate::instruction::Op::EnumAlloc(name, _, _) if name == OPTION_ENUM))
        .expect("expected an EnumAlloc for Some(42)");
    assert_eq!(
        alloc_inst.result_type,
        IrType::EnumRef(OPTION_ENUM.to_string(), vec![IrType::I64]),
    );
}

/// `enum_type_at` on a call inside a function parameter context:
/// `unwrap_or_default(Some("x"))` — the `Some(...)` is typed by sema
/// from the literal, so `enum_type_at` returns
/// `Type::Generic("Option", [String])` and the EnumAlloc's result type
/// carries `[StringRef]`.
#[test]
fn enum_type_at_preserves_multi_slot_payload() {
    let module = lower_source(
        r#"
function main() {
    let o = Some("hello")
}
"#,
    );
    let main = module.functions[module.function_index["main"].index()].func();
    let alloc_inst = main.blocks[0]
        .instructions
        .iter()
        .find(|i| matches!(&i.op, crate::instruction::Op::EnumAlloc(name, _, _) if name == OPTION_ENUM))
        .expect("expected an EnumAlloc for Some(\"hello\")");
    assert_eq!(
        alloc_inst.result_type,
        IrType::EnumRef(OPTION_ENUM.to_string(), vec![IrType::StringRef]),
    );
}

/// User-defined *generic* enum: `lower_type`'s `other` branch (neither
/// stdlib Option/Result nor a struct) must carry args through
/// `EnumRef(name, args)` just like stdlib enums. End-to-end compilation
/// is blocked on generic-enum monomorphization landing, but IR lowering
/// already produces the correct `EnumRef` — which is what this guards.
#[test]
fn user_defined_generic_enum_preserves_args_in_enum_ref() {
    let module = lower_source(
        r#"
enum Box<T> {
    Wrap(T)
    Empty
}
function main() {
    let b: Box<Int> = Wrap(42)
}
"#,
    );
    let main = module.functions[module.function_index["main"].index()].func();
    let alloc_inst = main.blocks[0]
        .instructions
        .iter()
        .find(|i| matches!(&i.op, crate::instruction::Op::EnumAlloc(name, _, _) if name == "Box"))
        .expect("expected an EnumAlloc for Wrap(42)");
    assert_eq!(
        alloc_inst.result_type,
        IrType::EnumRef("Box".to_string(), vec![IrType::I64]),
    );
}

/// Zero-field variant of a non-generic user enum — `enum_type_at` must
/// resolve `Red` via its span's `Type::Named("Color")` fallback branch
/// and produce `EnumRef("Color", [])`.
#[test]
fn enum_type_at_handles_zero_field_non_generic_variant() {
    let module = lower_source(
        r#"
enum Color { Red  Green  Blue }
function main() {
    let c = Red
}
"#,
    );
    let main = module.functions[module.function_index["main"].index()].func();
    let alloc_inst = main.blocks[0]
        .instructions
        .iter()
        .find(|i| matches!(&i.op, crate::instruction::Op::EnumAlloc(name, _, _) if name == "Color"))
        .expect("expected an EnumAlloc for Red");
    assert_eq!(
        alloc_inst.result_type,
        IrType::EnumRef("Color".to_string(), Vec::new()),
    );
}

/// Zero-field variant of a *generic* user enum: `Empty` in `Box<T>`
/// (companion to `enum_type_at_handles_zero_field_non_generic_variant`).
///
/// Sema cannot infer `T` from the `Empty` literal alone — the variant
/// carries no payload to pin the type parameter, and sema does *not*
/// currently thread the `let: Box<Int>` binding annotation back into the
/// RHS expression's type. The result is
/// `EnumRef("Box", [GENERIC_PLACEHOLDER])`, which Pass D would preserve
/// as-is (the placeholder is a concrete `StructRef`, not a `TypeVar`).
///
/// This test locks in the current behavior so a future sema improvement
/// that propagates annotations to zero-field variants is caught by the
/// test failing — at which point the expectation should switch to
/// `[I64]`. Guards the enum_type_at path from regressing in the opposite
/// direction (e.g. dropping args entirely).
#[test]
fn enum_type_at_handles_zero_field_generic_variant() {
    let module = lower_source(
        r#"
enum Box<T> {
    Wrap(T)
    Empty
}
function main() {
    let b: Box<Int> = Empty
}
"#,
    );
    let main = module.functions[module.function_index["main"].index()].func();
    let alloc_inst = main.blocks[0]
        .instructions
        .iter()
        .find(|i| matches!(&i.op, crate::instruction::Op::EnumAlloc(name, _, _) if name == "Box"))
        .expect("expected an EnumAlloc for Empty");
    assert_eq!(
        alloc_inst.result_type,
        IrType::EnumRef(
            "Box".to_string(),
            vec![IrType::StructRef(
                crate::types::GENERIC_PLACEHOLDER.to_string(),
                Vec::new()
            )]
        ),
    );
}

/// Monomorphizing a generic function that returns a user-defined generic
/// enum: `wrap<T>(x: T) -> Box<T>` specialized at `Int` must rewrite the
/// return type to `EnumRef("Box", [I64])`, and the inner `EnumAlloc`
/// that builds the result must carry the same substituted args. Guards
/// the interaction between `monomorphize::substitute` (which now recurses
/// into `EnumRef` args) and user-defined generic enums.
#[test]
fn monomorphization_substitutes_into_user_defined_enum_return_type() {
    let module = lower_source(
        r#"
enum Box<T> {
    Wrap(T)
    Empty
}
function wrap<T>(x: T) -> Box<T> { Wrap(x) }
function main() {
    let b: Box<Int> = wrap(42)
}
"#,
    );
    let spec = module.functions[module.function_index["wrap__i64"].index()].func();
    assert_eq!(
        spec.return_type,
        IrType::EnumRef("Box".to_string(), vec![IrType::I64]),
    );
    assert_eq!(spec.param_types, vec![IrType::I64]);

    // The inner `EnumAlloc` that builds the `Wrap(x)` value inside the
    // specialization must also carry the substituted arg — otherwise
    // downstream consumers (e.g. backend payload inference) would see a
    // `Box` alloc with empty args inside a function whose signature
    // claims `Box<Int>`.
    let alloc_inst = spec
        .blocks
        .iter()
        .flat_map(|b| &b.instructions)
        .find(|i| matches!(&i.op, crate::instruction::Op::EnumAlloc(name, _, _) if name == "Box"))
        .expect("expected an EnumAlloc for Wrap(x) in the specialization");
    assert_eq!(
        alloc_inst.result_type,
        IrType::EnumRef("Box".to_string(), vec![IrType::I64]),
    );
}

// ── Multi-module lowering ──────────────────────────────────────────────

/// Build a `ResolvedSourceModule` from a raw source string.
fn make_module(
    module_path: ModulePath,
    source: &str,
    source_id: SourceId,
    is_entry: bool,
) -> ResolvedSourceModule {
    use std::path::PathBuf;
    let tokens = tokenize(source, source_id);
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    ResolvedSourceModule {
        module_path,
        source_id,
        program,
        is_entry,
        file_path: PathBuf::from(format!("<test:{source_id:?}>")),
    }
}

/// `lower_modules` registers a non-entry-module function under its
/// module-qualified key in `function_index`, and pass 2's per-module
/// `current_module` switch is what lets a within-module call site
/// (`shout()` inside `lib::shout_twice`) resolve to the same qualified
/// `lib::shout` key during body lowering. Without the switch the call
/// site would `function_index.get("shout")` (bare), miss, and fall
/// through to an indirect call.
#[test]
fn lower_modules_qualifies_non_entry_function() {
    let entry = make_module(ModulePath::entry(), "function main() {}", SourceId(0), true);
    let lib = make_module(
        ModulePath(vec!["lib".to_string()]),
        "public function shout() -> String { \"hi\" }\n\
         public function shout_twice() -> String { shout() }\n\
         public function answer() -> Int { 42 }",
        SourceId(1),
        false,
    );

    let modules = vec![entry, lib];
    let analysis = checker::check_modules(&modules);
    assert!(
        analysis.diagnostics.is_empty(),
        "sema errors: {:?}",
        analysis
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );

    let ir_module = lower_modules(&modules, &analysis.module);

    // Entry-module function keeps a bare key.
    assert!(
        ir_module.function_index.contains_key("main"),
        "expected `main` under bare name; have: {:?}",
        ir_module.function_index.keys().collect::<Vec<_>>()
    );
    // Non-entry function keys are module-qualified.
    let shout_id = *ir_module
        .function_index
        .get("lib::shout")
        .unwrap_or_else(|| {
            panic!(
                "expected `lib::shout` in function_index; have: {:?}",
                ir_module.function_index.keys().collect::<Vec<_>>()
            )
        });
    let shout_twice_id = *ir_module
        .function_index
        .get("lib::shout_twice")
        .expect("expected `lib::shout_twice` in function_index");
    let answer_id = *ir_module
        .function_index
        .get("lib::answer")
        .expect("expected `lib::answer` in function_index");

    // Within-module call resolution: `shout_twice`'s body must contain a
    // direct `Op::Call` targeting `lib::shout`'s FuncId. This is the
    // teeth of the multi-module wiring — a registration-only stub or a
    // wrong-target call would slip past `!blocks.is_empty()`.
    let shout_twice = ir_module.lookup(shout_twice_id).expect("shout_twice slot");
    let calls_shout = shout_twice
        .blocks
        .iter()
        .flat_map(|b| &b.instructions)
        .any(|i| matches!(&i.op, Op::Call(target, _, _) if *target == shout_id));
    assert!(
        calls_shout,
        "lib::shout_twice did not resolve `shout()` to FuncId {:?} — \
         within-module qualification broke",
        shout_id
    );

    // Both leaf functions also have to actually have a lowered body; an
    // empty `blocks` Vec is the observable signature of "registration
    // ran but body lowering's `function_index.get` missed."
    let shout = ir_module.lookup(shout_id).expect("shout slot");
    assert!(
        !shout.blocks.is_empty(),
        "lib::shout body was not lowered (empty blocks)"
    );
    let answer = ir_module.lookup(answer_id).expect("answer slot");
    assert!(
        !answer.blocks.is_empty(),
        "lib::answer body was not lowered (empty blocks)"
    );

    // The whole module must verify — catches any structural breakage
    // (missing terminators, dangling FuncIds) introduced by the
    // multi-module path.
    let errs = verify::verify(&ir_module);
    assert!(
        errs.is_empty(),
        "verifier errors: {:?}",
        errs.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

/// `qualify_resolved` must pass through any name that already contains
/// `::` (treating it as canonical) and qualify bare names against
/// `current_module`. This is the contract the no-field enum-variant
/// path in `phoenix_sema::check_expr::lower_ident` depends on — it
/// produces a `Type::Named("lib::Color")` that body-lowering must not
/// re-prefix to `lib::lib::Color` when looking up `method_index`.
#[test]
fn qualify_resolved_passes_through_already_qualified_names() {
    // Any well-typed entry module gives us a `ResolvedModule` to back
    // the `LoweringContext`; the fixture's contents don't matter here.
    let entry = make_module(ModulePath::entry(), "function main() {}", SourceId(0), true);
    let modules = vec![entry];
    let analysis = checker::check_modules(&modules);
    assert!(analysis.diagnostics.is_empty());

    let mut ctx = LoweringContext::new(&analysis.module);
    ctx.current_module = ModulePath(vec!["lib".to_string()]);

    // Bare name → qualified against current module.
    assert_eq!(ctx.qualify_resolved("Foo").as_ref(), "lib::Foo");
    // Already-qualified name from `Type::Named` → pass through unchanged,
    // even when the qualifier matches the current module.
    assert_eq!(ctx.qualify_resolved("lib::Color").as_ref(), "lib::Color");
    // Already-qualified name from a *different* module → also pass
    // through; cross-module receivers are sema's job to resolve.
    assert_eq!(ctx.qualify_resolved("other::Foo").as_ref(), "other::Foo");

    // Same checks under entry — `qualify_resolved` reduces to identity
    // for both branches, matching `module_qualify(&entry, name) == name`.
    ctx.current_module = ModulePath::entry();
    assert_eq!(ctx.qualify_resolved("Foo").as_ref(), "Foo");
    assert_eq!(ctx.qualify_resolved("lib::Color").as_ref(), "lib::Color");
}

/// `lower_modules` on a single-element (entry-only) input must produce
/// the same `function_index` shape as the single-file `lower` API. Pins
/// the contract that `module_qualify(&entry, name) == name` keeps
/// existing snapshot/IR tests stable as the driver is migrated to the
/// multi-module path.
#[test]
fn lower_modules_single_entry_uses_bare_keys() {
    let src = "function helper() -> Int { 42 }\n\
               function main() -> Int { helper() }";
    let entry = make_module(ModulePath::entry(), src, SourceId(0), true);
    let modules = vec![entry];
    let analysis = checker::check_modules(&modules);
    assert!(analysis.diagnostics.is_empty());

    let ir_module = lower_modules(&modules, &analysis.module);
    assert!(ir_module.function_index.contains_key("helper"));
    assert!(ir_module.function_index.contains_key("main"));
    assert!(
        !ir_module.function_index.keys().any(|k| k.contains("::")),
        "entry-only lowering must not produce any qualified keys; have: {:?}",
        ir_module.function_index.keys().collect::<Vec<_>>()
    );
}

/// Default-argument wrapper synthesis.
///
/// A function with a non-literal default expression must:
/// 1. Get an entry in `default_wrapper_index` for that param slot.
/// 2. Have a synthesized wrapper function appended past
///    `synthesized_start`, named `__default_fn<FID>_<callee>_<idx>`.
/// 3. Have any caller's omitted-arg slot rewritten to a zero-arg
///    `Op::Call(wrapper_id, [], [])` instead of an inlined default
///    expression.
#[test]
fn default_wrapper_synthesized_for_non_literal_default() {
    let src = "function helper() -> Int { 42 }\n\
               function f(x: Int = helper()) -> Int { x }\n\
               function main() -> Int { f() }";
    let module = lower_source(src);

    // Sema's FuncId for `f` matches the IR's function_index key.
    let f_id = *module
        .function_index
        .get("f")
        .expect("`f` should be in function_index");

    // (1) The wrapper index has an entry for (f, slot 0).
    let wrapper_id = *module
        .default_wrapper_index
        .get(&(f_id, 0))
        .expect("default_wrapper_index should contain (f_id, 0)");

    // (2) The wrapper exists in the function table, named per the
    //     `__default_fn{FID}_<callee>_<slot>` convention (the FID
    //     prefix disambiguates against method-default wrappers), and
    //     was appended past `synthesized_start` (no FuncId clash with
    //     sema-pre-allocated ids).
    let wrapper = module.functions[wrapper_id.index()].func();
    assert_eq!(wrapper.name, format!("__default_fn{}_f_0", f_id.0));
    assert!(wrapper_id.0 >= module.synthesized_start);
    assert!(wrapper.param_types.is_empty());
    assert_eq!(wrapper.return_type, IrType::I64);

    // (3) `main`'s body calls `f` with one argument — the wrapper —
    //     not an inlined `helper()` call. Inspect ops to confirm.
    let main_id = *module
        .function_index
        .get("main")
        .expect("`main` should be in function_index");
    let main_fn = module.functions[main_id.index()].func();
    let mut saw_wrapper_call = false;
    let mut saw_f_call = false;
    for block in &main_fn.blocks {
        for instr in &block.instructions {
            match &instr.op {
                Op::Call(callee, _, args) if *callee == wrapper_id => {
                    saw_wrapper_call = true;
                    assert!(args.is_empty(), "wrapper call must take no args");
                }
                Op::Call(callee, _, args) if *callee == f_id => {
                    saw_f_call = true;
                    assert_eq!(
                        args.len(),
                        1,
                        "f's call site must pass one arg (the wrapper result)"
                    );
                }
                _ => {}
            }
        }
    }
    assert!(saw_wrapper_call, "main's body must call the wrapper");
    assert!(saw_f_call, "main's body must call f");
}

/// Pure-literal defaults must NOT trigger wrapper synthesis — the
/// inline-default path is the legacy behavior and stays the default
/// (cheaper, no extra function hop). Also verifies that the inlined
/// path materializes the literal as an `IConst` directly at the call
/// site (no `Op::Call` to a wrapper sneaks in).
#[test]
fn pure_literal_default_does_not_synthesize_wrapper() {
    let src = "function f(x: Int = 1) -> Int { x }\n\
               function main() -> Int { f() }";
    let module = lower_source(src);
    assert!(
        module.default_wrapper_index.is_empty(),
        "pure-literal defaults must not synthesize wrappers; got: {:?}",
        module.default_wrapper_index
    );
    // Strong guard: no IR function at all is a `__default_*` wrapper.
    // (`function_index` never gets wrappers regardless, so this checks
    // the function table itself.)
    assert!(
        !module
            .functions
            .iter()
            .any(|s| s.func().name.starts_with("__default_")),
        "no `__default_*` IrFunction should be appended for literal-only defaults"
    );
    // Function-table size matches sema's pre-allocated callable count
    // (no synthesized wrappers were appended). NOTE: this equality only
    // holds because the fixture has no other source of synthesized
    // functions (no closures, no monomorphized specializations). If
    // the fixture is ever extended with a generic call site or a
    // closure, switch to checking that no function past
    // `synthesized_start` has a `__default_*` name rather than
    // counting them.
    assert_eq!(
        module.functions.len() as u32,
        module.synthesized_start,
        "no functions should be appended past `synthesized_start` for literal-only defaults"
    );

    // `main` must call `f` with one arg, and that arg must come from
    // an inlined `ConstI64 1` in `main` itself — not a Call to anything.
    let f_id = *module.function_index.get("f").expect("f in function_index");
    let main_id = *module
        .function_index
        .get("main")
        .expect("main in function_index");
    let main_fn = module.functions[main_id.index()].func();
    let mut saw_const_one = false;
    let mut saw_f_call_with_one_arg = false;
    let mut other_calls = 0usize;
    for block in &main_fn.blocks {
        for instr in &block.instructions {
            match &instr.op {
                Op::ConstI64(1) => saw_const_one = true,
                Op::Call(callee, _, args) if *callee == f_id => {
                    saw_f_call_with_one_arg = args.len() == 1;
                }
                Op::Call(_, _, _) => other_calls += 1,
                _ => {}
            }
        }
    }
    assert!(
        saw_const_one,
        "main must materialize the inlined default as `ConstI64 1`"
    );
    assert!(
        saw_f_call_with_one_arg,
        "main must call f with exactly one argument (the inlined default)"
    );
    assert_eq!(
        other_calls, 0,
        "no other Call ops should appear in main — the default must be inlined, not wrapper-called"
    );
}

/// Built-in methods (`Option.unwrap`, `Option.unwrapOr`, …) carry
/// `func_id = None` in `MethodInfo` because the Cranelift backend
/// inlines them. The `assemble_call_args` wrapper-index probe is
/// `callee_id.and_then(...)` so `None` correctly skips it. No built-in
/// has a default today, but guard the lowering path — a regression
/// that crashed `assemble_call_args` for `callee_id = None` would
/// silently break every built-in call.
#[test]
fn builtin_method_call_lowers_without_wrapper_probe() {
    let src = "function main() -> Int {\n\
                   let x: Option<Int> = Some(5)\n\
                   x.unwrapOr(0)\n\
               }";
    let module = lower_source(src);
    assert!(
        module.default_wrapper_index.is_empty(),
        "built-in method calls should not synthesize wrappers; got: {:?}",
        module.default_wrapper_index
    );
    assert_eq!(
        module.functions.len() as u32,
        module.synthesized_start,
        "no functions should be appended past `synthesized_start` for a program \
         that only calls built-in methods"
    );
}

/// Chained defaults: `f(x = helper())` and `g(y = f())` both need
/// wrapping. Wrapper `W_g` is synthesized in `g`'s scope and its body
/// lowers `f()` — that call site must consult `default_wrapper_index`
/// and emit `Op::Call(W_f, [], [])` instead of inlining `helper()`.
/// This is the regression test for the wrapper-ordering bug fixed by
/// splitting synthesis into a register-all-then-lower-all two-pass.
#[test]
fn chained_default_wrappers_call_each_other_not_inlined() {
    let src = "function helper() -> Int { 42 }\n\
               function f(x: Int = helper()) -> Int { x }\n\
               function g(y: Int = f()) -> Int { y }\n\
               function main() -> Int { g() }";
    let module = lower_source(src);

    let helper_id = *module.function_index.get("helper").unwrap();
    let f_id = *module.function_index.get("f").unwrap();
    let g_id = *module.function_index.get("g").unwrap();
    let w_f = *module
        .default_wrapper_index
        .get(&(f_id, 0))
        .expect("wrapper for f's default");
    let w_g = *module
        .default_wrapper_index
        .get(&(g_id, 0))
        .expect("wrapper for g's default");

    // W_g's body lowers `f()` with one missing default. The wrapper
    // index already contains `(f_id, 0) -> w_f` by the time W_g is
    // lowered (Pass A registered every entry up front), so W_g must
    // emit `Op::Call(w_f, [], [])` to fill the slot — not an inlined
    // `helper()` call.
    let w_g_fn = module.functions[w_g.index()].func();
    let mut saw_wf_call = false;
    let mut saw_helper_call = false;
    let mut saw_f_call = false;
    for block in &w_g_fn.blocks {
        for instr in &block.instructions {
            if let Op::Call(callee, _, args) = &instr.op {
                if *callee == w_f && args.is_empty() {
                    saw_wf_call = true;
                } else if *callee == helper_id {
                    saw_helper_call = true;
                } else if *callee == f_id {
                    saw_f_call = true;
                }
            }
        }
    }
    assert!(
        saw_wf_call,
        "W_g must call W_f (the wrapper for f's default), not inline helper()"
    );
    assert!(
        !saw_helper_call,
        "W_g must NOT directly call helper() — that would defeat the privacy guarantee"
    );
    assert!(
        saw_f_call,
        "W_g must still call f itself (the outer call wrapped by W_g)"
    );
}

/// Method default referencing a free function generates a wrapper
/// under the `__default_m{FID}_{Type}__{method}_{slot}` naming scheme,
/// and a method-call site with a missing default routes through that
/// wrapper.
#[test]
fn method_default_wrapper_synthesized() {
    // Just declare; no call site needed to verify wrapper synthesis.
    // (A method call site is exercised separately by the
    // multi-module privacy test below.)
    let src = "function helper() -> Int { 7 }\n\
               struct Counter { Int n }\n\
               impl Counter {\n\
                   public function bump(self, by: Int = helper()) -> Int { self.n + by }\n\
               }\n\
               function main() {}";
    let module = lower_source(src);

    // Method FuncId comes from `method_index`; `bump` takes self at
    // slot 0 in the IR-level signature but the user-visible default
    // is at slot 0 of the `params` vector (self excluded), which is
    // the slot key used in `default_wrapper_index`.
    let bump_id = *module
        .method_index
        .get(&("Counter".to_string(), "bump".to_string()))
        .expect("Counter.bump in method_index");
    let wrapper_id = *module
        .default_wrapper_index
        .get(&(bump_id, 0))
        .expect("wrapper for Counter.bump's `by` default");

    let wrapper = module.functions[wrapper_id.index()].func();
    assert_eq!(
        wrapper.name,
        format!("__default_m{}_Counter__bump_0", bump_id.0),
        "method-default wrapper must use the `__default_m{{FID}}_{{Type}}__{{method}}_{{slot}}` naming"
    );
    assert!(wrapper.param_types.is_empty());
    assert_eq!(wrapper.return_type, IrType::I64);

    // Wrapper body resolves `helper` in the same scope where bump
    // was declared, so it must contain a direct Call to helper.
    let helper_id = *module.function_index.get("helper").unwrap();
    let wrapper_calls_helper = wrapper
        .blocks
        .iter()
        .flat_map(|b| &b.instructions)
        .any(|i| matches!(&i.op, Op::Call(target, _, _) if *target == helper_id));
    assert!(
        wrapper_calls_helper,
        "wrapper body must call `helper` — the resolution that the wrapper preserves in the callee's scope"
    );
}

/// Multi-module wrapper synthesis: when a non-entry module has a
/// function whose default references a private helper in the same
/// module, the wrapper is synthesized in that module's scope (so the
/// private symbol resolves against `lib`'s function table) and the
/// `default_wrapper_index` entry is keyed by the qualified callee
/// FuncId. A call site within `lib` that omits the defaulted arg
/// routes through the wrapper rather than inlining `_secret`, so
/// `_secret` is never duplicated into a foreign caller's IR.
#[test]
fn multi_module_default_wrapper_routes_through_wrapper() {
    // Entry is intentionally trivial — we exercise the multi-module
    // lowering path (`lower_modules`) and verify wrapper synthesis
    // works when the defaulted callee lives in a non-entry module.
    let entry = make_module(ModulePath::entry(), "function main() {}", SourceId(0), true);
    let lib = make_module(
        ModulePath(vec!["lib".to_string()]),
        "function _secret() -> Int { 99 }\n\
         public function greet(x: Int = _secret()) -> Int { x }\n\
         public function caller() -> Int { greet() }",
        SourceId(1),
        false,
    );

    let modules = vec![entry, lib];
    let analysis = checker::check_modules(&modules);
    assert!(
        analysis.diagnostics.is_empty(),
        "sema errors: {:?}",
        analysis
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );

    let ir_module = lower_modules(&modules, &analysis.module);

    let greet_id = *ir_module
        .function_index
        .get("lib::greet")
        .expect("lib::greet should be qualified in function_index");
    let secret_id = *ir_module
        .function_index
        .get("lib::_secret")
        .expect("lib::_secret should be in function_index (private but registered)");
    let caller_id = *ir_module
        .function_index
        .get("lib::caller")
        .expect("lib::caller should be in function_index");
    let wrapper_id = *ir_module
        .default_wrapper_index
        .get(&(greet_id, 0))
        .expect("wrapper for lib::greet's default must be indexed by greet's qualified FuncId");

    // The wrapper itself must call `_secret` — its body lowers in
    // `lib`'s scope, where `_secret` is visible.
    let wrapper_fn = ir_module.functions[wrapper_id.index()].func();
    let wrapper_calls_secret = wrapper_fn
        .blocks
        .iter()
        .flat_map(|b| &b.instructions)
        .any(|i| matches!(&i.op, Op::Call(target, _, _) if *target == secret_id));
    assert!(
        wrapper_calls_secret,
        "wrapper body must call `lib::_secret` — that's where the private resolution lives"
    );

    // `caller`'s body calls `greet()` with no args; the missing slot
    // must be filled by a call to the wrapper, not by inlining
    // `_secret()` into caller's body. If wrapper synthesis ever
    // regresses to the inline-default path, `caller` would directly
    // call `_secret` — that's exactly what we're guarding against.
    let caller_fn = ir_module.functions[caller_id.index()].func();
    let mut saw_wrapper_call = false;
    let mut caller_calls_secret = false;
    for block in &caller_fn.blocks {
        for instr in &block.instructions {
            if let Op::Call(callee, _, args) = &instr.op {
                if *callee == wrapper_id && args.is_empty() {
                    saw_wrapper_call = true;
                } else if *callee == secret_id {
                    caller_calls_secret = true;
                }
            }
        }
    }
    assert!(
        saw_wrapper_call,
        "lib::caller must fill greet's missing default by calling the wrapper"
    );
    assert!(
        !caller_calls_secret,
        "lib::caller must not directly call `_secret` — wrapper synthesis exists to keep that resolution \
         in lib's scope, so foreign callers (entry, future imports) never see the private symbol"
    );

    // Privacy property end-to-end: across the *entire* IR module
    // (every function in `ir_module.functions`, not just `caller`),
    // the only direct call to `_secret` lives inside the wrapper.
    // This is the load-bearing guarantee of the wrapper-synthesis
    // design — if any caller (in `lib`, in `entry`, or any future
    // module) ever inlines `_secret` into its own body, this count
    // increases and the test fails. Until cross-module call sites
    // can be exercised from this harness, this all-functions sweep
    // is the strongest privacy assertion available.
    let direct_secret_calls: usize = ir_module
        .functions
        .iter()
        .flat_map(|slot| slot.func().blocks.iter())
        .flat_map(|b| &b.instructions)
        .filter(|i| matches!(&i.op, Op::Call(target, _, _) if *target == secret_id))
        .count();
    assert_eq!(
        direct_secret_calls, 1,
        "expected exactly one direct call to `lib::_secret` across the whole IR module \
         (the one inside the wrapper); found {direct_secret_calls}. \
         Anything more means the inline-default path leaked the private symbol into a \
         non-wrapper function."
    );
}

/// A closure-valued default — `function f(cb: (Int) -> Int = function(x:
/// Int) -> Int { x * 2 }) -> Int { cb(10) }` — exercises a corner of
/// the wrapper synthesis path that pure-literal and free-function
/// defaults don't: the wrapper body itself appends a *new* closure
/// `IrFunction` to the module while lowering the default expression.
/// `with_synthetic_function` deliberately does *not* snapshot
/// `closure_counter` (so closure names stay globally unique across
/// the wrapper-pass / user-pass boundary), and a regression that
/// reset or shared the counter would surface here as either a name
/// collision or an unreachable closure body.
#[test]
fn closure_default_wrapper_synthesizes_and_lowers_closure() {
    let src = "function f(cb: (Int) -> Int = function(x: Int) -> Int { x * 2 }) -> Int { cb(10) }\n\
               function main() -> Int { f() }";
    let module = lower_source(src);

    let f_id = *module
        .function_index
        .get("f")
        .expect("`f` should be in function_index");
    let wrapper_id = *module
        .default_wrapper_index
        .get(&(f_id, 0))
        .expect("closure-valued default must synthesize a wrapper");

    // The wrapper exists, lives past `synthesized_start`, and its body
    // is non-empty — verifying the closure-allocation path inside the
    // wrapper actually ran.
    let wrapper = module.functions[wrapper_id.index()].func();
    assert!(wrapper_id.0 >= module.synthesized_start);
    assert_eq!(wrapper.name, format!("__default_fn{}_f_0", f_id.0));
    assert!(
        !wrapper.blocks.is_empty(),
        "wrapper body must contain at least the entry block + closure-alloc + return"
    );

    // The wrapper body must contain an Op::ClosureAlloc — that's the
    // signature of "the closure literal was lowered into the wrapper."
    // If a future change ever short-circuits closure-default lowering
    // (e.g. emitting a direct Call to the closure body instead of
    // packaging it as a closure value), this assertion would fail.
    let saw_closure_alloc = wrapper
        .blocks
        .iter()
        .flat_map(|b| &b.instructions)
        .any(|i| matches!(&i.op, Op::ClosureAlloc(_, _)));
    assert!(
        saw_closure_alloc,
        "wrapper body must contain an Op::ClosureAlloc — the closure literal must lower in the wrapper"
    );

    // The closure function itself must be appended past
    // `synthesized_start` — like the wrapper, it's a synthesized
    // callable. Closure names are unique because `closure_counter`
    // is shared (not snapshotted by `with_synthetic_function`); a
    // collision here would indicate the counter was reset on
    // wrapper-pass entry.
    let closure_fns: Vec<&str> = module
        .functions
        .iter()
        .map(|s| s.func().name.as_str())
        .filter(|n| n.starts_with("closure_") || n.starts_with("__closure"))
        .collect();
    assert!(
        !closure_fns.is_empty(),
        "the closure literal in the default must produce a synthesized closure function; \
         got module functions: {:?}",
        module
            .functions
            .iter()
            .map(|s| s.func().name.clone())
            .collect::<Vec<_>>()
    );

    // `main` calls the wrapper, never the inner closure body directly.
    let main_id = *module.function_index.get("main").unwrap();
    let main_fn = module.functions[main_id.index()].func();
    let saw_wrapper_call = main_fn
        .blocks
        .iter()
        .flat_map(|b| &b.instructions)
        .any(|i| matches!(&i.op, Op::Call(t, _, args) if *t == wrapper_id && args.is_empty()));
    assert!(
        saw_wrapper_call,
        "main must fill f's missing default by calling the closure-valued wrapper"
    );
}
