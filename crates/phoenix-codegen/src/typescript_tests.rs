//! Inline tests for the TypeScript generator, split out of `typescript.rs`
//! to keep the generator file readable (each feature slice grows both
//! halves). Declared as `mod tests` inside `typescript.rs` via `#[path]` so
//! the module path — and therefore every insta snapshot name — is unchanged
//! by the move.

use super::*;
use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

/// An interior empty doc line renders as a bare ` *` row (no trailing space)
/// inside the JSDoc block, matching Prettier. Guards the empty-line branch of
/// `render_jsdoc`, which the doc-comment integration tests don't hit.
#[test]
fn render_jsdoc_blanks_out_empty_lines() {
    assert_eq!(
        render_jsdoc("  ", "first\n\nthird"),
        "  /**\n   * first\n   *\n   * third\n   */\n"
    );
}

/// A whitespace-only doc line is trimmed to a bare ` *` row too (no trailing
/// space), exactly like a truly empty line — `render_jsdoc` trims per line,
/// matching the Go/Python sibling helpers rather than only special-casing the
/// `is_empty()` case.
#[test]
fn render_jsdoc_trims_whitespace_only_lines() {
    assert_eq!(
        render_jsdoc("  ", "first\n   \nthird"),
        "  /**\n   * first\n   *\n   * third\n   */\n"
    );
}

/// Parses, type-checks, and generates TypeScript from a Phoenix source string.
fn generate_from_source(source: &str) -> GeneratedFiles {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(
        parse_errors.is_empty(),
        "unexpected parse errors: {parse_errors:?}"
    );
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "unexpected check errors: {:?}",
        result.diagnostics
    );
    generate_typescript(&program, &result)
}

// ── types.ts tests ──────────────────────────────────────────────

#[test]
fn struct_to_interface() {
    let files = generate_from_source(
        r#"
/** A registered user */
struct User {
    id: Int
    name: String
    email: String
    bio: Option<String>
}
"#,
    );
    insta::assert_snapshot!("struct_to_interface_types", files.types);
}

#[test]
fn simple_enum_to_union() {
    let files = generate_from_source(
        r#"
/** User roles */
enum Role { Admin  Editor  Viewer }
"#,
    );
    insta::assert_snapshot!("simple_enum_to_union_types", files.types);
}

#[test]
fn tagged_enum_to_union() {
    let files = generate_from_source(
        r#"
enum Shape {
    Circle(Float)
    Rect(Float, Float)
    Point
}
"#,
    );
    insta::assert_snapshot!("tagged_enum_to_union_types", files.types);
}

// ── client.ts tests ─────────────────────────────────────────────

#[test]
fn get_with_path_param() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint getUser: GET "/api/users/{id}" {
    response User
}
"#,
    );
    insta::assert_snapshot!("get_with_path_param_types", files.types);
    insta::assert_snapshot!("get_with_path_param_client", files.client);
    insta::assert_snapshot!("get_with_path_param_handlers", files.handlers);
}

/// An `api version` block prefixes the path in BOTH the generated client's
/// request URL and the generated server's route registration. The roundtrip
/// harness proves the two agree; this pins the actual literal string so a
/// regression that emitted a wrong-but-consistent prefix (which the
/// roundtrip would not catch) is caught here.
#[test]
fn api_version_prefixes_generated_path() {
    let files = generate_from_source(
        r#"
struct Post { id: Int }
api version "v2" {
    endpoint listTaggedPosts: GET "/api/posts/tagged/{tag}" { response Post }
}
"#,
    );
    assert!(
        files.client.contains("/v2/api/posts/tagged/"),
        "client URL should carry the version prefix, got: {}",
        files.client
    );
    assert!(
        files.server.contains("/v2/api/posts/tagged/"),
        "server route should carry the version prefix, got: {}",
        files.server
    );
    // The unprefixed path must not leak alongside the prefixed one.
    assert!(
        !files.client.contains("\"/api/posts/tagged/")
            && !files.client.contains("`/api/posts/tagged/"),
        "client should not also emit the unprefixed path, got: {}",
        files.client
    );
}

#[test]
fn post_with_body_omit() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String  email: String }
endpoint createUser: POST "/api/users" {
    body User omit { id }
    response User
}
"#,
    );
    insta::assert_snapshot!("post_with_body_omit_types", files.types);
    insta::assert_snapshot!("post_with_body_omit_client", files.client);
    insta::assert_snapshot!("post_with_body_omit_handlers", files.handlers);
}

#[test]
fn patch_with_partial() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String  email: String  age: Int }
endpoint updateUser: PATCH "/api/users/{id}" {
    body User omit { id } partial
    response User
}
"#,
    );
    insta::assert_snapshot!("patch_with_partial_types", files.types);
    insta::assert_snapshot!("patch_with_partial_client", files.client);
}

#[test]
fn get_with_query_params_all_optional() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint listUsers: GET "/api/users" {
    query {
        page: Int = 1
        limit: Int = 20
        search: Option<String>
    }
    response List<User>
}
"#,
    );
    insta::assert_snapshot!("get_with_query_all_optional_types", files.types);
    insta::assert_snapshot!("get_with_query_all_optional_client", files.client);
    insta::assert_snapshot!("get_with_query_all_optional_handlers", files.handlers);
}

#[test]
fn get_with_query_params_some_required() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint searchUsers: GET "/api/users/search" {
    query {
        term: String
        page: Int = 1
    }
    response List<User>
}
"#,
    );
    insta::assert_snapshot!("get_with_query_some_required_client", files.client);
}

#[test]
fn endpoint_with_errors() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String  email: String }
endpoint createUser: POST "/api/users" {
    body User omit { id }
    response User
    error {
        ValidationError(400)
        Conflict(409)
    }
}
"#,
    );
    insta::assert_snapshot!("endpoint_with_errors_types", files.types);
    insta::assert_snapshot!("endpoint_with_errors_client", files.client);
}

#[test]
fn void_response() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint deleteUser: DELETE "/api/users/{id}" {
}
"#,
    );
    insta::assert_snapshot!("void_response_client", files.client);
    insta::assert_snapshot!("void_response_handlers", files.handlers);
}

#[test]
fn list_response_type() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint listUsers: GET "/api/users" {
    response List<User>
}
"#,
    );
    // Should import User (not List) from types
    insta::assert_snapshot!("list_response_client", files.client);
}

#[test]
fn multiple_endpoints() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String  email: String }
endpoint listUsers: GET "/api/users" {
    response List<User>
}
endpoint createUser: POST "/api/users" {
    body User omit { id }
    response User
}
endpoint getUser: GET "/api/users/{id}" {
    response User
}
endpoint deleteUser: DELETE "/api/users/{id}" {
}
"#,
    );
    insta::assert_snapshot!("multiple_endpoints_types", files.types);
    insta::assert_snapshot!("multiple_endpoints_client", files.client);
    insta::assert_snapshot!("multiple_endpoints_handlers", files.handlers);
}

