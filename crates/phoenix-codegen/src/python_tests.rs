//! Inline tests for the Python generator, split out of `python.rs` to keep
//! the generator file readable (each feature slice grows both halves).
//! Declared as `mod tests` inside `python.rs` via `#[path]` so the module
//! path — and therefore every insta snapshot name — is unchanged by the move.

use super::*;
use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

/// An interior empty doc line renders as a bare `#` (no trailing space) so
/// ruff E265 stays happy. Guards the empty-line branch of
/// `render_hash_comment`, which the doc-comment integration tests don't hit.
#[test]
fn render_hash_comment_blanks_out_empty_lines() {
    assert_eq!(
        render_hash_comment("    ", "first\n\nthird"),
        "    # first\n    #\n    # third\n"
    );
}

fn generate_from_source(source: &str) -> PythonFiles {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "check errors: {:?}",
        result.diagnostics
    );
    generate_python(&program, &result)
}

#[test]
fn struct_to_model() {
    let files = generate_from_source(
        r#"
/** A registered user */
struct User {
    id: Int
    name: String
    bio: Option<String>
}
"#,
    );
    insta::assert_snapshot!("py_struct_to_model", files.models);
}

#[test]
fn simple_enum() {
    let files = generate_from_source("enum Role { Admin  Editor  Viewer }");
    insta::assert_snapshot!("py_simple_enum", files.models);
}

#[test]
fn model_with_constraints() {
    let files = generate_from_source(
        r#"
struct User {
    id: Int
    name: String where self.length > 0 && self.length <= 100
    age: Int where self >= 0 && self <= 150
}
"#,
    );
    insta::assert_snapshot!("py_model_constraints", files.models);
}

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
    insta::assert_snapshot!("py_get_client", files.client);
    insta::assert_snapshot!("py_get_handler", files.handlers);
    insta::assert_snapshot!("py_get_server", files.server);
}

#[test]
fn post_with_body() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String  email: String }
endpoint createUser: POST "/api/users" {
    body User omit { id }
    response User
    error { Conflict(409) }
}
"#,
    );
    insta::assert_snapshot!("py_post_client", files.client);
    insta::assert_snapshot!("py_post_server", files.server);
    insta::assert_snapshot!("py_post_models", files.models);
}

#[test]
fn query_params_with_defaults() {
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
    insta::assert_snapshot!("py_query_client", files.client);
    insta::assert_snapshot!("py_query_handler", files.handlers);
    insta::assert_snapshot!("py_query_server", files.server);
}

/// A required, camelCase query param renders `= Query(alias=...)` — a
/// syntactic default — so it must sort AFTER a required plain param, or the
/// generated server is invalid Python ("non-default argument follows default
/// argument"). Guards the parameter partitioning in `emit_server_route`.
#[test]
fn required_aliased_query_param_sorts_after_plain() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint searchUsers: GET "/api/users" {
    query {
        maxResults: Int
        page: Int
    }
    response List<User>
}
"#,
    );
    let plain = files.server.find("page: int").expect("plain param present");
    let aliased = files
        .server
        .find("max_results: int = Query(alias=\"maxResults\")")
        .expect("aliased param present");
    assert!(
        plain < aliased,
        "required plain param must precede the aliased (defaulted) one:\n{}",
        files.server
    );
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
    insta::assert_snapshot!("py_void_client", files.client);
    insta::assert_snapshot!("py_void_server", files.server);
}

/// A request header with an auto-derived wire name (`idempotencyKey →
/// Idempotency-Key`) must: appear as a snake_case keyword-only client arg,
/// be sent on a `headers` dict keyed by the EXACT wire name, bind on the
/// server via `Header(alias="<wire_name>")`, and thread into the handler.
#[test]
fn request_header_auto_wire_name() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
    headers { idempotencyKey: String }
    response User
}
"#,
    );
    insta::assert_snapshot!("py_req_header_client", files.client);
    insta::assert_snapshot!("py_req_header_handler", files.handlers);
    insta::assert_snapshot!("py_req_header_server", files.server);
    // Client sends the exact wire name; server aliases to it.
    assert!(
        files
            .client
            .contains("headers[\"Idempotency-Key\"] = idempotency_key"),
        "client must key the header dict on the wire name:\n{}",
        files.client
    );
    assert!(
        files.server.contains("Header(alias=\"Idempotency-Key\")"),
        "server must alias the Header param to the wire name:\n{}",
        files.server
    );
}

/// An explicit `as "Exact-Wire-Name"` override pins the wire name verbatim,
/// overriding the auto Title-Kebab transform on both client and server.
#[test]
fn request_header_as_override() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
    headers { token: String as "X-Auth" }
    response User
}
"#,
    );
    assert!(
        files.client.contains("headers[\"X-Auth\"] = token"),
        "client must use the override wire name:\n{}",
        files.client
    );
    assert!(
        files
            .server
            .contains("token: str = Header(alias=\"X-Auth\")"),
        "server must alias to the override wire name:\n{}",
        files.server
    );
}

/// An optional request header renders `T | None = Header(None, alias=...)`
/// on the server, a defaulted keyword arg on the client, and is only added
/// to the wire dict when present.
#[test]
fn optional_request_header() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
    headers { traceId: Option<String> }
    response User
}
"#,
    );
    assert!(
        files
            .server
            .contains("trace_id: str | None = Header(None, alias=\"Trace-Id\")"),
        "optional header must render Header(None, alias=...):\n{}",
        files.server
    );
    assert!(
        files.client.contains("trace_id: str | None = None"),
        "optional header must be a defaulted client kwarg:\n{}",
        files.client
    );
    assert!(
        files
            .client
            .contains("if trace_id is not None:\n            headers[\"Trace-Id\"] = trace_id"),
        "optional header must be guarded on the wire:\n{}",
        files.client
    );
}

