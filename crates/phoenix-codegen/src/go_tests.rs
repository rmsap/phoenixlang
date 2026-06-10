//! Inline tests for the Go generator, split out of `go.rs` to keep the
//! generator file readable (each feature slice grows both halves).
//! Declared as `mod tests` inside `go.rs` via `#[path]` so the module path —
//! and therefore every insta snapshot name — is unchanged by the move.

use super::*;
use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

/// An interior empty doc line renders as a bare `//` (trailing space trimmed)
/// so the output stays gofmt-clean. Guards the empty-line branch of
/// `render_line_comment`, which the doc-comment integration tests don't hit.
#[test]
fn render_line_comment_blanks_out_empty_lines() {
    assert_eq!(
        render_line_comment("// ", "first\n\nthird"),
        "// first\n//\n// third\n"
    );
}

/// The tab-indented prefix (used for handler doc comments) must keep its
/// leading tab on a blank line while still trimming the trailing space after
/// `//`, i.e. `"\t// "` → `"\t//"`. `trim_end` only strips trailing
/// whitespace, so the indent survives — but nothing else pins that, so guard
/// it explicitly.
#[test]
fn render_line_comment_keeps_indent_on_empty_lines() {
    assert_eq!(
        render_line_comment("\t// ", "first\n\nthird"),
        "\t// first\n\t//\n\t// third\n"
    );
}

fn generate_from_source(source: &str) -> GoFiles {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "check errors: {:?}",
        result.diagnostics
    );
    generate_go(&program, &result)
}

#[test]
fn struct_to_go() {
    let files = generate_from_source(
        r#"
/** A registered user */
struct User {
    Int id
    String name
    Option<String> bio
}
"#,
    );
    insta::assert_snapshot!("go_struct", files.types);
}

#[test]
fn simple_enum() {
    let files = generate_from_source("enum Role { Admin  Editor  Viewer }");
    insta::assert_snapshot!("go_enum", files.types);
}

#[test]
fn get_with_path_param() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
endpoint getUser: GET "/api/users/{id}" {
    response User
}
"#,
    );
    insta::assert_snapshot!("go_get_client", files.client);
    insta::assert_snapshot!("go_get_handler", files.handlers);
    insta::assert_snapshot!("go_get_server", files.server);
}

#[test]
fn post_with_body_and_errors() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name  String email }
endpoint createUser: POST "/api/users" {
    body User omit { id }
    response User
    error { Conflict(409) }
}
"#,
    );
    insta::assert_snapshot!("go_post_client", files.client);
    insta::assert_snapshot!("go_post_server", files.server);
    insta::assert_snapshot!("go_post_types", files.types);
}

#[test]
fn query_params() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
endpoint listUsers: GET "/api/users" {
    query {
        Int page = 1
        Int limit = 20
        Option<String> search
    }
    response List<User>
}
"#,
    );
    insta::assert_snapshot!("go_query_client", files.client);
    insta::assert_snapshot!("go_query_server", files.server);
}

#[test]
fn void_response() {
    let files = generate_from_source(
        r#"
endpoint deleteUser: DELETE "/api/users/{id}" {
    error { NotFound(404) }
}
"#,
    );
    insta::assert_snapshot!("go_void_client", files.client);
    insta::assert_snapshot!("go_void_server", files.server);
}

/// A multi-line doc comment must have EVERY line prefixed with `//`, not just
/// the first — otherwise continuation lines leak into the file as invalid Go.
/// Regression guard for `render_line_comment`.
#[test]
fn multiline_doc_comment_is_fully_commented() {
    let files = generate_from_source(
        r#"
struct Widget { Int id }
/**
 * Fetch a widget by id
 * with extra detail on the second line
 */
endpoint getWidget: GET "/api/widgets/{id}" {
    response Widget
}
"#,
    );
    assert!(
        files.client.contains(
            "// GetWidget fetch a widget by id\n// with extra detail on the second line.\n"
        ),
        "every doc line must be commented:\n{}",
        files.client
    );
    // The continuation line must never appear UNcommented (leaked as code).
    assert!(
        !files.client.contains("\nwith extra detail"),
        "continuation doc line leaked as code:\n{}",
        files.client
    );
}

#[test]
fn multiple_endpoints() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name  String email }
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
    insta::assert_snapshot!("go_multi_types", files.types);
    insta::assert_snapshot!("go_multi_client", files.client);
    insta::assert_snapshot!("go_multi_handlers", files.handlers);
    insta::assert_snapshot!("go_multi_server", files.server);
}

#[test]
fn pascal_case_conversion() {
    assert_eq!(to_pascal_case("createUser"), "CreateUser");
    assert_eq!(to_pascal_case("id"), "Id");
    assert_eq!(to_pascal_case("listUsers"), "ListUsers");
    assert_eq!(to_pascal_case("User"), "User");
}

#[test]
fn multiple_path_params() {
    let files = generate_from_source(
        r#"
struct Comment { Int id  String text }
endpoint getComment: GET "/api/users/{userId}/posts/{postId}" {
    response Comment
}
"#,
    );
    insta::assert_snapshot!("go_multi_path_client", files.client);
}

#[test]
fn partial_body() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name  Int age }
endpoint updateUser: PATCH "/api/users/{id}" {
    body User omit { id } partial
    response User
}
"#,
    );
    insta::assert_snapshot!("go_partial_types", files.types);
}

// ── Gap-filling tests ───────────────────────────────────────────

/// `pick` modifier in derived body.
#[test]
fn pick_body() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name  String email  Int age }
endpoint updateEmail: PATCH "/api/users/{id}" {
    body User pick { email }
    response User
}
"#,
    );
    insta::assert_snapshot!("go_pick_types", files.types);
}

