//! Integration tests for Python code generation.

use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

const FULL_SCHEMA: &str = r#"
/** A registered user */
struct User {
    id: Int
    name: String where self.length > 0 && self.length <= 100
    email: String
    age: Int where self >= 0 && self <= 150
    bio: Option<String>
}

/** User permission levels */
enum Role { Admin  Editor  Viewer }

/** List all users */
endpoint listUsers: GET "/api/users" {
    query {
        page: Int = 1
        limit: Int = 20
        search: Option<String>
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

fn generate_full() -> phoenix_codegen::PythonFiles {
    let tokens = tokenize(FULL_SCHEMA, SourceId(0));
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    phoenix_codegen::generate_python(&program, &result)
}

// ── __init__.py tests ───────────────────────────────────────────────

/// The generator must emit a non-empty `__init__.py` so the output dir is a
/// package and the intra-package relative imports (`from .models import ...`)
/// in client/handlers/server resolve. Locks that the package marker keeps
/// being produced — its absence would break those imports at import time.
#[test]
fn emits_package_init() {
    let files = generate_full();
    assert!(
        !files.init.trim().is_empty(),
        "expected a non-empty __init__.py, got: {:?}",
        files.init
    );
    // It is a comment-only marker, not runnable logic, so it must not declare
    // any names. Guard against accidentally emitting code here.
    assert!(
        files.init.lines().all(|l| {
            let l = l.trim();
            l.is_empty() || l.starts_with('#')
        }),
        "expected __init__.py to be comment-only, got: {:?}",
        files.init
    );
}

// ── models.py tests ─────────────────────────────────────────────────

#[test]
fn models_has_pydantic_import() {
    let files = generate_full();
    assert!(
        files
            .models
            .contains("from pydantic import BaseModel, Field")
    );
}

#[test]
fn models_has_user_class() {
    let files = generate_full();
    assert!(files.models.contains("class User(BaseModel):"));
    assert!(files.models.contains("    id: int"));
    assert!(files.models.contains("    name: str = Field("));
    assert!(files.models.contains("    email: str"));
    assert!(files.models.contains("    age: int = Field("));
    assert!(files.models.contains("    bio: str | None = None"));
}

#[test]
fn models_has_constraints() {
    let files = generate_full();
    assert!(files.models.contains("min_length=1"));
    assert!(files.models.contains("max_length=100"));
    assert!(files.models.contains("ge=0"));
    assert!(files.models.contains("le=150"));
}

#[test]
fn models_has_enum() {
    let files = generate_full();
    assert!(files.models.contains("class Role(str, Enum):"));
    assert!(files.models.contains("ADMIN = \"Admin\""));
    assert!(files.models.contains("EDITOR = \"Editor\""));
}

#[test]
fn models_has_derived_body() {
    let files = generate_full();
    assert!(files.models.contains("class CreateUserBody(BaseModel):"));
    // Should NOT contain id (omitted) or bio (omitted)
    assert!(
        !files
            .models
            .contains("class CreateUserBody(BaseModel):\n    id:")
    );
}

#[test]
fn models_has_doc_comments() {
    let files = generate_full();
    assert!(files.models.contains("# A registered user"));
    assert!(files.models.contains("# User permission levels"));
}

// ── client.py tests ─────────────────────────────────────────────────

#[test]
fn client_has_httpx_import() {
    let files = generate_full();
    assert!(files.client.contains("import httpx"));
}

#[test]
fn client_has_api_class() {
    let files = generate_full();
    assert!(files.client.contains("class ApiClient:"));
    assert!(files.client.contains("def __init__(self, base_url: str)"));
}

#[test]
fn client_has_list_with_query_params() {
    let files = generate_full();
    // The signature exceeds black's 88-col line length, so it is emitted in the
    // exploded one-param-per-line form.
    assert!(files.client.contains("async def list_users(\n"));
    assert!(files.client.contains("        page: int = 1,\n"));
    assert!(files.client.contains("        limit: int = 20,\n"));
    assert!(
        files
            .client
            .contains("        search: str | None = None,\n")
    );
}

#[test]
fn client_has_post_with_body() {
    let files = generate_full();
    assert!(
        files
            .client
            .contains("async def create_user(self, body: CreateUserBody)")
    );
    assert!(files.client.contains("json=body.model_dump()"));
}

#[test]
fn client_has_get_with_path_param() {
    let files = generate_full();
    assert!(files.client.contains("async def get_user(self, id: str)"));
    assert!(files.client.contains("/api/users/{id}"));
}

#[test]
fn client_has_delete() {
    let files = generate_full();
    assert!(
        files
            .client
            .contains("async def delete_user(self, id: str)")
    );
}

// ── handlers.py tests ───────────────────────────────────────────────

#[test]
fn handlers_has_class() {
    let files = generate_full();
    assert!(files.handlers.contains("class Handlers(Protocol):"));
}

#[test]
fn handlers_has_all_methods() {
    let files = generate_full();
    assert!(files.handlers.contains("async def list_users("));
    assert!(files.handlers.contains("async def create_user("));
    assert!(files.handlers.contains("async def get_user("));
    assert!(files.handlers.contains("async def delete_user("));
}

#[test]
fn handlers_imports_types() {
    let files = generate_full();
    assert!(files.handlers.contains("from .models import"));
    assert!(files.handlers.contains("User"));
    assert!(files.handlers.contains("CreateUserBody"));
}

// ── server.py tests ─────────────────────────────────────────────────

#[test]
fn server_has_fastapi_imports() {
    let files = generate_full();
    assert!(
        files
            .server
            .contains("from fastapi import APIRouter, HTTPException")
    );
}

#[test]
fn server_has_create_router() {
    let files = generate_full();
    assert!(
        files
            .server
            .contains("def create_router(handlers: Handlers) -> APIRouter:")
    );
}

#[test]
fn server_has_routes() {
    let files = generate_full();
    assert!(files.server.contains("@router.get(\"/api/users\")"));
    assert!(files.server.contains("@router.post(\"/api/users\")"));
    assert!(files.server.contains("@router.get(\"/api/users/{id}\")"));
    assert!(files.server.contains("@router.delete(\"/api/users/{id}\""));
}

#[test]
fn server_has_error_handling() {
    let files = generate_full();
    assert!(files.server.contains("HTTPException(status_code=409"));
    assert!(files.server.contains("HTTPException(status_code=404"));
}

#[test]
fn server_void_has_204() {
    let files = generate_full();
    assert!(files.server.contains("status_code=204"));
}

// ── Cross-cutting tests ─────────────────────────────────────────────

#[test]
fn all_files_non_empty() {
    let files = generate_full();
    assert!(!files.models.is_empty());
    assert!(!files.client.is_empty());
    assert!(!files.handlers.is_empty());
    assert!(!files.server.is_empty());
}

#[test]
fn regeneration_is_deterministic() {
    let a = generate_full();
    let b = generate_full();
    assert_eq!(a.models, b.models);
    assert_eq!(a.client, b.client);
    assert_eq!(a.handlers, b.handlers);
    assert_eq!(a.server, b.server);
}

#[test]
fn snake_case_in_generated_code() {
    let files = generate_full();
    // Endpoint names should be snake_case
    assert!(files.client.contains("list_users"));
    assert!(files.client.contains("create_user"));
    assert!(files.client.contains("get_user"));
    assert!(files.client.contains("delete_user"));
}

#[test]
fn all_files_have_header() {
    let files = generate_full();
    assert!(files.models.starts_with("# Generated by Phoenix Gen"));
    assert!(files.client.starts_with("# Generated by Phoenix Gen"));
    assert!(files.handlers.starts_with("# Generated by Phoenix Gen"));
    assert!(files.server.starts_with("# Generated by Phoenix Gen"));
}

// ── Derived type modifier tests ─────────────────────────────────────

#[test]
fn pick_modifier_generates_correct_fields() {
    let source = r#"
struct User { id: Int  name: String  email: String  age: Int }
endpoint updateEmail: PATCH "/api/users/{id}" {
    body User pick { email }
    response User
}
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    let files = phoenix_codegen::generate_python(&program, &result);
    assert!(files.models.contains("class UpdateEmailBody(BaseModel):"));
    assert!(files.models.contains("    email: str"));
    // Should NOT have name or age
    let body_section = files.models.split("class UpdateEmailBody").nth(1).unwrap();
    assert!(!body_section.contains("name:"));
    assert!(!body_section.contains("age:"));
}

#[test]
fn partial_modifier_makes_fields_optional() {
    let source = r#"
struct User { id: Int  name: String  age: Int }
endpoint updateUser: PATCH "/api/users/{id}" {
    body User omit { id } partial
    response User
}
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    let files = phoenix_codegen::generate_python(&program, &result);
    assert!(files.models.contains("name: str | None = None"));
    assert!(files.models.contains("age: int | None = None"));
}

#[test]
fn partial_with_constraints_inherited() {
    let source = r#"
struct User { id: Int  name: String where self.length > 0  age: Int where self >= 0 }
endpoint updateUser: PATCH "/api/users/{id}" {
    body User omit { id } partial
    response User
}
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    let files = phoenix_codegen::generate_python(&program, &result);
    // Optional fields with constraints should use Field(None, ...)
    assert!(
        files
            .models
            .contains("name: str | None = Field(None, min_length=1)")
    );
    assert!(files.models.contains("age: int | None = Field(None, ge=0)"));
}

// ── Additional type/method tests ────────────────────────────────────

#[test]
fn map_type_in_struct() {
    let source = r#"
struct Config { settings: Map<String, String> }
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    let files = phoenix_codegen::generate_python(&program, &result);
    assert!(files.models.contains("settings: dict[str, str]"));
}

#[test]
fn put_and_patch_methods() {
    let source = r#"
struct User { id: Int  name: String }
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
    let files = phoenix_codegen::generate_python(&program, &result);
    assert!(files.server.contains("@router.put("));
    assert!(files.server.contains("@router.patch("));
    assert!(files.client.contains("self.client.put("));
    assert!(files.client.contains("self.client.patch("));
}

#[test]
fn multiple_path_params_snake_case() {
    let source = r#"
struct Comment { id: Int  text: String }
endpoint getComment: GET "/api/users/{userId}/posts/{postId}" {
    response Comment
}
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    let files = phoenix_codegen::generate_python(&program, &result);
    assert!(files.client.contains("user_id: str"));
    assert!(files.client.contains("post_id: str"));
    assert!(
        files
            .client
            .contains("/api/users/{user_id}/posts/{post_id}")
    );
}

#[test]
fn schema_does_not_affect_python_output() {
    let with_schema = r#"
struct User { id: Int  name: String }
endpoint getUser: GET "/api/users/{id}" { response User }
schema db { table users from User { primary key id } }
"#;
    let without_schema = r#"
struct User { id: Int  name: String }
endpoint getUser: GET "/api/users/{id}" { response User }
"#;
    let t1 = tokenize(with_schema, SourceId(0));
    let (p1, _) = parser::parse(&t1);
    let r1 = checker::check(&p1);
    let f1 = phoenix_codegen::generate_python(&p1, &r1);

    let t2 = tokenize(without_schema, SourceId(0));
    let (p2, _) = parser::parse(&t2);
    let r2 = checker::check(&p2);
    let f2 = phoenix_codegen::generate_python(&p2, &r2);

    assert_eq!(f1.models, f2.models);
    assert_eq!(f1.client, f2.client);
}

/// End-to-end Python codegen across a schema containing a `dyn Trait`
/// field. The Python codegen should emit the trait name verbatim so the
/// user's hand-written `Protocol` / ABC can line up; a fall-through to
/// `object` would silently erase the contract.
#[test]
fn dyn_trait_in_schema_generates_python_models() {
    let source = r#"
trait Renderable {
    function render(self) -> String
}
struct Card {
    id: Int
    hero: dyn Renderable
}
endpoint getCard: GET "/api/cards/{id}" {
    response Card
}
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "check errors: {:?}",
        result.diagnostics
    );
    let files = phoenix_codegen::generate_python(&program, &result);
    assert!(
        files.models.contains("hero: Renderable"),
        "expected `hero: Renderable` in Python models, got: {}",
        files.models
    );
}

/// Python codegen: two unrelated `dyn` traits on the same struct must
/// map each field to the corresponding trait name without cross-
/// contamination. Regression guard for trait-name caching.
#[test]
fn dyn_multiple_traits_on_same_struct_generates_python() {
    let source = r#"
trait Renderable {
    function render(self) -> String
}
trait Serializable {
    function serialize(self) -> String
}
struct Widget {
    id: Int
    view: dyn Renderable
    data: dyn Serializable
}
endpoint getWidget: GET "/api/widgets/{id}" {
    response Widget
}
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "check errors: {:?}",
        result.diagnostics
    );
    let files = phoenix_codegen::generate_python(&program, &result);
    assert!(
        files.models.contains("view: Renderable"),
        "expected `view: Renderable`, got: {}",
        files.models
    );
    assert!(
        files.models.contains("data: Serializable"),
        "expected `data: Serializable`, got: {}",
        files.models
    );
}