/// A `Bool` request header must be serialized as lowercase `true`/`false` on
/// the wire — NOT Python's `str(True)` → `"True"`. This matches every other
/// path (Go `strconv.FormatBool`, TS `String(bool)`, and this generator's own
/// response-header set/read which use lowercase + `== "true"`), so a bool
/// header round-trips across languages. Regression guard for the capitalized
/// `str(...)` bug.
#[test]
fn bool_request_header_serializes_lowercase() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
    headers { debug: Bool }
    response User
}
"#,
    );
    assert!(
        files
            .client
            .contains("headers[\"Debug\"] = \"true\" if debug else \"false\""),
        "bool header must serialize lowercase, not str(bool):\n{}",
        files.client
    );
    assert!(
        !files.client.contains("str(debug)"),
        "bool header must not use str() (yields capitalized True/False):\n{}",
        files.client
    );
}

/// A request header with a literal default binds the default into both the
/// server `Header(<default>, alias=...)` and the client kwarg
/// (`max_stale: int = 60`). Per the documented "defaulted request headers"
/// gap, the client sends it unconditionally (no `Option<T>` guard).
#[test]
fn defaulted_request_header_binds_default() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint createUser: POST "/api/users" {
    headers { maxStale: Int = 60 }
    response User
}
"#,
    );
    assert!(
        files
            .server
            .contains("max_stale: int = Header(60, alias=\"Max-Stale\")"),
        "server must bind the default into Header(...):\n{}",
        files.server
    );
    assert!(
        files.client.contains("max_stale: int = 60"),
        "client must default the kwarg:\n{}",
        files.client
    );
    assert!(
        files
            .client
            .contains("headers[\"Max-Stale\"] = str(max_stale)"),
        "client must send the defaulted header unconditionally:\n{}",
        files.client
    );
}

/// An endpoint with response headers returns a typed `<Endpoint>Result`
/// envelope: a pydantic model bundling `body` + each header (snake_case),
/// the handler returns it, the server sets headers on the `Response` and
/// returns `result.body`, and the client reads headers off the response.
#[test]
fn response_header_envelope() {
    let files = generate_from_source(
        r#"
struct Post { id: Int  title: String }
endpoint getPost: GET "/api/posts/{id}" {
    response Post headers { ratelimitRemaining: Int as "X-RateLimit-Remaining" }
}
"#,
    );
    insta::assert_snapshot!("py_resp_header_models", files.models);
    insta::assert_snapshot!("py_resp_header_client", files.client);
    insta::assert_snapshot!("py_resp_header_handler", files.handlers);
    insta::assert_snapshot!("py_resp_header_server", files.server);
    // Envelope shape.
    assert!(
        files.models.contains(
            "class GetPostResult(BaseModel):\n    body: Post\n    ratelimit_remaining: int\n"
        ),
        "envelope model must bundle body + typed header:\n{}",
        files.models
    );
    // Handler returns the envelope.
    assert!(
        files.handlers.contains("-> GetPostResult: ..."),
        "handler must return the envelope:\n{}",
        files.handlers
    );
    // Server sets the header off the result and returns the body.
    assert!(
        files.server.contains(
            "response.headers[\"X-RateLimit-Remaining\"] = str(result.ratelimit_remaining)"
        ),
        "server must set the response header from the result:\n{}",
        files.server
    );
    assert!(
        files.server.contains("return result.body\n"),
        "server must return the bare body:\n{}",
        files.server
    );
    // Client reads the header into the envelope it returns.
    assert!(
        files
            .client
            .contains("ratelimit_remaining_raw = response.headers.get(\"X-RateLimit-Remaining\")"),
        "client must read the response header off the wire:\n{}",
        files.client
    );
    assert!(
        files.client.contains("return GetPostResult("),
        "client must return the typed envelope:\n{}",
        files.client
    );
}

// ── Multipart upload / binary download ──────────────────────────

/// A request body containing a `File` field is `multipart/form-data`: the
/// server explodes it into `<f>: UploadFile = File(...)` + `<s>: <T> =
/// Form(...)` params (importing File/Form/UploadFile from fastapi), the
/// handler takes `UploadFile`/scalar params, and the client sends `files=`/
/// `data=` with the file field typed `bytes`. No `XBody` model is emitted.
#[test]
fn multipart_request_body() {
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
    insta::assert_snapshot!("py_multipart_models", files.models);
    insta::assert_snapshot!("py_multipart_client", files.client);
    insta::assert_snapshot!("py_multipart_handler", files.handlers);
    insta::assert_snapshot!("py_multipart_server", files.server);
    // Server: a non-aliased file is a bare `UploadFile` param (FastAPI
    // auto-detects it; avoids ruff B008 on a `File(...)` default), scalars use
    // `Form(...)`. Only `Form` + `UploadFile` are imported (no `File`, since no
    // file needs an alias here).
    assert!(
        files
            .server
            .contains("from fastapi import APIRouter, Form, UploadFile"),
        "server must import Form/UploadFile from fastapi:\n{}",
        files.server
    );
    assert!(
        files.server.contains("avatar: UploadFile,"),
        "non-aliased File field must be a bare UploadFile param:\n{}",
        files.server
    );
    assert!(
        !files.server.contains("File("),
        "non-aliased File field must not emit a File(...) default (ruff B008):\n{}",
        files.server
    );
    assert!(
        files.server.contains("caption: str = Form(...)"),
        "scalar field must bind via Form(...):\n{}",
        files.server
    );
    // No XBody model emitted for a multipart body.
    assert!(
        !files.models.contains("UploadAvatarBody"),
        "multipart body must not emit an XBody model:\n{}",
        files.models
    );
    // Client sends files=/data=, file field typed `FileUpload`.
    assert!(
        files.client.contains("avatar: FileUpload"),
        "client file field must be typed FileUpload:\n{}",
        files.client
    );
    assert!(
        files.client.contains("files=files") && files.client.contains("data=data"),
        "client must send files=/data=:\n{}",
        files.client
    );
    assert!(
        files
            .client
            .contains("files[\"avatar\"] = (avatar.filename, avatar.content)"),
        "client must put the file in the files dict (filename, content) by wire name:\n{}",
        files.client
    );
    // The shared FileUpload dataclass is emitted into models.py and imported.
    assert!(
        files.models.contains("class FileUpload:"),
        "models must emit the FileUpload dataclass:\n{}",
        files.models
    );
    assert!(
        files.client.contains("from .models import") && files.client.contains("FileUpload"),
        "client must import FileUpload:\n{}",
        files.client
    );
    // Handler takes UploadFile + scalar.
    assert!(
        files.handlers.contains("avatar: UploadFile"),
        "handler must take UploadFile param:\n{}",
        files.handlers
    );
}

