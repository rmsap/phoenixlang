mod common;
use common::*;

#[test]
fn list_basics() {
    run_expect(
        r#"
function main() {
  let nums: List<Int> = [10, 20, 30]
  print(nums.length())
  print(nums.get(0))
  print(nums.get(2))
  let nums2: List<Int> = nums.push(40)
  print(nums2.length())

  let names: List<String> = ["alice", "bob"]
  print(names.get(1))

  let empty: List<Int> = []
  print(empty.length())

  let mut sum: Int = 0
  let mut i: Int = 0
  while i < nums.length() {
    sum = sum + nums.get(i)
    i = i + 1
  }
  print(sum)
}
"#,
        &["3", "10", "30", "4", "bob", "0", "60"],
    );
}

#[test]
fn list_type_errors() {
    expect_type_error(
        "function main() { let nums: List<Int> = [1, \"hello\"] }",
        "list element type mismatch",
    );
}

// --- Collections (List<T>) expanded coverage ---

#[test]
fn list_push_returns_new_list() {
    run_expect(
        r#"
function main() {
    let a: List<Int> = [1, 2]
    let b: List<Int> = a.push(3)
    print(a.length())
    print(b.length())
}
"#,
        &["2", "3"],
    );
}

#[test]
fn list_get_out_of_bounds() {
    expect_runtime_error(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3]
    print(nums.get(5))
}
"#,
        "index 5 out of bounds",
    );
}

#[test]
fn list_get_negative_index() {
    expect_runtime_error(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3]
    print(nums.get(-1))
}
"#,
        "index -1 out of bounds",
    );
}

#[test]
fn list_empty_operations() {
    run_expect(
        r#"
function main() {
    let empty: List<Int> = []
    print(empty.length())
    let withOne: List<Int> = empty.push(42)
    print(withOne.length())
    print(withOne.get(0))
}
"#,
        &["0", "1", "42"],
    );
}

#[test]
fn list_push_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    let nums: List<Int> = [1, 2]
    print(nums.push(3, 4))
}
"#,
        "takes 1 argument(s), got 2",
    );
}

#[test]
fn list_get_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    let nums: List<Int> = [1, 2]
    print(nums.get(0, 1))
}
"#,
        "takes 1 argument(s), got 2",
    );
}

#[test]
fn list_unknown_method() {
    expect_type_error(
        r#"
function main() {
    let nums: List<Int> = [1, 2]
    print(nums.pop())
}
"#,
        "no method `pop` on type `List`",
    );
}

#[test]
fn list_nested_operations() {
    run_expect(
        r#"
function main() {
    let a: List<Int> = [10, 20, 30]
    let b: List<Int> = a.push(40)
    print(b.get(3))
    print(b.length())
}
"#,
        &["40", "4"],
    );
}

// ── List Edge Cases ───────────────────────────────────────────────────

#[test]
fn list_of_structs() {
    run_expect(
        r#"
struct Point { Int x  Int y }
function main() {
    let points: List<Point> = [Point(1, 2), Point(3, 4)]
    print(points.length())
    let first: Point = points.get(0)
    print(first.x)
    print(first.y)
}
"#,
        &["2", "1", "2"],
    );
}

#[test]
fn list_of_enums() {
    run_expect(
        r#"
function main() {
    let opts: List<Option<Int>> = [Some(1), None, Some(3)]
    print(opts.length())
    print(opts.get(0).unwrap())
    print(opts.get(1).isNone())
}
"#,
        &["3", "1", "true"],
    );
}

#[test]
fn list_push_type_mismatch() {
    expect_type_error(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3]
    let bad: List<Int> = nums.push("hello")
}
"#,
        "expected Int but got String",
    );
}

#[test]
fn nested_list() {
    run_expect(
        r#"
function main() {
    let matrix: List<List<Int>> = [[1, 2], [3, 4]]
    print(matrix.length())
    let row: List<Int> = matrix.get(0)
    print(row.get(1))
}
"#,
        &["2", "2"],
    );
}