/// A constrained `Option<T>` field carried into a body keeps `optional ==
/// false` (no `partial` applied) yet renders as a Go pointer, so the body's
/// `Validate()` must nil-guard and dereference it — exactly like the source
/// struct's own `Validate()`. Guards `emit_body_validate_method`'s pointer
/// detection against regressing to a bare `f.optional` check.
#[test]
fn body_validate_optional_constrained_field() {
    let files = generate_from_source(
        r#"
struct Account {
    Int id
    Option<String> displayName where self.length <= 60
}
endpoint updateAccount: PATCH "/api/accounts/{id}" {
    body Account omit { id }
    response Account
}
"#,
    );
    assert!(
        files
            .types
            .contains("func (s UpdateAccountBody) Validate() error {"),
        "body type should have a Validate method:\n{}",
        files.types
    );
    assert!(
        files
            .types
            .contains("if s.DisplayName != nil && !(len(*s.DisplayName) <= 60) {"),
        "Option body field must be nil-guarded and dereferenced:\n{}",
        files.types
    );
}

/// A constrained `Option<T>` field that ALSO gets `partial` applied must not
/// render as `**T`: `type_to_go` already maps `Option<T>` to `*T`, and
/// `partial` only marks it optional — it must stay a single pointer so both
/// the struct field and the body `Validate()` (single deref `*s.Field`) are
/// valid Go. Regression guard for the `derived_field_go_type` double-pointer
/// collapse.
#[test]
fn body_validate_partial_option_constrained_field() {
    let files = generate_from_source(
        r#"
struct Account {
    Int id
    Option<String> displayName where self.length <= 60
}
endpoint patchAccount: PATCH "/api/accounts/{id}" {
    body Account omit { id } partial { displayName }
    response Account
}
"#,
    );
    assert!(
        !files.types.contains("**"),
        "an optional Option field must collapse to a single pointer, not **T:\n{}",
        files.types
    );
    assert!(
        files
            .types
            .contains("DisplayName *string `json:\"displayName,omitempty\"`"),
        "partial Option field should render as a single *string:\n{}",
        files.types
    );
    assert!(
        files
            .types
            .contains("if s.DisplayName != nil && !(len(*s.DisplayName) <= 60) {"),
        "partial Option body field must be nil-guarded and single-dereferenced:\n{}",
        files.types
    );
}

/// Map<K,V> and Bool fields in struct.
#[test]
fn map_and_bool_fields() {
    let files = generate_from_source(
        r#"
struct Config {
    Map<String, String> settings
    Bool enabled
    Float threshold
}
"#,
    );
    insta::assert_snapshot!("go_map_bool_float_types", files.types);
}

/// PUT method.
#[test]
fn put_method() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
endpoint replaceUser: PUT "/api/users/{id}" {
    body User
    response User
}
"#,
    );
    insta::assert_snapshot!("go_put_server", files.server);
}

/// String and Bool query param defaults.
#[test]
fn string_bool_query_defaults() {
    let files = generate_from_source(
        r#"
struct Item { Int id  String name }
endpoint listItems: GET "/api/items" {
    query {
        String sortBy = "name"
        Bool ascending = true
    }
    response List<Item>
}
"#,
    );
    insta::assert_snapshot!("go_string_bool_query_client", files.client);
    insta::assert_snapshot!("go_string_bool_query_server", files.server);
}

/// Required `Float` and enum query params, plus an optional enum. Exercises
/// the server-parse paths whose local type must match the handler signature:
/// `float64` via `strconv.ParseFloat`, and a `T(v)` / `*T` conversion for the
/// string-backed enum. Also pins the conditional server import set.
#[test]
fn float_and_enum_query_params() {
    let files = generate_from_source(
        r#"
enum Sort { Asc  Desc }
struct Item { Int id  String name }
endpoint listItems: GET "/api/items" {
    query {
        Float minScore = 0.5
        Sort sort
        Option<Sort> fallback
    }
    response List<Item>
}
"#,
    );
    insta::assert_snapshot!("go_float_enum_query_client", files.client);
    insta::assert_snapshot!("go_float_enum_query_server", files.server);
    insta::assert_snapshot!("go_float_enum_query_handlers", files.handlers);
}

/// A derived body that omits every field collapses to `struct{}` — gofmt
/// rewrites the multi-line empty form, so this guards `render_struct`.
#[test]
fn empty_derived_body_is_gofmt_clean() {
    let files = generate_from_source(
        r#"
struct Ping { Int id }
endpoint ping: POST "/api/ping" {
    body Ping omit { id }
    response Ping
}
"#,
    );
    assert!(
        files.types.contains("type PingBody struct{}"),
        "expected collapsed empty struct, got:\n{}",
        files.types
    );
    assert!(!files.types.contains("type PingBody struct {\n}"));
}

/// A schema with types but no endpoints emits a client with no methods, so
/// `fmt` (only ever used inside a method) must not be imported — an unused
/// import would fail `go build`. `net/http` stays (the client struct holds a
/// `*http.Client`).
#[test]
fn types_only_client_omits_fmt_import() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
"#,
    );
    assert!(
        !files.client.contains("\"fmt\""),
        "types-only client must not import fmt:\n{}",
        files.client
    );
    assert!(
        files.client.contains("\"net/http\""),
        "client must still import net/http:\n{}",
        files.client
    );
}

/// Enum as response type.
#[test]
fn enum_response() {
    let files = generate_from_source(
        r#"
enum Status { Active  Inactive  Banned }
endpoint getStatus: GET "/api/status" {
    response Status
}
"#,
    );
    insta::assert_snapshot!("go_enum_response_client", files.client);
}