/// An `Option<File>` body field is an optional upload: server renders
/// `UploadFile | None = File(None)`, client `bytes | None = None` only added
/// to the files dict when present, handler `UploadFile | None = None`.
#[test]
fn multipart_optional_file() {
    let files = generate_from_source(
        r#"
struct MaybeUpload { avatar: Option<File>  caption: String }
endpoint upload: POST "/api/maybe" {
    body MaybeUpload
}
"#,
    );
    insta::assert_snapshot!("py_multipart_optional_client", files.client);
    insta::assert_snapshot!("py_multipart_optional_handler", files.handlers);
    insta::assert_snapshot!("py_multipart_optional_server", files.server);
    assert!(
        files.server.contains("avatar: UploadFile | None = None"),
        "optional non-aliased file must render a bare UploadFile | None = None:\n{}",
        files.server
    );
    assert!(
        files.client.contains("avatar: FileUpload | None = None"),
        "optional file client param must default None:\n{}",
        files.client
    );
    assert!(
            files.client.contains(
                "if avatar is not None:\n            files[\"avatar\"] = (avatar.filename, avatar.content)"
            ),
            "optional file must be guarded before adding to files:\n{}",
            files.client
        );
    assert!(
        files.handlers.contains("avatar: UploadFile | None = None"),
        "optional file handler param:\n{}",
        files.handlers
    );
}

/// A camelCase `File` field needs the wire name pinned. Since a `File(...)`
/// default trips ruff B008 (unlike `Form`/`Query`/`Header`), the alias is
/// carried in the *annotation* — `Annotated[UploadFile, File(alias="<wire>")]`
/// — not in default position, pulling in `Annotated` + `File` imports.
#[test]
fn multipart_aliased_file_uses_annotated() {
    let files = generate_from_source(
        r#"
struct Upload { avatarImage: File  caption: String }
endpoint upload: POST "/api/upload" {
    body Upload
}
"#,
    );
    insta::assert_snapshot!("py_multipart_aliased_server", files.server);
    assert!(
        files.server.contains("from typing import Annotated"),
        "aliased file must import Annotated:\n{}",
        files.server
    );
    assert!(
        files
            .server
            .contains("from fastapi import APIRouter, File, Form, UploadFile"),
        "aliased file must import File (for the alias):\n{}",
        files.server
    );
    assert!(
        files
            .server
            .contains("avatar_image: Annotated[UploadFile, File(alias=\"avatarImage\")]"),
        "aliased file must use Annotated[..., File(alias=...)]:\n{}",
        files.server
    );
    // Client keys the files dict by the wire name.
    assert!(
        files
            .client
            .contains("files[\"avatarImage\"] = (avatar_image.filename, avatar_image.content)"),
        "client must key the file by the wire name:\n{}",
        files.client
    );
}

/// A response whose type is a single-`File` struct is a binary download: the
/// server returns `Response(content=..., media_type="application/octet-stream")`,
/// the handler returns `bytes`, the client returns `response.content`. No
/// response model is emitted and the response struct is not imported.
#[test]
fn binary_response_download() {
    let files = generate_from_source(
        r#"
struct Doc { data: File }
endpoint download: GET "/api/doc/{id}" {
    response Doc
}
"#,
    );
    insta::assert_snapshot!("py_binary_models", files.models);
    insta::assert_snapshot!("py_binary_client", files.client);
    insta::assert_snapshot!("py_binary_handler", files.handlers);
    insta::assert_snapshot!("py_binary_server", files.server);
    // Server returns a Response with octet-stream media type.
    assert!(
        files
            .server
            .contains("from fastapi import APIRouter, Response"),
        "server must import Response:\n{}",
        files.server
    );
    assert!(
        files.server.contains("-> Response:"),
        "binary route must return Response:\n{}",
        files.server
    );
    assert!(
        files
            .server
            .contains("return Response(content=data, media_type=\"application/octet-stream\")"),
        "server must wrap bytes in an octet-stream Response:\n{}",
        files.server
    );
    // Handler returns bytes; client returns response.content.
    assert!(
        files.handlers.contains("-> bytes: ..."),
        "handler must return bytes:\n{}",
        files.handlers
    );
    assert!(
        files.client.contains("-> bytes:") && files.client.contains("return response.content"),
        "client must return response.content as bytes:\n{}",
        files.client
    );
    // No model emitted for the file-bearing response struct.
    assert!(
        !files.models.contains("class Doc"),
        "file-bearing response struct must not emit a model:\n{}",
        files.models
    );
    assert!(
        !files.client.contains("UploadFile"),
        "client must not reference UploadFile:\n{}",
        files.client
    );
}

