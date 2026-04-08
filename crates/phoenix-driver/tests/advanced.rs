mod common;
use common::*;

// --- B2: lexer EOF escape ---

#[test]
fn unterminated_string_with_trailing_backslash() {
    // Ensure the lexer doesn't panic on a backslash at EOF inside a string.
    let tokens = phoenix_lexer::tokenize("\"hello\\", phoenix_common::span::SourceId(0));
    assert!(
        tokens
            .iter()
            .any(|t| t.kind == phoenix_lexer::TokenKind::Error)
    );
}

// --- Lexer: range operator tokenization ---

#[test]
fn lexer_range_operator() {
    let tokens = phoenix_lexer::tokenize("0..10", phoenix_common::span::SourceId(0));
    let kinds: Vec<_> = tokens.iter().map(|t| t.kind).collect();
    assert_eq!(
        kinds,
        vec![
            phoenix_lexer::TokenKind::IntLiteral,
            phoenix_lexer::TokenKind::DotDot,
            phoenix_lexer::TokenKind::IntLiteral,
            phoenix_lexer::TokenKind::Eof,
        ]
    );
}

#[test]
fn destructuring_partial_fields() {
    run_expect(
        r#"
struct Point { Int x  Int y  Int z }
function main() {
    let p: Point = Point(1, 2, 3)
    let Point { x, z } = p
    print(x)
    print(z)
}
"#,
        &["1", "3"],
    );
}

// ── Destructuring edge cases ────────────────────────────────────────────

#[test]
fn destructuring_with_mut() {
    run_expect(
        r#"
struct Point { Int x  Int y }
function main() {
    let p: Point = Point(1, 2)
    let mut Point { x, y } = p
    x = 10
    print(x)
    print(y)
}
"#,
        &["10", "2"],
    );
}

// ── Cross-feature: destructuring + function return ──────────────────────

#[test]
fn destructuring_in_loop_body() {
    run_expect(
        r#"
struct Point { Int x  Int y }
function main() {
    let points: List<Point> = [Point(1, 2), Point(3, 4)]
    for p in points {
        let Point { x, y } = p
        print(x + y)
    }
}
"#,
        &["3", "7"],
    );
}

#[test]
fn default_param_expression() {
    run_expect(
        r#"
function foo(x: Int, y: Int = 2 + 3) -> Int { x + y }
function main() {
    print(foo(10))
    print(foo(10, 1))
}
"#,
        &["15", "11"],
    );
}

// ── Tier A: Bug fix tests ────────────────────────────────────────────────

#[test]
fn substring_negative_start_index() {
    expect_runtime_error(
        r#"
function main() {
    let s: String = "hello"
    print(s.substring(-1, 3))
}
"#,
        "non-negative",
    );
}

#[test]
fn substring_negative_end_index() {
    expect_runtime_error(
        r#"
function main() {
    let s: String = "hello"
    print(s.substring(0, -1))
}
"#,
        "non-negative",
    );
}