/// default_value_to_go covers all variants.
#[test]
fn default_value_conversions() {
    assert_eq!(default_value_to_go(&DefaultValue::Int(42)), "42");
    assert_eq!(default_value_to_go(&DefaultValue::Float(1.5)), "1.5");
    assert_eq!(
        default_value_to_go(&DefaultValue::String("hello".into())),
        "\"hello\""
    );
    assert_eq!(default_value_to_go(&DefaultValue::Bool(true)), "true");
    assert_eq!(default_value_to_go(&DefaultValue::Bool(false)), "false");
}

// ── Validation tests ───────────────────────────────────────────

/// Validate method with numeric and string length constraints.
#[test]
fn validate_numeric_and_string() {
    let files = generate_from_source(
        r#"
struct User {
    Int id
    String name where self.length > 0 && self.length <= 100
    Int age where self >= 0 && self <= 150
}
"#,
    );
    insta::assert_snapshot!("go_validate_types", files.types);
}

/// Validate method with `contains` constraint (requires strings import).
#[test]
fn validate_contains() {
    let files = generate_from_source(
        r#"
struct User {
    Int id
    String email where self.contains("@") && self.length > 3
}
"#,
    );
    insta::assert_snapshot!("go_validate_contains_types", files.types);
}

/// No Validate method when struct has no constraints.
#[test]
fn no_validate_without_constraints() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
"#,
    );
    assert!(
        !files.types.contains("Validate"),
        "should not emit Validate when no constraints"
    );
    assert!(
        !files.types.contains("import"),
        "should not emit imports when no constraints"
    );
}

/// `dyn Trait` maps to a bare Go interface name (the interface itself
/// is expected to be defined in hand-written Go alongside the generated
/// struct).  Parallel to the TS/Python behavior.
#[test]
fn dyn_type_erases_to_trait_name() {
    assert_eq!(type_to_go(&Type::Dyn("Drawable".to_string())), "Drawable");
}

// ── Headers ─────────────────────────────────────────────────────

/// A required request header with an auto-derived wire name
/// (`idempotencyKey` → `Idempotency-Key`) threads through the client param
/// list, the `req.Header.Set` call, the handler signature, and the
/// server-side `r.Header.Get` parse.
#[test]
fn request_header_auto_wire_name() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
endpoint createUser: POST "/api/users" {
    body User omit { id }
    headers {
        String idempotencyKey
    }
    response User
}
"#,
    );
    insta::assert_snapshot!("go_req_header_client", files.client);
    insta::assert_snapshot!("go_req_header_handlers", files.handlers);
    insta::assert_snapshot!("go_req_header_server", files.server);
}

/// An explicit `as "..."` override pins the wire name verbatim (used on the
/// client `Set` and the server `Get`), while the local/param stays camelCase.
#[test]
fn request_header_as_override() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
endpoint getUser: GET "/api/users/{id}" {
    headers {
        String authToken as "X-Auth-Token"
    }
    response User
}
"#,
    );
    insta::assert_snapshot!("go_req_header_override_client", files.client);
    insta::assert_snapshot!("go_req_header_override_server", files.server);
}

/// An optional request header is a `*string` param, sent only behind a nil
/// guard on the client and parsed into a nil-able `*string` on the server.
#[test]
fn optional_request_header() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
endpoint getUser: GET "/api/users/{id}" {
    headers {
        Option<String> traceId
    }
    response User
}
"#,
    );
    insta::assert_snapshot!("go_opt_header_client", files.client);
    insta::assert_snapshot!("go_opt_header_server", files.server);
}

/// A `Bool` request header serializes via `strconv.FormatBool`, which emits
/// lowercase `true`/`false` — the cross-language wire convention every
/// backend must agree on (TS `String(bool)`, Python `"true"/"false"`), so a
/// bool header round-trips. Locks the convention on the Go side.
#[test]
fn bool_request_header_serializes_lowercase() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
endpoint getUser: GET "/api/users/{id}" {
    headers {
        Bool debug
    }
    response User
}
"#,
    );
    assert!(
        files
            .client
            .contains("req.Header.Set(\"Debug\", strconv.FormatBool(debug))"),
        "bool header must serialize via strconv.FormatBool (lowercase):\n{}",
        files.client
    );
}

/// A request header with a literal default seeds the server-side local with
/// that default (`maxStale := int64(60)`) before the optional `r.Header.Get`
/// overwrite, so an absent header lands on the declared default rather than
/// the Go zero value. Per the documented "defaulted request headers" gap, the
/// generated client still takes it as a required positional arg.
#[test]
fn defaulted_request_header_seeds_server_default() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
endpoint getUser: GET "/api/users/{id}" {
    headers {
        Int maxStale = 60
    }
    response User
}
"#,
    );
    assert!(
        files.server.contains("maxStale := int64(60)"),
        "server must seed the local with the declared default:\n{}",
        files.server
    );
    assert!(
        files
            .server
            .contains("if v := r.Header.Get(\"Max-Stale\"); v != \"\""),
        "server must still overwrite from the header when present:\n{}",
        files.server
    );
    assert!(
        files.client.contains("maxStale int64"),
        "client must take the defaulted header as a required positional arg:\n{}",
        files.client
    );
}

/// A response header produces the `<Endpoint>Result` envelope: the handler
/// and client return `*GetPostResult` (body + typed header), the server
/// writes the header via `w.Header().Set` and encodes `result.Body`, and the
/// client reads it back from `resp.Header`. Covers a required `int64` header
/// (numeric stringify/parse both directions) and an optional one.
#[test]
fn response_header_envelope() {
    let files = generate_from_source(
        r#"
struct Post { Int id  String title }
endpoint getPost: GET "/api/posts/{id}" {
    response Post headers {
        Int ratelimitRemaining
        Option<String> requestId as "X-Request-Id"
    }
}
"#,
    );
    insta::assert_snapshot!("go_resp_header_types", files.types);
    insta::assert_snapshot!("go_resp_header_client", files.client);
    insta::assert_snapshot!("go_resp_header_handlers", files.handlers);
    insta::assert_snapshot!("go_resp_header_server", files.server);
}