/// A schema whose only model-relevant declaration is a multipart body (a
/// file-bearing struct emits no model) with no response model: models.py
/// emits just the `FileUpload` dataclass under `from dataclasses import
/// dataclass`, and NO `from pydantic import BaseModel`. Guards the import
/// block + spacing for the dataclass-only branch (`needs_base_model == false`).
#[test]
fn multipart_only_no_response_emits_dataclass_only() {
    let files = generate_from_source(
        r#"
struct AvatarUpload { avatar: File  caption: String }
endpoint upload: POST "/api/upload" { body AvatarUpload }
"#,
    );
    assert!(
        files.models.contains("from dataclasses import dataclass"),
        "the FileUpload dataclass needs the dataclasses import:\n{}",
        files.models
    );
    assert!(
        !files.models.contains("from pydantic import BaseModel"),
        "no pydantic BaseModel is needed when the only struct is a multipart body:\n{}",
        files.models
    );
    assert!(
        files.models.contains("class FileUpload:"),
        "FileUpload dataclass must be emitted:\n{}",
        files.models
    );
    assert!(
        !files.models.contains("class AvatarUpload"),
        "file-bearing body struct must not emit a model:\n{}",
        files.models
    );
    insta::assert_snapshot!("py_multipart_only_models", files.models);
}

/// A multipart endpoint with a path param: the generated FastAPI route must
/// order params as path param(s) → required `UploadFile` → defaulted
/// `Form(...)`. Python forbids a non-default param after a defaulted one, so
/// the relative order is load-bearing (a required `UploadFile` must precede
/// every `Form(...)` scalar). The handler call forwards each field by name.
#[test]
fn multipart_with_path_param_orders_route_params() {
    let files = generate_from_source(
        r#"
struct AvatarUpload { avatar: File  caption: String }
struct UploadResult { url: String }
endpoint uploadAvatar: POST "/api/authors/{id}/avatar" {
    body AvatarUpload
    response UploadResult
}
"#,
    );
    let id_pos = files.server.find("id: str").expect("id param in route");
    let avatar_pos = files
        .server
        .find("avatar: UploadFile")
        .expect("avatar param in route");
    let caption_pos = files
        .server
        .find("caption: str = Form(...)")
        .expect("caption param in route");
    assert!(
        id_pos < avatar_pos && avatar_pos < caption_pos,
        "route params must order path → UploadFile → Form(...):\n{}",
        files.server
    );
    // Each exploded field is forwarded to the handler by name.
    for frag in ["id=id", "avatar=avatar", "caption=caption"] {
        assert!(
            files.server.contains(frag),
            "route must forward `{frag}` to the handler:\n{}",
            files.server
        );
    }
}

/// A `File` field relaxed by `partial` is an OPTIONAL upload, exactly like
/// `Option<File>`: the client/handler params become `FileUpload | None = None`
/// / `UploadFile | None = None` and the server binds a bare `UploadFile | None
/// = None` (no `File(...)` default). Regression guard for the `partial`-over-a-
/// multipart-body parity fix (the `is_file` branch must honor `f.optional`).
#[test]
fn multipart_partial_file_is_optional() {
    let files = generate_from_source(
        r#"
struct AvatarUpload { avatar: File  caption: String }
endpoint uploadAvatar: POST "/api/avatar" {
    body AvatarUpload partial { avatar }
}
"#,
    );
    assert!(
        files.client.contains("avatar: FileUpload | None = None"),
        "partial-relaxed file client param must be optional:\n{}",
        files.client
    );
    assert!(
        files.handlers.contains("avatar: UploadFile | None = None"),
        "partial-relaxed file handler param must be optional:\n{}",
        files.handlers
    );
    assert!(
        files.server.contains("avatar: UploadFile | None = None"),
        "partial-relaxed file server param must be a bare optional UploadFile:\n{}",
        files.server
    );
    // The optional file must be guarded before being added to the files dict.
    assert!(
        files.client.contains("if avatar is not None:"),
        "partial-relaxed file must be guarded on the client:\n{}",
        files.client
    );
}

/// A multipart body with `Int`/`Bool` scalars: the client `data` dict
/// stringifies them onto the form. A `bool` serializes as canonical lowercase
/// `"true"`/`"false"` (the TS server compares `=== "true"`), and the ternary
/// is emitted WITHOUT surrounding parens — it lands on the RHS of a
/// `data[...] = ` assignment, where `black` strips redundant parentheses, so
/// the wrapped form would fail `black --check`. Regression guard for that
/// (caught by the multipart round-trip's `crop` field).
#[test]
fn multipart_scalar_bool_data_line_is_paren_free() {
    let files = generate_from_source(
        r#"
struct Upload { avatar: File  rotation: Int  crop: Bool }
endpoint upload: POST "/api/upload" { body Upload }
"#,
    );
    assert!(
        files
            .client
            .contains("data[\"crop\"] = \"true\" if crop else \"false\""),
        "multipart bool must serialize lowercase with no wrapping parens (black strips them):\n{}",
        files.client
    );
    assert!(
        files.client.contains("data[\"rotation\"] = rotation"),
        "multipart int is passed through to the form data dict as-is:\n{}",
        files.client
    );
}