#[test]
fn list_of_booleans() {
    run_expect(
        r#"
function main() {
    let flags: List<Bool> = [true, false, true]
    print(flags.length())
    print(flags.get(1))
}
"#,
        &["3", "false"],
    );
}

#[test]
fn list_equality() {
    run_expect(
        r#"
function main() {
    let a: List<Int> = [1, 2, 3]
    let b: List<Int> = [1, 2, 3]
    let c: List<Int> = [1, 2, 4]
    print(a == b)
    print(a == c)
}
"#,
        &["true", "false"],
    );
}

#[test]
fn list_in_struct() {
    run_expect(
        r#"
struct Team {
    String name
    List<String> members
}
function main() {
    let t: Team = Team("Phoenix", ["Alice", "Bob", "Charlie"])
    print(t.name)
    print(t.members.length())
    print(t.members.get(1))
}
"#,
        &["Phoenix", "3", "Bob"],
    );
}

#[test]
fn empty_list_type_annotation() {
    run_expect(
        r#"
function main() {
    let empty: List<String> = []
    let withItem: List<String> = empty.push("hello")
    print(withItem.get(0))
}
"#,
        &["hello"],
    );
}

// ── List method argument validation ───────────────────────────────────

#[test]
fn list_length_with_args_error() {
    expect_type_error(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3]
    print(nums.length(42))
}
"#,
        "method `length` takes 0 argument(s)",
    );
}

#[test]
fn list_get_non_int_arg_error() {
    expect_type_error(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3]
    print(nums.get("hello"))
}
"#,
        "expected Int but got String",
    );
}

#[test]
fn output_list_basics() {
    run_expect(
        r#"
function main() {
  let xs: List<Int> = [1, 2, 3]
  print(xs.length())
  print(xs.get(0))
  let ys: List<Int> = xs.push(4)
  print(ys.length())
  print(ys.get(3))
}
"#,
        &["3", "1", "4", "4"],
    );
}

// ── for...in over collections ───────────────────────────────────────────

#[test]
fn for_in_list_literal() {
    run_expect(
        r#"
function main() {
    for x in [1, 2, 3] {
        print(x)
    }
}
"#,
        &["1", "2", "3"],
    );
}

#[test]
fn for_in_list_variable() {
    run_expect(
        r#"
function main() {
    let xs: List<String> = ["a", "b", "c"]
    for s in xs {
        print(s)
    }
}
"#,
        &["a", "b", "c"],
    );
}

#[test]
fn for_in_list_with_break() {
    run_expect(
        r#"
function main() {
    for x in [10, 20, 30, 40] {
        if x == 30 { break }
        print(x)
    }
}
"#,
        &["10", "20"],
    );
}

#[test]
fn for_in_list_with_else() {
    run_expect(
        r#"
function main() {
    for x in [1, 2] {
        print(x)
    } else {
        print("done")
    }
}
"#,
        &["1", "2", "done"],
    );
}

#[test]
fn for_in_empty_list() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = []
    for x in xs {
        print(x)
    } else {
        print("empty")
    }
}
"#,
        &["empty"],
    );
}

// ── Map<K,V> collection ────────────────────────────────────────────────

#[test]
fn map_literal_and_methods() {
    run_expect(
        r#"
function main() {
    let m: Map<String, Int> = {"a": 1, "b": 2, "c": 3}
    print(m.length())
    print(m.contains("a"))
    print(m.contains("z"))
}
"#,
        &["3", "true", "false"],
    );
}

#[test]
fn map_get_returns_option() {
    run_expect(
        r#"
function main() {
    let m: Map<String, Int> = {"x": 42}
    let found: Option<Int> = m.get("x")
    let missing: Option<Int> = m.get("y")
    print(found.unwrap())
    print(missing.isNone())
}
"#,
        &["42", "true"],
    );
}

#[test]
fn map_set_and_remove() {
    run_expect(
        r#"
function main() {
    let m: Map<String, Int> = {"a": 1}
    let m2: Map<String, Int> = m.set("b", 2)
    print(m2.length())
    let m3: Map<String, Int> = m2.remove("a")
    print(m3.length())
    print(m3.get("b").unwrap())
}
"#,
        &["2", "1", "2"],
    );
}

