//! Round-trip tests: List, Map, and String operations.

mod common;
use common::roundtrip;

// ── Collections ──────────────────────────────────────────────────────

#[test]
fn list_operations() {
    roundtrip(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3]
    print(nums.length())
    print(nums.get(0))
    let more: List<Int> = nums.push(4)
    print(more.length())
    print(nums.contains(2))
    print(nums.first())
    print(nums.last())
}
"#,
    );
}

#[test]
fn list_higher_order() {
    roundtrip(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3, 4, 5]
    let doubled: List<Int> = nums.map(function(x: Int) -> Int { return x * 2 })
    print(doubled)
    let evens: List<Int> = nums.filter(function(x: Int) -> Bool { return x % 2 == 0 })
    print(evens)
    let sum: Int = nums.reduce(0, function(acc: Int, x: Int) -> Int { return acc + x })
    print(sum)
}
"#,
    );
}

#[test]
fn list_take_drop() {
    roundtrip(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3, 4, 5]
    print(nums.take(3))
    print(nums.drop(2))
}
"#,
    );
}

#[test]
fn list_find() {
    roundtrip(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3, 4, 5]
    let found: Option<Int> = nums.find(function(x: Int) -> Bool { return x > 3 })
    print(found)
    let missing: Option<Int> = nums.find(function(x: Int) -> Bool { return x > 10 })
    print(missing)
}
"#,
    );
}

#[test]
fn list_any_all() {
    roundtrip(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3, 4, 5]
    print(nums.any(function(x: Int) -> Bool { return x > 3 }))
    print(nums.any(function(x: Int) -> Bool { return x > 10 }))
    print(nums.all(function(x: Int) -> Bool { return x > 0 }))
    print(nums.all(function(x: Int) -> Bool { return x > 3 }))
}
"#,
    );
}

#[test]
fn list_flat_map() {
    roundtrip(
        r#"
function main() {
    let nums: List<Int> = [1, 2, 3]
    let result: List<Int> = nums.flatMap(function(x: Int) -> List<Int> {
        return [x, x * 10]
    })
    print(result)
}
"#,
    );
}

#[test]
fn list_sort_by() {
    roundtrip(
        r#"
function main() {
    let nums: List<Int> = [3, 1, 4, 1, 5]
    let sorted: List<Int> = nums.sortBy(function(a: Int, b: Int) -> Int {
        return a - b
    })
    print(sorted)
}
"#,
    );
}

// ── Map operations ──────────────────────────────────────────────────

#[test]
fn map_operations() {
    roundtrip(
        r#"
function main() {
    let m: Map<String, Int> = {"a": 1, "b": 2}
    print(m.length())
    print(m.get("a"))
    print(m.contains("b"))
    let m2: Map<String, Int> = m.set("c", 3)
    print(m2.length())
    print(m.keys())
    print(m.values())
}
"#,
    );
}

#[test]
fn map_set_remove() {
    roundtrip(
        r#"
function main() {
    let m: Map<String, Int> = {"a": 1, "b": 2}
    let m2: Map<String, Int> = m.set("c", 3)
    print(m2.length())
    print(m2.get("c"))
    let m3: Map<String, Int> = m2.remove("a")
    print(m3.length())
    print(m3.get("a"))
}
"#,
    );
}

#[test]
fn map_keys_values_after_mutation() {
    roundtrip(
        r#"
function main() {
    let m: Map<String, Int> = {"a": 1, "b": 2}
    let m2: Map<String, Int> = m.set("c", 3).remove("a")
    print(m2.keys())
    print(m2.values())
}
"#,
    );
}

// ── Strings ──────────────────────────────────────────────────────────

#[test]
fn string_methods() {
    roundtrip(
        r#"
function main() {
    let s: String = "Hello, World!"
    print(s.length())
    print(s.contains("World"))
    print(s.startsWith("Hello"))
    print(s.endsWith("!"))
    print(s.toLowerCase())
    print(s.toUpperCase())
    print("  hi  ".trim())
}
"#,
    );
}

#[test]
fn string_interpolation() {
    roundtrip(
        r#"
function main() {
    let name: String = "Phoenix"
    let version: Int = 1
    print("${name} v${version}")
}
"#,
    );
}

#[test]
fn string_split() {
    roundtrip(
        r#"
function main() {
    let s: String = "a,b,c"
    let parts: List<String> = s.split(",")
    print(parts)
    print(parts.length())
}
"#,
    );
}

#[test]
fn string_replace() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello world"
    print(s.replace("world", "Phoenix"))
}
"#,
    );
}

#[test]
fn string_substring() {
    roundtrip(
        r#"
function main() {
    let s: String = "Hello, World!"
    print(s.substring(0, 5))
    print(s.substring(7, 12))
}
"#,
    );
}

#[test]
fn string_index_of() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello world"
    print(s.indexOf("world"))
    print(s.indexOf("missing"))
}
"#,
    );
}