/// An endpoint that is BOTH a multipart upload AND a binary download
/// (`body_is_multipart` + `response_is_binary`): the server route explodes
/// the body into `UploadFile`/`Form(...)` params yet still returns a
/// `Response` (the binary branch), the handler takes the exploded params and
/// returns `bytes`, and the client sends `files=`/`data=` while returning
/// `response.content`. Guards that the multipart-param and binary-response
/// branches compose in one route without conflicting.
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
    // Server: multipart params + a Response return + octet-stream wrap.
    assert!(
        files
            .server
            .contains("from fastapi import APIRouter, Form, Response, UploadFile"),
        "server must import Form/Response/UploadFile:\n{}",
        files.server
    );
    assert!(
        files.server.contains("avatar: UploadFile,")
            && files.server.contains("caption: str = Form(...)")
            && files.server.contains("-> Response:"),
        "server route must explode the body yet return Response:\n{}",
        files.server
    );
    assert!(
        files
            .server
            .contains("data = await handlers.convert_avatar(")
            && files
                .server
                .contains("return Response(content=data, media_type=\"application/octet-stream\")"),
        "server must call the handler with the exploded args and wrap the bytes:\n{}",
        files.server
    );
    // Handler takes the exploded params and returns bytes.
    assert!(
        files.handlers.contains("avatar: UploadFile") && files.handlers.contains("-> bytes: ..."),
        "handler must take UploadFile + return bytes:\n{}",
        files.handlers
    );
    // Client sends files=/data= and returns the raw response bytes.
    assert!(
        files.client.contains("files=files")
            && files.client.contains("data=data")
            && files.client.contains("return response.content"),
        "client must send multipart and return response.content:\n{}",
        files.client
    );
}

/// A multi-line doc comment must have EVERY line prefixed with `#`, not just
/// the first — otherwise continuation lines leak into the file as code.
/// Regression guard for `render_hash_comment`.
#[test]
fn multiline_doc_comment_is_fully_commented() {
    let files = generate_from_source(
        r#"
struct Widget { id: Int }
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
        files
            .client
            .contains("    # Fetch a widget by id\n    # with extra detail on the second line\n"),
        "every doc line must be commented:\n{}",
        files.client
    );
    // The continuation line must never appear UNcommented (leaked as code).
    assert!(
        !files.client.contains("\n    with extra detail"),
        "continuation doc line leaked as code:\n{}",
        files.client
    );
}

#[test]
fn snake_case_conversion() {
    assert_eq!(to_snake_case("createUser"), "create_user");
    assert_eq!(to_snake_case("getHTTPResponse"), "get_h_t_t_p_response");
    assert_eq!(to_snake_case("id"), "id");
    assert_eq!(to_snake_case("userId"), "user_id");
    assert_eq!(to_snake_case("listUsers"), "list_users");
}

#[test]
fn screaming_snake_conversion() {
    assert_eq!(to_screaming_snake("Admin"), "ADMIN");
    assert_eq!(to_screaming_snake("NotFound"), "NOT_FOUND");
    assert_eq!(to_screaming_snake("ValidationError"), "VALIDATION_ERROR");
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
    insta::assert_snapshot!("py_multi_models", files.models);
    insta::assert_snapshot!("py_multi_client", files.client);
    insta::assert_snapshot!("py_multi_handlers", files.handlers);
    insta::assert_snapshot!("py_multi_server", files.server);
}

// ── Gap-filling tests ───────────────────────────────────────────

/// `pick` modifier in derived body.
#[test]
fn body_pick_only() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String  email: String  age: Int }
endpoint updateEmail: PATCH "/api/users/{id}" {
    body User pick { email }
    response User
}
"#,
    );
    insta::assert_snapshot!("py_pick_models", files.models);
    insta::assert_snapshot!("py_pick_client", files.client);
}

/// `partial` modifier makes all fields optional.
#[test]
fn body_partial() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String where self.length > 0  age: Int where self >= 0 }
endpoint updateUser: PATCH "/api/users/{id}" {
    body User omit { id } partial
    response User
}
"#,
    );
    insta::assert_snapshot!("py_partial_models", files.models);
}

/// A `partial` body field whose type already contains an inner `| None`
/// (here `List<Option<String>>` → `list[str | None]`) must still gain the
/// *outer* `| None` from `partial` — the container is not itself optional.
/// Guards against the `already_optional` heuristic matching the inner union.
#[test]
fn body_partial_inner_optional_container() {
    let files = generate_from_source(
        r#"
struct Post { id: Int  tags: List<Option<String>> }
endpoint updatePost: PATCH "/api/posts/{id}" {
    body Post omit { id } partial
    response Post
}
"#,
    );
    assert!(
        files
            .models
            .contains("tags: list[str | None] | None = None"),
        "expected outer `| None` on a partial List<Option<String>> field, got:\n{}",
        files.models
    );
}

/// Multiple path params.
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
    insta::assert_snapshot!("py_multi_path_client", files.client);
    insta::assert_snapshot!("py_multi_path_handler", files.handlers);
}

/// Float constraints in Pydantic Field.
#[test]
fn float_constraints() {
    let files = generate_from_source(
        r#"
struct Measurement { value: Float where self >= 0.0 && self <= 100.5 }
"#,
    );
    insta::assert_snapshot!("py_float_constraints", files.models);
}

/// Bool and String query param defaults.
#[test]
fn bool_string_defaults() {
    let files = generate_from_source(
        r#"
struct Item { id: Int  name: String }
endpoint listItems: GET "/api/items" {
    query {
        sortBy: String = "name"
        ascending: Bool = true
        limit: Int = 50
    }
    response List<Item>
}
"#,
    );
    insta::assert_snapshot!("py_bool_string_defaults_client", files.client);
    insta::assert_snapshot!("py_bool_string_defaults_server", files.server);
}