/// A multipart request body (one `File` + one scalar): types.go gains the
/// `FileUpload` helper and a `<Endpoint>ClientBody` (File field → `FileUpload`)
/// while the server `<Endpoint>Body` keeps `*multipart.FileHeader`; the client
/// builds a `multipart.Writer` (CreateFormFile + WriteField); the server calls
/// `r.ParseMultipartForm` + `r.FormFile`/`r.FormValue`; the handler takes the
/// `*multipart.FileHeader`-bearing body unchanged.
#[test]
fn multipart_request_body() {
    let files = generate_from_source(
        r#"
struct AvatarUpload { File avatar  String caption }
struct User { Int id  String name }
endpoint uploadAvatar: POST "/api/avatar" {
    body AvatarUpload
    response User
}
"#,
    );
    insta::assert_snapshot!("go_multipart_req_types", files.types);
    insta::assert_snapshot!("go_multipart_req_client", files.client);
    insta::assert_snapshot!("go_multipart_req_handlers", files.handlers);
    insta::assert_snapshot!("go_multipart_req_server", files.server);
}

/// An optional file part (`Option<File>`): the client body field is
/// `*FileUpload` (nil-guarded in the multipart writer), and the server's
/// `r.FormFile` tolerates an absent part.
#[test]
fn multipart_optional_file() {
    let files = generate_from_source(
        r#"
struct DocUpload { Option<File> attachment  String title }
struct Doc { Int id }
endpoint uploadDoc: POST "/api/docs" {
    body DocUpload
    response Doc
}
"#,
    );
    insta::assert_snapshot!("go_multipart_opt_types", files.types);
    insta::assert_snapshot!("go_multipart_opt_client", files.client);
    insta::assert_snapshot!("go_multipart_opt_server", files.server);
}

/// A binary response body (a struct with exactly one `File` field): the
/// handler returns `(io.Reader, error)`, the server streams via `io.Copy`
/// with `application/octet-stream`, and the client returns
/// `(io.ReadCloser, error)` handing back the raw `resp.Body`.
#[test]
fn binary_response_download() {
    let files = generate_from_source(
        r#"
struct FileDownload { File contents }
endpoint downloadFile: GET "/api/files/{id}" {
    response FileDownload
}
"#,
    );
    insta::assert_snapshot!("go_binary_resp_types", files.types);
    insta::assert_snapshot!("go_binary_resp_client", files.client);
    insta::assert_snapshot!("go_binary_resp_handlers", files.handlers);
    insta::assert_snapshot!("go_binary_resp_server", files.server);
}

/// Two REQUIRED `File` fields in one multipart body: each form-file part must
/// be emitted in its own block so the `part, err :=` declarations don't
/// collide (a second `:=` with no new variable on the left does not compile
/// in Go). Regression guard.
#[test]
fn multipart_two_required_files() {
    let files = generate_from_source(
        r#"
struct TwoFiles { File first  File second }
struct Ok { Int id }
endpoint uploadBoth: POST "/api/both" {
    body TwoFiles
    response Ok
}
"#,
    );
    // Both files build a part, but the declarations are block-scoped so they
    // never appear back-to-back at the same indent.
    assert_eq!(
        files
            .client
            .matches("part, err := writer.CreateFormFile")
            .count(),
        2,
        "both required files must build a part:\n{}",
        files.client
    );
    assert!(
        files
            .client
            .contains("part, err := writer.CreateFormFile(\"first\", body.First.Filename)"),
        "first file part:\n{}",
        files.client
    );
    assert!(
        files
            .client
            .contains("part, err := writer.CreateFormFile(\"second\", body.Second.Filename)"),
        "second file part:\n{}",
        files.client
    );
    // Server side: two required files parse into distinct `_, fh<Field>, err :=`
    // declarations at function scope. This compiles because `:=` only needs one
    // new variable on the left (`fhFirst`/`fhSecond` differ), but guard it with a
    // snapshot since the diff's block-scoping rationale is a server concern too.
    assert!(
        files
            .server
            .contains(", fhFirst, err := r.FormFile(\"first\")")
            && files
                .server
                .contains(", fhSecond, err := r.FormFile(\"second\")"),
        "both required files must parse server-side into distinct vars:\n{}",
        files.server
    );
    insta::assert_snapshot!("go_multipart_two_files_client", files.client);
    insta::assert_snapshot!("go_multipart_two_files_server", files.server);
}

/// A multipart body whose scalar field carries a `where` constraint still
/// gets validated server-side: the `<Endpoint>Body` keeps its `Validate()`
/// method and the server calls it after assembling the body from the parsed
/// form (the JSON path does the same). Go is the one target that validates
/// multipart bodies — see `docs/known-issues.md`. A validate failure maps to
/// the endpoint's declared `ValidationError` variant.
#[test]
fn multipart_body_with_constraint_validates() {
    let files = generate_from_source(
        r#"
struct AvatarUpload { File avatar  String caption where self.length > 0 }
struct User { Int id }
endpoint uploadAvatar: POST "/api/avatar" {
    body AvatarUpload
    response User
    error { ValidationError(400) }
}
"#,
    );
    // The constrained body type carries a Validate() method...
    assert!(
        files
            .types
            .contains("func (s UploadAvatarBody) Validate() error {"),
        "constrained multipart body must get a Validate() method:\n{}",
        files.types
    );
    // ...and the server calls it after assembling the body from the form.
    assert!(
        files
            .server
            .contains("if err := body.Validate(); err != nil {"),
        "server must validate the assembled multipart body:\n{}",
        files.server
    );
    // A validate failure maps to the declared ValidationError(400).
    assert!(
        files
            .server
            .contains("http.Error(w, \"ValidationError\", 400)"),
        "validate failure maps to ValidationError(400):\n{}",
        files.server
    );
}

