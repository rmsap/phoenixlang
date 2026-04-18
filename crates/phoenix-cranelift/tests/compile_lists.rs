//! Integration tests for compiled `List<T>` operations.
//!
//! Covers list literals, get/push/contains/take/drop/first/last,
//! closure-based methods (map, filter, find, any, all, reduce, flatMap,
//! sortBy), string split (returns `List<String>`), and for-each loops.

mod common;

use common::{compile_and_run, roundtrip};

#[test]
fn list_literal_int() {
    roundtrip(
        r#"
function main() {
    let xs = [1, 2, 3]
    print(xs.length())
}
"#,
    );
}

#[test]
fn list_empty() {
    roundtrip(
        r#"
function main() {
    let xs: List<Int> = []
    print(xs.length())
}
"#,
    );
}

#[test]
fn list_get() {
    roundtrip(
        r#"
function main() {
    let xs = [10, 20, 30]
    print(xs.get(0))
    print(xs.get(1))
    print(xs.get(2))
}
"#,
    );
}

#[test]
fn list_push() {
    roundtrip(
        r#"
function main() {
    let xs = [1, 2]
    let ys = xs.push(3)
    print(ys.length())
    print(ys.get(2))
    print(xs.length())
}
"#,
    );
}

#[test]
fn list_contains() {
    roundtrip(
        r#"
function main() {
    let xs = [1, 2, 3]
    print(xs.contains(2))
    print(xs.contains(5))
}
"#,
    );
}