/// Map<K,V> type in struct.
#[test]
fn map_type() {
    let files = generate_from_source(
        r#"
struct Config { settings: Map<String, String>  version: Int }
"#,
    );
    insta::assert_snapshot!("py_map_type", files.models);
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
    insta::assert_snapshot!("py_enum_response_client", files.client);
}

/// PUT method.
#[test]
fn put_method() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint replaceUser: PUT "/api/users/{id}" {
    body User
    response User
}
"#,
    );
    insta::assert_snapshot!("py_put_server", files.server);
}

/// default_value_to_python covers all variants.
#[test]
fn default_value_conversions() {
    assert_eq!(default_value_to_python(&DefaultValue::Int(42)), "42");
    assert_eq!(default_value_to_python(&DefaultValue::Float(1.5)), "1.5");
    assert_eq!(
        default_value_to_python(&DefaultValue::String("hello".into())),
        "\"hello\""
    );
    assert_eq!(default_value_to_python(&DefaultValue::Bool(true)), "True");
    assert_eq!(default_value_to_python(&DefaultValue::Bool(false)), "False");
}

/// `dyn Trait` erases to the trait name — callers are expected to have
/// a matching Protocol / ABC defined in hand-written Python.
#[test]
fn dyn_type_erases_to_trait_name() {
    assert_eq!(
        type_to_python(&Type::Dyn("Drawable".to_string())),
        "Drawable"
    );
}

/// `dyn Trait` gets recorded as an import so the generator wires up the
/// user-defined trait symbol the same way it does named structs/enums.
#[test]
fn dyn_type_records_import() {
    let mut imports = BTreeSet::new();
    collect_python_imports(&Type::Dyn("Drawable".to_string()), &mut imports);
    assert!(imports.contains("Drawable"));
}

// ── pagination tests ────────────────────────────────────────────

/// An offset-paginated endpoint produces an `<Endpoint>Page` pydantic model
/// `{ items: list[T]; total_count: int }` (snake_case attrs ARE the wire
/// names — no `Field(alias=...)`, matching every other Python model). The
/// client returns and `model_validate`s the page; the handler returns the
/// page; the server route is annotated with and returns the page.
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
    insta::assert_snapshot!("py_offset_pagination_models", files.models);
    insta::assert_snapshot!("py_offset_pagination_client", files.client);
    insta::assert_snapshot!("py_offset_pagination_handlers", files.handlers);
    insta::assert_snapshot!("py_offset_pagination_server", files.server);

    // Model shape: snake_case wire names, no alias machinery.
    assert!(
        files.models.contains(
            "class ListPostsPage(BaseModel):\n    items: list[Post]\n    total_count: int\n"
        ),
        "offset page model must be {{ items: list[Post]; total_count: int }}:\n{}",
        files.models
    );
    assert!(
        !files.models.contains("alias=") && !files.models.contains("populate_by_name"),
        "page model must NOT use Field(alias=...) / populate_by_name:\n{}",
        files.models
    );
    // Client return type + model_validate of the page.
    assert!(
        files.client.contains("-> ListPostsPage:"),
        "client method must return ListPostsPage:\n{}",
        files.client
    );
    assert!(
        files
            .client
            .contains("return ListPostsPage.model_validate(response.json())"),
        "client must parse the JSON body into the page:\n{}",
        files.client
    );
    // Client imports only the page type, never the bare item type.
    assert!(
        files.client.contains("from .models import ListPostsPage")
            && !files.client.contains("import Post"),
        "client must import only the page type:\n{}",
        files.client
    );
    // Handler return type.
    assert!(
        files.handlers.contains("-> ListPostsPage: ..."),
        "handler must return ListPostsPage:\n{}",
        files.handlers
    );
    // Server route annotated with and returns the page.
    assert!(
        files.server.contains("-> ListPostsPage:")
            && files.server.contains("return await handlers.list_posts"),
        "server route must return the page:\n{}",
        files.server
    );
}

/// A cursor-paginated endpoint produces `{ items: list[T]; next_cursor: str
/// | None = None }` (the cursor is optional — absent on the last page).
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
    insta::assert_snapshot!("py_cursor_pagination_models", files.models);
    insta::assert_snapshot!("py_cursor_pagination_client", files.client);
    insta::assert_snapshot!("py_cursor_pagination_handlers", files.handlers);
    insta::assert_snapshot!("py_cursor_pagination_server", files.server);

    // Model shape: optional next_cursor with a None default.
    assert!(
            files.models.contains(
                "class ListPostsPage(BaseModel):\n    items: list[Post]\n    next_cursor: str | None = None\n"
            ),
            "cursor page model must be {{ items: list[Post]; next_cursor: str | None = None }}:\n{}",
            files.models
        );
    assert!(
        files.client.contains("-> ListPostsPage:"),
        "client method must return ListPostsPage:\n{}",
        files.client
    );
    assert!(
        files.handlers.contains("-> ListPostsPage: ..."),
        "handler must return ListPostsPage:\n{}",
        files.handlers
    );
}

/// A plain (non-paginated) `List<T>` response is unchanged: no `Page`
/// envelope, and both client and handler keep the bare `list[T]`.
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
        !files.models.contains("Page"),
        "no Page envelope for a non-paginated list:\n{}",
        files.models
    );
    assert!(
        files.client.contains("-> list[Post]:"),
        "client must return the bare list[Post]:\n{}",
        files.client
    );
    assert!(
        files.handlers.contains("-> list[Post]: ..."),
        "handler must return the bare list[Post]:\n{}",
        files.handlers
    );
}