#[test]
fn doc_comments_passthrough() {
    let files = generate_from_source(
        r#"
/** A registered user */
struct User { id: Int  name: String }
/** Create a new user */
endpoint createUser: POST "/api/users" {
    body User omit { id }
    response User
}
"#,
    );
    insta::assert_snapshot!("doc_comments_types", files.types);
    insta::assert_snapshot!("doc_comments_client", files.client);
    insta::assert_snapshot!("doc_comments_handlers", files.handlers);
}

/// A multi-line doc comment must expand to a JSDoc block with every line on
/// its own ` * ` row, not a single `/** ... */` whose continuation lines
/// leak out of the comment as code. Regression guard for `render_jsdoc`.
#[test]
fn multiline_doc_comment_expands_to_jsdoc_block() {
    let files = generate_from_source(
        r#"
/**
 * Fetch a widget by id
 * with extra detail on the second line
 */
endpoint getWidget: GET "/api/widgets/{id}" {
    response Widget
}
struct Widget { id: Int }
"#,
    );
    assert!(
        files
            .client
            .contains("   * Fetch a widget by id\n   * with extra detail on the second line\n"),
        "multi-line doc must be a JSDoc block:\n{}",
        files.client
    );
    // The continuation line must never appear outside the comment.
    assert!(
        !files.client.contains("\n  with extra detail"),
        "continuation doc line leaked as code:\n{}",
        files.client
    );
}

#[test]
fn selective_partial() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String  email: String  age: Int }
endpoint patchUser: PATCH "/api/users/{id}" {
    body User omit { id } partial { email, age }
    response User
}
"#,
    );
    insta::assert_snapshot!("selective_partial_types", files.types);
}

// ── Edge case tests ─────────────────────────────────────────────

/// Minimal endpoint: no body, no response, no query, no errors.
#[test]
fn minimal_endpoint() {
    let files = generate_from_source(
        r#"
endpoint healthCheck: GET "/api/health" {
}
"#,
    );
    insta::assert_snapshot!("minimal_endpoint_client", files.client);
    insta::assert_snapshot!("minimal_endpoint_handlers", files.handlers);
}

/// Enum used as response type.
#[test]
fn enum_response_type() {
    let files = generate_from_source(
        r#"
enum Status { Active  Inactive  Banned }
endpoint getStatus: GET "/api/status" {
    response Status
}
"#,
    );
    insta::assert_snapshot!("enum_response_client", files.client);
    insta::assert_snapshot!("enum_response_handlers", files.handlers);
}

/// `pick` modifier without `omit`.
#[test]
fn body_pick_only() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String  email: String  age: Int }
endpoint updateEmail: PATCH "/api/users/{id}/email" {
    body User pick { email }
    response User
}
"#,
    );
    insta::assert_snapshot!("body_pick_only_types", files.types);
    insta::assert_snapshot!("body_pick_only_client", files.client);
}

/// Endpoint with multiple path parameters.
#[test]
fn multiple_path_params() {
    let files = generate_from_source(
        r#"
struct Comment { id: Int  text: String }
endpoint getComment: GET "/api/users/{userId}/posts/{postId}" {
    response Comment
}
"#,
    );
    insta::assert_snapshot!("multiple_path_params_client", files.client);
    insta::assert_snapshot!("multiple_path_params_handlers", files.handlers);
}

/// `Option<User>` response type.
#[test]
fn option_response_type() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint findUser: GET "/api/users/{id}" {
    response Option<User>
}
"#,
    );
    insta::assert_snapshot!("option_response_client", files.client);
}

/// `Map<String, User>` response type.
#[test]
fn map_response_type() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint getUserMap: GET "/api/users/map" {
    response Map<String, User>
}
"#,
    );
    insta::assert_snapshot!("map_response_client", files.client);
}

/// Query params with a required (no default, non-optional) param.
#[test]
fn query_required_param() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint searchUsers: GET "/api/users/search" {
    query {
        term: String
        page: Int = 1
    }
    response List<User>
}
"#,
    );
    insta::assert_snapshot!("query_required_param_client", files.client);
    insta::assert_snapshot!("query_required_param_handlers", files.handlers);
}

/// Single error variant (not multiple).
#[test]
fn single_error_variant() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint getUser: GET "/api/users/{id}" {
    response User
    error { NotFound(404) }
}
"#,
    );
    insta::assert_snapshot!("single_error_types", files.types);
    insta::assert_snapshot!("single_error_client", files.client);
}

/// Endpoint with body but no response.
#[test]
fn body_no_response() {
    let files = generate_from_source(
        r#"
struct Feedback { message: String  rating: Int }
endpoint submitFeedback: POST "/api/feedback" {
    body Feedback
}
"#,
    );
    insta::assert_snapshot!("body_no_response_client", files.client);
    insta::assert_snapshot!("body_no_response_handlers", files.handlers);
}

/// Struct with `Option<T>` fields generates optional properties in the interface.
#[test]
fn struct_with_optional_fields() {
    let files = generate_from_source(
        r#"
struct Profile {
    id: Int
    name: String
    bio: Option<String>
    age: Option<Int>
}
"#,
    );
    insta::assert_snapshot!("struct_optional_fields_types", files.types);
}

// ── server.ts tests ─────────────────────────────────────────────

/// Server router for a GET endpoint with path params.
#[test]
fn server_get_with_path_param() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint getUser: GET "/api/users/{id}" {
    response User
}
"#,
    );
    insta::assert_snapshot!("server_get_path_param", files.server);
}

/// Server router for a POST endpoint with body and errors.
#[test]
fn server_post_with_body_and_errors() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String  email: String }
endpoint createUser: POST "/api/users" {
    body User omit { id }
    response User
    error {
        ValidationError(400)
        Conflict(409)
    }
}
"#,
    );
    insta::assert_snapshot!("server_post_body_errors", files.server);
}

/// Server router for a GET endpoint with query params and defaults.
#[test]
fn server_get_with_query_params() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint listUsers: GET "/api/users" {
    query {
        page: Int = 1
        limit: Int = 20
        search: Option<String>
    }
    response List<User>
}
"#,
    );
    insta::assert_snapshot!("server_get_query_params", files.server);
}

/// Server router for a DELETE endpoint (void response).
#[test]
fn server_delete_void() {
    let files = generate_from_source(
        r#"
endpoint deleteUser: DELETE "/api/users/{id}" {
    error { NotFound(404) }
}
"#,
    );
    insta::assert_snapshot!("server_delete_void", files.server);
}

/// Server router with multiple endpoints.
#[test]
fn server_multiple_endpoints() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String  email: String }
endpoint listUsers: GET "/api/users" {
    response List<User>
}
endpoint createUser: POST "/api/users" {
    body User omit { id }
    response User
}
endpoint getUser: GET "/api/users/{id}" {
    response User
}
endpoint deleteUser: DELETE "/api/users/{id}" {
}
"#,
    );
    insta::assert_snapshot!("server_multiple_endpoints", files.server);
}