#[test]
fn map_keys_and_values() {
    run_expect(
        r#"
function main() {
    let m: Map<String, Int> = {"x": 10, "y": 20}
    let ks: List<String> = m.keys()
    let vs: List<Int> = m.values()
    print(ks.length())
    print(vs.length())
}
"#,
        &["2", "2"],
    );
}

#[test]
fn map_empty_literal() {
    run_expect(
        r#"
function main() {
    let m: Map<String, Int> = {:}
    print(m.length())
}
"#,
        &["0"],
    );
}

// ── Functional collection methods (Phase 1.9.4) ──

#[test]
fn list_first_last() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    print(xs.first().unwrap())
    print(xs.last().unwrap())
    let empty: List<Int> = []
    print(empty.first().isNone())
}
"#,
        &["1", "3", "true"],
    );
}

#[test]
fn list_contains() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    print(xs.contains(2))
    print(xs.contains(5))
}
"#,
        &["true", "false"],
    );
}

#[test]
fn list_take_drop() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3, 4, 5]
    let taken: List<Int> = xs.take(3)
    let dropped: List<Int> = xs.drop(3)
    print(taken.length())
    print(dropped.length())
    print(taken.get(0))
    print(dropped.get(0))
}
"#,
        &["3", "2", "1", "4"],
    );
}

#[test]
fn list_map() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    let doubled: List<Int> = xs.map(function(x: Int) -> Int { x * 2 })
    print(doubled.get(0))
    print(doubled.get(1))
    print(doubled.get(2))
}
"#,
        &["2", "4", "6"],
    );
}

#[test]
fn list_filter() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3, 4, 5]
    let evens: List<Int> = xs.filter(function(x: Int) -> Bool { x % 2 == 0 })
    print(evens.length())
    print(evens.get(0))
    print(evens.get(1))
}
"#,
        &["2", "2", "4"],
    );
}

#[test]
fn list_find() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3, 4, 5]
    let found: Option<Int> = xs.find(function(x: Int) -> Bool { x > 3 })
    print(found.unwrap())
    let notFound: Option<Int> = xs.find(function(x: Int) -> Bool { x > 10 })
    print(notFound.isNone())
}
"#,
        &["4", "true"],
    );
}

#[test]
fn list_any_all() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    print(xs.any(function(x: Int) -> Bool { x > 2 }))
    print(xs.all(function(x: Int) -> Bool { x > 0 }))
    print(xs.all(function(x: Int) -> Bool { x > 2 }))
}
"#,
        &["true", "true", "false"],
    );
}

#[test]
fn list_reduce() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3, 4]
    let sum: Int = xs.reduce(0, function(acc: Int, x: Int) -> Int { acc + x })
    print(sum)
}
"#,
        &["10"],
    );
}

#[test]
fn list_flat_map() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    let result: List<Int> = xs.flatMap(function(x: Int) -> List<Int> { [x, x * 10] })
    print(result.length())
    print(result.get(0))
    print(result.get(1))
    print(result.get(2))
    print(result.get(3))
}
"#,
        &["6", "1", "10", "2", "20"],
    );
}

#[test]
fn list_sort_by() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [3, 1, 4, 1, 5]
    let sorted: List<Int> = xs.sortBy(function(a: Int, b: Int) -> Int { a - b })
    print(sorted.get(0))
    print(sorted.get(1))
    print(sorted.get(2))
    print(sorted.get(3))
    print(sorted.get(4))
}
"#,
        &["1", "1", "3", "4", "5"],
    );
}

// ── for-in edge cases ───────────────────────────────────────────────────

#[test]
fn for_in_with_continue() {
    run_expect(
        r#"
function main() {
    for x in [1, 2, 3, 4, 5] {
        if x % 2 == 0 { continue }
        print(x)
    }
}
"#,
        &["1", "3", "5"],
    );
}

#[test]
fn for_in_break_skips_else() {
    run_expect(
        r#"
function main() {
    for x in [1, 2, 3] {
        if x == 2 { break }
        print(x)
    } else {
        print("should not print")
    }
}
"#,
        &["1"],
    );
}