/// A multi-status endpoint declaring two TYPED statuses sharing one body
/// (`response { 200: User  201: User }`) generates the `<Endpoint>Response`
/// envelope `{ status: int; body: User | None = None }`. Handler and client
/// return the envelope; the client records `response.status_code` and parses
/// the body when present; the server returns a dynamic-status `Response`.
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
    assert!(
        files
            .models
            .contains("class UpsertUserResponse(BaseModel):")
            && files.models.contains("    status: int\n")
            && files.models.contains("    body: User | None = None\n"),
        "envelope must be {{ status: int; body: User | None = None }}:\n{}",
        files.models
    );
    assert!(
        files.handlers.contains("-> UpsertUserResponse: ..."),
        "handler must return UpsertUserResponse:\n{}",
        files.handlers
    );
    assert!(
        files.client.contains("-> UpsertUserResponse:")
            && files.client.contains(
                "return UpsertUserResponse(status=response.status_code, body=response_body)"
            )
            && files
                .client
                .contains("User.model_validate(response.json())"),
        "client must build the envelope from response.status_code + parsed body:\n{}",
        files.client
    );
    // The client references BOTH the envelope and the shared body type, so
    // both must be imported — `User.model_validate` without the `User`
    // import is a NameError at runtime.
    assert!(
        files
            .client
            .contains("from .models import UpsertUserResponse, User"),
        "client must import the body type it validates against:\n{}",
        files.client
    );
    assert!(
        files
            .server
            .contains("result = await handlers.upsert_user(id=id)")
            && files
                .server
                .contains("content=result.body.model_dump_json(),")
            && files.server.contains("status_code=result.status,")
            && !files.server.contains("status_code=204"),
        "server must return a dynamic-status Response from result.status/body:\n{}",
        files.server
    );
    // The handler-chosen status is validated against the declared set; an
    // undeclared status (0, a smuggled 4xx, ...) is a handler bug → 500.
    assert!(
        files.server.contains("if result.status not in (200, 201):")
            && files.server.contains("handler returned undeclared status"),
        "server must reject a handler status outside the declared set:\n{}",
        files.server
    );
    // Body-shape guard: every declared status is typed, so a None body is a
    // handler bug; there is no typeless arm at all.
    assert!(
        files
            .server
            .contains("if result.status in (200, 201) and result.body is None:")
            && files
                .server
                .contains("handler returned no body for a typed status")
            && !files.server.contains("bodyless status"),
        "server must reject a typed status without a body (no typeless arm):\n{}",
        files.server
    );
    insta::assert_snapshot!("py_multi_status_shared_body_models", files.models);
    insta::assert_snapshot!("py_multi_status_shared_body_client", files.client);
    insta::assert_snapshot!("py_multi_status_shared_body_handlers", files.handlers);
    insta::assert_snapshot!("py_multi_status_shared_body_server", files.server);
}

/// A multi-status endpoint mixing a TYPED status with a TYPELESS one
/// (`response { 200: User  204 }`): the envelope still carries `body: User |
/// None = None` (at least one typed status). The client guards the body parse
/// (a 204 carries none); the server returns the body only when present.
#[test]
fn multi_status_typed_and_typeless() {
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
            .models
            .contains("class UpdateUserResponse(BaseModel):")
            && files.models.contains("    body: User | None = None\n"),
        "mixed block still carries body: User | None:\n{}",
        files.models
    );
    assert!(
        files.handlers.contains("-> UpdateUserResponse: ..."),
        "handler must return UpdateUserResponse:\n{}",
        files.handlers
    );
    // The empty-body guard is on CONTENT only — no status-code special case
    // (decision 5: a typeless 202 sends an empty body just like a 204).
    assert!(
        files.client.contains("if response.content")
            && !files.client.contains("status_code != 204"),
        "client must guard the body parse on content, not status code:\n{}",
        files.client
    );
    assert!(
        files
            .client
            .contains("from .models import UpdateUserResponse, User"),
        "client must import the body type it validates against:\n{}",
        files.client
    );
    assert!(
        files.server.contains("if result.body is not None:")
            && files
                .server
                .contains("return Response(status_code=result.status)"),
        "server must return body only when present and use result.status:\n{}",
        files.server
    );
    // Body-shape guard, both directions: the typed 200 requires a body, the
    // typeless 204 forbids one — Starlette only suppresses bodies on 204/304,
    // so without the guard a body paired with a typeless 202-style status
    // would hit the wire.
    assert!(
        files
            .server
            .contains("if result.status in (200,) and result.body is None:")
            && files
                .server
                .contains("if result.status in (204,) and result.body is not None:")
            && files
                .server
                .contains("handler returned no body for a typed status")
            && files
                .server
                .contains("handler returned a body for a bodyless status"),
        "server must enforce body presence per declared status shape:\n{}",
        files.server
    );
    insta::assert_snapshot!("py_multi_status_mixed_models", files.models);
    insta::assert_snapshot!("py_multi_status_mixed_client", files.client);
    insta::assert_snapshot!("py_multi_status_mixed_handlers", files.handlers);
    insta::assert_snapshot!("py_multi_status_mixed_server", files.server);
}

