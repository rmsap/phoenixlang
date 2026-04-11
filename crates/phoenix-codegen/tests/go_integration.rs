//! Integration tests for Go code generation.

use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

const FULL_SCHEMA: &str = r#"
/** A registered user */
struct User {
    Int id
    String name
    String email
    Int age
    Option<String> bio
}

/** User permission levels */
enum Role { Admin  Editor  Viewer }

/** List all users */
endpoint listUsers: GET "/api/users" {
    query {
        Int page = 1
        Int limit = 20
    }
    response List<User>
}

/** Create a new user */
endpoint createUser: POST "/api/users" {
    body User omit { id, bio }
    response User
    error {
        ValidationError(400)
        Conflict(409)
    }
}

/** Get a user by ID */
endpoint getUser: GET "/api/users/{id}" {
    response User
    error { NotFound(404) }
}

/** Delete a user */
endpoint deleteUser: DELETE "/api/users/{id}" {
    error { NotFound(404) }
}
"#;

fn generate_full() -> phoenix_codegen::GoFiles {
    let tokens = tokenize(FULL_SCHEMA, SourceId(0));
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    phoenix_codegen::generate_go(&program, &result)
}

// ── types.go ────────────────────────────────────────────────────────

#[test]
fn types_has_package() {
    let files = generate_full();
    assert!(files.types.contains("package api"));
}

#[test]
fn types_has_user_struct() {
    let files = generate_full();
    assert!(files.types.contains("type User struct {"));
    assert!(files.types.contains("Id int64 `json:\"id\"`"));
    assert!(files.types.contains("Name string `json:\"name\"`"));
    assert!(files.types.contains("Bio *string `json:\"bio\"`"));
}

#[test]
fn types_has_enum() {
    let files = generate_full();
    assert!(files.types.contains("type Role string"));
    assert!(files.types.contains("RoleAdmin Role = \"Admin\""));
    assert!(files.types.contains("RoleEditor Role = \"Editor\""));
}

#[test]
fn types_has_derived_body() {
    let files = generate_full();
    assert!(files.types.contains("type CreateUserBody struct {"));
}

#[test]
fn types_has_doc_comment() {
    let files = generate_full();
    assert!(files.types.contains("// User is a registered user."));
}

// ── client.go ───────────────────────────────────────────────────────

#[test]
fn client_has_struct() {
    let files = generate_full();
    assert!(files.client.contains("type ApiClient struct {"));
    assert!(
        files
            .client
            .contains("func NewApiClient(baseURL string) *ApiClient")
    );
}

#[test]
fn client_has_all_methods() {
    let files = generate_full();
    assert!(files.client.contains("func (c *ApiClient) ListUsers("));
    assert!(files.client.contains("func (c *ApiClient) CreateUser("));
    assert!(files.client.contains("func (c *ApiClient) GetUser("));
    assert!(files.client.contains("func (c *ApiClient) DeleteUser("));
}

#[test]
fn client_has_json_encoding() {
    let files = generate_full();
    assert!(files.client.contains("json.Marshal(body)"));
    assert!(files.client.contains("json.NewDecoder(resp.Body).Decode"));
}

// ── handlers.go ─────────────────────────────────────────────────────

#[test]
fn handlers_has_interface() {
    let files = generate_full();
    assert!(files.handlers.contains("type Handlers interface {"));
    assert!(files.handlers.contains("ListUsers("));
    assert!(files.handlers.contains("CreateUser("));
    assert!(files.handlers.contains("GetUser("));
    assert!(files.handlers.contains("DeleteUser("));
}

// ── server.go ───────────────────────────────────────────────────────

#[test]
fn server_has_router() {
    let files = generate_full();
    assert!(
        files
            .server
            .contains("func NewRouter(h Handlers) *http.ServeMux")
    );
    assert!(files.server.contains("mux := http.NewServeMux()"));
}

#[test]
fn server_has_routes() {
    let files = generate_full();
    assert!(files.server.contains("\"GET /api/users\""));
    assert!(files.server.contains("\"POST /api/users\""));
    assert!(files.server.contains("\"GET /api/users/{id}\""));
    assert!(files.server.contains("\"DELETE /api/users/{id}\""));
}

#[test]
fn server_has_error_mapping() {
    let files = generate_full();
    assert!(files.server.contains("409"));
    assert!(files.server.contains("404"));
}

#[test]
fn server_void_returns_204() {
    let files = generate_full();
    assert!(files.server.contains("http.StatusNoContent"));
}

// ── Cross-cutting ───────────────────────────────────────────────────