#[test]
fn for_in_type_error_non_list() {
    expect_type_error(
        r#"
function main() {
    for x in 42 {
        print(x)
    }
}
"#,
        "for...in requires a List",
    );
}

// ── Map edge cases ──────────────────────────────────────────────────────

#[test]
fn map_int_keys() {
    run_expect(
        r#"
function main() {
    let m: Map<Int, String> = {1: "one", 2: "two", 3: "three"}
    print(m.get(2).unwrap())
    print(m.contains(1))
    print(m.contains(99))
}
"#,
        &["two", "true", "false"],
    );
}

#[test]
fn map_set_overwrites_existing() {
    run_expect(
        r#"
function main() {
    let m: Map<String, Int> = {"a": 1}
    let m2: Map<String, Int> = m.set("a", 99)
    print(m2.get("a").unwrap())
    print(m2.length())
}
"#,
        &["99", "1"],
    );
}

#[test]
fn map_remove_nonexistent() {
    run_expect(
        r#"
function main() {
    let m: Map<String, Int> = {"a": 1, "b": 2}
    let m2: Map<String, Int> = m.remove("z")
    print(m2.length())
}
"#,
        &["2"],
    );
}

#[test]
fn map_empty_operations() {
    run_expect(
        r#"
function main() {
    let m: Map<String, Int> = {:}
    print(m.get("x").isNone())
    print(m.contains("x"))
    print(m.keys().length())
    print(m.values().length())
}
"#,
        &["true", "false", "0", "0"],
    );
}

// ── Functional collection edge cases ────────────────────────────────────

#[test]
fn list_empty_edge_cases() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = []
    print(xs.last().isNone())
    print(xs.find(function(x: Int) -> Bool { x > 0 }).isNone())
    print(xs.any(function(x: Int) -> Bool { x > 0 }))
    print(xs.all(function(x: Int) -> Bool { x > 0 }))
}
"#,
        &["true", "true", "false", "true"],
    );
}

#[test]
fn list_take_drop_edge_cases() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    let takenAll: List<Int> = xs.take(100)
    print(takenAll.length())
    let takenZero: List<Int> = xs.take(0)
    print(takenZero.length())
    let droppedAll: List<Int> = xs.drop(100)
    print(droppedAll.length())
    let droppedZero: List<Int> = xs.drop(0)
    print(droppedZero.length())
}
"#,
        &["3", "0", "0", "3"],
    );
}

#[test]
fn list_reduce_empty() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = []
    let sum: Int = xs.reduce(42, function(acc: Int, x: Int) -> Int { acc + x })
    print(sum)
}
"#,
        &["42"],
    );
}

#[test]
fn list_sort_by_edge_cases() {
    run_expect(
        r#"
function main() {
    let empty: List<Int> = []
    let sortedEmpty: List<Int> = empty.sortBy(function(a: Int, b: Int) -> Int { a - b })
    print(sortedEmpty.length())
    let single: List<Int> = [42]
    let sortedSingle: List<Int> = single.sortBy(function(a: Int, b: Int) -> Int { a - b })
    print(sortedSingle.get(0))
}
"#,
        &["0", "42"],
    );
}

#[test]
fn list_flat_map_empty_results() {
    run_expect(
        r#"
function expand(x: Int) -> List<Int> {
    if x % 2 == 0 { return [x, x] }
    return []
}
function main() {
    let xs: List<Int> = [1, 2, 3]
    let result: List<Int> = xs.flatMap(function(x: Int) -> List<Int> { expand(x) })
    print(result.length())
    print(result.get(0))
}
"#,
        &["2", "2"],
    );
}

#[test]
fn list_contains_various_types() {
    run_expect(
        r#"
function main() {
    let ints: List<Int> = [1, 2, 3]
    print(ints.contains(2))
    print(ints.contains(99))
    let strs: List<String> = ["a", "b"]
    print(strs.contains("b"))
    print(strs.contains("z"))
    let bools: List<Bool> = [true, false]
    print(bools.contains(true))
}
"#,
        &["true", "false", "true", "false", "true"],
    );
}

