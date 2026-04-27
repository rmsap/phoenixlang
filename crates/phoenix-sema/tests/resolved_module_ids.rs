//! Integration tests for [`phoenix_sema::ResolvedModule`] lookup
//! helpers and id-allocation contracts.
//!
//! Migrated out of `crates/phoenix-sema/src/checker_tests.rs` to keep
//! that already-large unit-test module focused on type-checking
//! diagnostics.  These tests exercise the public id surface
//! (`*_id`, `*_info_by_name`, `*_with_names`) and the sema-side
//! invariants of the resolved schema.

use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::Analysis;
use phoenix_sema::checker::check;
use phoenix_sema::types::Type;

fn check_full(source: &str) -> Analysis {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    check(&program)
}

#[test]
fn function_id_returns_none_for_unknown_name() {
    let result = check_full("function main() { }");
    assert!(result.module.function_id("nonexistent").is_none());
    assert!(result.module.function_info_by_name("nonexistent").is_none());
}

#[test]
fn struct_enum_trait_id_return_none_for_unknown_names() {
    let result = check_full("function main() { }");
    assert!(result.module.struct_id("NoSuchStruct").is_none());
    assert!(result.module.enum_id("NoSuchEnum").is_none());
    assert!(result.module.trait_id("NoSuchTrait").is_none());
    assert!(result.module.struct_info_by_name("NoSuchStruct").is_none());
    assert!(result.module.enum_info_by_name("NoSuchEnum").is_none());
    assert!(result.module.trait_info_by_name("NoSuchTrait").is_none());
}

#[test]
fn method_lookup_returns_none_for_unknown_pair() {
    let result = check_full(
        r#"
struct Counter {
    Int v
    function get(self) -> Int { return self.v }
}
function main() { }
"#,
    );
    assert!(result.module.method_func_id("Counter", "missing").is_none());
    assert!(
        result
            .module
            .method_info_by_name("Counter", "missing")
            .is_none()
    );
    assert!(result.module.method_func_id("NoSuchType", "get").is_none());
}

#[test]
fn method_lookup_finds_user_methods_via_func_id() {
    let result = check_full(
        r#"
struct Counter {
    Int v
    function get(self) -> Int { return self.v }
}
function main() { }
"#,
    );
    let fid = result
        .module
        .method_func_id("Counter", "get")
        .expect("user method");
    let info = result.module.user_method(fid);
    assert_eq!(info.return_type, Type::Int);
    let by_name = result.module.method_info_by_name("Counter", "get").unwrap();
    assert_eq!(by_name.return_type, info.return_type);
}

#[test]
fn method_lookup_falls_back_to_builtin_methods() {
    let result = check_full("function main() { }");
    let unwrap = result
        .module
        .method_info_by_name("Option", "unwrap")
        .expect("builtin");
    assert!(unwrap.func_id.is_none(), "builtin must not have a FuncId");
    assert!(
        result.module.method_func_id("Option", "unwrap").is_none(),
        "method_func_id must skip builtins"
    );
}

#[test]
fn function_func_ids_are_dense_and_in_declaration_order() {
    let result = check_full(
        r#"
function alpha() { }
function beta() { }
function gamma() { }
"#,
    );
    let alpha = result.module.function_id("alpha").unwrap();
    let beta = result.module.function_id("beta").unwrap();
    let gamma = result.module.function_id("gamma").unwrap();
    assert_eq!(alpha.0, 0, "alpha is the first declaration");
    assert_eq!(beta.0, 1);
    assert_eq!(gamma.0, 2);
    assert_eq!(result.module.function(alpha).param_names.len(), 0);
    assert_eq!(result.module.functions.len(), 3);
    assert_eq!(
        result.module.user_method_offset, 3,
        "methods start past free funcs"
    );
}

