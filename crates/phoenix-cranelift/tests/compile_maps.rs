//! Integration tests for compiled `Map<K, V>` operations.
//!
//! Covers map literals, get/set/contains/remove/length/keys/values,
//! iteration, and string key/value combinations.

mod common;

use common::{compile_and_run, roundtrip};

#[test]
fn map_literal() {
    roundtrip(
        r#"
function main() {
    let m = {"a": 1, "b": 2}
    print(m.length())
}
"#,
    );
}

#[test]
fn map_empty() {
    roundtrip(
        r#"
function main() {
    let m: Map<String, Int> = {}
    print(m.length())
}
"#,
    );
}

#[test]
fn map_get_found() {
    roundtrip(
        r#"
function main() {
    let m = {"x": 10, "y": 20}
    match m.get("x") {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn map_get_not_found() {
    roundtrip(
        r#"
function main() {
    let m = {"x": 10, "y": 20}
    match m.get("z") {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn map_set() {
    roundtrip(
        r#"
function main() {
    let m = {"a": 1}
    let m2 = m.set("b", 2)
    print(m2.length())
    match m2.get("b") {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn map_set_overwrite() {
    roundtrip(
        r#"
function main() {
    let m = {"a": 1}
    let m2 = m.set("a", 99)
    print(m2.length())
    match m2.get("a") {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn map_contains() {
    roundtrip(
        r#"
function main() {
    let m = {"a": 1, "b": 2}
    print(m.contains("a"))
    print(m.contains("c"))
}
"#,
    );
}

#[test]
fn map_remove() {
    roundtrip(
        r#"
function main() {
    let m = {"a": 1, "b": 2}
    let m2 = m.remove("a")
    print(m2.length())
    print(m2.contains("a"))
    print(m2.contains("b"))
}
"#,
    );
}

#[test]
fn map_keys() {
    roundtrip(
        r#"
function main() {
    let m = {"x": 1, "y": 2}
    let ks = m.keys()
    print(ks.length())
}
"#,
    );
}

#[test]
fn map_values() {
    roundtrip(
        r#"
function main() {
    let m = {"x": 10, "y": 20}
    let vs = m.values()
    print(vs.length())
}
"#,
    );
}

#[test]
fn map_int_keys() {
    roundtrip(
        r#"
function main() {
    let m = {1: "one", 2: "two"}
    print(m.length())
    match m.get(1) {
        Some(v) -> print(v)
        None -> print("not found")
    }
}
"#,
    );
}

#[test]
fn map_string_string() {
    roundtrip(
        r#"
function main() {
    let m = {"greeting": "hello", "target": "world"}
    print(m.length())
    match m.get("greeting") {
        Some(v) -> print(v)
        None -> print("not found")
    }
    let m2 = m.set("greeting", "hi")
    match m2.get("greeting") {
        Some(v) -> print(v)
        None -> print("not found")
    }
}
"#,
    );
}

#[test]
fn map_get_empty() {
    roundtrip(
        r#"
function main() {
    let m: Map<String, Int> = {}
    match m.get("x") {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn map_keys_iterate() {
    roundtrip(
        r#"
function main() {
    let m = {"a": 1, "b": 2, "c": 3}
    let ks = m.keys()
    print(ks.length())
    for k in ks {
        print(k)
    }
}
"#,
    );
}

#[test]
fn map_values_iterate() {
    roundtrip(
        r#"
function main() {
    let m = {"x": 10, "y": 20}
    let vs = m.values()
    for v in vs {
        print(v)
    }
}
"#,
    );
}

#[test]
fn map_remove_nonexistent_key() {
    roundtrip(
        r#"
function main() {
    let m: Map<String, Int> = {"a": 1, "b": 2}
    let m2: Map<String, Int> = m.remove("z")
    print(m2.length())
}
"#,
    );
}

#[test]
fn map_set_on_empty() {
    roundtrip(
        r#"
function main() {
    let m: Map<String, Int> = {}
    let m2: Map<String, Int> = m.set("a", 1)
    print(m2.length())
    print(m2.get("a").unwrap())
}
"#,
    );
}

#[test]
fn map_contains_empty() {
    roundtrip(
        r#"
function main() {
    let m: Map<String, Int> = {}
    print(m.contains("a"))
}
"#,
    );
}

#[test]
fn map_keys_empty() {
    roundtrip(
        r#"
function main() {
    let m: Map<String, Int> = {}
    let ks: List<String> = m.keys()
    print(ks.length())
}
"#,
    );
}

#[test]
fn map_values_empty() {
    roundtrip(
        r#"
function main() {
    let m: Map<String, Int> = {}
    let vs: List<Int> = m.values()
    print(vs.length())
}
"#,
    );
}

#[test]
fn map_remove_middle_then_get_later() {
    roundtrip(
        r#"
function main() {
    let m = {"a": 1, "b": 2, "c": 3}
    let m2 = m.remove("b")
    print(m2.length())
    match m2.get("a") {
        Some(v) -> print(v)
        None -> print(-1)
    }
    match m2.get("c") {
        Some(v) -> print(v)
        None -> print(-1)
    }
    match m2.get("b") {
        Some(v) -> print(v)
        None -> print(-1)
    }
}
"#,
    );
}

#[test]
fn map_int_string_get_not_found() {
    roundtrip(
        r#"
function main() {
    let m = {1: "one", 2: "two"}
    match m.get(3) {
        Some(v) -> print(v)
        None -> print("not found")
    }
}
"#,
    );
}

#[test]
fn map_int_string_get_found() {
    roundtrip(
        r#"
function main() {
    let m = {1: "one", 2: "two"}
    match m.get(2) {
        Some(v) -> print(v)
        None -> print("not found")
    }
}
"#,
    );
}

#[test]
fn map_keys_content_verified() {
    let out = compile_and_run(
        r#"
function main() {
    let m = {"a": 1, "b": 2}
    let ks = m.keys()
    for k in ks {
        print(k)
    }
}
"#,
    );
    assert_eq!(out.len(), 2);
    assert!(out.contains(&"a".to_string()));
    assert!(out.contains(&"b".to_string()));
}

#[test]
fn map_values_content_verified() {
    let out = compile_and_run(
        r#"
function main() {
    let m = {"x": 10, "y": 20}
    let vs = m.values()
    let mut sum = 0
    for v in vs {
        sum += v
    }
    print(sum)
}
"#,
    );
    assert_eq!(out, vec!["30"]);
}

#[test]
fn map_set_overwrite_multi_entry() {
    let out = compile_and_run(
        r#"
function main() {
    let m = {"a": 1, "b": 2, "c": 3}
    let m2 = m.set("b", 99)
    match m2.get("a") {
        Some(v) -> print(v)
        None -> print("missing")
    }
    match m2.get("b") {
        Some(v) -> print(v)
        None -> print("missing")
    }
    match m2.get("c") {
        Some(v) -> print(v)
        None -> print("missing")
    }
}
"#,
    );
    assert_eq!(out, vec!["1", "99", "3"]);
}

#[test]
fn map_keys_piped_to_filter() {
    let out = compile_and_run(
        r#"
function main() {
    let m = {"apple": 1, "banana": 2, "avocado": 3}
    let ks = m.keys()
    let a_keys = ks.filter(function(k: String) -> Bool { k.startsWith("a") })
    print(a_keys.length())
    for k in a_keys {
        print(k)
    }
}
"#,
    );
    // Two keys start with "a": "apple" and "avocado"
    assert_eq!(out.len(), 3); // length line + 2 key lines
    assert_eq!(out[0], "2");
    assert!(out.contains(&"apple".to_string()));
    assert!(out.contains(&"avocado".to_string()));
}

/// Create a Map<String, String>, set entries, remove one, verify it's gone.
#[test]
fn map_string_string_remove() {
    let out = compile_and_run(
        r#"
function main() {
    let m = {"name": "alice", "role": "admin", "team": "eng"}
    let m2 = m.remove("role")
    print(m2.length())
    print(m2.contains("role"))
    print(m2.contains("name"))
    match m2.get("name") {
        Some(v) -> print(v)
        None -> print("missing")
    }
    match m2.get("role") {
        Some(v) -> print(v)
        None -> print("missing")
    }
}
"#,
    );
    assert_eq!(out, vec!["2", "false", "true", "alice", "missing"]);
}

/// Create a Map<String, String>, call .keys(), iterate and print.
#[test]
fn map_string_string_keys_iterate() {
    let out = compile_and_run(
        r#"
function main() {
    let m = {"greeting": "hello", "target": "world"}
    let ks = m.keys()
    print(ks.length())
    for k in ks {
        print(k)
    }
}
"#,
    );
    assert_eq!(out.len(), 3); // length line + 2 key lines
    assert_eq!(out[0], "2");
    assert!(out.contains(&"greeting".to_string()));
    assert!(out.contains(&"target".to_string()));
}

#[test]
fn map_int_int() {
    let out = compile_and_run(
        r#"
function main() {
    let m = {1: 10, 2: 20, 3: 30}
    print(m.length())
    match m.get(2) {
        Some(v) -> print(v)
        None -> print(-1)
    }
    let m2 = m.set(4, 40)
    print(m2.length())
    print(m2.contains(4))
    print(m2.contains(5))
    let m3 = m2.remove(1)
    print(m3.length())
    print(m3.contains(1))
}
"#,
    );
    assert_eq!(out, vec!["3", "20", "4", "true", "false", "3", "false"]);
}