// ════════════════════════════════════════════════════════════════════════
// Audit round 2 — additional edge case tests
// ════════════════════════════════════════════════════════════════════════

// ── Map: display, equality, type errors ─────────────────────────────────

#[test]
fn map_display_and_print() {
    run_expect(
        r#"
function main() {
    let m: Map<String, Int> = {"x": 10, "y": 20}
    print(m)
}
"#,
        &["{x: 10, y: 20}"],
    );
}

#[test]
fn map_equality() {
    run_expect(
        r#"
function main() {
    let m1: Map<String, Int> = {"a": 1, "b": 2}
    let m2: Map<String, Int> = {"a": 1, "b": 2}
    let m3: Map<String, Int> = {"a": 1, "b": 99}
    print(m1 == m2)
    print(m1 == m3)
    print(m1 != m3)
}
"#,
        &["true", "false", "true"],
    );
}

#[test]
fn map_set_wrong_value_type() {
    expect_type_error(
        r#"
function main() {
    let m: Map<String, Int> = {"a": 1}
    let m2: Map<String, Int> = m.set("b", "wrong")
}
"#,
        "expected Int but got String",
    );
}

#[test]
fn map_get_wrong_key_type() {
    expect_type_error(
        r#"
function main() {
    let m: Map<String, Int> = {"a": 1}
    let v: Option<Int> = m.get(42)
}
"#,
        "expected String but got Int",
    );
}

// ── for-in over function results and chained operations ─────────────────

#[test]
fn for_in_over_function_result() {
    run_expect(
        r#"
function getItems() -> List<Int> { [10, 20, 30] }
function main() {
    for x in getItems() {
        print(x)
    }
}
"#,
        &["10", "20", "30"],
    );
}

#[test]
fn for_in_over_map_keys() {
    run_expect(
        r#"
function main() {
    let m: Map<String, Int> = {"a": 1, "b": 2}
    for k in m.keys() {
        print(k)
    }
}
"#,
        &["a", "b"],
    );
}

#[test]
fn for_in_over_map_values() {
    run_expect(
        r#"
function main() {
    let m: Map<String, Int> = {"x": 10, "y": 20}
    for v in m.values() {
        print(v)
    }
}
"#,
        &["10", "20"],
    );
}

#[test]
fn for_in_over_filtered_list() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3, 4, 5, 6]
    let evens: List<Int> = xs.filter(function(x: Int) -> Bool { x % 2 == 0 })
    for x in evens {
        print(x)
    }
}
"#,
        &["2", "4", "6"],
    );
}

// ── sort_by: descending and stability ───────────────────────────────────

#[test]
fn sort_by_descending() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [3, 1, 4, 1, 5, 9]
    let desc: List<Int> = xs.sortBy(function(a: Int, b: Int) -> Int { b - a })
    print(desc.get(0))
    print(desc.get(1))
    print(desc.get(2))
}
"#,
        &["9", "5", "4"],
    );
}

// ── Cross-feature: for-in + list methods ────────────────────────────────

#[test]
fn for_in_over_mapped_list() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    let doubled: List<Int> = xs.map(function(x: Int) -> Int { x * 2 })
    for x in doubled {
        print(x)
    }
}
"#,
        &["2", "4", "6"],
    );
}

#[test]
fn map_get_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    let m: Map<String, Int> = {"a": 1}
    let v: Option<Int> = m.get()
}
"#,
        "takes 1 argument",
    );
}

#[test]
fn map_set_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    let m: Map<String, Int> = {"a": 1}
    let m2: Map<String, Int> = m.set("b")
}
"#,
        "takes 2 argument",
    );
}

// ── Tier C: Edge case tests ──────────────────────────────────────────────

#[test]
fn empty_list_map() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = []
    let ys: List<Int> = xs.map(function(x: Int) -> Int { x + 1 })
    print(ys)
}
"#,
        &["[]"],
    );
}

#[test]
fn empty_list_filter() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = []
    let ys: List<Int> = xs.filter(function(x: Int) -> Bool { x > 0 })
    print(ys)
}
"#,
        &["[]"],
    );
}