#[test]
fn user_methods_occupy_contiguous_func_ids_after_free_functions() {
    let result = check_full(
        r#"
function alpha() { }
struct Counter {
    Int v
    function tick(self) -> Int { return self.v + 1 }
}
function beta() { }
"#,
    );
    let alpha = result.module.function_id("alpha").unwrap();
    let beta = result.module.function_id("beta").unwrap();
    let tick = result.module.method_func_id("Counter", "tick").unwrap();
    assert_eq!(alpha.0, 0);
    assert_eq!(beta.0, 1);
    assert_eq!(
        tick.0, result.module.user_method_offset,
        "user methods start at user_method_offset"
    );
    assert_eq!(result.module.user_methods.len(), 1);
    let info = result.module.user_method(tick);
    assert_eq!(info.func_id, Some(tick));
}

#[test]
fn enum_id_zero_one_pinned_to_option_result_builtins() {
    let result = check_full("function main() { }");
    let option_id = result
        .module
        .enum_id("Option")
        .expect("Option pre-registered");
    let result_id = result
        .module
        .enum_id("Result")
        .expect("Result pre-registered");
    assert_eq!(option_id.0, 0, "Option must be EnumId(0)");
    assert_eq!(result_id.0, 1, "Result must be EnumId(1)");
}

#[test]
fn traits_with_names_iterates_in_trait_id_order() {
    let result = check_full(
        r#"
trait Alpha { }
trait Beta { }
trait Gamma { }
function main() { }
"#,
    );
    let names: Vec<&str> = result.module.traits_with_names().map(|(n, _)| n).collect();
    assert_eq!(names, vec!["Alpha", "Beta", "Gamma"]);
}

#[test]
fn func_ids_stable_across_repeated_check_calls() {
    let source = r#"
struct Dog { Int age }
struct Cat { Int age }
enum Mood {
    Happy
    Grumpy
}
function alpha() { }
function beta() { }
function main() { }
"#;
    let r1 = check_full(source);
    let r2 = check_full(source);
    assert_eq!(
        r1.module.function_id("alpha"),
        r2.module.function_id("alpha")
    );
    assert_eq!(r1.module.function_id("beta"), r2.module.function_id("beta"));
    assert_eq!(r1.module.struct_id("Dog"), r2.module.struct_id("Dog"));
    assert_eq!(r1.module.struct_id("Cat"), r2.module.struct_id("Cat"));
    assert_eq!(r1.module.enum_id("Mood"), r2.module.enum_id("Mood"));
    assert_eq!(r1.module.user_method_offset, r2.module.user_method_offset);
}

#[test]
fn user_method_ids_with_multiple_impl_blocks_same_type() {
    let result = check_full(
        r#"
struct Counter { Int v }
impl Counter { function inc(self) -> Int { return self.v + 1 } }
impl Counter { function dec(self) -> Int { return self.v - 1 } }
function main() { }
"#,
    );
    let inc = result.module.method_func_id("Counter", "inc").unwrap();
    let dec = result.module.method_func_id("Counter", "dec").unwrap();
    assert_eq!(
        inc.0, result.module.user_method_offset,
        "first method takes the first user-method id"
    );
    assert_eq!(
        dec.0,
        inc.0 + 1,
        "second method follows immediately in source order"
    );
    assert_eq!(result.module.user_methods.len(), 2);
}

#[test]
fn user_method_ids_with_inline_methods_then_standalone_impl() {
    let result = check_full(
        r#"
struct Counter {
    Int v
    function get(self) -> Int { return self.v }
}
impl Counter { function bump(self) -> Int { return self.v + 1 } }
function main() { }
"#,
    );
    let get = result.module.method_func_id("Counter", "get").unwrap();
    let bump = result.module.method_func_id("Counter", "bump").unwrap();
    assert_eq!(get.0, result.module.user_method_offset);
    assert_eq!(bump.0, get.0 + 1);
}

#[test]
fn enum_with_inherent_methods_allocates_user_method_ids() {
    // Pre-pass B has a parallel walk for `Declaration::Enum` that
    // covers inline methods and trait impls — independent from the
    // struct branch.  Pin both inherent and contiguous-id contracts.
    let result = check_full(
        r#"
enum Color {
    Red
    Green
    Blue

    function isRed(self) -> Bool { return false }
    function name(self) -> String { return "color" }
}
function main() { }
"#,
    );
    let is_red = result.module.method_func_id("Color", "isRed").unwrap();
    let name = result.module.method_func_id("Color", "name").unwrap();
    assert_eq!(
        is_red.0, result.module.user_method_offset,
        "first enum method lands at user_method_offset"
    );
    assert_eq!(
        name.0,
        is_red.0 + 1,
        "second enum method follows immediately"
    );
    assert_eq!(result.module.user_methods.len(), 2);
    let info = result.module.user_method(is_red);
    assert!(
        info.has_self,
        "enum method declared with `self` must record has_self"
    );
}