/// Server router for PATCH endpoint with body + path param.
#[test]
fn server_patch_with_body() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String  email: String  age: Int }
endpoint updateUser: PATCH "/api/users/{id}" {
    body User omit { id } partial
    response User
    error { NotFound(404) }
}
"#,
    );
    insta::assert_snapshot!("server_patch_body", files.server);
}

/// Server router for endpoint with both query params and body.
#[test]
fn server_query_and_body() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String  email: String }
endpoint createUser: POST "/api/users" {
    query { notify: Bool = false }
    body User omit { id }
    response User
}
"#,
    );
    insta::assert_snapshot!("server_query_and_body", files.server);
}

/// Server router for endpoint with multiple path params.
#[test]
fn server_multiple_path_params() {
    let files = generate_from_source(
        r#"
struct Comment { id: Int  text: String }
endpoint getComment: GET "/api/users/{userId}/posts/{postId}" {
    response Comment
}
"#,
    );
    insta::assert_snapshot!("server_multiple_path_params", files.server);
}

/// Server router for endpoint with string and bool query defaults.
#[test]
fn server_string_and_bool_defaults() {
    let files = generate_from_source(
        r#"
struct Item { id: Int  name: String }
endpoint listItems: GET "/api/items" {
    query {
        sort: String = "name"
        ascending: Bool = true
        limit: Int = 50
    }
    response List<Item>
}
"#,
    );
    insta::assert_snapshot!("server_string_bool_defaults", files.server);
}

/// Server for minimal endpoint (no body/query/response/errors).
#[test]
fn server_minimal_endpoint() {
    let files = generate_from_source(
        r#"
endpoint healthCheck: GET "/api/health" {
}
"#,
    );
    insta::assert_snapshot!("server_minimal_endpoint", files.server);
}

// ── Where constraint codegen tests ──────────────────────────────

/// Validation function generated for endpoint with constrained body fields.
#[test]
fn validation_function_numeric_and_string() {
    let files = generate_from_source(
        r#"
struct User {
    id: Int
    name: String where self.length > 0 && self.length <= 100
    age: Int where self >= 0 && self <= 150
}
endpoint createUser: POST "/api/users" {
    body User omit { id }
    response User
}
"#,
    );
    insta::assert_snapshot!("validation_numeric_string_types", files.types);
}

/// Validation function with `self.contains()` constraint.
#[test]
fn validation_function_contains() {
    let files = generate_from_source(
        r#"
struct User {
    id: Int
    email: String where self.contains("@") && self.length > 3
}
endpoint createUser: POST "/api/users" {
    body User omit { id }
    response User
}
"#,
    );
    insta::assert_snapshot!("validation_contains_types", files.types);
}

/// Server.ts calls validation function when body has constraints.
#[test]
fn server_calls_validation() {
    let files = generate_from_source(
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
    insta::assert_snapshot!("server_with_validation", files.server);
}

/// No validation function when body has no constraints.
#[test]
fn no_validation_without_constraints() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
    body User omit { id }
    response User
}
"#,
    );
    assert!(
        !files.types.contains("validate"),
        "should not emit validation function when no constraints"
    );
}

/// Validation function with `or` constraint.
#[test]
fn validation_or_constraint() {
    let files = generate_from_source(
        r#"
struct Range { id: Int  x: Int where self < 0 || self > 100 }
endpoint create: POST "/api/ranges" {
    body Range omit { id }
    response Range
}
"#,
    );
    insta::assert_snapshot!("validation_or_types", files.types);
}

/// Validation with optional field (partial) that has constraint.
#[test]
fn validation_optional_constrained_field() {
    let files = generate_from_source(
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
    insta::assert_snapshot!("validation_optional_types", files.types);
}

/// A constrained `Option<T>` body field must be `typeof`-narrowed before its
/// constraint is checked, exactly like the source struct's validator — its
/// raw type is `Option<String>` (no primitive `typeof`), so without unwrapping
/// it the body validator emitted `!(obj.x.length …)` on an un-narrowed
/// `unknown`, which fails to compile (`tsc` TS18047 "possibly null" / TS2339
/// "no `length` on `{}`"). The field is also skippable when absent even though
/// no `partial` applied (Option ⇒ optional). Regression guard for the
/// struct/body validator `Option` drift fixed in [`validation_field`].
#[test]
fn validation_option_constrained_body_field() {
    let files = generate_from_source(
        r#"
struct Account {
    id: Int
    displayName: Option<String> where self.length <= 60
}
endpoint updateAccount: PATCH "/api/accounts/{id}" {
    body Account omit { id }
    response Account
}
"#,
    );
    // The Option field is narrowed to `string` before the constraint runs,
    // and skipped when absent (`!== undefined`) despite no `partial`.
    assert!(
        files.types.contains(
            "if (obj.displayName !== undefined && typeof obj.displayName !== \"string\")"
        ),
        "Option body field must be typeof-narrowed and undefined-skipped:\n{}",
        files.types
    );
    assert!(
        files
            .types
            .contains("if (obj.displayName !== undefined && !(obj.displayName.length <= 60))"),
        "Option body field constraint must be guarded + dereferenced:\n{}",
        files.types
    );
}

/// Multiple endpoints with constraints — only one ValidationError class.
#[test]
fn validation_single_error_class() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String where self.length > 0 }
struct Item { id: Int  price: Int where self > 0 }
endpoint createUser: POST "/api/users" {
    body User omit { id }
    response User
}
endpoint createItem: POST "/api/items" {
    body Item omit { id }
    response Item
}
"#,
    );
    let count = files.types.matches("class ValidationError").count();
    assert_eq!(
        count, 1,
        "ValidationError class should be emitted exactly once"
    );
    assert!(files.types.contains("validateCreateUserBody"));
    assert!(files.types.contains("validateCreateItemBody"));
}

// ── Struct-level validation tests ──────────────────────────────

/// Struct-level validate function for constrained fields.
#[test]
fn struct_validation_numeric_and_string() {
    let files = generate_from_source(
        r#"
struct User {
    id: Int
    name: String where self.length > 0 && self.length <= 100
    age: Int where self >= 0 && self <= 150
}
"#,
    );
    insta::assert_snapshot!("struct_validation_types", files.types);
}

/// Struct-level validate function with `contains` constraint.
#[test]
fn struct_validation_contains() {
    let files = generate_from_source(
        r#"
struct User {
    id: Int
    email: String where self.contains("@") && self.length > 3
}
"#,
    );
    insta::assert_snapshot!("struct_validation_contains_types", files.types);
}