/// An endpoint that is BOTH a multipart upload AND a binary download
/// (`body_is_multipart` + `response_is_binary`): the server must parse the
/// multipart form (which declares an `err` via `r.FormFile`) AND then call
/// the handler with `result, err := h.X(...)` — the second `:=` only
/// compiles because `result` is a fresh variable on the left, so this guards
/// that the two branches compose without an `err` redeclaration conflict.
/// The handler returns `(io.Reader, error)` and the server streams it via
/// `io.Copy` with an octet-stream content type.
#[test]
fn multipart_upload_with_binary_response() {
    let files = generate_from_source(
        r#"
struct AvatarUpload { File avatar  String caption }
struct Thumbnail { File data }
endpoint convertAvatar: POST "/api/avatar/convert" {
    body AvatarUpload
    response Thumbnail
}
"#,
    );
    // Server parses the multipart form (declaring `err` via FormFile)...
    assert!(
        files.server.contains("r.ParseMultipartForm(")
            && files
                .server
                .contains(", fhAvatar, err := r.FormFile(\"avatar\")"),
        "server must parse the multipart body:\n{}",
        files.server
    );
    // ...then calls the handler with a fresh `result` (so the reused `err`
    // does not trip a no-new-variable `:=` error) and streams the result.
    assert!(
        files
            .server
            .contains("result, err := h.ConvertAvatar(body)"),
        "server must call the handler with `result, err :=` after the form parse:\n{}",
        files.server
    );
    assert!(
        files.server.contains("_, _ = io.Copy(w, result)")
            && files
                .server
                .contains("w.Header().Set(\"Content-Type\", \"application/octet-stream\")"),
        "server must stream the binary response via io.Copy:\n{}",
        files.server
    );
    // Handler returns the upload body + an io.Reader.
    assert!(
        files
            .handlers
            .contains("ConvertAvatar(body ConvertAvatarBody) (io.Reader, error)"),
        "handler must take the multipart body and return (io.Reader, error):\n{}",
        files.handlers
    );
    // Client builds the multipart writer and returns the raw response stream.
    assert!(
        files.client.contains("multipart.NewWriter(&buf)")
            && files.client.contains("(io.ReadCloser, error)")
            && files.client.contains("return resp.Body, nil"),
        "client must send multipart and return the response stream:\n{}",
        files.client
    );
}

/// A multipart upload with a REQUIRED `File` field and NO `response`: the
/// required `r.FormFile(...)` declares an `err` in the route closure, so the
/// no-response handler call MUST be statement-scoped (`if err := h.X(...);
/// err != nil`) rather than a bare `err := h.X(...)` — the latter is a second
/// `:=` with `err` as its only new variable and does not compile ("no new
/// variables on left side of :="). Regression guard for that collision.
#[test]
fn multipart_upload_no_response() {
    let files = generate_from_source(
        r#"
struct AvatarUpload { File avatar  String caption }
endpoint uploadAvatar: POST "/api/avatar" {
    body AvatarUpload
}
"#,
    );
    // The required file parse declares a closure-scoped `err`...
    assert!(
        files
            .server
            .contains(", fhAvatar, err := r.FormFile(\"avatar\")"),
        "required file must parse into a closure-scoped err:\n{}",
        files.server
    );
    // ...so the no-response handler call must NOT be a bare statement-level
    // `err := h.X(...)` (which would redeclare the closure-scoped err and not
    // compile). The leading `\t\t` distinguishes the bare form from the
    // accepted statement-scoped `\t\tif err := h.X(...)`.
    assert!(
        !files.server.contains("\t\terr := h.UploadAvatar("),
        "no-response handler call must not redeclare err (would not compile):\n{}",
        files.server
    );
    assert!(
        files
            .server
            .contains("if err := h.UploadAvatar(body); err != nil {"),
        "no-response handler call must be statement-scoped:\n{}",
        files.server
    );
    // The client method returns bare `error` (no response value), so every
    // error path in the multipart build must `return err` — a hardcoded
    // `return nil, err` would not compile. Regression guard: the multipart
    // branch shares `err_ret` with the JSON-body and no-body branches.
    assert!(
        files.client.contains(") error {"),
        "no-response client method must return bare error:\n{}",
        files.client
    );
    assert!(
        !files.client.contains("return nil, err"),
        "bare-error client must not return a (value, error) pair:\n{}",
        files.client
    );
}

/// A JSON body with NO `response`: the client method returns bare `error`, so
/// the marshal/build error paths must `return err`, not `return nil, err`
/// (which would not compile — "too many return values"). Mirrors the TS
/// `body_no_response` test; regression guard for the shared `err_ret` arity.
#[test]
fn body_no_response() {
    let files = generate_from_source(
        r#"
struct Feedback { String message  Int rating }
endpoint submitFeedback: POST "/api/feedback" {
    body Feedback
}
"#,
    );
    assert!(
        files
            .client
            .contains("func (c *ApiClient) SubmitFeedback(body ")
            && files.client.contains(") error {"),
        "no-response client method must return bare error:\n{}",
        files.client
    );
    assert!(
        !files.client.contains("return nil, err"),
        "bare-error client must not return a (value, error) pair:\n{}",
        files.client
    );
    insta::assert_snapshot!("go_body_no_response_client", files.client);
    insta::assert_snapshot!("go_body_no_response_handlers", files.handlers);
}