#[test]
fn traits_with_names_repeatable() {
    let result = check_full(
        r#"
trait Alpha { }
trait Beta { }
function main() { }
"#,
    );
    let first: Vec<&str> = result.module.traits_with_names().map(|(n, _)| n).collect();
    let second: Vec<&str> = result.module.traits_with_names().map(|(n, _)| n).collect();
    assert_eq!(first, second);
}

#[test]
fn user_method_index_round_trip() {
    let result = check_full(
        r#"
struct Counter {
    Int v
    function inc(self) -> Int { return self.v + 1 }
}
function main() { }
"#,
    );
    let inc = result.module.method_func_id("Counter", "inc").unwrap();
    let idx = result
        .module
        .user_method_index(inc)
        .expect("user method id");
    assert_eq!(idx, 0, "first user method lives at user_methods[0]");
    let main = result.module.function_id("main").unwrap();
    assert_eq!(result.module.user_method_index(main), None);
}

#[test]
fn method_info_by_name_user_shadows_would_be_builtin() {
    let result = check_full("function main() { }");
    let info = result
        .module
        .method_info_by_name("Option", "unwrap")
        .unwrap();
    assert!(
        info.func_id.is_none(),
        "built-in path produces None func_id, so any user method (Some) shadows it"
    );
}

#[test]
fn builtin_method_emission_canonical_order_pinned() {
    let result = check_full("function main() { }");
    for ty in ["Option", "Result"] {
        assert!(
            result.module.builtin_methods.contains_key(ty),
            "expected built-in methods registered for {ty}"
        );
    }
    assert!(
        result
            .module
            .method_info_by_name("Option", "unwrap")
            .is_some(),
        "Option.unwrap must be a built-in method"
    );
    assert!(
        result
            .module
            .method_info_by_name("Result", "unwrap")
            .is_some(),
        "Result.unwrap must be a built-in method"
    );
}

#[test]
fn duplicate_function_emits_diagnostic_and_keeps_first_id_stable() {
    let tokens = tokenize(
        r#"
function dup() -> Int { return 1 }
function dup() -> Bool { return true }
function main() { }
"#,
        SourceId(0),
    );
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let result = check(&program);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("`dup` is already defined")),
        "expected duplicate-name diagnostic, got: {:?}",
        result.diagnostics
    );
    let dup_id = result.module.function_id("dup").expect("first dup wins");
    let info = result.module.function(dup_id);
    assert_eq!(
        info.return_type,
        Type::Int,
        "first declaration's signature must survive"
    );
    assert_eq!(result.module.functions.len(), 2);
    let main_id = result.module.function_id("main").expect("main");
    assert_eq!(main_id.0, 1);
}

#[test]
fn duplicate_method_emits_diagnostic_and_keeps_first_definition() {
    // Two methods with the same `(type, method)` pair: the second
    // is rejected by `register_impl` with a duplicate-method
    // diagnostic, and the first survives in the resolved module.
    // The dedup contract here is what `build_from_checker`'s
    // `slot.is_none()` assertion relies on — this test pins it.
    let tokens = tokenize(
        r#"
struct Counter {
    Int v
    function get(self) -> Int { return self.v }
    function get(self) -> Bool { return true }
}
function main() { }
"#,
        SourceId(0),
    );
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let result = check(&program);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("`get` is already defined")),
        "expected duplicate-method diagnostic, got: {:?}",
        result.diagnostics
    );
    let get = result
        .module
        .method_func_id("Counter", "get")
        .expect("first get survives");
    let info = result.module.user_method(get);
    assert_eq!(
        info.return_type,
        Type::Int,
        "first declaration's signature must survive"
    );
    assert_eq!(
        result.module.user_methods.len(),
        1,
        "duplicate method must not occupy a second slot"
    );
}