/// A bare method-call constraint negates *without* redundant parens: `!` binds
/// looser than member/call, so `!obj.path.includes("/")` is correct and
/// `!(obj.path.includes("/"))` is what `prettier --check` rejects. (The compound
/// `&&` case above keeps its parens — `!` over a low-precedence operator needs
/// them.) Regression for the redundant-parens prettier divergence.
#[test]
fn struct_validation_bare_method_call_constraint_has_no_redundant_parens() {
    let files = generate_from_source(
        r#"
struct Key {
    path: String where self.contains("/")
}
"#,
    );
    assert!(
        files.types.contains(r#"if (!obj.path.includes("/"))"#),
        "expected a paren-free negated method-call guard:\n{}",
        files.types
    );
    assert!(
        !files.types.contains(r#"!(obj.path.includes("/"))"#),
        "negated method-call constraint kept redundant parens (prettier rejects):\n{}",
        files.types
    );
}

/// A constraint that is itself a `!x` collapses to `x` in the violation guard
/// rather than the double negation `!!x` — `where !self` on a Bool field guards
/// with `if (obj.active)`, not `if (!!obj.active)` (which eslint's
/// `no-unnecessary-condition`/`no-extra-boolean-cast` flags). Regression for the
/// double-negation guard.
#[test]
fn struct_validation_negated_constraint_collapses_double_negation() {
    let files = generate_from_source(
        r#"
struct Flag {
    active: Bool where !self
}
"#,
    );
    assert!(
        files.types.contains("if (obj.active) throw"),
        "expected the `!x` constraint to collapse to a bare `obj.active` guard:\n{}",
        files.types
    );
    assert!(
        !files.types.contains("!!obj.active"),
        "negated `!x` constraint produced a double negation `!!obj.active`:\n{}",
        files.types
    );
}

/// A `!(a || b)` constraint collapses to the bare `a || b` violation guard — NOT
/// the `!!(a || b)` double negation eslint flags. Because the collapsed guard is
/// `||`-rooted (looser than `&&`), it must be parenthesized ONLY where it's
/// AND-joined with an optional field's presence check, and left bare as a
/// standalone guard so prettier doesn't reject a redundant `if ((a || b))`:
///
///   * required `n` → `if (obj.n < 0 || obj.n > 100)` (bare, no parens)
///   * optional `m` → `if (obj.m !== undefined && (obj.m.length < 3 || …))`
///
/// Regression for the `||`-rooted negated-constraint double-negation / conjunct
/// precedence gap.
#[test]
fn struct_validation_negated_or_constraint_parenthesizes_only_as_conjunct() {
    let files = generate_from_source(
        r#"
struct Bounded {
    n: Int where !(self < 0 || self > 100)
    m: Option<String> where !(self.length < 3 || self.length > 20)
}
"#,
    );
    // No double negation anywhere.
    assert!(
        !files.types.contains("!!"),
        "negated `!(a || b)` constraint produced a double negation:\n{}",
        files.types
    );
    // Required field: bare guard, no redundant `if ((…))` parens.
    assert!(
        files.types.contains("if (obj.n < 0 || obj.n > 100)"),
        "expected a bare `||` guard for the required field:\n{}",
        files.types
    );
    assert!(
        !files.types.contains("if ((obj.n"),
        "standalone `||` guard kept redundant parens (prettier rejects):\n{}",
        files.types
    );
    // Optional field: the `||` guard is parenthesized so the `&&` conjunction
    // parses as `presence && (a || b)`, not `(presence && a) || b`.
    assert!(
        files
            .types
            .contains("(obj.m.length < 3 || obj.m.length > 20)"),
        "expected the optional-field `||` guard to be parenthesized as a conjunct:\n{}",
        files.types
    );
    assert!(
        files.types.contains("obj.m !== undefined"),
        "expected an optional-field presence check:\n{}",
        files.types
    );
}

/// No struct-level validation when no fields have constraints.
#[test]
fn no_struct_validation_without_constraints() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
"#,
    );
    assert!(
        !files.types.contains("validateUser"),
        "should not emit struct validation function when no constraints"
    );
}

// ── dyn Trait handling ───────────────────────────────────────────

/// `dyn Trait` erases to the trait name on the TypeScript side —
/// structural interface dispatch handles the runtime variance.
#[test]
fn dyn_type_erases_to_trait_name() {
    use phoenix_sema::types::Type;
    assert_eq!(type_to_ts(&Type::Dyn("Drawable".to_string())), "Drawable");
}

/// End-to-end: a parser-level `TypeExpr::Dyn` erases to the trait
/// name at the parser-expr codegen site too (exercises
/// `type_expr_to_ts`, not just `type_to_ts`).
#[test]
fn dyn_type_expr_erases_to_trait_name() {
    use phoenix_common::span::Span;
    use phoenix_parser::ast::{DynType, TypeExpr};
    let te = TypeExpr::Dyn(DynType {
        trait_name: "Drawable".to_string(),
        span: Span::BUILTIN,
    });
    assert_eq!(type_expr_to_ts(&te), "Drawable");
}

// ── header tests ─────────────────────────────────────────────────

/// A required request header with an auto-derived wire name
/// (`idempotencyKey` → `Idempotency-Key`): added to the client `headers`
/// param and the handler `headers` arg, sent via a `Headers` instance, and
/// read server-side with `req.header(...)` and the exact wire name.
#[test]
fn request_header_auto_wire_name() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
    headers { idempotencyKey: String }
    body User
    response User
}
"#,
    );
    insta::assert_snapshot!("request_header_auto_client", files.client);
    insta::assert_snapshot!("request_header_auto_handlers", files.handlers);
    insta::assert_snapshot!("request_header_auto_server", files.server);
}

/// An `as "..."` override pins the wire name verbatim on both the client
/// send (`requestHeaders.set("X-Api-Key", ...)`) and the server read
/// (`req.header("X-Api-Key")`), while the idiomatic camelCase local
/// (`apiKey`) names the field.
#[test]
fn request_header_as_override() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint listUsers: GET "/api/users" {
    headers { apiKey: String as "X-Api-Key" }
    response List<User>
}
"#,
    );
    insta::assert_snapshot!("request_header_override_client", files.client);
    insta::assert_snapshot!("request_header_override_server", files.server);
}

/// An optional (`Option<T>`) request header: the client `headers` param and
/// field are optional, the send is guarded with `!== undefined`, and the
/// server coercion maps a missing header to `undefined`.
#[test]
fn request_header_optional() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint listUsers: GET "/api/users" {
    headers { traceId: Option<String> }
    response List<User>
}
"#,
    );
    insta::assert_snapshot!("request_header_optional_client", files.client);
    insta::assert_snapshot!("request_header_optional_handlers", files.handlers);
    insta::assert_snapshot!("request_header_optional_server", files.server);
}

/// A `Bool` request header serializes via `String(...)`, which emits
/// lowercase `true`/`false` — the cross-language wire convention every
/// backend agrees on (Go `strconv.FormatBool`, Python `"true"/"false"`), and
/// the server reads it back with `=== "true"`. Locks the convention on the
/// TS side (and proves the send/read pair is internally consistent).
#[test]
fn bool_request_header_serializes_lowercase() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint listUsers: GET "/api/users" {
    headers { debug: Bool }
    response List<User>
}
"#,
    );
    assert!(
        files
            .client
            .contains("requestHeaders.set(\"Debug\", String(headers.debug));"),
        "bool header must serialize via String(...) (lowercase true/false):\n{}",
        files.client
    );
    assert!(
        files.server.contains("req.header(\"Debug\") === \"true\""),
        "server must read the bool header with a lowercase `=== \"true\"` check:\n{}",
        files.server
    );
}