/// An OFFSET-paginated endpoint: the bare `List<Post>` response becomes the
/// `<Endpoint>Page` envelope `{ Items []Post; TotalCount int64 }`. The handler
/// and client both return `*ListPostsPage`; the client decodes the whole body
/// into the page (the body IS the page object); the server JSON-encodes the
/// handler's returned `*ListPostsPage`.
#[test]
fn pagination_offset_envelope() {
    let files = generate_from_source(
        r#"
struct Post { Int id  String title }
endpoint listPosts: GET "/api/posts" {
    response List<Post> pagination { offset }
}
"#,
    );
    // Envelope struct: Items + TotalCount (offset's defining metadata).
    assert!(
        files.types.contains("type ListPostsPage struct {")
            && files.types.contains("Items      []Post `json:\"items\"`")
            && files
                .types
                .contains("TotalCount int64  `json:\"totalCount\"`"),
        "offset envelope must be {{ Items []Post; TotalCount int64 }}:\n{}",
        files.types
    );
    // Handler returns the page envelope, not the bare slice.
    assert!(
        files
            .handlers
            .contains("ListPosts() (*ListPostsPage, error)"),
        "handler must return *ListPostsPage:\n{}",
        files.handlers
    );
    // Client returns the envelope and decodes the whole body into it.
    assert!(
        files
            .client
            .contains("func (c *ApiClient) ListPosts() (*ListPostsPage, error)")
            && files.client.contains("var result ListPostsPage")
            && files
                .client
                .contains("json.NewDecoder(resp.Body).Decode(&result)"),
        "client must return *ListPostsPage and decode the body into it:\n{}",
        files.client
    );
    // Server JSON-encodes the handler's returned page.
    assert!(
        files.server.contains("result, err := h.ListPosts()")
            && files.server.contains("json.NewEncoder(w).Encode(result)"),
        "server must encode the returned *ListPostsPage:\n{}",
        files.server
    );
    insta::assert_snapshot!("go_pagination_offset_types", files.types);
    insta::assert_snapshot!("go_pagination_offset_client", files.client);
    insta::assert_snapshot!("go_pagination_offset_handlers", files.handlers);
    insta::assert_snapshot!("go_pagination_offset_server", files.server);
}

/// A CURSOR-paginated endpoint: the envelope is `{ Items []Post; NextCursor
/// *string }`. `NextCursor` is a pointer so an absent cursor serializes to
/// `null` (nil marks the last page). Handler/client return `*ListPostsPage`
/// and the body decode/encode path is identical to the offset case.
#[test]
fn pagination_cursor_envelope() {
    let files = generate_from_source(
        r#"
struct Post { Int id  String title }
endpoint listPosts: GET "/api/posts" {
    response List<Post> pagination { cursor }
}
"#,
    );
    // Envelope struct: Items + NextCursor *string (nil = last page).
    assert!(
        files.types.contains("type ListPostsPage struct {")
            && files.types.contains("Items      []Post  `json:\"items\"`")
            && files
                .types
                .contains("NextCursor *string `json:\"nextCursor\"`"),
        "cursor envelope must be {{ Items []Post; NextCursor *string }}:\n{}",
        files.types
    );
    assert!(
        files
            .handlers
            .contains("ListPosts() (*ListPostsPage, error)"),
        "handler must return *ListPostsPage:\n{}",
        files.handlers
    );
    assert!(
        files
            .client
            .contains("func (c *ApiClient) ListPosts() (*ListPostsPage, error)")
            && files.client.contains("var result ListPostsPage"),
        "client must return *ListPostsPage:\n{}",
        files.client
    );
    assert!(
        files.server.contains("json.NewEncoder(w).Encode(result)"),
        "server must encode the returned *ListPostsPage:\n{}",
        files.server
    );
    insta::assert_snapshot!("go_pagination_cursor_types", files.types);
    insta::assert_snapshot!("go_pagination_cursor_client", files.client);
    insta::assert_snapshot!("go_pagination_cursor_handlers", files.handlers);
    insta::assert_snapshot!("go_pagination_cursor_server", files.server);
}