/// An ALL-TYPELESS multi-status block (`response { 202  204 }`): there is no
/// shared body type `T`, so the envelope is just `{ status: int }` with no
/// `body` field. The client never parses a body; the server returns an
/// empty-body `Response` with the chosen status.
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
    assert!(
        files
            .models
            .contains("class EnqueueJobResponse(BaseModel):")
            && files.models.contains("    status: int\n")
            && !files.models.contains("    body:"),
        "all-typeless envelope must be just {{ status: int }} (no body):\n{}",
        files.models
    );
    assert!(
        files.handlers.contains("-> EnqueueJobResponse: ..."),
        "handler must return EnqueueJobResponse:\n{}",
        files.handlers
    );
    assert!(
        files
            .client
            .contains("return EnqueueJobResponse(status=response.status_code)")
            && !files.client.contains("model_validate"),
        "client must record status only (no body parse):\n{}",
        files.client
    );
    assert!(
        files
            .server
            .contains("return Response(status_code=result.status)")
            && !files.server.contains("result.body"),
        "server must return an empty-body Response with result.status:\n{}",
        files.server
    );
    assert!(
        files.server.contains("if result.status not in (202, 204):"),
        "server must validate the handler status against the declared set:\n{}",
        files.server
    );
    insta::assert_snapshot!("py_multi_status_all_typeless_models", files.models);
    insta::assert_snapshot!("py_multi_status_all_typeless_client", files.client);
    insta::assert_snapshot!("py_multi_status_all_typeless_handlers", files.handlers);
    insta::assert_snapshot!("py_multi_status_all_typeless_server", files.server);
}

/// Multi-status + `error { }` on one endpoint: the route's except block maps
/// each declared variant to its HTTPException while the envelope guards live
/// inside the try — one route carries both. The roundtrip suite covers this
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
        files.server.contains("if str(e) == \"ValidationError\":")
            && files.server.contains(
                "raise HTTPException(status_code=400, detail=\"ValidationError\") from e"
            )
            && files
                .server
                .contains("raise HTTPException(status_code=401, detail=\"Unauthorized\") from e"),
        "multi-status server must map declared error variants:\n{}",
        files.server
    );
    // ...and the envelope machinery is intact alongside it.
    assert!(
        files.server.contains("if result.status not in (200, 201):")
            && files.server.contains("status_code=result.status,"),
        "envelope status validation and write must coexist with the error mapping:\n{}",
        files.server
    );
    insta::assert_snapshot!("py_multi_status_errors_server", files.server);
}

/// A SINGLE-entry block (`response { 201: User }`): the membership guards
/// render singleton tuples, which must keep the trailing comma — `(201)` is
/// just a parenthesized int and `in (201)` is a TypeError at runtime. Pins
/// `py_status_tuple`'s singleton form on the membership guard (the mixed test
/// only exercises it on the body-shape guards).
#[test]
fn multi_status_single_entry() {
    let files = generate_from_source(
        r#"
struct User { id: Int  name: String }
endpoint registerUser: POST "/api/users" {
    response { 201: User }
}
"#,
    );
    assert!(
        files.server.contains("if result.status not in (201,):")
            && files
                .server
                .contains("if result.status in (201,) and result.body is None:"),
        "singleton status tuples must keep the trailing comma:\n{}",
        files.server
    );
    // Still a multi-status endpoint: envelope in, bare body out.
    assert!(
        files.handlers.contains("-> RegisterUserResponse: ..."),
        "single-entry block still returns the envelope:\n{}",
        files.handlers
    );
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
    // Client: header/query inputs coexist with the envelope return.
    assert!(
        files.client.contains("-> RestartJobResponse:")
            && files.client.contains("priority")
            && files.client.contains("authorization"),
        "client must carry header/query inputs and return the envelope:\n{}",
        files.client
    );
    // Server: input parsing and the envelope guards coexist.
    assert!(
        files
            .server
            .contains("if result.status not in (200, 202, 204):"),
        "server must keep the envelope guards alongside input parsing:\n{}",
        files.server
    );
    insta::assert_snapshot!("py_multi_status_inputs_client", files.client);
    insta::assert_snapshot!("py_multi_status_inputs_server", files.server);
}

/// A typeless 205 (Reset Content): like 204 it must carry no body, but unlike
/// 204 the HTTP stacks don't suppress one — the server's body-shape guard is
/// the only protection. Pin that 205 lands in the typeless guard tuple and
/// the bodyless Response path stays live.
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
            .contains("if result.status in (205,) and result.body is not None:"),
        "205 must be guarded as a bodyless status:\n{}",
        files.server
    );
    assert!(
        files
            .server
            .contains("return Response(status_code=result.status)"),
        "the bodyless Response path must use the handler-chosen status:\n{}",
        files.server
    );
}

/// A multi-status block COMBINED with a request `body` parameter: the client
/// method has a parameter literally named `body`, so the response-decode local
/// must be `response_body` — a bare `body` local would shadow the parameter
/// with a different type and trip mypy. The TS generator hit the same
/// collision as a hard compile error (TS2300) and renamed its local; pin the
/// Python rename the same way. (The roundtrip's upsertPost2 covers this end
/// to end; this pins it at the generator level.)
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
        files.client.contains("body: UpsertUserBody")
            && files.client.contains("json=body.model_dump()"),
        "request body must serialize from the `body` parameter:\n{}",
        files.client
    );
    // ...while the response decode uses the non-colliding `response_body`
    // local, annotated `User | None` for the conditional assignment.
    assert!(
        files.client.contains("response_body: User | None = None")
            && files.client.contains(
                "return UpsertUserResponse(status=response.status_code, body=response_body)"
            ),
        "response decode must use the `response_body` local, not `body`:\n{}",
        files.client
    );
    insta::assert_snapshot!("py_multi_status_request_body_client", files.client);
}