/// A request header with a literal default is a client-optional field
/// (`maxStale?: number`); the server coercion applies the default when the
/// header is absent (`req.header(...) !== undefined ? Number(...) : 60`).
#[test]
fn defaulted_request_header_applies_server_default() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint listUsers: GET "/api/users" {
    headers { maxStale: Int = 60 }
    response List<User>
}
"#,
    );
    assert!(
        files.client.contains("maxStale?: number"),
        "defaulted header must be an optional client field:\n{}",
        files.client
    );
    // Prettier may wrap the ternary across lines, so check the pieces rather
    // than a single-line form: the absent-header guard, the coercion, and the
    // default applied in the else branch.
    assert!(
        files
            .server
            .contains("req.header(\"Max-Stale\") !== undefined"),
        "server must guard on the header being absent:\n{}",
        files.server
    );
    assert!(
        files.server.contains("Number(req.header(\"Max-Stale\"))"),
        "server must coerce the present header to a number:\n{}",
        files.server
    );
    assert!(
        files.server.contains(": 60"),
        "server must apply the default in the else branch:\n{}",
        files.server
    );
}

/// A response header produces a `<Endpoint>Result` envelope: the type bundles
/// `body` + the typed header; the client returns the envelope (reading the
/// header off `response.headers`); the handler resolves it; and the server
/// sets the header before `res.json(result.body)`.
#[test]
fn response_header_envelope() {
    let files = generate_from_source(
        r#"
struct Post { id: Int  title: String }
endpoint getPost: GET "/api/posts/{id}" {
    response Post headers {
        ratelimitRemaining: Int as "X-RateLimit-Remaining"
        requestId: Option<String>
    }
}
"#,
    );
    insta::assert_snapshot!("response_header_envelope_types", files.types);
    insta::assert_snapshot!("response_header_envelope_client", files.client);
    insta::assert_snapshot!("response_header_envelope_handlers", files.handlers);
    insta::assert_snapshot!("response_header_envelope_server", files.server);
}

/// Regression guard: an endpoint with response headers returns the envelope
/// but the client STILL casts the decoded JSON to the bare body type
/// (`(await response.json()) as Post`), so the client must import BOTH the
/// envelope and the body type. The bug this guards against imported only the
/// envelope, leaving `Post` undefined — invisible whenever another endpoint
/// happens to import the body type, so it must be exercised in isolation
/// (this endpoint is the sole user of `Post`).
#[test]
fn response_header_envelope_imports_body_type() {
    let files = generate_from_source(
        r#"
struct Post { id: Int  title: String }
endpoint getPost: GET "/api/posts/{id}" {
    response Post headers { ratelimitRemaining: Int as "X-RateLimit-Remaining" }
}
"#,
    );
    assert!(
        files.client.contains("(await response.json()) as Post"),
        "client must cast the body to the bare response type:\n{}",
        files.client
    );
    assert!(
        files
            .client
            .contains("import type { GetPostResult, Post } from \"./types\";"),
        "client must import BOTH the envelope and the body type it casts to:\n{}",
        files.client
    );
}

// ── pagination tests ────────────────────────────────────────────

/// An offset-paginated endpoint produces a `<Endpoint>Page` envelope
/// `{ items: T[]; totalCount: number }`; the client returns and casts the
/// JSON body to the page; the handler resolves the page; and the server
/// sends it with `res.json(result)`.
#[test]
fn offset_pagination_envelope() {
    let files = generate_from_source(
        r#"
struct Post { id: Int  title: String }
endpoint listPosts: GET "/api/posts" {
    query { page: Int  limit: Int }
    response List<Post>
    pagination { offset }
}
"#,
    );
    insta::assert_snapshot!("offset_pagination_types", files.types);
    insta::assert_snapshot!("offset_pagination_client", files.client);
    insta::assert_snapshot!("offset_pagination_handlers", files.handlers);
    insta::assert_snapshot!("offset_pagination_server", files.server);

    // Interface shape.
    assert!(
        files.types.contains(
            "export interface ListPostsPage {\n  items: Post[];\n  totalCount: number;\n}"
        ),
        "offset envelope must be {{ items: T[]; totalCount: number }}:\n{}",
        files.types
    );
    // Client return type + typed JSON cast to the page.
    assert!(
        files.client.contains("Promise<ListPostsPage>"),
        "client method must return Promise<ListPostsPage>:\n{}",
        files.client
    );
    assert!(
        files
            .client
            .contains("return (await response.json()) as ListPostsPage;"),
        "client must cast the JSON body to the page:\n{}",
        files.client
    );
    // Handler return type.
    assert!(
        files.handlers.contains("Promise<ListPostsPage>"),
        "handler must return Promise<ListPostsPage>:\n{}",
        files.handlers
    );
    // Imports reference only the page type, never the bare item type in the
    // client (the JSON is read as the whole page object).
    assert!(
        files
            .client
            .contains("import type { ListPostsPage } from \"./types\";"),
        "client must import only the page type:\n{}",
        files.client
    );
}

/// A cursor-paginated endpoint produces `{ items: T[]; nextCursor?: string }`
/// (the cursor is optional — absent on the last page).
#[test]
fn cursor_pagination_envelope() {
    let files = generate_from_source(
        r#"
struct Post { id: Int  title: String }
endpoint listPosts: GET "/api/posts" {
    query { cursor: Option<String>  limit: Int }
    response List<Post>
    pagination { cursor }
}
"#,
    );
    insta::assert_snapshot!("cursor_pagination_types", files.types);
    insta::assert_snapshot!("cursor_pagination_client", files.client);
    insta::assert_snapshot!("cursor_pagination_handlers", files.handlers);

    // Interface shape: optional nextCursor.
    assert!(
        files.types.contains(
            "export interface ListPostsPage {\n  items: Post[];\n  nextCursor?: string;\n}"
        ),
        "cursor envelope must be {{ items: T[]; nextCursor?: string }}:\n{}",
        files.types
    );
    assert!(
        files.client.contains("Promise<ListPostsPage>"),
        "client method must return Promise<ListPostsPage>:\n{}",
        files.client
    );
    assert!(
        files.handlers.contains("Promise<ListPostsPage>"),
        "handler must return Promise<ListPostsPage>:\n{}",
        files.handlers
    );
}