#[test]
fn all_files_non_empty() {
    let files = generate_full();
    assert!(!files.types.is_empty());
    assert!(!files.client.is_empty());
    assert!(!files.handlers.is_empty());
    assert!(!files.server.is_empty());
}

#[test]
fn regeneration_is_deterministic() {
    let a = generate_full();
    let b = generate_full();
    assert_eq!(a.types, b.types);
    assert_eq!(a.client, b.client);
    assert_eq!(a.handlers, b.handlers);
    assert_eq!(a.server, b.server);
}

#[test]
fn all_files_have_header() {
    let files = generate_full();
    assert!(files.types.starts_with("// Generated by Phoenix Gen"));
    assert!(files.client.starts_with("// Generated by Phoenix Gen"));
    assert!(files.handlers.starts_with("// Generated by Phoenix Gen"));
    assert!(files.server.starts_with("// Generated by Phoenix Gen"));
}

#[test]
fn pascal_case_in_generated_code() {
    let files = generate_full();
    assert!(files.handlers.contains("ListUsers"));
    assert!(files.handlers.contains("CreateUser"));
    assert!(files.handlers.contains("GetUser"));
    assert!(files.handlers.contains("DeleteUser"));
}

#[test]
fn schema_does_not_affect_go_output() {
    let with_schema = r#"
struct User { Int id  String name }
endpoint getUser: GET "/api/users/{id}" { response User }
schema db { table users from User { primary key id } }
"#;
    let without_schema = r#"
struct User { Int id  String name }
endpoint getUser: GET "/api/users/{id}" { response User }
"#;
    let t1 = tokenize(with_schema, SourceId(0));
    let (p1, _) = parser::parse(&t1);
    let r1 = checker::check(&p1);
    let f1 = phoenix_codegen::generate_go(&p1, &r1);

    let t2 = tokenize(without_schema, SourceId(0));
    let (p2, _) = parser::parse(&t2);
    let r2 = checker::check(&p2);
    let f2 = phoenix_codegen::generate_go(&p2, &r2);

    assert_eq!(f1.types, f2.types);
    assert_eq!(f1.client, f2.client);
}

// ── Modifier and type tests ─────────────────────────────────────────

#[test]
fn pick_modifier_correct_fields() {
    let source = r#"
struct User { Int id  String name  String email  Int age }
endpoint updateEmail: PATCH "/api/users/{id}" {
    body User pick { email }
    response User
}
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    let files = phoenix_codegen::generate_go(&program, &result);
    assert!(files.types.contains("type UpdateEmailBody struct {"));
    assert!(files.types.contains("Email string"));
    let body_section = files.types.split("type UpdateEmailBody").nth(1).unwrap();
    assert!(
        !body_section.contains("Name "),
        "picked body should not have Name"
    );
    assert!(
        !body_section.contains("Age "),
        "picked body should not have Age"
    );
}

#[test]
fn partial_modifier_pointer_types() {
    let source = r#"
struct User { Int id  String name  Int age }
endpoint updateUser: PATCH "/api/users/{id}" {
    body User omit { id } partial
    response User
}
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    let files = phoenix_codegen::generate_go(&program, &result);
    assert!(files.types.contains("*string"));
    assert!(files.types.contains("*int64"));
    assert!(files.types.contains("omitempty"));
}

#[test]
fn map_type_in_struct() {
    let source = "struct Config { Map<String, String> settings  Bool enabled }";
    let tokens = tokenize(source, SourceId(0));
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    let files = phoenix_codegen::generate_go(&program, &result);
    assert!(files.types.contains("map[string]string"));
    assert!(files.types.contains("bool"));
}

#[test]
fn put_and_patch_methods() {
    let source = r#"
struct User { Int id  String name }
endpoint replaceUser: PUT "/api/users/{id}" {
    body User
    response User
}
endpoint patchUser: PATCH "/api/users/{id}" {
    body User
    response User
}
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    let files = phoenix_codegen::generate_go(&program, &result);
    assert!(files.server.contains("\"PUT /api/users/{id}\""));
    assert!(files.server.contains("\"PATCH /api/users/{id}\""));
    assert!(files.client.contains("func (c *ApiClient) ReplaceUser("));
    assert!(files.client.contains("func (c *ApiClient) PatchUser("));
}

#[test]
fn multiple_path_params_in_server() {
    let source = r#"
struct Comment { Int id  String text }
endpoint getComment: GET "/api/users/{userId}/posts/{postId}" {
    response Comment
}
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    let files = phoenix_codegen::generate_go(&program, &result);
    assert!(files.server.contains("r.PathValue(\"userId\")"));
    assert!(files.server.contains("r.PathValue(\"postId\")"));
}