#[test]
fn empty_list_reduce() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = []
    let sum: Int = xs.reduce(0, function(acc: Int, x: Int) -> Int { acc + x })
    print(sum)
}
"#,
        &["0"],
    );
}

#[test]
fn empty_list_find() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = []
    let found: Option<Int> = xs.find(function(x: Int) -> Bool { x > 0 })
    print(found)
}
"#,
        &["None"],
    );
}

#[test]
fn empty_list_any() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = []
    print(xs.any(function(x: Int) -> Bool { x > 0 }))
}
"#,
        &["false"],
    );
}

#[test]
fn empty_list_all() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = []
    print(xs.all(function(x: Int) -> Bool { x > 0 }))
}
"#,
        &["true"],
    );
}

#[test]
fn empty_list_flat_map() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = []
    let ys: List<Int> = xs.flatMap(function(x: Int) -> List<Int> { [x, x] })
    print(ys)
}
"#,
        &["[]"],
    );
}

#[test]
fn empty_list_sort_by() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = []
    let ys: List<Int> = xs.sortBy(function(a: Int, b: Int) -> Int { a - b })
    print(ys)
}
"#,
        &["[]"],
    );
}

#[test]
fn sort_by_callback_returns_non_int() {
    expect_type_error(
        r#"
function main() {
    let xs: List<Int> = [3, 1, 2]
    let ys: List<Int> = xs.sortBy(function(a: Int, b: Int) -> Bool { true })
}
"#,
        "sortBy callback must return Int",
    );
}

/// Bug 4: `take()` should error on negative argument (not silently clamp).
#[test]
fn list_take_negative_error() {
    expect_runtime_error(
        r#"
function main() {
  let xs: List<Int> = [1, 2, 3]
  let ys: List<Int> = xs.take(-1)
}
"#,
        "non-negative",
    );
}

/// Bug 4: `drop()` should error on negative argument (not silently clamp).
#[test]
fn list_drop_negative_error() {
    expect_runtime_error(
        r#"
function main() {
  let xs: List<Int> = [1, 2, 3]
  let ys: List<Int> = xs.drop(-1)
}
"#,
        "non-negative",
    );
}

/// Method call chaining on collections: filter → map → reduce.
#[test]
fn collection_method_chaining() {
    run_expect(
        r#"
function main() {
  let xs: List<Int> = [1, 2, 3, 4, 5, 6]
  let result: Int = xs.filter(function(x: Int) -> Bool { x % 2 == 0 }).map(function(x: Int) -> Int { x * 10 }).reduce(0, function(acc: Int, x: Int) -> Int { acc + x })
  print(result)
}
"#,
        &["120"],
    );
}

/// Recursive type: linked list operations.
#[test]
fn recursive_type_linked_list_operations() {
    run_expect(
        r#"
enum IntList {
  Nil
  Cons(Int, IntList)
}
function sum(l: IntList) -> Int {
  return match l {
    Nil -> 0
    Cons(head, tail) -> head + sum(tail)
  }
}
function length(l: IntList) -> Int {
  return match l {
    Nil -> 0
    Cons(_, tail) -> 1 + length(tail)
  }
}
function main() {
  let list: IntList = Cons(1, Cons(2, Cons(3, Nil)))
  print(sum(list))
  print(length(list))
}
"#,
        &["6", "3"],
    );
}

// ══════════════════════════════════════════════════════════════════════
// P2 — Edge cases and error messages
// ══════════════════════════════════════════════════════════════════════

/// Empty list: first() and last() return None.
#[test]
fn empty_list_first_last() {
    run_expect(
        r#"
function main() {
  let xs: List<Int> = []
  print(xs.first().isNone())
  print(xs.last().isNone())
}
"#,
        &["true", "true"],
    );
}

/// For-in over empty collection triggers else.
#[test]
fn for_in_empty_collection_else() {
    run_expect(
        r#"
function main() {
  let xs: List<Int> = []
  for x in xs {
    print("nope")
  } else {
    print("empty")
  }
}
"#,
        &["empty"],
    );
}