/// A plain (non-paginated) `List<T>` response is byte-for-byte unchanged:
/// no `Page` envelope, and both client and handler return the bare `T[]`.
#[test]
fn plain_list_response_unchanged_by_pagination() {
    let files = generate_from_source(
        r#"
struct Post { id: Int  title: String }
endpoint listPosts: GET "/api/posts" {
    response List<Post>
}
"#,
    );
    assert!(
        !files.types.contains("Page"),
        "no Page envelope for a non-paginated list:\n{}",
        files.types
    );
    assert!(
        files.client.contains("Promise<Post[]>"),
        "client must return the bare Post[]:\n{}",
        files.client
    );
    assert!(
        files.handlers.contains("Promise<Post[]>"),
        "handler must return the bare Post[]:\n{}",
        files.handlers
    );
}

// ── multipart / binary tests ────────────────────────────────────

/// Multipart upload (a `File` field + a scalar): client builds FormData and
/// omits Content-Type; server reads files/scalars from the multipart request;
/// the handler body param stays the body type (File field typed `Blob`).
#[test]
fn multipart_upload_client_server_handlers() {
    let files = generate_from_source(
        r#"
struct AvatarUpload { avatar: File  caption: String }
struct UploadResult { url: String }
endpoint uploadAvatar: POST "/api/avatar" {
    body AvatarUpload
    response UploadResult
}
"#,
    );
    // Client: FormData built, no Content-Type, body: formData.
    assert!(
        files.client.contains("const formData = new FormData();"),
        "client must build FormData:\n{}",
        files.client
    );
    assert!(
        files
            .client
            .contains("formData.append(\"avatar\", body.avatar);"),
        "client must append the file field:\n{}",
        files.client
    );
    assert!(
        files
            .client
            .contains("formData.append(\"caption\", String(body.caption));"),
        "client must stringify scalar fields:\n{}",
        files.client
    );
    assert!(
        !files.client.contains("JSON.stringify(body)"),
        "multipart client must not JSON.stringify:\n{}",
        files.client
    );
    assert!(
        !files.client.contains("\"Content-Type\""),
        "multipart client must not set Content-Type:\n{}",
        files.client
    );
    insta::assert_snapshot!("multipart_upload_client", files.client);
    insta::assert_snapshot!("multipart_upload_server", files.server);
    insta::assert_snapshot!("multipart_upload_handlers", files.handlers);
}

/// Optional-file upload (`Option<File>`): the FormData append is guarded.
#[test]
fn multipart_optional_file_upload() {
    let files = generate_from_source(
        r#"
struct MaybeUpload { avatar: Option<File>  caption: String }
endpoint upload: POST "/api/maybe" { body MaybeUpload }
"#,
    );
    assert!(
        files.client.contains("if (body.avatar !== undefined) {"),
        "optional file append must be guarded:\n{}",
        files.client
    );
    insta::assert_snapshot!("multipart_optional_client", files.client);
    insta::assert_snapshot!("multipart_optional_server", files.server);
}

/// An optional scalar in a multipart body (`Option<String>`): the server
/// must read it as `undefined` when absent rather than coercing the missing
/// form value to `NaN`/`false` — so an `Option<Int>` reads through a
/// `!== undefined ? Number(...) : undefined` guard.
#[test]
fn multipart_optional_scalar_guarded() {
    let files = generate_from_source(
        r#"
struct Upload { avatar: File  rotation: Option<Int> }
endpoint upload: POST "/api/upload" { body Upload }
"#,
    );
    // The formatter may wrap the ternary across lines, so assert on its
    // distinctive fragments rather than a single-line spelling.
    assert!(
        files
            .server
            .contains("multipart.body.rotation !== undefined")
            && files.server.contains("Number(multipart.body.rotation)")
            && files.server.contains(": undefined"),
        "optional multipart scalar must stay undefined when absent:\n{}",
        files.server
    );
    // The CLIENT must likewise guard the optional scalar: appending
    // `String(undefined)` would send the literal "undefined" string (which
    // the server then coerces to `NaN`). The required `avatar` file is
    // appended unconditionally.
    assert!(
        files.client.contains("if (body.rotation !== undefined) {")
            && files
                .client
                .contains("formData.append(\"rotation\", String(body.rotation));"),
        "optional multipart scalar must be guarded on the client:\n{}",
        files.client
    );
    assert!(
        files
            .client
            .contains("formData.append(\"avatar\", body.avatar);"),
        "required file must be appended unconditionally:\n{}",
        files.client
    );
    insta::assert_snapshot!("multipart_optional_scalar_client", files.client);
    insta::assert_snapshot!("multipart_optional_scalar_server", files.server);
}

/// Binary download (single-`File` response struct): client reads a Blob;
/// server sends a Buffer with an octet-stream content type; handler returns
/// `Buffer`. The response struct is NOT imported anywhere.
#[test]
fn binary_download_client_server_handlers() {
    let files = generate_from_source(
        r#"
struct Doc { data: File }
endpoint download: GET "/api/doc/{id}" { response Doc }
"#,
    );
    assert!(
        files.client.contains("): Promise<Blob> {"),
        "client returns Promise<Blob>:\n{}",
        files.client
    );
    assert!(
        files.client.contains("return await response.blob();"),
        "client reads response.blob():\n{}",
        files.client
    );
    assert!(
        !files.client.contains("Doc"),
        "binary client must not reference the response struct:\n{}",
        files.client
    );
    assert!(
        files.handlers.contains("Promise<Buffer>"),
        "handler returns Promise<Buffer>:\n{}",
        files.handlers
    );
    assert!(
        !files.handlers.contains("Doc"),
        "binary handler must not import the response struct:\n{}",
        files.handlers
    );
    assert!(
        files
            .server
            .contains("res.setHeader(\"Content-Type\", \"application/octet-stream\");"),
        "server sets octet-stream:\n{}",
        files.server
    );
    assert!(
        files.server.contains("res.send(result);"),
        "server sends the buffer:\n{}",
        files.server
    );
    insta::assert_snapshot!("binary_download_client", files.client);
    insta::assert_snapshot!("binary_download_server", files.server);
    insta::assert_snapshot!("binary_download_handlers", files.handlers);
}

/// Regression: an ordinary JSON endpoint in a schema that ALSO has a
/// multipart endpoint keeps its JSON body path (Content-Type + JSON.stringify
/// + req.body cast) untouched.
#[test]
fn json_endpoint_unaffected_by_multipart_sibling() {
    let files = generate_from_source(
        r#"
struct AvatarUpload { avatar: File  caption: String }
struct Note { id: Int  text: String }
endpoint uploadAvatar: POST "/api/avatar" { body AvatarUpload }
endpoint createNote: POST "/api/notes" { body Note  response Note }
"#,
    );
    assert!(
        files
            .client
            .contains("headers: { \"Content-Type\": \"application/json\" }"),
        "JSON sibling keeps its Content-Type:\n{}",
        files.client
    );
    assert!(
        files.client.contains("body: JSON.stringify(body)"),
        "JSON sibling keeps JSON.stringify:\n{}",
        files.client
    );
    assert!(
        files
            .server
            .contains("const body = req.body as CreateNoteBody;"),
        "JSON sibling keeps req.body cast:\n{}",
        files.server
    );
}