/// A multi-status endpoint with a SHARED body across two typed statuses
/// (`response { 200: User  201: User }`): the bare body is replaced by the
/// `<Endpoint>Response` envelope `{ Status int; Body *User }`. Handler and
/// client return `*UpsertUserResponse`; the client records `resp.StatusCode`
/// and decodes the body into `*User` when present; the server writes the
/// handler-chosen `result.Status` (not a hardcoded 200) and encodes the body.
#[test]
fn multi_status_shared_body() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
endpoint upsertUser: PUT "/api/users/{id}" {
    response {
        200: User
        201: User
    }
}
"#,
    );
    // Envelope struct: Status int + Body *User (Option, omitempty).
    assert!(
        files.types.contains("type UpsertUserResponse struct {")
            && files.types.contains("Status int   `json:\"status\"`")
            && files
                .types
                .contains("Body   *User `json:\"body,omitempty\"`"),
        "envelope must be {{ Status int; Body *User }}:\n{}",
        files.types
    );
    // Handler returns the envelope, not the bare body.
    assert!(
        files
            .handlers
            .contains("UpsertUser(id string) (*UpsertUserResponse, error)"),
        "handler must return *UpsertUserResponse:\n{}",
        files.handlers
    );
    // Client returns the envelope, records the status, decodes the body.
    assert!(
        files
            .client
            .contains("func (c *ApiClient) UpsertUser(id string) (*UpsertUserResponse, error)")
            && files
                .client
                .contains("result := UpsertUserResponse{Status: resp.StatusCode}")
            && files.client.contains("result.Body = &body"),
        "client must build the envelope from resp.StatusCode + decoded body:\n{}",
        files.client
    );
    // Server writes the handler-chosen status (NOT a hardcoded 200/204).
    assert!(
        files.server.contains("result, err := h.UpsertUser(id)")
            && files.server.contains("w.WriteHeader(result.Status)")
            && files
                .server
                .contains("json.NewEncoder(w).Encode(result.Body)")
            && !files.server.contains("w.WriteHeader(http.StatusNoContent)"),
        "server must write result.Status and encode result.Body:\n{}",
        files.server
    );
    // The handler-chosen status is validated against the declared set; an
    // undeclared status (a zero-value envelope would panic WriteHeader(0),
    // a smuggled 4xx would bypass `error { }`) is a handler bug → 500.
    assert!(
        files.server.contains("case 200, 201:")
            && files.server.contains("handler returned undeclared status"),
        "server must reject a handler status outside the declared set:\n{}",
        files.server
    );
    // Body-shape guard: every declared status is typed, so a nil body is a
    // handler bug; there is no typeless arm at all.
    assert!(
        files
            .server
            .contains("handler returned no body for a typed status")
            && !files.server.contains("bodyless status"),
        "server must reject a typed status without a body (no typeless arm):\n{}",
        files.server
    );
    insta::assert_snapshot!("go_multi_status_shared_body_types", files.types);
    insta::assert_snapshot!("go_multi_status_shared_body_client", files.client);
    insta::assert_snapshot!("go_multi_status_shared_body_handlers", files.handlers);
    insta::assert_snapshot!("go_multi_status_shared_body_server", files.server);
}

/// A multi-status endpoint mixing a TYPED status with a TYPELESS one
/// (`response { 200: User  204 }`): the envelope still carries the shared body
/// (`Body *User`) since at least one status is typed. The client only sets
/// `result.Body` when the response carries a body (a 204 leaves it nil); the
/// server writes the chosen status and encodes the body only when present.
#[test]
fn multi_status_typed_and_typeless() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
endpoint updateUser: PUT "/api/users/{id}" {
    response {
        200: User
        204
    }
}
"#,
    );
    assert!(
        files.types.contains("type UpdateUserResponse struct {")
            && files.types.contains("Status int   `json:\"status\"`")
            && files
                .types
                .contains("Body   *User `json:\"body,omitempty\"`"),
        "mixed block still carries Body *User:\n{}",
        files.types
    );
    assert!(
        files
            .handlers
            .contains("UpdateUser(id string) (*UpdateUserResponse, error)"),
        "handler must return *UpdateUserResponse:\n{}",
        files.handlers
    );
    // Body decode is guarded so a typeless 204 leaves result.Body nil.
    assert!(
        files
            .client
            .contains("result := UpdateUserResponse{Status: resp.StatusCode}")
            && files.client.contains("if resp.ContentLength != 0 {"),
        "client must guard the body decode for typeless statuses:\n{}",
        files.client
    );
    // Server encodes the body only when present.
    assert!(
        files.server.contains("if result.Body != nil {")
            && files.server.contains("w.WriteHeader(result.Status)"),
        "server must encode body only when present and write result.Status:\n{}",
        files.server
    );
    // Body-shape guard, both directions: the typed 200 arm requires a body,
    // the typeless 204 arm forbids one — `net/http` only suppresses bodies on
    // 1xx/204/304, so without the guard a body paired with a typeless
    // 202-style status would hit the wire.
    assert!(
        files.server.contains("case 200:")
            && files.server.contains("case 204:")
            && files
                .server
                .contains("handler returned no body for a typed status")
            && files
                .server
                .contains("handler returned a body for a bodyless status"),
        "server must enforce body presence per declared status shape:\n{}",
        files.server
    );
    insta::assert_snapshot!("go_multi_status_mixed_types", files.types);
    insta::assert_snapshot!("go_multi_status_mixed_client", files.client);
    insta::assert_snapshot!("go_multi_status_mixed_handlers", files.handlers);
    insta::assert_snapshot!("go_multi_status_mixed_server", files.server);
}

/// An ALL-TYPELESS multi-status block (`response { 202  204 }`): there is no
/// shared body type `T`, so the envelope is just `{ Status int }` with no
/// `Body` field. The client never decodes a body; the server writes the chosen
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
    // Envelope is just { Status int } — no Body field.
    assert!(
        files.types.contains("type EnqueueJobResponse struct {")
            && files.types.contains("Status int `json:\"status\"`")
            && !files.types.contains("Body"),
        "all-typeless envelope must be {{ Status int }} with no Body:\n{}",
        files.types
    );
    assert!(
        files
            .handlers
            .contains("EnqueueJob() (*EnqueueJobResponse, error)"),
        "handler must return *EnqueueJobResponse:\n{}",
        files.handlers
    );
    // Client builds the status-only envelope, never decodes a body.
    assert!(
        files
            .client
            .contains("result := EnqueueJobResponse{Status: resp.StatusCode}")
            && files.client.contains("return &result, nil")
            && !files.client.contains("result.Body"),
        "client must build a status-only envelope:\n{}",
        files.client
    );
    // Server writes the chosen status, no body.
    assert!(
        files.server.contains("result, err := h.EnqueueJob()")
            && files.server.contains("w.WriteHeader(result.Status)")
            && !files.server.contains("result.Body"),
        "server must write result.Status with no body:\n{}",
        files.server
    );
    assert!(
        files.server.contains("case 202, 204:"),
        "server must validate the handler status against the declared set:\n{}",
        files.server
    );
    insta::assert_snapshot!("go_multi_status_all_typeless_types", files.types);
    insta::assert_snapshot!("go_multi_status_all_typeless_client", files.client);
    insta::assert_snapshot!("go_multi_status_all_typeless_handlers", files.handlers);
    insta::assert_snapshot!("go_multi_status_all_typeless_server", files.server);
}