#[test]
fn list_first_last() {
    roundtrip(
        r#"
function main() {
    let xs = [10, 20, 30]
    match xs.first() {
        Some(v) -> print(v)
        None -> print(-1)
    }
    match xs.last() {
        Some(v) -> print(v)
        None -> print(-1)
    }
    let empty: List<Int> = []
    match empty.first() {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn list_take_drop() {
    roundtrip(
        r#"
function main() {
    let xs = [1, 2, 3, 4, 5]
    let taken = xs.take(3)
    print(taken.length())
    print(taken.get(0))
    print(taken.get(2))
    let dropped = xs.drop(2)
    print(dropped.length())
    print(dropped.get(0))
}
"#,
    );
}

#[test]
fn list_of_strings() {
    roundtrip(
        r#"
function main() {
    let xs = ["hello", "world"]
    print(xs.length())
    print(xs.get(0))
    print(xs.get(1))
}
"#,
    );
}

#[test]
fn list_map() {
    roundtrip(
        r#"
function main() {
    let xs = [1, 2, 3]
    let ys = xs.map(function(x: Int) -> Int { x * 2 })
    print(ys.get(0))
    print(ys.get(1))
    print(ys.get(2))
}
"#,
    );
}

#[test]
fn list_filter() {
    roundtrip(
        r#"
function main() {
    let xs = [1, 2, 3, 4, 5]
    let evens = xs.filter(function(x: Int) -> Bool { x % 2 == 0 })
    print(evens.length())
    print(evens.get(0))
    print(evens.get(1))
}
"#,
    );
}

#[test]
fn list_find() {
    roundtrip(
        r#"
function main() {
    let xs = [1, 2, 3, 4]
    match xs.find(function(x: Int) -> Bool { x > 2 }) {
        Some(v) -> print(v)
        None -> print(-1)
    }
    match xs.find(function(x: Int) -> Bool { x > 10 }) {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn list_any_all() {
    roundtrip(
        r#"
function main() {
    let xs = [1, 2, 3, 4]
    print(xs.any(function(x: Int) -> Bool { x > 3 }))
    print(xs.any(function(x: Int) -> Bool { x > 10 }))
    print(xs.all(function(x: Int) -> Bool { x > 0 }))
    print(xs.all(function(x: Int) -> Bool { x > 2 }))
}
"#,
    );
}

#[test]
fn list_reduce() {
    roundtrip(
        r#"
function main() {
    let xs = [1, 2, 3, 4]
    let sum = xs.reduce(0, function(acc: Int, x: Int) -> Int { acc + x })
    print(sum)
}
"#,
    );
}

#[test]
fn list_flat_map() {
    roundtrip(
        r#"
function main() {
    let xs = [1, 2, 3]
    let ys = xs.flatMap(function(x: Int) -> List<Int> { [x, x * 10] })
    print(ys.length())
    print(ys.get(0))
    print(ys.get(1))
    print(ys.get(2))
    print(ys.get(3))
}
"#,
    );
}

#[test]
fn list_sort_by() {
    roundtrip(
        r#"
function main() {
    let xs = [3, 1, 4, 1, 5]
    let sorted = xs.sortBy(function(a: Int, b: Int) -> Int { a - b })
    print(sorted.get(0))
    print(sorted.get(1))
    print(sorted.get(2))
    print(sorted.get(3))
    print(sorted.get(4))
}
"#,
    );
}

#[test]
fn list_for_each_loop() {
    roundtrip(
        r#"
function main() {
    let xs = [10, 20, 30]
    for x in xs {
        print(x)
    }
}
"#,
    );
}

#[test]
fn list_string_push() {
    roundtrip(
        r#"
function main() {
    let xs = ["a", "b"]
    let ys = xs.push("c")
    print(ys.length())
    print(ys.get(0))
    print(ys.get(1))
    print(ys.get(2))
}
"#,
    );
}

#[test]
fn list_string_contains() {
    roundtrip(
        r#"
function main() {
    let xs = ["hello", "world"]
    print(xs.contains("hello"))
    print(xs.contains("foo"))
}
"#,
    );
}

#[test]
fn list_map_to_string() {
    roundtrip(
        r#"
function main() {
    let xs = [1, 2, 3]
    let strs = xs.map(function(x: Int) -> String { toString(x) })
    print(strs.get(0))
    print(strs.get(1))
}
"#,
    );
}

#[test]
fn string_split() {
    roundtrip(
        r#"
function main() {
    let parts = "a,b,c".split(",")
    print(parts.length())
    print(parts.get(0))
    print(parts.get(1))
    print(parts.get(2))
}
"#,
    );
}

#[test]
fn list_sort_by_empty() {
    roundtrip(
        r#"
function main() {
    let xs: List<Int> = []
    let sorted = xs.sortBy(function(a: Int, b: Int) -> Int { a - b })
    print(sorted.length())
}
"#,
    );
}

#[test]
fn list_reduce_string() {
    roundtrip(
        r#"
function main() {
    let xs = ["a", "b", "c"]
    let joined = xs.reduce("", function(acc: String, x: String) -> String { acc + x })
    print(joined)
}
"#,
    );
}

#[test]
fn list_flat_map_strings() {
    roundtrip(
        r#"
function main() {
    let xs = ["hello world", "foo bar"]
    let words = xs.flatMap(function(s: String) -> List<String> { s.split(" ") })
    print(words.length())
    print(words.get(0))
    print(words.get(1))
    print(words.get(2))
    print(words.get(3))
}
"#,
    );
}

#[test]
fn list_take_beyond_length() {
    roundtrip(
        r#"
function main() {
    let xs = [1, 2, 3]
    let taken = xs.take(10)
    print(taken.length())
    print(taken.get(0))
    print(taken.get(2))
}
"#,
    );
}

#[test]
fn list_drop_beyond_length() {
    roundtrip(
        r#"
function main() {
    let xs = [1, 2, 3]
    let dropped = xs.drop(10)
    print(dropped.length())
}
"#,
    );
}

#[test]
fn list_reduce_empty() {
    roundtrip(
        r#"
function main() {
    let xs: List<Int> = []
    let sum = xs.reduce(0, function(acc: Int, x: Int) -> Int { acc + x })
    print(sum)
}
"#,
    );
}

#[test]
fn list_find_empty() {
    roundtrip(
        r#"
function main() {
    let xs: List<Int> = []
    match xs.find(function(x: Int) -> Bool { x > 0 }) {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn list_flat_map_empty() {
    roundtrip(
        r#"
function main() {
    let xs: List<Int> = []
    let ys = xs.flatMap(function(x: Int) -> List<Int> { [x, x * 10] })
    print(ys.length())
}
"#,
    );
}

#[test]
fn list_filter_removes_all() {
    roundtrip(
        r#"
function main() {
    let xs = [1, 2, 3]
    let empty = xs.filter(function(x: Int) -> Bool { x > 10 })
    print(empty.length())
}
"#,
    );
}

#[test]
fn list_any_empty() {
    roundtrip(
        r#"
function main() {
    let xs: List<Int> = []
    print(xs.any(function(x: Int) -> Bool { x > 0 }))
}
"#,
    );
}

#[test]
fn list_all_empty() {
    roundtrip(
        r#"
function main() {
    let xs: List<Int> = []
    print(xs.all(function(x: Int) -> Bool { x > 0 }))
}
"#,
    );
}

#[test]
fn list_float_basic() {
    // Test List<Float> with literal, map, and contains
    roundtrip(
        r#"
function main() {
    let xs: List<Float> = [1.5, 2.5, 3.5]
    print(xs.length())
    print(xs.first().unwrap())
    print(xs.last().unwrap())
}
"#,
    );
}

#[test]
fn list_map_empty() {
    // Test map on empty list
    roundtrip(
        r#"
function main() {
    let xs: List<Int> = []
    let ys: List<Int> = xs.map(function(n: Int) -> Int { n * 2 })
    print(ys.length())
}
"#,
    );
}

#[test]
fn list_filter_empty() {
    // Test filter on empty list
    roundtrip(
        r#"
function main() {
    let xs: List<Int> = []
    let ys: List<Int> = xs.filter(function(n: Int) -> Bool { n > 0 })
    print(ys.length())
}
"#,
    );
}

#[test]
fn list_push_on_empty() {
    roundtrip(
        r#"
function main() {
    let xs: List<Int> = []
    let ys: List<Int> = xs.push(42)
    print(ys.length())
    print(ys.first().unwrap())
}
"#,
    );
}

#[test]
fn list_contains_empty() {
    roundtrip(
        r#"
function main() {
    let xs: List<Int> = []
    print(xs.contains(1))
}
"#,
    );
}

#[test]
fn list_chained_operations() {
    // Test chaining filter -> map -> reduce
    roundtrip(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3, 4, 5, 6]
    let evens: List<Int> = xs.filter(function(n: Int) -> Bool { n % 2 == 0 })
    let scaled: List<Int> = evens.map(function(n: Int) -> Int { n * 10 })
    let result: Int = scaled.reduce(0, function(acc: Int, n: Int) -> Int { acc + n })
    print(result)
}
"#,
    );
}

#[test]
fn list_bool_filter() {
    roundtrip(
        r#"
function main() {
    let xs: List<Bool> = [true, false, true, false]
    let trues: List<Bool> = xs.filter(function(b: Bool) -> Bool { b })
    print(trues.length())
}
"#,
    );
}

#[test]
fn list_float_map() {
    roundtrip(
        r#"
function main() {
    let xs: List<Float> = [1.0, 2.0, 3.0]
    let doubled: List<Float> = xs.map(function(f: Float) -> Float { f * 2.0 })
    print(doubled.get(0))
    print(doubled.get(1))
    print(doubled.get(2))
}
"#,
    );
}

#[test]
fn list_float_reduce() {
    roundtrip(
        r#"
function main() {
    let xs: List<Float> = [1.5, 2.5, 3.0]
    let sum: Float = xs.reduce(0.0, function(acc: Float, x: Float) -> Float { acc + x })
    print(sum)
}
"#,
    );
}

#[test]
fn list_float_any_all() {
    roundtrip(
        r#"
function main() {
    let xs: List<Float> = [1.0, 2.0, 3.0]
    print(xs.any(function(f: Float) -> Bool { f > 2.5 }))
    print(xs.all(function(f: Float) -> Bool { f > 0.0 }))
}
"#,
    );
}

#[test]
fn list_of_options() {
    roundtrip(
        r#"
function main() {
    let xs: List<Option<Int>> = [Some(1), None, Some(3)]
    print(xs.length())
    match xs.get(0) {
        Some(v) -> print(v)
        None -> print(-1)
    }
    match xs.get(1) {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn option_of_list() {
    roundtrip(
        r#"
function main() {
    let xs: Option<List<Int>> = Some([1, 2, 3])
    let ys: Option<List<Int>> = None
    match xs {
        Some(list) -> print(list.length())
        None -> print(-1)
    }
    match ys {
        Some(list) -> print(list.length())
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn list_get_out_of_bounds_panics() {
    common::expect_panic(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    print(xs.get(5))
}
"#,
        "out of bounds",
    );
}

#[test]
fn list_get_negative_index_panics() {
    common::expect_panic(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    print(xs.get(-1))
}
"#,
        "out of bounds",
    );
}

#[test]
fn list_map_with_capture() {
    let out = compile_and_run(
        r#"
function main() {
    let factor = 10
    let xs = [1, 2, 3]
    let ys = xs.map(function(x: Int) -> Int { x * factor })
    print(ys.get(0))
    print(ys.get(1))
    print(ys.get(2))
}
"#,
    );
    assert_eq!(out, vec!["10", "20", "30"]);
}

#[test]
fn list_filter_with_capture() {
    let out = compile_and_run(
        r#"
function main() {
    let threshold = 3
    let xs = [1, 2, 3, 4, 5]
    let ys = xs.filter(function(x: Int) -> Bool { x > threshold })
    print(ys.length())
    print(ys.get(0))
    print(ys.get(1))
}
"#,
    );
    assert_eq!(out, vec!["2", "4", "5"]);
}

#[test]
fn list_get_hardcoded() {
    let out = compile_and_run(
        r#"
function main() {
    let xs = [10, 20, 30]
    print(xs.get(0))
    print(xs.get(1))
    print(xs.get(2))
}
"#,
    );
    assert_eq!(out, vec!["10", "20", "30"]);
}

#[test]
fn list_first_last_string() {
    let out = compile_and_run(
        r#"
function main() {
    let xs = ["hello", "world"]
    match xs.first() {
        Some(v) -> print(v)
        None -> print("none")
    }
    match xs.last() {
        Some(v) -> print(v)
        None -> print("none")
    }
}
"#,
    );
    assert_eq!(out, vec!["hello", "world"]);
}

#[test]
fn list_sortby_single_element() {
    let out = compile_and_run(
        r#"
function main() {
    let xs = [42]
    let ys = xs.sortBy(function(a: Int, b: Int) -> Int { a - b })
    print(ys.get(0))
    print(ys.length())
}
"#,
    );
    assert_eq!(out, vec!["42", "1"]);
}

#[test]
fn list_take_zero_and_drop_zero() {
    let out = compile_and_run(
        r#"
function main() {
    let xs = [1, 2, 3]
    let t = xs.take(0)
    let d = xs.drop(0)
    print(t.length())
    print(d.length())
    print(d.get(0))
}
"#,
    );
    assert_eq!(out, vec!["0", "3", "1"]);
}

#[test]
fn list_of_lists() {
    roundtrip(
        r#"
function main() {
    let a = [1, 2]
    let b = [3, 4]
    let nested: List<List<Int>> = [a, b]
    print(nested.length())
    let first: List<Int> = nested.get(0)
    print(first.get(0))
    print(first.get(1))
    let second: List<Int> = nested.get(1)
    print(second.get(0))
}
"#,
    );
}

#[test]
fn list_string_sort_by_length() {
    // Tests the multi-slot (16-byte) swap path in translate_list_sortby.
    let out = compile_and_run(
        r#"
function main() {
    let xs = ["cherry", "hi", "apple"]
    let sorted = xs.sortBy(function(a: String, b: String) -> Int {
        a.length() - b.length()
    })
    print(sorted.get(0))
    print(sorted.get(1))
    print(sorted.get(2))
}
"#,
    );
    assert_eq!(out, vec!["hi", "apple", "cherry"]);
}

#[test]
fn list_string_sort_by_length_descending() {
    let out = compile_and_run(
        r#"
function main() {
    let xs = ["cherry", "hi", "apple"]
    let sorted = xs.sortBy(function(a: String, b: String) -> Int {
        b.length() - a.length()
    })
    print(sorted.get(0))
    print(sorted.get(1))
    print(sorted.get(2))
}
"#,
    );
    assert_eq!(out, vec!["cherry", "apple", "hi"]);
}

#[test]
fn list_float_contains() {
    let out = compile_and_run(
        r#"
function main() {
    let xs = [1.0, 2.5, 3.14]
    print(xs.contains(2.5))
    print(xs.contains(9.9))
}
"#,
    );
    assert_eq!(out, vec!["true", "false"]);
}

#[test]
fn list_filter_string_elements() {
    roundtrip(
        r#"
function main() {
    let xs = ["apple", "banana", "avocado", "cherry"]
    let a_words = xs.filter(function(s: String) -> Bool { s.startsWith("a") })
    print(a_words.length())
    print(a_words.get(0))
    print(a_words.get(1))
}
"#,
    );
}

#[test]
fn list_find_string_elements() {
    roundtrip(
        r#"
function main() {
    let xs = ["apple", "banana", "cherry"]
    match xs.find(function(s: String) -> Bool { s == "banana" }) {
        Some(v) -> print(v)
        None -> print("not found")
    }
    match xs.find(function(s: String) -> Bool { s == "mango" }) {
        Some(v) -> print(v)
        None -> print("not found")
    }
}
"#,
    );
}

#[test]
fn list_any_string_elements() {
    roundtrip(
        r#"
function main() {
    let xs = ["apple", "banana", "cherry"]
    print(xs.any(function(s: String) -> Bool { s == "banana" }))
    print(xs.any(function(s: String) -> Bool { s == "mango" }))
}
"#,
    );
}

#[test]
fn list_all_string_elements() {
    roundtrip(
        r#"
function main() {
    let xs = ["apple", "banana", "cherry"]
    print(xs.all(function(s: String) -> Bool { s.length() > 0 }))
    print(xs.all(function(s: String) -> Bool { s.length() > 5 }))
}
"#,
    );
}

#[test]
fn list_for_each_string() {
    roundtrip(
        r#"
function main() {
    let xs = ["hello", "world", "foo"]
    for x in xs {
        print(x)
    }
}
"#,
    );
}

#[test]
fn list_map_string_to_int() {
    let out = compile_and_run(
        r#"
function main() {
    let xs = ["hi", "hello", "hey"]
    let lens = xs.map(function(s: String) -> Int { s.length() })
    print(lens.get(0))
    print(lens.get(1))
    print(lens.get(2))
}
"#,
    );
    assert_eq!(out, vec!["2", "5", "3"]);
}

/// Map List<Int> (8-byte elements) to List<String> (16-byte elements).
#[test]
fn list_map_int_to_string() {
    let out = compile_and_run(
        r#"
function main() {
    let xs = [1, 2, 3]
    let ys = xs.map(function(x: Int) -> String { toString(x) })
    print(ys.get(0))
    print(ys.get(1))
    print(ys.get(2))
    print(ys.length())
}
"#,
    );
    assert_eq!(out, vec!["1", "2", "3", "3"]);
}

/// Sort a list with duplicate values and verify the output is stable.
/// For integers this is trivially stable, but documents the behavior.
#[test]
fn list_sort_by_stability() {
    let out = compile_and_run(
        r#"
function main() {
    let xs = [3, 1, 2, 1]
    let sorted = xs.sortBy(function(a: Int, b: Int) -> Int { a - b })
    print(sorted.get(0))
    print(sorted.get(1))
    print(sorted.get(2))
    print(sorted.get(3))
}
"#,
    );
    assert_eq!(out, vec!["1", "1", "2", "3"]);
}

/// IEEE 754: -0.0 == 0.0, so [1.0, 0.0].contains(-0.0) should be true.
#[test]
fn list_float_contains_neg_zero() {
    let out = compile_and_run(
        r#"
function main() {
    let xs = [1.0, 0.0]
    print(xs.contains(-0.0))
}
"#,
    );
    assert_eq!(out, vec!["true"]);
}

/// NaN != NaN under IEEE 754, so a list containing NaN should not
/// find NaN via contains.  We construct NaN with 0.0 / 0.0.
#[test]
fn list_float_contains_nan() {
    let out = compile_and_run(
        r#"
function main() {
    let nan = 0.0 / 0.0
    let xs = [nan, 1.0]
    print(xs.contains(nan))
}
"#,
    );
    assert_eq!(out, vec!["false"]);
}