/// An endpoint that is BOTH a multipart upload AND a binary download
/// (`body_is_multipart` + `response_is_binary`): the client builds FormData
/// for the request yet reads the response as a `Blob`; the server assembles
/// the body from the multipart request yet streams a `Buffer` back. Guards
/// that the multipart-body and binary-response branches compose in one route.
#[test]
fn multipart_upload_with_binary_response() {
    let files = generate_from_source(
        r#"
struct AvatarUpload { avatar: File  caption: String }
struct Thumbnail { data: File }
endpoint convertAvatar: POST "/api/avatar/convert" {
    body AvatarUpload
    response Thumbnail
}
"#,
    );
    // Client: builds FormData for the request, returns a Blob for the response.
    assert!(
        files.client.contains("const formData = new FormData();")
            && files.client.contains("body: formData")
            && files.client.contains("): Promise<Blob> {")
            && files.client.contains("return await response.blob();"),
        "client must send FormData and read a Blob:\n{}",
        files.client
    );
    // Server: reads the multipart request, then streams a Buffer back.
    assert!(
        files
            .server
            .contains("const multipart = req as unknown as MultipartRequest;")
            && files
                .server
                .contains("res.setHeader(\"Content-Type\", \"application/octet-stream\");")
            && files.server.contains("res.send(result);"),
        "server must assemble from multipart and stream the buffer:\n{}",
        files.server
    );
    // Handler takes the body type and returns a Buffer.
    assert!(
        files.handlers.contains("Promise<Buffer>"),
        "handler must return Promise<Buffer>:\n{}",
        files.handlers
    );
}

/// A multi-status endpoint with a SHARED body type across statuses
/// (`response { 200: User  201: User }`): the generated `<Endpoint>Response`
/// envelope is `{ status: number; body?: User }`. Handler and client return
/// it; the client records `response.status` and parses the body into `body`;
/// the server writes the handler-chosen `result.status` (not a hardcoded 200).
#[test]
fn multi_status_shared_body() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint upsertUser: PUT "/api/users/{id}" {
    response {
        200: User
        201: User
    }
}
"#,
    );
    // Envelope interface: status: number + body?: User (optional).
    assert!(
        files
            .types
            .contains("export interface UpsertUserResponse {")
            && files.types.contains("  status: number;")
            && files.types.contains("  body?: User;"),
        "envelope must be {{ status: number; body?: User }}:\n{}",
        files.types
    );
    // Handler returns the envelope, not the bare body.
    assert!(
        files
            .handlers
            .contains("upsertUser(id: string): Promise<UpsertUserResponse>;"),
        "handler must return Promise<UpsertUserResponse>:\n{}",
        files.handlers
    );
    // Client returns the envelope, records the status, parses the body.
    assert!(
        files.client.contains("): Promise<UpsertUserResponse> {")
            && files
                .client
                .contains("responseBody = JSON.parse(responseText) as User;")
            && files
                .client
                .contains("return { status: response.status, body: responseBody };"),
        "client must build the envelope from response.status + parsed body:\n{}",
        files.client
    );
    // Server writes the handler-chosen status (NOT a hardcoded 200/204).
    assert!(
        files
            .server
            .contains("const result = await handlers.upsertUser(id);")
            && files
                .server
                .contains("res.status(result.status).json(result.body);")
            && !files.server.contains("res.status(204).end();"),
        "server must write result.status and encode result.body:\n{}",
        files.server
    );
    // The handler-chosen status is validated against the declared set; an
    // undeclared status (`res.status(0)` throws, a smuggled 4xx bypasses
    // `error { }`) is a handler bug → 500.
    assert!(
        files
            .server
            .contains("if (![200, 201].includes(result.status)) {")
            && files.server.contains("handler returned undeclared status"),
        "server must reject a handler status outside the declared set:\n{}",
        files.server
    );
    // Body-shape guard: every declared status is typed, so a missing body is
    // a handler bug; there is no typeless arm at all.
    assert!(
        files
            .server
            .contains("if ([200, 201].includes(result.status) && result.body === undefined) {")
            && files
                .server
                .contains("handler returned no body for a typed status")
            && !files.server.contains("bodyless status"),
        "server must reject a typed status without a body (no typeless arm):\n{}",
        files.server
    );
    insta::assert_snapshot!("multi_status_shared_body_types", files.types);
    insta::assert_snapshot!("multi_status_shared_body_client", files.client);
    insta::assert_snapshot!("multi_status_shared_body_handlers", files.handlers);
    insta::assert_snapshot!("multi_status_shared_body_server", files.server);
}

/// A multi-status endpoint mixing a TYPED status with a TYPELESS one
/// (`response { 200: User  204 }`): the envelope still carries the shared body
/// (`body?: User`) since at least one status is typed. The client only parses
/// the body when the response carries one (a 204 leaves it undefined); the
/// server writes the chosen status and encodes the body only when present.
#[test]
fn multi_status_mixed() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint updateUser: PUT "/api/users/{id}" {
    response {
        200: User
        204
    }
}
"#,
    );
    assert!(
        files
            .types
            .contains("export interface UpdateUserResponse {")
            && files.types.contains("  status: number;")
            && files.types.contains("  body?: User;"),
        "mixed block still carries body?: User:\n{}",
        files.types
    );
    assert!(
        files
            .handlers
            .contains("updateUser(id: string): Promise<UpdateUserResponse>;"),
        "handler must return Promise<UpdateUserResponse>:\n{}",
        files.handlers
    );
    // Body parse is guarded on CONTENT (not status code) so ANY typeless
    // status — 204 here, but equally a 202 — leaves body undefined instead
    // of throwing on empty JSON input.
    assert!(
        files
            .client
            .contains("const responseText = await response.text();")
            && files.client.contains("if (responseText) {")
            && files
                .client
                .contains("responseBody = JSON.parse(responseText) as User;"),
        "client must guard the body parse on content for typeless statuses:\n{}",
        files.client
    );
    // Server encodes the body only when present, writes the chosen status.
    assert!(
        files.server.contains("if (result.body !== undefined) {")
            && files
                .server
                .contains("res.status(result.status).json(result.body);")
            && files.server.contains("res.status(result.status).end();"),
        "server must encode body only when present and write result.status:\n{}",
        files.server
    );
    // Body-shape guard, both directions: the typed 200 requires a body, the
    // typeless 204 forbids one — Express only suppresses bodies on 204/304,
    // so without the guard a body paired with a typeless 202-style status
    // would hit the wire.
    assert!(
        files
            .server
            .contains("if ([200].includes(result.status) && result.body === undefined) {")
            && files
                .server
                .contains("if ([204].includes(result.status) && result.body !== undefined) {")
            && files
                .server
                .contains("handler returned no body for a typed status")
            && files
                .server
                .contains("handler returned a body for a bodyless status"),
        "server must enforce body presence per declared status shape:\n{}",
        files.server
    );
    insta::assert_snapshot!("multi_status_mixed_types", files.types);
    insta::assert_snapshot!("multi_status_mixed_client", files.client);
    insta::assert_snapshot!("multi_status_mixed_handlers", files.handlers);
    insta::assert_snapshot!("multi_status_mixed_server", files.server);
}