/// Python codegen for a schema where `dyn Trait` is the endpoint's
/// response type. Verifies the generated client uses the trait name
/// directly rather than falling through to `object`.
#[test]
fn dyn_trait_as_endpoint_response_generates_python() {
    let source = r#"
trait Renderable {
    function render(self) -> String
}
struct Widget {
    id: Int
    body: dyn Renderable
}
endpoint getWidget: GET "/api/widgets/{id}" {
    response Widget
}
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "check errors: {:?}",
        result.diagnostics
    );
    let files = phoenix_codegen::generate_python(&program, &result);
    assert!(
        files.models.contains("body: Renderable"),
        "expected `body: Renderable`, got: {}",
        files.models
    );
    assert!(
        !files.models.contains(": object") && !files.models.contains(": Any"),
        "Python output should not fall through to `object` or `Any` for dyn fields"
    );
}

/// Python codegen for `dyn Trait` nested inside container types —
/// `List<dyn T>`, `Option<dyn T>`. The Python type mapper recurses
/// through generic args, so the nested case must produce
/// `list[Renderable]` and `Renderable | None`. Closes the audit gap on
/// nested-position coverage for the Python backend.
#[test]
fn dyn_trait_nested_in_container_types_python() {
    let source = r#"
trait Renderable {
    function render(self) -> String
}
struct Gallery {
    id: Int
    items: List<dyn Renderable>
    featured: Option<dyn Renderable>
}
endpoint getGallery: GET "/api/galleries/{id}" {
    response Gallery
}
"#;
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "check errors: {:?}",
        result.diagnostics
    );
    let files = phoenix_codegen::generate_python(&program, &result);
    assert!(
        files.models.contains("list[Renderable]"),
        "expected `list[Renderable]` for `List<dyn Renderable>`, got: {}",
        files.models
    );
    assert!(
        files.models.contains("Renderable | None"),
        "expected `Renderable | None` for `Option<dyn Renderable>`, got: {}",
        files.models
    );
    assert!(
        !files.models.contains(": object") && !files.models.contains(": Any"),
        "Python output must not fall through to `object` or `Any` for nested dyn"
    );
}