/// Map: get on missing key returns None.
#[test]
fn map_get_missing_key() {
    run_expect(
        r#"
function main() {
  let m: Map<String, Int> = {"a": 1}
  let v: Option<Int> = m.get("b")
  print(v.isNone())
}
"#,
        &["true"],
    );
}

/// Map: remove returns a new map without the key (immutable API).
#[test]
fn map_remove_returns_new_map() {
    run_expect(
        r#"
function main() {
  let m: Map<String, Int> = {"a": 1, "b": 2, "c": 3}
  let m2: Map<String, Int> = m.remove("b")
  print(m2.length())
  print(m2.contains("b"))
  print(m.length())
}
"#,
        &["2", "false", "3"],
    );
}

// ---------------------------------------------------------------------------
// Feature coverage tests with output verification
// ---------------------------------------------------------------------------

/// List map, filter, reduce end-to-end.
#[test]
fn list_map_filter_reduce_e2e() {
    run_expect(
        r#"
function main() {
  let nums: List<Int> = [1, 2, 3, 4, 5]
  let doubled: List<Int> = nums.map(function(x: Int) -> Int { x * 2 })
  print(doubled)
  let evens: List<Int> = nums.filter(function(x: Int) -> Bool { x > 3 })
  print(evens)
  let sum: Int = nums.reduce(0, function(acc: Int, x: Int) -> Int { acc + x })
  print(sum)
}
"#,
        &["[2, 4, 6, 8, 10]", "[4, 5]", "15"],
    );
}

/// List find, any, all end-to-end.
#[test]
fn list_find_any_all_e2e() {
    run_expect(
        r#"
function main() {
  let nums: List<Int> = [1, 2, 3, 4]
  print(nums.find(function(x: Int) -> Bool { x > 2 }))
  print(nums.any(function(x: Int) -> Bool { x == 3 }))
  print(nums.all(function(x: Int) -> Bool { x > 0 }))
  print(nums.all(function(x: Int) -> Bool { x > 2 }))
}
"#,
        &["Some(3)", "true", "true", "false"],
    );
}

/// List flat_map and sort_by end-to-end.
#[test]
fn list_flat_map_sort_by_e2e() {
    run_expect(
        r#"
function main() {
  let nums: List<Int> = [3, 1, 2]
  let sorted: List<Int> = nums.sortBy(function(a: Int, b: Int) -> Int { a - b })
  print(sorted)
  let expanded: List<Int> = [1, 2].flatMap(function(x: Int) -> List<Int> { [x, x * 10] })
  print(expanded)
}
"#,
        &["[1, 2, 3]", "[1, 10, 2, 20]"],
    );
}

/// List first, last, contains, take, drop end-to-end.
#[test]
fn list_first_last_contains_take_drop_e2e() {
    run_expect(
        r#"
function main() {
  let nums: List<Int> = [10, 20, 30, 40, 50]
  print(nums.first())
  print(nums.last())
  print(nums.contains(30))
  print(nums.contains(99))
  print(nums.take(3))
  print(nums.drop(3))
}
"#,
        &[
            "Some(10)",
            "Some(50)",
            "true",
            "false",
            "[10, 20, 30]",
            "[40, 50]",
        ],
    );
}

/// Map set, remove, keys/values end-to-end.
#[test]
fn map_set_remove_keys_values_e2e() {
    run_expect(
        r#"
function main() {
  let mut m: Map<String, Int> = {"a": 1, "b": 2}
  m = m.set("c", 3)
  print(m.get("c"))
  m = m.remove("a")
  print(m.contains("a"))
  print(m.length())
}
"#,
        &["Some(3)", "false", "2"],
    );
}

/// flat_map callback must return a List — type checker should reject non-List return.
#[test]
fn flat_map_rejects_non_list_callback() {
    expect_type_error(
        r#"
function main() {
  let xs: List<Int> = [1, 2, 3]
  let ys: List<Int> = xs.flatMap(function(x: Int) -> Int { x + 1 })
  print(ys)
}
"#,
        "flatMap callback must return a List",
    );
}