#[test]
fn duplicate_method_across_impl_blocks_emits_diagnostic() {
    // Same dedup contract, this time with two separate `impl Counter`
    // blocks contributing the same method name.  Pre-pass B dedupes
    // the FuncId allocation; `register_impl` emits the diagnostic.
    let tokens = tokenize(
        r#"
struct Counter { Int v }
impl Counter { function bump(self) -> Int { return self.v + 1 } }
impl Counter { function bump(self) -> Int { return self.v + 2 } }
function main() { }
"#,
        SourceId(0),
    );
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let result = check(&program);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("`bump` is already defined")),
        "expected duplicate-method diagnostic, got: {:?}",
        result.diagnostics
    );
    assert_eq!(result.module.user_methods.len(), 1);
}

#[test]
fn functions_with_names_round_trips_through_function_by_name() {
    let result = check_full(
        r#"
function alpha() { }
function beta(x: Int) -> Int { return x }
function gamma() { }
"#,
    );
    for (name, fid, _info) in result.module.functions_with_names() {
        assert_eq!(
            result.module.function_by_name.get(name).copied(),
            Some(fid),
            "functions_with_names emitted ({name}, {fid}) but function_by_name disagrees"
        );
    }
    let emitted: std::collections::HashSet<&str> = result
        .module
        .functions_with_names()
        .map(|(n, _, _)| n)
        .collect();
    for name in result.module.function_by_name.keys() {
        assert!(
            emitted.contains(name.as_str()),
            "functions_with_names omitted {name}"
        );
    }
}

#[test]
fn user_methods_with_names_round_trips_through_method_index() {
    let result = check_full(
        r#"
struct Counter {
    Int v
    function get(self) -> Int { return self.v }
    function bump(self) -> Int { return self.v + 1 }
}
function main() { }
"#,
    );
    for ((tn, mn), fid, _info) in result.module.user_methods_with_names() {
        assert_eq!(
            result.module.method_func_id(tn, mn),
            Some(fid),
            "user_methods_with_names emitted ({tn}.{mn}, {fid}) but method_func_id disagrees"
        );
    }
}

#[test]
fn structs_with_names_and_enums_with_names_round_trip() {
    let result = check_full(
        r#"
struct Dog { Int age }
struct Cat { Int age }
enum Mood {
    Happy
    Grumpy
}
function main() { }
"#,
    );
    for (name, sid, _) in result.module.structs_with_names() {
        assert_eq!(result.module.struct_by_name.get(name).copied(), Some(sid));
    }
    for (name, eid, _) in result.module.enums_with_names() {
        assert_eq!(result.module.enum_by_name.get(name).copied(), Some(eid));
    }
}

#[test]
fn build_from_checker_invariants_hold_for_realistic_program() {
    let result = check_full(
        r#"
trait Greet { function hi(self) -> String }
struct Dog {
    String name
    impl Greet { function hi(self) -> String { return self.name } }
}
struct Cat {
    String name
    function meow(self) -> String { return self.name }
}
enum Mood {
    Happy
    Grumpy
}
function alpha() { }
function beta(x: Int) -> Int { return x }
function main() { }
"#,
    );
    let m = &result.module;
    assert_eq!(m.functions.len(), m.function_by_name.len());
    assert_eq!(m.structs.len(), m.struct_by_name.len());
    assert_eq!(m.enums.len(), m.enum_by_name.len());
    assert_eq!(m.traits.len(), m.trait_by_name.len());
    let method_count: usize = m.method_index.values().map(|x| x.len()).sum();
    assert_eq!(m.user_methods.len(), method_count);
    assert_eq!(m.user_method_offset as usize, m.functions.len());
    assert_eq!(
        m.enum_id("Option"),
        Some(phoenix_common::ids::OPTION_ENUM_ID)
    );
    assert_eq!(
        m.enum_id("Result"),
        Some(phoenix_common::ids::RESULT_ENUM_ID)
    );
    let mood = m.enum_id("Mood").expect("Mood enum");
    assert_eq!(mood, phoenix_common::ids::FIRST_USER_ENUM_ID);
}