/// Multi-status + `error { }` on one endpoint: the multi-status server branch
/// emits the per-variant error mapping (via `emit_server_error_mapping`)
/// BEFORE the envelope guards. The roundtrip suite covers this combination at
/// runtime (`upsertPost2_error_validation`); this snapshot pins the generated
/// code so a regression fails here without needing that harness.
#[test]
fn multi_status_with_errors() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
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
            .contains("if strings.Contains(err.Error(), \"ValidationError\") {")
            && files
                .server
                .contains("http.Error(w, \"ValidationError\", 400)")
            && files
                .server
                .contains("http.Error(w, \"Unauthorized\", 401)"),
        "multi-status server must map declared error variants:\n{}",
        files.server
    );
    // ...and the envelope machinery is intact alongside it.
    assert!(
        files.server.contains("case 200, 201:")
            && files.server.contains("w.WriteHeader(result.Status)"),
        "envelope status validation and write must coexist with the error mapping:\n{}",
        files.server
    );
    insta::assert_snapshot!("go_multi_status_errors_server", files.server);
}

/// Multi-status COMBINED with request headers and query params: the inputs are
/// orthogonal to the response envelope (they emit before the response-decode
/// branch), but nothing else pins the combination — sema explicitly allows it
/// (and known-issues.md documents that request headers "combine freely with
/// multi-status"), so guard it at the generator level too.
#[test]
fn multi_status_with_inputs() {
    let files = generate_from_source(
        r#"
struct Job { Int id  String state }
endpoint restartJob: POST "/api/jobs/{id}/restart" {
    headers {
        String authorization
        Option<String> traceId
    }
    query {
        Int priority = 5
    }
    response {
        200: Job
        202: Job
        204
    }
}
"#,
    );
    // Client: the header/query inputs coexist with the envelope return.
    assert!(
        files
            .client
            .contains("req.Header.Set(\"Authorization\", authorization)")
            && files.client.contains("priority")
            && files.client.contains("(*RestartJobResponse, error)"),
        "client must carry header/query inputs and return the envelope:\n{}",
        files.client
    );
    // Server: input parsing and the envelope guards coexist.
    assert!(
        files.server.contains("r.Header.Get(\"Authorization\")")
            && files.server.contains("case 200, 202:")
            && files.server.contains("case 204:"),
        "server must parse inputs and keep the envelope guards:\n{}",
        files.server
    );
    insta::assert_snapshot!("go_multi_status_inputs_client", files.client);
    insta::assert_snapshot!("go_multi_status_inputs_server", files.server);
}

/// A typeless 205 (Reset Content): net/http only auto-suppresses bodies on
/// 1xx/204/304 — NOT 205 — so the server's body-shape guard is the only thing
/// keeping an illegal 205 body off the wire. Pin that 205 lands in the
/// typeless guard arm and the bodyless write path.
#[test]
fn multi_status_typeless_205() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
endpoint resetUser: PUT "/api/users/{id}/reset" {
    response {
        200: User
        205
    }
}
"#,
    );
    assert!(
        files.server.contains("case 205:")
            && files
                .server
                .contains("handler returned a body for a bodyless status"),
        "205 must be guarded as a bodyless status:\n{}",
        files.server
    );
    assert!(
        files.server.contains("w.WriteHeader(result.Status)"),
        "the bodyless write path must use the handler-chosen status:\n{}",
        files.server
    );
}

/// A multi-status block COMBINED with a request `body` parameter: the client
/// method has a parameter literally named `body`, and the response decode
/// declares `var body User` — legal in Go ONLY because the decode is
/// block-scoped inside the `ContentLength` guard, where shadowing the
/// (no-longer-needed) parameter is fine. TS hit a duplicate-identifier error
/// on this same combination and renamed its local; Go relies on the shadow
/// instead, so pin both halves: the request marshals from the parameter, and
/// the decode shadow stays inside its block. (The roundtrip's upsertPost2
/// covers this end to end; this pins it at the generator level.)
#[test]
fn multi_status_with_request_body() {
    let files = generate_from_source(
        r#"
struct User { Int id  String name }
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
    // The client takes the request body as the `body` parameter and the
    // request still marshals from it.
    assert!(
        files.client.contains(
            "func (c *ApiClient) UpsertUser(id string, body UpsertUserBody) (*UpsertUserResponse, error)"
        ) && files.client.contains("data, err := json.Marshal(body)"),
        "request body must marshal from the `body` parameter:\n{}",
        files.client
    );
    // The response decode shadows `body` INSIDE the ContentLength block — a
    // top-level `var body User` would be a compile error (redeclaration in the
    // function scope), so the guard and the decl must stay paired.
    assert!(
        files
            .client
            .contains("\tif resp.ContentLength != 0 {\n\t\tvar body User\n")
            && files.client.contains("result.Body = &body"),
        "response decode must shadow `body` only inside the ContentLength block:\n{}",
        files.client
    );
    // Marshal/request errors return the envelope arity (`nil, err`), matching
    // the `(*UpsertUserResponse, error)` signature.
    assert!(
        !files.client.contains("\t\treturn err\n"),
        "every error path must return the two-value arity:\n{}",
        files.client
    );
    insta::assert_snapshot!("go_multi_status_request_body_client", files.client);
}