/// An ALL-TYPELESS multi-status block (`response { 202  204 }`): there is no
/// shared body type `T`, so the envelope is just `{ status: number }` with no
/// `body` field. The client never parses a body; the server writes the chosen
/// status with no body.
#[test]
fn multi_status_all_typeless() {
    let files = generate_from_source(
        r#"
endpoint enqueueJob: POST "/api/jobs" {
    response {
        202
        204
    }
}
"#,
    );
    // Envelope is just { status: number } — no body field.
    assert!(
        files
            .types
            .contains("export interface EnqueueJobResponse {")
            && files.types.contains("  status: number;")
            && !files.types.contains("body"),
        "all-typeless envelope must be {{ status: number }} with no body:\n{}",
        files.types
    );
    assert!(
        files
            .handlers
            .contains("enqueueJob(): Promise<EnqueueJobResponse>;"),
        "handler must return Promise<EnqueueJobResponse>:\n{}",
        files.handlers
    );
    // Client builds the status-only envelope, never reads or parses a body —
    // but it cancels the unread stream so the connection is released for
    // reuse instead of being held until GC.
    assert!(
        files.client.contains("return { status: response.status };")
            && files
                .client
                .contains("await response.body?.cancel().catch(() => undefined);")
            && !files.client.contains("response.json()")
            && !files.client.contains("response.text()"),
        "client must build a status-only envelope and cancel the unread body:\n{}",
        files.client
    );
    // Server writes the chosen status, no body.
    assert!(
        files
            .server
            .contains("const result = await handlers.enqueueJob();")
            && files.server.contains("res.status(result.status).end();")
            && !files.server.contains("result.body"),
        "server must write result.status with no body:\n{}",
        files.server
    );
    assert!(
        files
            .server
            .contains("if (![202, 204].includes(result.status)) {"),
        "server must validate the handler status against the declared set:\n{}",
        files.server
    );
    insta::assert_snapshot!("multi_status_all_typeless_types", files.types);
    insta::assert_snapshot!("multi_status_all_typeless_client", files.client);
    insta::assert_snapshot!("multi_status_all_typeless_handlers", files.handlers);
    insta::assert_snapshot!("multi_status_all_typeless_server", files.server);
}

/// Multi-status + `error { }` on one endpoint: the route's catch block maps
/// each declared variant to its status while the envelope guards live inside
/// the try — one route carries both. The roundtrip suite covers this
/// combination at runtime (`upsertPost2_error_validation`); this snapshot
/// pins the generated code so a regression fails here without that harness.
#[test]
fn multi_status_with_errors() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint upsertUser: PUT "/api/users/{id}" {
    response {
        200: User
        201: User
    }
    error {
        ValidationError(400)
        Unauthorized(401)
    }
}
"#,
    );
    // The declared-variant mapping answers its mapped status...
    assert!(
        files
            .server
            .contains("if (error.message === \"ValidationError\") {")
            && files
                .server
                .contains("res.status(400).json({ error: \"ValidationError\" });")
            && files
                .server
                .contains("res.status(401).json({ error: \"Unauthorized\" });"),
        "multi-status server must map declared error variants:\n{}",
        files.server
    );
    // ...and the envelope machinery is intact alongside it.
    assert!(
        files
            .server
            .contains("if (![200, 201].includes(result.status)) {")
            && files
                .server
                .contains("res.status(result.status).json(result.body);"),
        "envelope status validation and write must coexist with the error mapping:\n{}",
        files.server
    );
    insta::assert_snapshot!("multi_status_errors_server", files.server);
}

/// A multi-status block COMBINED with a request `body` parameter: the client
/// method has a parameter literally named `body`, so the response-decode local
/// must be `responseBody` — a `let body` would redeclare the parameter, which
/// TS rejects. Pins the rename at the generator level (the roundtrip covers it
/// end to end).
#[test]
fn multi_status_with_request_body() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint upsertUser: PUT "/api/users/{id}" {
    body User omit { id }
    response {
        200: User
        201: User
        204
    }
}
"#,
    );
    // The request body still serializes from the `body` parameter...
    assert!(
        files.client.contains("body: JSON.stringify(body)"),
        "request body must serialize from the `body` parameter:\n{}",
        files.client
    );
    // ...while the response decode uses the non-colliding local.
    assert!(
        files
            .client
            .contains("responseBody = JSON.parse(responseText) as User;")
            && files
                .client
                .contains("return { status: response.status, body: responseBody };"),
        "response decode must use the `responseBody` local, not `body`:\n{}",
        files.client
    );
    insta::assert_snapshot!("multi_status_request_body_client", files.client);
}

/// Multi-status COMBINED with request headers and query params: the inputs are
/// orthogonal to the response envelope (they emit before the response-decode
/// branch), but nothing else pins the combination — sema explicitly allows it,
/// so guard it at the generator level too.
#[test]
fn multi_status_with_inputs() {
    let files = generate_from_source(
        r#"
struct Job { id: Int  state: String }
endpoint restartJob: POST "/api/jobs/{id}/restart" {
    headers {
        authorization: String
        traceId: Option<String>
    }
    query {
        priority: Int = 5
    }
    response {
        200: Job
        202: Job
        204
    }
}
"#,
    );
    // Client: header/query inputs coexist with the envelope return + decode.
    assert!(
        files.client.contains("): Promise<RestartJobResponse> {")
            && files.client.contains("priority")
            && files
                .client
                .contains("return { status: response.status, body: responseBody };"),
        "client must carry header/query inputs and return the envelope:\n{}",
        files.client
    );
    // Server: input parsing and the envelope guards coexist.
    assert!(
        files
            .server
            .contains("if (![200, 202, 204].includes(result.status)) {"),
        "server must keep the envelope guards alongside input parsing:\n{}",
        files.server
    );
    insta::assert_snapshot!("multi_status_inputs_client", files.client);
    insta::assert_snapshot!("multi_status_inputs_server", files.server);
}

/// A typeless 205 (Reset Content): Express only auto-suppresses bodies on
/// 204/304 — NOT 205 — so the server's body-shape guard is the only thing
/// keeping an illegal 205 body off the wire. Pin that 205 lands in the
/// typeless guard set and the `.end()` write path stays live.
#[test]
fn multi_status_typeless_205() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint resetUser: PUT "/api/users/{id}/reset" {
    response {
        200: User
        205
    }
}
"#,
    );
    assert!(
        files
            .server
            .contains("if ([205].includes(result.status) && result.body !== undefined) {"),
        "205 must be guarded as a bodyless status:\n{}",
        files.server
    );
    assert!(
        files.server.contains("res.status(result.status).end();"),
        "the bodyless write path must use the handler-chosen status:\n{}",
        files.server
    );
}
