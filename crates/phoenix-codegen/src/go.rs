//! Go code generation for Phoenix Gen.
//!
//! Generates four files from a Phoenix program:
//! - **types.go** — Go structs with JSON tags, string-typed enums with constants,
//!   and derived body types.
//! - **client.go** — an HTTP client with typed methods for each endpoint.
//! - **handlers.go** — a `Handlers` interface that server implementations must
//!   satisfy.
//! - **server.go** — a `net/http`-compatible router that wires HTTP routes to
//!   handler methods with parameter parsing and error mapping.

use std::collections::BTreeSet;

use phoenix_parser::ast::{Declaration, EnumDecl, Expr, PaginationMode, Program, StructDecl};
use phoenix_sema::Analysis;
use phoenix_sema::checker::{
    DefaultValue, DerivedField, EndpointInfo, HeaderParamInfo, QueryParamInfo, ResolvedDerivedType,
};
use phoenix_sema::types::Type;

/// The error variant a failed body `Validate()` maps to. Used both as the lookup
/// key into the endpoint's declared errors and as the fallback name/status when
/// the endpoint declares no such variant — kept as a single source so the three
/// uses can never drift.
const VALIDATION_ERROR_VARIANT: &str = "ValidationError";
/// Status used for [`VALIDATION_ERROR_VARIANT`] when the endpoint declares no
/// matching error variant (a client-input error → HTTP 400).
const VALIDATION_ERROR_FALLBACK_STATUS: i64 = 400;
/// Max bytes a multipart request keeps in memory before spilling parts to temp
/// files (passed to `r.ParseMultipartForm`). 32 MiB matches Go's own default
/// (`http.defaultMaxMemory`); the literal is emitted inline as `32 << 20`.
const MULTIPART_MAX_MEMORY: &str = "32 << 20";

/// The output of Go code generation: four file contents.
pub struct GoFiles {
    /// Content for `types.go` — structs, enums, derived body types.
    pub types: String,
    /// Content for `client.go` — HTTP client SDK.
    pub client: String,
    /// Content for `handlers.go` — handler interface.
    pub handlers: String,
    /// Content for `server.go` — net/http router wiring.
    pub server: String,
}

/// Generates Go code from a parsed and type-checked Phoenix program.
pub fn generate_go(program: &Program, check_result: &Analysis) -> GoFiles {
    let generator = GoGenerator::new(check_result);
    generator.generate(program)
}

/// Internal Go code generator.
struct GoGenerator<'a> {
    check_result: &'a Analysis,
    types_out: String,
    client_out: String,
    handlers_out: String,
    server_out: String,
    emitted_derived_types: BTreeSet<String>,
    /// Whether types.go needs a `"fmt"` import (for Validate methods).
    types_needs_fmt: bool,
    /// Whether types.go needs a `"strings"` import (for contains constraints).
    types_needs_strings: bool,
    /// Whether client.go needs a `"fmt"` import. Every client method formats its
    /// URL with `fmt.Sprintf` and wraps HTTP errors with `fmt.Errorf`, so this is
    /// true iff at least one endpoint exists — a schema with types but no
    /// endpoints would otherwise emit an unused `fmt` import.
    client_needs_fmt: bool,
    /// Whether client.go needs a `"net/url"` import (any endpoint has query params).
    client_needs_url: bool,
    /// Whether client.go needs a `"strconv"` import (any non-String query param).
    client_needs_strconv: bool,
    /// Whether client.go needs a `"bytes"` import (any endpoint has a request body).
    client_needs_bytes: bool,
    /// Whether client.go needs an `"encoding/json"` import (any endpoint has a
    /// request body to marshal or a response to decode).
    client_needs_json: bool,
    /// Whether server.go needs an `"encoding/json"` import (any endpoint has a
    /// body to decode or a response to encode).
    server_needs_json: bool,
    /// Whether server.go needs a `"strconv"` import (any numeric/bool query param).
    server_needs_strconv: bool,
    /// Whether server.go needs a `"strings"` import (any endpoint maps errors).
    server_needs_strings: bool,
    /// Whether types.go needs a `"mime/multipart"` import (any struct or derived
    /// body type carries a `File` field → `*multipart.FileHeader`).
    types_needs_multipart: bool,
    /// Whether types.go needs an `"io"` import (the `FileUpload` client helper
    /// struct carries an `io.Reader`).
    types_needs_io: bool,
    /// Whether client.go needs a `"mime/multipart"` import (a multipart request
    /// body builds a `multipart.Writer`).
    client_needs_multipart: bool,
    /// Whether client.go needs an `"io"` import (a binary response returns
    /// `io.ReadCloser`).
    client_needs_io: bool,
    /// Whether server.go needs an `"io"` import (a binary response streams via
    /// `io.Copy`).
    server_needs_io: bool,
    /// Whether the `FileUpload` client helper struct has been emitted to
    /// types.go (emitted once, lazily, by the first multipart endpoint).
    file_upload_emitted: bool,
    /// Whether handlers.go needs an `"io"` import (a binary-download handler
    /// returns `io.Reader`).
    handlers_needs_io: bool,
}

impl<'a> GoGenerator<'a> {
    /// Creates a new Go generator with the given semantic analysis results.
    fn new(check_result: &'a Analysis) -> Self {
        Self {
            check_result,
            types_out: String::new(),
            client_out: String::new(),
            handlers_out: String::new(),
            server_out: String::new(),
            emitted_derived_types: BTreeSet::new(),
            types_needs_fmt: false,
            types_needs_strings: false,
            client_needs_fmt: false,
            client_needs_url: false,
            client_needs_strconv: false,
            client_needs_bytes: false,
            client_needs_json: false,
            server_needs_json: false,
            server_needs_strconv: false,
            server_needs_strings: false,
            types_needs_multipart: false,
            types_needs_io: false,
            client_needs_multipart: false,
            client_needs_io: false,
            server_needs_io: false,
            file_upload_emitted: false,
            handlers_needs_io: false,
        }
    }

    /// Generates `types.go`, `client.go`, `handlers.go`, and `server.go`
    /// from the program AST.
    fn generate(mut self, program: &Program) -> GoFiles {
        // ── types.go (body — header/imports composed at end) ────
        for decl in &program.declarations {
            match decl {
                Declaration::Struct(s) => {
                    // A file-bearing (body-only) struct never appears as a
                    // normal Go value: as a multipart request body it is emitted
                    // as the derived `<Endpoint>Body`/`<Endpoint>ClientBody`
                    // pair, and as a binary response it is streamed via
                    // `io.Reader`. Emitting the bare struct (with a meaningless
                    // `*multipart.FileHeader json:"..."` field) and its
                    // `Validate()` would be dead, misleading code, so skip it.
                    let is_file_bearing = self
                        .check_result
                        .module
                        .struct_info_by_name(&s.name)
                        .is_some_and(|si| si.is_file_bearing);
                    if !is_file_bearing {
                        self.emit_struct(s);
                        self.emit_validate_method(s);
                    }
                }
                Declaration::Enum(e) => self.emit_enum(e),
                _ => {}
            }
        }

        for ep in &self.check_result.endpoints {
            self.emit_derived_type(ep);
            self.emit_response_envelope(ep);
            self.emit_pagination_envelope(ep);
        }

        // Compose types.go with header and conditional imports
        let types_body = std::mem::take(&mut self.types_out);
        self.types_out
            .push_str("// Generated by Phoenix Gen — do not edit manually.\n\n");
        self.types_out.push_str("package api\n\n");
        {
            let mut imports: Vec<&str> = Vec::new();
            if self.types_needs_fmt {
                imports.push("\"fmt\"");
            }
            if self.types_needs_io {
                imports.push("\"io\"");
            }
            if self.types_needs_multipart {
                imports.push("\"mime/multipart\"");
            }
            if self.types_needs_strings {
                imports.push("\"strings\"");
            }
            imports.sort_unstable();
            if !imports.is_empty() {
                self.types_out.push_str(&format!(
                    "import (\n{}\n)\n\n",
                    imports
                        .iter()
                        .map(|i| format!("\t{i}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
        }
        // Each emitted type ends with `}\n\n`, leaving a trailing blank line at
        // EOF. gofmt requires exactly one trailing newline, so trim and re-add.
        self.types_out.push_str(types_body.trim_end());
        self.types_out.push('\n');

        // ── client.go ───────────────────────────────────────────────
        // client.go body (struct, constructor, methods) is buffered first so the
        // import block can be composed conditionally afterward.
        self.client_out
            .push_str("// ApiClient is a typed HTTP client for the API.\n");
        self.client_out
            .push_str("type ApiClient struct {\n\tBaseURL string\n\tClient  *http.Client\n}\n\n");
        self.client_out
            .push_str("// NewApiClient creates a new API client with the given base URL.\n");
        self.client_out.push_str(
            "func NewApiClient(baseURL string) *ApiClient {\n\treturn &ApiClient{BaseURL: baseURL, Client: http.DefaultClient}\n}\n",
        );

        for ep in &self.check_result.endpoints {
            self.emit_client_method(ep);
        }

        // Compose client.go with header and conditional imports. `net/http` is
        // always used (the client struct holds a `*http.Client`); the rest are
        // emitted only when referenced so the output has no unused imports:
        // `fmt` for URL formatting + error wrapping (any endpoint), `bytes` for a
        // request-body reader, `encoding/json` for body/response (de)serialization,
        // `net/url` for query params, and `strconv` for non-String query-param
        // formatting.
        let client_body = std::mem::take(&mut self.client_out);
        self.client_out
            .push_str("// Generated by Phoenix Gen — do not edit manually.\n\n");
        self.client_out.push_str("package api\n\n");
        {
            let mut imports = vec!["\"net/http\""];
            if self.client_needs_fmt {
                imports.push("\"fmt\"");
            }
            if self.client_needs_bytes {
                imports.push("\"bytes\"");
            }
            if self.client_needs_io {
                imports.push("\"io\"");
            }
            if self.client_needs_json {
                imports.push("\"encoding/json\"");
            }
            if self.client_needs_multipart {
                imports.push("\"mime/multipart\"");
            }
            if self.client_needs_url {
                imports.push("\"net/url\"");
            }
            if self.client_needs_strconv {
                imports.push("\"strconv\"");
            }
            imports.sort_unstable();
            self.client_out.push_str(&format!(
                "import (\n{}\n)\n\n",
                imports
                    .iter()
                    .map(|i| format!("\t{i}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        self.client_out.push_str(&client_body);

        // ── handlers.go (body buffered first so imports compose at end) ──
        self.handlers_out.push_str(
            "// Handlers defines the interface that server implementations must satisfy.\n",
        );
        self.handlers_out.push_str("type Handlers interface {\n");

        for ep in &self.check_result.endpoints {
            self.emit_handler_method(ep);
        }

        self.handlers_out.push_str("}\n");

        // Compose handlers.go with header and a conditional `io` import (only a
        // binary-download handler return — `io.Reader` — needs it).
        let handlers_body = std::mem::take(&mut self.handlers_out);
        self.handlers_out
            .push_str("// Generated by Phoenix Gen — implement the handler methods below.\n\n");
        self.handlers_out.push_str("package api\n\n");
        if self.handlers_needs_io {
            self.handlers_out.push_str("import (\n\t\"io\"\n)\n\n");
        }
        self.handlers_out.push_str(&handlers_body);

        // ── server.go (body — header/imports composed at end) ───────
        self.server_out.push_str(
            "// NewRouter creates an http.ServeMux that routes requests to the given handlers.\n",
        );
        self.server_out
            .push_str("func NewRouter(h Handlers) *http.ServeMux {\n");
        self.server_out.push_str("\tmux := http.NewServeMux()\n");

        for ep in &self.check_result.endpoints {
            self.emit_server_route(ep);
        }

        self.server_out.push_str("\treturn mux\n}\n");

        // Compose server.go with header and conditional imports. `net/http` is
        // always used (the router and `http.Error`); the rest are emitted only
        // when actually referenced so the output is free of unused imports:
        // `encoding/json` for body/response (de)serialization, `strconv` for
        // numeric/bool query-param parsing, `strings` for error mapping.
        let server_body = std::mem::take(&mut self.server_out);
        self.server_out
            .push_str("// Generated by Phoenix Gen — do not edit manually.\n\n");
        self.server_out.push_str("package api\n\n");
        {
            let mut imports = vec!["\"net/http\""];
            if self.server_needs_io {
                imports.push("\"io\"");
            }
            if self.server_needs_json {
                imports.push("\"encoding/json\"");
            }
            if self.server_needs_strconv {
                imports.push("\"strconv\"");
            }
            if self.server_needs_strings {
                imports.push("\"strings\"");
            }
            imports.sort_unstable();
            self.server_out.push_str(&format!(
                "import (\n{}\n)\n\n",
                imports
                    .iter()
                    .map(|i| format!("\t{i}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        self.server_out.push_str(&server_body);

        GoFiles {
            types: self.types_out,
            client: self.client_out,
            handlers: self.handlers_out,
            server: self.server_out,
        }
    }

    // ── types.go emission ───────────────────────────────────────────

    /// Emits a Go struct with JSON tags for a Phoenix struct.
    fn emit_struct(&mut self, s: &StructDecl) {
        if let Some(ref doc) = s.doc_comment {
            self.types_out.push_str(&render_line_comment(
                "// ",
                &format!("{} is {}.", s.name, doc.to_lowercase()),
            ));
        }
        let rows: Vec<(String, String, String)> = self
            .check_result
            .module
            .struct_info_by_name(&s.name)
            .map(|info| {
                info.fields
                    .iter()
                    .map(|f| {
                        if type_contains_file(&f.ty) {
                            self.types_needs_multipart = true;
                        }
                        (
                            to_pascal_case(&f.name),
                            type_to_go(&f.ty),
                            format!("`json:\"{}\"`", f.name),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.types_out.push_str(&render_struct(&s.name, &rows));
    }

    /// Emits Go string constants for a Phoenix enum.
    fn emit_enum(&mut self, e: &EnumDecl) {
        let all_unit = e.variants.iter().all(|v| v.fields.is_empty());
        if !all_unit {
            return;
        }

        if let Some(ref doc) = e.doc_comment {
            self.types_out.push_str(&render_line_comment(
                "// ",
                &format!("{} is {}.", e.name, doc.to_lowercase()),
            ));
        }
        self.types_out
            .push_str(&format!("type {} string\n\n", e.name));
        self.types_out.push_str("const (\n");
        // gofmt aligns the constant-name column within the block; the right-hand
        // side (`Type = "value"`) is identical-width-agnostic, so only the name
        // column needs padding to the block maximum.
        let names: Vec<String> = e
            .variants
            .iter()
            .map(|v| format!("{}{}", e.name, v.name))
            .collect();
        let max_name = names.iter().map(|n| n.len()).max().unwrap_or(0);
        for (name, v) in names.iter().zip(&e.variants) {
            self.types_out.push_str(&format!(
                "\t{:<width$} {} = \"{}\"\n",
                name,
                e.name,
                v.name,
                width = max_name
            ));
        }
        self.types_out.push_str(")\n\n");
    }

    /// Emits a derived Go struct for an endpoint body type.
    fn emit_derived_type(&mut self, ep: &EndpointInfo) {
        let Some(ref body) = ep.body else { return };
        let type_name = format!("{}Body", capitalize(&ep.name));

        if !self.emitted_derived_types.insert(type_name.clone()) {
            return;
        }

        let rows: Vec<(String, String, String)> = body
            .fields
            .iter()
            .map(|f| {
                if type_contains_file(&f.ty) {
                    self.types_needs_multipart = true;
                }
                let (go_type, _) = derived_field_go_type(f);
                let omitempty = if f.optional { ",omitempty" } else { "" };
                (
                    to_pascal_case(&f.name),
                    go_type,
                    format!("`json:\"{}{}\"`", f.name, omitempty),
                )
            })
            .collect();
        self.types_out.push_str(&render_struct(&type_name, &rows));

        // A multipart body needs a client-side request struct: the server/handler
        // body carries `*multipart.FileHeader` (a parse type the client cannot
        // construct), so the client takes a parallel `<Endpoint>ClientBody` whose
        // File fields are `FileUpload { Filename string; Content io.Reader }`.
        if ep.body_is_multipart {
            self.emit_file_upload_helper();
            self.emit_client_body_struct(ep, &type_name);
        }

        self.emit_body_validate_method(ep, &type_name);
    }

    /// Emits the shared `FileUpload` helper struct (once) to types.go: the
    /// client-side representation of a file to upload — a filename plus an
    /// `io.Reader` yielding the bytes. The client's multipart writer reads
    /// `Content` into a `CreateFormFile` part named after the field.
    fn emit_file_upload_helper(&mut self) {
        if self.file_upload_emitted {
            return;
        }
        self.file_upload_emitted = true;
        self.types_needs_io = true;
        self.types_out.push_str(
            "// FileUpload is the client-side representation of a file part in a\n\
             // multipart request body: a filename and a reader yielding its bytes.\n",
        );
        self.types_out.push_str(&render_struct(
            "FileUpload",
            &[
                (
                    "Filename".to_string(),
                    "string".to_string(),
                    "`json:\"filename\"`".to_string(),
                ),
                (
                    "Content".to_string(),
                    "io.Reader".to_string(),
                    "`json:\"-\"`".to_string(),
                ),
            ],
        ));
    }

    /// Emits the client-side multipart request struct `<Endpoint>ClientBody`,
    /// parallel to the server `<Endpoint>Body` but with each `File` /
    /// `Option<File>` field typed as `FileUpload` (constructible client-side)
    /// rather than `*multipart.FileHeader` (a server parse type).
    fn emit_client_body_struct(&mut self, ep: &EndpointInfo, server_type: &str) {
        let Some(ref body) = ep.body else { return };
        let client_type = format!("{}ClientBody", capitalize(&ep.name));
        if !self.emitted_derived_types.insert(client_type.clone()) {
            return;
        }
        let rows: Vec<(String, String, String)> = body
            .fields
            .iter()
            .map(|f| {
                let go_type = if type_contains_file(&f.ty) {
                    self.types_needs_io = true;
                    // An optional file part (`Option<File>`) is a nil-able
                    // `*FileUpload` the caller may omit; a required one is a value.
                    if f.optional || matches!(&f.ty, Type::Generic(n, _) if n == "Option") {
                        "*FileUpload".to_string()
                    } else {
                        "FileUpload".to_string()
                    }
                } else {
                    derived_field_go_type(f).0
                };
                let omitempty = if f.optional { ",omitempty" } else { "" };
                (
                    to_pascal_case(&f.name),
                    go_type,
                    format!("`json:\"{}{}\"`", f.name, omitempty),
                )
            })
            .collect();
        self.types_out.push_str(&format!(
            "// {client_type} is the client-side request body for the multipart\n\
             // upload endpoint; the server-side parsed form is {server_type}.\n"
        ));
        self.types_out.push_str(&render_struct(&client_type, &rows));
    }

    /// Emits the generated `<Endpoint>Result` envelope struct for an endpoint
    /// that declares response headers: a `Body` field of the response type plus
    /// one typed field per response header (PascalCase name, `*T` when optional).
    /// Endpoints WITHOUT response headers emit nothing — the common case returns
    /// the bare body unchanged. The handler returns this type and the client
    /// reconstructs it; the field names here are the single source the
    /// server/client wiring reads/writes.
    fn emit_response_envelope(&mut self, ep: &EndpointInfo) {
        if ep.response_headers.is_empty() {
            return;
        }
        let type_name = header_result_type_name(ep);
        if !self.emitted_derived_types.insert(type_name.clone()) {
            return;
        }
        let body_type = ep
            .response
            .as_ref()
            .map(type_to_go)
            .unwrap_or_else(|| "interface{}".to_string());
        let mut rows: Vec<(String, String, String)> =
            vec![("Body".to_string(), body_type, "`json:\"-\"`".to_string())];
        for h in &ep.response_headers {
            rows.push((
                to_pascal_case(&h.name),
                type_to_go(&h.ty),
                "`json:\"-\"`".to_string(),
            ));
        }
        self.types_out.push_str(&render_struct(&type_name, &rows));
    }

    /// Emits the generated `<Endpoint>Page` pagination envelope struct for an
    /// endpoint that declares a `pagination { }` block. The response is no longer
    /// the bare `[]T` — it becomes this envelope wrapping the items plus the
    /// per-mode metadata field:
    /// - **offset** → `{ Items []T `json:"items"`; TotalCount int64 `json:"totalCount"` }`
    /// - **cursor** → `{ Items []T `json:"items"`; NextCursor *string `json:"nextCursor"` }`
    ///   (pointer so an absent cursor serializes to `null` — nil marks the last page)
    ///
    /// This is the exact response-envelope machinery the response-header
    /// `<Endpoint>Result` uses, with `Items` + a metadata field instead of `Body` +
    /// headers; the two are mutually exclusive (sema rejects the combination), so
    /// an endpoint never emits both. Endpoints WITHOUT pagination emit nothing and
    /// keep returning the bare `[]T`.
    fn emit_pagination_envelope(&mut self, ep: &EndpointInfo) {
        let Some(ref pag) = ep.pagination else {
            return;
        };
        let type_name = page_type_name(ep);
        if !self.emitted_derived_types.insert(type_name.clone()) {
            return;
        }
        let item_type = type_to_go(&pag.item_type);
        let mut rows: Vec<(String, String, String)> = vec![(
            "Items".to_string(),
            format!("[]{item_type}"),
            "`json:\"items\"`".to_string(),
        )];
        match pag.mode {
            PaginationMode::Offset => rows.push((
                "TotalCount".to_string(),
                "int64".to_string(),
                "`json:\"totalCount\"`".to_string(),
            )),
            PaginationMode::Cursor => rows.push((
                "NextCursor".to_string(),
                "*string".to_string(),
                "`json:\"nextCursor\"`".to_string(),
            )),
        }
        self.types_out.push_str(&render_struct(&type_name, &rows));
    }

    /// Emits a `Validate() error` method on a derived body type if any of its
    /// fields carry a constraint inherited from the source struct.
    ///
    /// Constraints propagate from the source struct's `where` clauses through the
    /// `omit`/`pick`/`partial` modifier chain (the sema layer already records the
    /// surviving constraint on each [`DerivedField`]). Pointer-ness comes from
    /// [`derived_field_go_type`] — the single source of truth shared with
    /// [`GoGenerator::emit_derived_type`], so a `partial`-applied `Option<T>`
    /// collapses to one `*T` (never `**T`) and the nil-guard here can never
    /// disagree with the rendered field type. The actual emission is shared with
    /// the source-struct validator via [`render_validate_fn`].
    fn emit_body_validate_method(&mut self, ep: &EndpointInfo, type_name: &str) {
        let Some(ref body) = ep.body else { return };
        let fields: Vec<(&str, &Expr, bool)> = body
            .fields
            .iter()
            .filter_map(|f| {
                f.constraint
                    .as_ref()
                    .map(|c| (f.name.as_str(), c, derived_field_go_type(f).1))
            })
            .collect();
        if fields.is_empty() {
            return;
        }
        render_validate_fn(
            ValidateSink {
                out: &mut self.types_out,
                needs_fmt: &mut self.types_needs_fmt,
                needs_strings: &mut self.types_needs_strings,
            },
            type_name,
            &fields,
        );
    }

    // ── validation emission ────────────────────────────────────────

    /// Emits a `Validate() error` method on a Go struct if any of its fields
    /// have `where` constraints. An `Option<T>` field is rendered as `*T`, so its
    /// check is nil-guarded and dereferenced; the actual emission is shared with
    /// the derived-body validator via [`render_validate_fn`].
    fn emit_validate_method(&mut self, s: &StructDecl) {
        let Some(info) = self.check_result.module.struct_info_by_name(&s.name) else {
            return;
        };
        let fields: Vec<(&str, &Expr, bool)> = info
            .fields
            .iter()
            .filter_map(|f| {
                let is_option = matches!(&f.ty, Type::Generic(name, _) if name == "Option");
                f.constraint
                    .as_ref()
                    .map(|c| (f.name.as_str(), c, is_option))
            })
            .collect();
        if fields.is_empty() {
            return;
        }
        render_validate_fn(
            ValidateSink {
                out: &mut self.types_out,
                needs_fmt: &mut self.types_needs_fmt,
                needs_strings: &mut self.types_needs_strings,
            },
            &s.name,
            &fields,
        );
    }

    // ── client.go emission ──────────────────────────────────────────

    /// Emits a Go method on `ApiClient` for an endpoint.
    fn emit_client_method(&mut self, ep: &EndpointInfo) {
        let method_name = to_pascal_case(&ep.name);
        let http_method = ep.method.as_upper_str();
        let response_type = ep.response.as_ref().map(type_to_go);
        // Endpoints that declare response headers return a typed envelope
        // `<Endpoint>Result` (body + each header) instead of the bare body.
        let has_resp_headers = !ep.response_headers.is_empty();
        let result_type = header_result_type_name(ep);
        // A paginated endpoint returns the `<Endpoint>Page` envelope (`{items,
        // <metadata>}`) instead of the bare `[]T` — the whole JSON body IS the
        // page object, decoded like any struct response (mutually exclusive with
        // response headers).
        let is_paginated = ep.pagination.is_some();
        let page_type = page_type_name(ep);

        // Build parameter list
        let mut params = Vec::new();
        for pp in &ep.path_params {
            params.push(format!("{} string", to_camel(pp)));
        }
        if ep.body.is_some() {
            // A multipart upload takes the client-side request struct (File fields
            // are `FileUpload`, constructible client-side); JSON bodies take the
            // server body struct unchanged.
            let body_type = if ep.body_is_multipart {
                format!("{}ClientBody", capitalize(&ep.name))
            } else {
                format!("{}Body", capitalize(&ep.name))
            };
            params.push(format!("body {}", body_type));
        }
        for qp in &ep.query_params {
            let go_type = type_to_go(&qp.ty);
            params.push(format!("{} {}", to_camel(&qp.name), go_type));
        }
        // Request headers follow query params, same optional `*T` convention.
        for h in &ep.headers {
            params.push(format!("{} {}", to_camel(&h.name), type_to_go(&h.ty)));
        }
        let params_str = params.join(", ");

        // Whether the method returns a value (and thus a `(T, error)` pair rather
        // than a bare `error`): true with response headers (→ envelope), a binary
        // download (→ stream), or a bare response type. The error-return prefix
        // below must match this exactly.
        let returns_value = has_resp_headers || response_type.is_some();

        // Return type. A binary download returns the raw response stream
        // (`io.ReadCloser`) the caller drains and closes; with response headers
        // the method returns the envelope pointer; otherwise the bare response.
        let return_sig = if ep.response_is_binary {
            "(io.ReadCloser, error)".to_string()
        } else if has_resp_headers {
            format!("(*{}, error)", result_type)
        } else if is_paginated {
            format!("(*{}, error)", page_type)
        } else {
            match &response_type {
                Some(rt) => format!("(*{}, error)", rt),
                None => "error".to_string(),
            }
        };

        if let Some(ref doc) = ep.doc_comment {
            self.client_out.push('\n');
            self.client_out.push_str(&render_line_comment(
                "// ",
                &format!("{} {}.", method_name, doc.to_lowercase()),
            ));
        }
        self.client_out.push_str(&format!(
            "func (c *ApiClient) {}({}) {} {{\n",
            method_name, params_str, return_sig
        ));

        // Build URL. Every method formats its URL with `fmt.Sprintf` and (below)
        // wraps HTTP errors with `fmt.Errorf`, so emitting any method needs `fmt`.
        self.client_needs_fmt = true;
        let url_expr = build_go_url(&ep.path, &ep.path_params);
        self.client_out.push_str(&format!(
            "\tu := fmt.Sprintf(\"%s{}\", c.BaseURL{})\n",
            url_expr.0, url_expr.1
        ));

        // Query params
        if !ep.query_params.is_empty() {
            self.client_needs_url = true;
            self.client_out.push_str("\tq := url.Values{}\n");
            for qp in &ep.query_params {
                let name = to_camel(&qp.name);
                let (optional, inner) = query_param_shape(&qp.ty);
                // For an optional query param the value is a pointer; only set it
                // when non-nil, dereferencing for the formatting expression.
                let value_expr = if optional {
                    format!("*{name}")
                } else {
                    name.clone()
                };
                let set_expr = match inner {
                    Type::Int => {
                        self.client_needs_strconv = true;
                        format!(
                            "q.Set(\"{}\", strconv.FormatInt({}, 10))",
                            qp.name, value_expr
                        )
                    }
                    Type::Float => {
                        self.client_needs_strconv = true;
                        format!(
                            "q.Set(\"{}\", strconv.FormatFloat({}, 'f', -1, 64))",
                            qp.name, value_expr
                        )
                    }
                    Type::Bool => {
                        self.client_needs_strconv = true;
                        format!("q.Set(\"{}\", strconv.FormatBool({}))", qp.name, value_expr)
                    }
                    Type::String => format!("q.Set(\"{}\", {})", qp.name, value_expr),
                    _ => format!("q.Set(\"{}\", fmt.Sprint({}))", qp.name, value_expr),
                };
                if optional {
                    self.client_out
                        .push_str(&format!("\tif {name} != nil {{\n\t\t{set_expr}\n\t}}\n"));
                } else {
                    self.client_out.push_str(&format!("\t{set_expr}\n"));
                }
            }
            self.client_out.push_str("\tu += \"?\" + q.Encode()\n");
        }

        // Build request. A JSON request body pulls in `bytes` (for the reader) and
        // `encoding/json` (to marshal); a JSON response pulls in `encoding/json` to
        // decode. A binary download decodes nothing (the caller drains the stream),
        // so it never sets the json flag. Track each so the import block carries
        // only what's used.
        if response_type.is_some() && !ep.response_is_binary {
            self.client_needs_json = true;
        }
        if ep.body_is_multipart {
            self.emit_client_multipart_body(ep, http_method);
        } else if ep.body.is_some() {
            self.client_needs_bytes = true;
            self.client_needs_json = true;
            self.client_out
                .push_str("\tdata, err := json.Marshal(body)\n");
            self.client_out
                .push_str("\tif err != nil {\n\t\treturn nil, err\n\t}\n");
            self.client_out.push_str(&format!(
                "\treq, err := http.NewRequest(\"{}\", u, bytes.NewReader(data))\n",
                http_method
            ));
            self.client_out
                .push_str("\tif err != nil {\n\t\treturn nil, err\n\t}\n");
            self.client_out
                .push_str("\treq.Header.Set(\"Content-Type\", \"application/json\")\n");
        } else {
            self.client_out.push_str(&format!(
                "\treq, err := http.NewRequest(\"{}\", u, nil)\n",
                http_method
            ));
            let err_ret = if response_type.is_some() {
                "nil, err"
            } else {
                "err"
            };
            self.client_out.push_str(&format!(
                "\tif err != nil {{\n\t\treturn {}\n\t}}\n",
                err_ret
            ));
        }

        // Request headers. Non-string types are stringified the same way query
        // params are; optionals (`*T`) are guarded and dereferenced. The exact
        // wire name from sema is the single source of truth — never recomputed.
        for h in &ep.headers {
            let name = to_camel(&h.name);
            let (optional, inner) = query_param_shape(&h.ty);
            let value_expr = if optional {
                format!("*{name}")
            } else {
                name.clone()
            };
            let str_expr = header_string_expr(inner, &value_expr, &mut self.client_needs_strconv);
            let set_expr = format!("req.Header.Set(\"{}\", {})", h.wire_name, str_expr);
            if optional {
                self.client_out
                    .push_str(&format!("\tif {name} != nil {{\n\t\t{set_expr}\n\t}}\n"));
            } else {
                self.client_out.push_str(&format!("\t{set_expr}\n"));
            }
        }

        // Execute
        let err_ret = if returns_value { "nil, " } else { "" };
        self.client_out
            .push_str("\tresp, err := c.Client.Do(req)\n");
        self.client_out.push_str(&format!(
            "\tif err != nil {{\n\t\treturn {}err\n\t}}\n",
            err_ret
        ));
        // A binary download hands `resp.Body` back to the caller, who closes it;
        // closing it here (via defer) would race the caller's read. JSON paths
        // fully consume + close the body before returning.
        if ep.response_is_binary {
            // Explicitly discard the Close error (`_ =`): on the error path we
            // are already returning a more useful HTTP-status error, and the
            // bare `resp.Body.Close()` would otherwise be flagged by errcheck.
            self.client_out.push_str(&format!(
                "\tif resp.StatusCode >= 400 {{\n\t\t_ = resp.Body.Close()\n\t\treturn {}fmt.Errorf(\"HTTP %d\", resp.StatusCode)\n\t}}\n",
                err_ret
            ));
        } else {
            self.client_out.push_str("\tdefer resp.Body.Close()\n");
            self.client_out.push_str(&format!(
                "\tif resp.StatusCode >= 400 {{\n\t\treturn {}fmt.Errorf(\"HTTP %d\", resp.StatusCode)\n\t}}\n",
                err_ret
            ));
        }

        // Decode response. A binary download returns the raw stream; with response
        // headers, decode the body into the envelope's `Body` field, then read each
        // header from `resp.Header`.
        if ep.response_is_binary {
            self.client_needs_io = true;
            self.client_out.push_str("\treturn resp.Body, nil\n");
        } else if has_resp_headers {
            self.client_needs_json = true;
            self.client_out
                .push_str(&format!("\tvar result {}\n", result_type));
            self.client_out.push_str(
                "\tif err := json.NewDecoder(resp.Body).Decode(&result.Body); err != nil {\n\t\treturn nil, err\n\t}\n",
            );
            for h in &ep.response_headers {
                self.emit_client_response_header_read(h);
            }
            self.client_out.push_str("\treturn &result, nil\n");
        } else if is_paginated {
            // The whole response body is the page object (`{items, <metadata>}`),
            // decoded into the `<Endpoint>Page` envelope like any struct response.
            self.client_needs_json = true;
            self.client_out
                .push_str(&format!("\tvar result {}\n", page_type));
            self.client_out.push_str(
                "\tif err := json.NewDecoder(resp.Body).Decode(&result); err != nil {\n\t\treturn nil, err\n\t}\n",
            );
            self.client_out.push_str("\treturn &result, nil\n");
        } else if let Some(ref rt) = response_type {
            self.client_out.push_str(&format!("\tvar result {}\n", rt));
            self.client_out.push_str(&format!(
                "\tif err := json.NewDecoder(resp.Body).Decode(&result); err != nil {{\n\t\treturn {}err\n\t}}\n",
                err_ret
            ));
            self.client_out.push_str("\treturn &result, nil\n");
        } else {
            self.client_out.push_str("\treturn nil\n");
        }

        self.client_out.push_str("}\n");
    }

    /// Emits the client-side multipart request build for an upload endpoint: a
    /// `multipart.Writer` over a `bytes.Buffer`, a `CreateFormFile` part per file
    /// field (copying `FileUpload.Content`) and a `WriteField` per scalar field
    /// (stringified like query params / headers). The request `Content-Type` is
    /// the writer's `FormDataContentType()` (carries the boundary). All error
    /// paths return `(nil, err)` — a multipart endpoint always returns a value
    /// (it has a JSON response, an envelope, or a binary stream).
    fn emit_client_multipart_body(&mut self, ep: &EndpointInfo, http_method: &str) {
        let Some(ref body) = ep.body else { return };
        self.client_needs_bytes = true;
        self.client_needs_multipart = true;
        self.client_needs_io = true;

        self.client_out.push_str("\tvar buf bytes.Buffer\n");
        self.client_out
            .push_str("\twriter := multipart.NewWriter(&buf)\n");

        for f in &body.fields {
            let field = to_pascal_case(&f.name);
            let wire = &f.name;
            if type_contains_file(&f.ty) {
                let optional = f.optional || matches!(&f.ty, Type::Generic(n, _) if n == "Option");
                // Build the form-file part and copy the reader's bytes into it.
                // Each file part lives in its own block so its `part, err :=`
                // declarations never collide across multiple file fields — Go
                // rejects a second `:=` with no new variable on the left, so two
                // required files at the same scope would not compile. An optional
                // file uses an `if body.<F> != nil` block (an omitted file is
                // skipped); a required file uses a bare block for the same fresh
                // scope. `FileUpload`/`*FileUpload` both reach fields via
                // `body.<F>.` (Go auto-dereferences the pointer), so the inner
                // body is identical.
                let open = if optional {
                    format!("\tif body.{field} != nil {{\n")
                } else {
                    "\t{\n".to_string()
                };
                self.client_out.push_str(&open);
                self.client_out.push_str(&format!(
                    "\t\tpart, err := writer.CreateFormFile(\"{wire}\", body.{field}.Filename)\n\t\tif err != nil {{\n\t\t\treturn nil, err\n\t\t}}\n\t\tif _, err := io.Copy(part, body.{field}.Content); err != nil {{\n\t\t\treturn nil, err\n\t\t}}\n"
                ));
                self.client_out.push_str("\t}\n");
            } else {
                let (opt_scalar, inner) = query_param_shape(&f.ty);
                let optional = f.optional || opt_scalar;
                let value_expr = if optional {
                    format!("*body.{field}")
                } else {
                    format!("body.{field}")
                };
                let str_expr =
                    header_string_expr(inner, &value_expr, &mut self.client_needs_strconv);
                // `WriteField` only writes to the in-memory `bytes.Buffer`, but
                // check its error anyway — it keeps the multipart build uniformly
                // error-checked (matching the file path) and satisfies errcheck.
                let i = if optional { "\t\t" } else { "\t" };
                let write = format!(
                    "{i}if err := writer.WriteField(\"{wire}\", {str_expr}); err != nil {{\n{i}\treturn nil, err\n{i}}}\n"
                );
                if optional {
                    self.client_out
                        .push_str(&format!("\tif body.{field} != nil {{\n{write}\t}}\n"));
                } else {
                    self.client_out.push_str(&write);
                }
            }
        }

        self.client_out
            .push_str("\tif err := writer.Close(); err != nil {\n\t\treturn nil, err\n\t}\n");
        self.client_out.push_str(&format!(
            "\treq, err := http.NewRequest(\"{http_method}\", u, &buf)\n",
        ));
        self.client_out
            .push_str("\tif err != nil {\n\t\treturn nil, err\n\t}\n");
        self.client_out
            .push_str("\treq.Header.Set(\"Content-Type\", writer.FormDataContentType())\n");
    }

    /// Emits client-side parsing of one response header from `resp.Header` into
    /// the envelope field `result.<PascalName>`. String headers are assigned
    /// directly; numeric/bool are parsed; optional (`Option<T>`) headers parse
    /// into a `*T` left nil when the header is absent — mirroring the server-side
    /// request-header parse and the query-param parse.
    fn emit_client_response_header_read(&mut self, h: &HeaderParamInfo) {
        let field = to_pascal_case(&h.name);
        let wire = &h.wire_name;
        let (optional, inner) = query_param_shape(&h.ty);
        let body = if optional {
            match inner {
                Type::Int => {
                    self.client_needs_strconv = true;
                    format!(
                        "if v := resp.Header.Get(\"{wire}\"); v != \"\" {{\n\t\tif n, err := strconv.ParseInt(v, 10, 64); err == nil {{\n\t\t\tresult.{field} = &n\n\t\t}}\n\t}}\n"
                    )
                }
                Type::Float => {
                    self.client_needs_strconv = true;
                    format!(
                        "if v := resp.Header.Get(\"{wire}\"); v != \"\" {{\n\t\tif n, err := strconv.ParseFloat(v, 64); err == nil {{\n\t\t\tresult.{field} = &n\n\t\t}}\n\t}}\n"
                    )
                }
                Type::Bool => {
                    self.client_needs_strconv = true;
                    format!(
                        "if v := resp.Header.Get(\"{wire}\"); v != \"\" {{\n\t\tif b, err := strconv.ParseBool(v); err == nil {{\n\t\t\tresult.{field} = &b\n\t\t}}\n\t}}\n"
                    )
                }
                Type::String => {
                    format!(
                        "if v := resp.Header.Get(\"{wire}\"); v != \"\" {{\n\t\tresult.{field} = &v\n\t}}\n"
                    )
                }
                other => {
                    let go_type = type_to_go(other);
                    format!(
                        "if v := resp.Header.Get(\"{wire}\"); v != \"\" {{\n\t\tcv := {go_type}(v)\n\t\tresult.{field} = &cv\n\t}}\n"
                    )
                }
            }
        } else {
            match inner {
                Type::Int => {
                    self.client_needs_strconv = true;
                    format!(
                        "if v := resp.Header.Get(\"{wire}\"); v != \"\" {{\n\t\tresult.{field}, _ = strconv.ParseInt(v, 10, 64)\n\t}}\n"
                    )
                }
                Type::Float => {
                    self.client_needs_strconv = true;
                    format!(
                        "if v := resp.Header.Get(\"{wire}\"); v != \"\" {{\n\t\tresult.{field}, _ = strconv.ParseFloat(v, 64)\n\t}}\n"
                    )
                }
                Type::Bool => {
                    self.client_needs_strconv = true;
                    format!(
                        "if v := resp.Header.Get(\"{wire}\"); v != \"\" {{\n\t\tresult.{field}, _ = strconv.ParseBool(v)\n\t}}\n"
                    )
                }
                Type::String => {
                    format!("result.{field} = resp.Header.Get(\"{wire}\")\n")
                }
                other => {
                    let go_type = type_to_go(other);
                    format!("result.{field} = {go_type}(resp.Header.Get(\"{wire}\"))\n")
                }
            }
        };
        self.client_out.push('\t');
        self.client_out.push_str(&body);
    }

    // ── handlers.go emission ────────────────────────────────────────

    /// Emits a Go interface method signature for an endpoint.
    fn emit_handler_method(&mut self, ep: &EndpointInfo) {
        let method_name = to_pascal_case(&ep.name);

        let mut params = Vec::new();
        for pp in &ep.path_params {
            params.push(format!("{} string", to_camel(pp)));
        }
        if ep.body.is_some() {
            let body_type = format!("{}Body", capitalize(&ep.name));
            params.push(format!("body {}", body_type));
        }
        for qp in &ep.query_params {
            params.push(format!("{} {}", to_camel(&qp.name), type_to_go(&qp.ty)));
        }
        // Request headers follow query params (same optional `*T` convention).
        for h in &ep.headers {
            params.push(format!("{} {}", to_camel(&h.name), type_to_go(&h.ty)));
        }
        let params_str = params.join(", ");

        // A binary download handler returns the file as an `io.Reader` the server
        // streams to the wire; with response headers the handler returns the typed
        // envelope; otherwise the bare response type (unchanged for the common
        // case).
        let return_type = if ep.response_is_binary {
            self.handlers_needs_io = true;
            "(io.Reader, error)".to_string()
        } else if !ep.response_headers.is_empty() {
            format!("(*{}, error)", header_result_type_name(ep))
        } else if ep.pagination.is_some() {
            // A paginated endpoint's handler supplies the page envelope (items +
            // metadata) instead of the bare `[]T`.
            format!("(*{}, error)", page_type_name(ep))
        } else {
            match ep.response.as_ref().map(type_to_go) {
                Some(rt) => format!("(*{}, error)", rt),
                None => "error".to_string(),
            }
        };

        if let Some(ref doc) = ep.doc_comment {
            self.handlers_out.push_str(&render_line_comment(
                "\t// ",
                &format!("{} {}.", method_name, doc.to_lowercase()),
            ));
        }
        self.handlers_out.push_str(&format!(
            "\t{}({}) {}\n",
            method_name, params_str, return_type
        ));
    }

    // ── server.go emission ──────────────────────────────────────────

    /// Emits a `net/http` handler registration for an endpoint.
    fn emit_server_route(&mut self, ep: &EndpointInfo) {
        let method = ep.method.as_upper_str();
        // Go 1.22+ mux pattern: "METHOD /path"
        let pattern = format!("{} {}", method, ep.path);

        self.server_out.push_str(&format!(
            "\tmux.HandleFunc(\"{}\", func(w http.ResponseWriter, r *http.Request) {{\n",
            pattern
        ));

        // Parse path params
        for pp in &ep.path_params {
            self.server_out.push_str(&format!(
                "\t\t{camel} := r.PathValue(\"{name}\")\n",
                name = pp,
                camel = to_camel(pp)
            ));
        }

        // Parse body
        if let Some(ref body) = ep.body {
            let body_type = format!("{}Body", capitalize(&ep.name));
            if ep.body_is_multipart {
                self.emit_server_multipart_parse(body, &body_type);
            } else {
                self.server_needs_json = true;
                self.server_out.push_str(&format!(
                    "\t\tvar body {}\n\t\tif err := json.NewDecoder(r.Body).Decode(&body); err != nil {{\n\t\t\thttp.Error(w, err.Error(), http.StatusBadRequest)\n\t\t\treturn\n\t\t}}\n",
                    body_type
                ));
            }
            // Validate the decoded body against the constraints inherited from the
            // source struct. Only emitted when the body type actually has a
            // `Validate()` (i.e. at least one constrained field). A failure is a
            // client-input error: map it to the endpoint's declared
            // `ValidationError` variant — honoring that variant's declared status,
            // exactly like the handler error mapping below — and fall back to a
            // 400 `ValidationError` when the endpoint declares no such variant.
            if body.fields.iter().any(|f| f.constraint.is_some()) {
                let (name, code) = ep
                    .errors
                    .iter()
                    .find(|(name, _)| name == VALIDATION_ERROR_VARIANT)
                    .map(|(name, code)| (name.as_str(), *code))
                    .unwrap_or((VALIDATION_ERROR_VARIANT, VALIDATION_ERROR_FALLBACK_STATUS));
                self.server_out.push_str(&format!(
                    "\t\tif err := body.Validate(); err != nil {{\n\t\t\thttp.Error(w, \"{name}\", {code})\n\t\t\treturn\n\t\t}}\n",
                ));
            }
        }

        // Parse query params. Required params parse into a value type; optional
        // params (`Option<T>`) parse into a `*T` that is nil when absent, matching
        // the handler signature produced by `emit_handler_method`.
        for qp in &ep.query_params {
            self.emit_query_param_parse(qp);
        }

        // Parse request headers (parallel to query params, off `r.Header.Get`).
        for h in &ep.headers {
            self.emit_header_param_parse(h);
        }

        // Call handler
        let mut args = Vec::new();
        for pp in &ep.path_params {
            args.push(to_camel(pp));
        }
        if ep.body.is_some() {
            args.push("body".to_string());
        }
        for qp in &ep.query_params {
            args.push(to_camel(&qp.name));
        }
        for h in &ep.headers {
            args.push(to_camel(&h.name));
        }
        let args_str = args.join(", ");

        let handler_name = to_pascal_case(&ep.name);
        let has_resp_headers = !ep.response_headers.is_empty();

        // Error mapping uses `strings.Contains`; encoding a response uses
        // `encoding/json`. Record both so the import block stays minimal.
        if !ep.errors.is_empty() {
            self.server_needs_strings = true;
        }
        if ep.response_is_binary {
            // Binary download: the handler returns an `io.Reader`; stream it to
            // the wire as `application/octet-stream` (no JSON encoding).
            self.server_needs_io = true;
            self.server_out.push_str(&format!(
                "\t\tresult, err := h.{}({})\n",
                handler_name, args_str
            ));
            self.server_out.push_str("\t\tif err != nil {\n");
            for (name, code) in &ep.errors {
                self.server_out.push_str(&format!(
                    "\t\t\tif strings.Contains(err.Error(), \"{name}\") {{\n\t\t\t\thttp.Error(w, \"{name}\", {code})\n\t\t\t\treturn\n\t\t\t}}\n"
                ));
            }
            self.server_out
                .push_str("\t\t\thttp.Error(w, err.Error(), http.StatusInternalServerError)\n");
            self.server_out.push_str("\t\t\treturn\n\t\t}\n");
            self.server_out
                .push_str("\t\tw.Header().Set(\"Content-Type\", \"application/octet-stream\")\n");
            // The status line and headers are already committed, so a streaming
            // failure here is unrecoverable — discard the error explicitly.
            self.server_out.push_str("\t\t_, _ = io.Copy(w, result)\n");
        } else if ep.response.is_some() {
            self.server_needs_json = true;
            self.server_out.push_str(&format!(
                "\t\tresult, err := h.{}({})\n",
                handler_name, args_str
            ));
            self.server_out.push_str("\t\tif err != nil {\n");
            // Error mapping
            for (name, code) in &ep.errors {
                self.server_out.push_str(&format!(
                    "\t\t\tif strings.Contains(err.Error(), \"{name}\") {{\n\t\t\t\thttp.Error(w, \"{name}\", {code})\n\t\t\t\treturn\n\t\t\t}}\n"
                ));
            }
            self.server_out
                .push_str("\t\t\thttp.Error(w, err.Error(), http.StatusInternalServerError)\n");
            self.server_out.push_str("\t\t\treturn\n\t\t}\n");
            // Response headers: set each on `w.Header()` (stringified, optional
            // guarded) before the body is encoded. With an envelope the body
            // lives in `result.Body`; otherwise `result` is the body itself.
            for h in &ep.response_headers {
                self.emit_response_header_set(h);
            }
            self.server_out
                .push_str("\t\tw.Header().Set(\"Content-Type\", \"application/json\")\n");
            let encode_target = if has_resp_headers {
                "result.Body"
            } else {
                "result"
            };
            self.server_out
                .push_str(&format!("\t\tjson.NewEncoder(w).Encode({encode_target})\n"));
        } else {
            // No response body. Scope `err` to the `if` statement rather than
            // declaring it with a bare `err := h.X(...)`: a multipart body with a
            // required `File` field already declares an `err` in this closure
            // (via `r.FormFile`), and a second `:=` with `err` as its only
            // left-hand variable would not compile ("no new variables on left
            // side of :="). The statement-scoped form composes with either body.
            self.server_out.push_str(&format!(
                "\t\tif err := h.{}({}); err != nil {{\n",
                handler_name, args_str
            ));
            for (name, code) in &ep.errors {
                self.server_out.push_str(&format!(
                    "\t\t\tif strings.Contains(err.Error(), \"{name}\") {{\n\t\t\t\thttp.Error(w, \"{name}\", {code})\n\t\t\t\treturn\n\t\t\t}}\n"
                ));
            }
            self.server_out
                .push_str("\t\t\thttp.Error(w, err.Error(), http.StatusInternalServerError)\n");
            self.server_out.push_str("\t\t\treturn\n\t\t}\n");
            self.server_out
                .push_str("\t\tw.WriteHeader(http.StatusNoContent)\n");
        }

        self.server_out.push_str("\t})\n");
    }

    /// Emits server-side multipart parsing for an upload endpoint: parse the form
    /// with a bounded in-memory buffer (overflow spills to temp files, per
    /// `net/http`), pull each `File` field via `r.FormFile` (→
    /// `*multipart.FileHeader`) and each scalar via `r.FormValue` (coerced to the
    /// field's Go type), then assemble the `<Endpoint>Body` struct the handler
    /// receives. An optional file (`Option<File>`) tolerates an absent part (left
    /// nil); an optional scalar (`Option<T>`) parses into a `*T` left nil when the
    /// form value is empty — matching the JSON-body field shapes.
    fn emit_server_multipart_parse(&mut self, body: &ResolvedDerivedType, body_type: &str) {
        self.server_out.push_str(&format!(
            "\t\tif err := r.ParseMultipartForm({MULTIPART_MAX_MEMORY}); err != nil {{\n\t\t\thttp.Error(w, err.Error(), http.StatusBadRequest)\n\t\t\treturn\n\t\t}}\n"
        ));
        // Parts larger than the in-memory limit spill to temp files that
        // `net/http` does NOT auto-remove; `RemoveAll` deletes them when the
        // handler returns. Wrapped in a closure that discards the cleanup error
        // (`_ =`) — a bare `defer r.MultipartForm.RemoveAll()` would otherwise
        // trip errcheck (unlike `os.RemoveAll`, the method is not on its default
        // allowlist).
        self.server_out
            .push_str("\t\tdefer func() { _ = r.MultipartForm.RemoveAll() }()\n");
        self.server_out
            .push_str(&format!("\t\tvar body {body_type}\n"));

        for f in &body.fields {
            let field = to_pascal_case(&f.name);
            let wire = &f.name;
            if type_contains_file(&f.ty) {
                let optional = f.optional || matches!(&f.ty, Type::Generic(n, _) if n == "Option");
                // `r.FormFile` *opens* the part (a temp-file handle once the
                // upload spills past the in-memory limit). Only the
                // `*multipart.FileHeader` is kept on the body (the handler
                // re-opens via `.Open()`), so close the opened file immediately
                // to avoid leaking a descriptor per request. The Close error is
                // discarded explicitly (`_ =`) to satisfy errcheck.
                if optional {
                    // Tolerate an absent optional file: only assign on success.
                    self.server_out.push_str(&format!(
                        "\t\tif f, fh, err := r.FormFile(\"{wire}\"); err == nil {{\n\t\t\t_ = f.Close()\n\t\t\tbody.{field} = fh\n\t\t}}\n"
                    ));
                } else {
                    self.server_out.push_str(&format!(
                        "\t\tf{field}, fh{field}, err := r.FormFile(\"{wire}\")\n\t\tif err != nil {{\n\t\t\thttp.Error(w, err.Error(), http.StatusBadRequest)\n\t\t\treturn\n\t\t}}\n\t\t_ = f{field}.Close()\n\t\tbody.{field} = fh{field}\n"
                    ));
                }
            } else {
                self.emit_server_form_value_parse(f, &field, wire);
            }
        }
    }

    /// Emits coercion of one multipart scalar form value (`r.FormValue`) into the
    /// `<Endpoint>Body` field, parallel to [`Self::emit_query_param_parse`] but
    /// reading the form rather than the query string. Required scalars assign a
    /// value; optional (`Option<T>`) scalars parse into a `*T` left nil when the
    /// value is empty.
    fn emit_server_form_value_parse(&mut self, f: &DerivedField, field: &str, wire: &str) {
        let (optional, inner) = query_param_shape(&f.ty);
        let optional = optional || f.optional;
        let out = &mut self.server_out;
        if optional {
            match inner {
                Type::Int => {
                    self.server_needs_strconv = true;
                    out.push_str(&format!(
                        "\t\tif v := r.FormValue(\"{wire}\"); v != \"\" {{\n\t\t\tif n, err := strconv.ParseInt(v, 10, 64); err == nil {{\n\t\t\t\tbody.{field} = &n\n\t\t\t}}\n\t\t}}\n"
                    ));
                }
                Type::Float => {
                    self.server_needs_strconv = true;
                    out.push_str(&format!(
                        "\t\tif v := r.FormValue(\"{wire}\"); v != \"\" {{\n\t\t\tif n, err := strconv.ParseFloat(v, 64); err == nil {{\n\t\t\t\tbody.{field} = &n\n\t\t\t}}\n\t\t}}\n"
                    ));
                }
                Type::Bool => {
                    self.server_needs_strconv = true;
                    out.push_str(&format!(
                        "\t\tif v := r.FormValue(\"{wire}\"); v != \"\" {{\n\t\t\tif b, err := strconv.ParseBool(v); err == nil {{\n\t\t\t\tbody.{field} = &b\n\t\t\t}}\n\t\t}}\n"
                    ));
                }
                Type::String => {
                    out.push_str(&format!(
                        "\t\tif v := r.FormValue(\"{wire}\"); v != \"\" {{\n\t\t\tbody.{field} = &v\n\t\t}}\n"
                    ));
                }
                other => {
                    let go_type = type_to_go(other);
                    out.push_str(&format!(
                        "\t\tif v := r.FormValue(\"{wire}\"); v != \"\" {{\n\t\t\tcv := {go_type}(v)\n\t\t\tbody.{field} = &cv\n\t\t}}\n"
                    ));
                }
            }
        } else {
            match inner {
                Type::Int => {
                    self.server_needs_strconv = true;
                    out.push_str(&format!(
                        "\t\tbody.{field}, _ = strconv.ParseInt(r.FormValue(\"{wire}\"), 10, 64)\n"
                    ));
                }
                Type::Float => {
                    self.server_needs_strconv = true;
                    out.push_str(&format!(
                        "\t\tbody.{field}, _ = strconv.ParseFloat(r.FormValue(\"{wire}\"), 64)\n"
                    ));
                }
                Type::Bool => {
                    self.server_needs_strconv = true;
                    out.push_str(&format!(
                        "\t\tbody.{field}, _ = strconv.ParseBool(r.FormValue(\"{wire}\"))\n"
                    ));
                }
                Type::String => {
                    out.push_str(&format!("\t\tbody.{field} = r.FormValue(\"{wire}\")\n"));
                }
                other => {
                    let go_type = type_to_go(other);
                    out.push_str(&format!(
                        "\t\tbody.{field} = {go_type}(r.FormValue(\"{wire}\"))\n"
                    ));
                }
            }
        }
    }

    /// Emits server-side parsing for a single query parameter into a local whose
    /// Go type matches the handler signature ([`Self::emit_handler_method`]).
    ///
    /// Required params parse into a value (using the declared default, else the
    /// Go zero value); optional `Option<T>` params parse into a `*T` that stays
    /// nil when the parameter is absent or malformed. Numeric/bool parsing pulls
    /// in `strconv`; enum (named) params are produced via a direct `T(v)` string
    /// conversion, since Phoenix enums lower to `type T string`. That conversion
    /// is unchecked — an out-of-range value reaches the handler as-is, matching
    /// how Gen represents enums everywhere else (no runtime validation).
    fn emit_query_param_parse(&mut self, qp: &QueryParamInfo) {
        let camel = to_camel(&qp.name);
        let name = &qp.name;
        let (optional, inner) = query_param_shape(&qp.ty);

        // Optional params deliberately ignore `qp.default_value`: `Option<T>`
        // already models "may be absent" as a nil pointer, so a default would be
        // redundant (and there is no pointer slot to pre-seed). Only the required
        // branch below consults the declared default.
        let body = if optional {
            match inner {
                Type::Int => {
                    self.server_needs_strconv = true;
                    format!(
                        "var {camel} *int64\n\t\tif v := r.URL.Query().Get(\"{name}\"); v != \"\" {{\n\t\t\tif n, err := strconv.ParseInt(v, 10, 64); err == nil {{\n\t\t\t\t{camel} = &n\n\t\t\t}}\n\t\t}}\n"
                    )
                }
                Type::Float => {
                    self.server_needs_strconv = true;
                    format!(
                        "var {camel} *float64\n\t\tif v := r.URL.Query().Get(\"{name}\"); v != \"\" {{\n\t\t\tif n, err := strconv.ParseFloat(v, 64); err == nil {{\n\t\t\t\t{camel} = &n\n\t\t\t}}\n\t\t}}\n"
                    )
                }
                Type::Bool => {
                    self.server_needs_strconv = true;
                    format!(
                        "var {camel} *bool\n\t\tif v := r.URL.Query().Get(\"{name}\"); v != \"\" {{\n\t\t\tif b, err := strconv.ParseBool(v); err == nil {{\n\t\t\t\t{camel} = &b\n\t\t\t}}\n\t\t}}\n"
                    )
                }
                Type::String => {
                    format!(
                        "var {camel} *string\n\t\tif v := r.URL.Query().Get(\"{name}\"); v != \"\" {{\n\t\t\t{camel} = &v\n\t\t}}\n"
                    )
                }
                other => {
                    // Enum / named string-backed type: convert the raw value.
                    let go_type = type_to_go(other);
                    format!(
                        "var {camel} *{go_type}\n\t\tif v := r.URL.Query().Get(\"{name}\"); v != \"\" {{\n\t\t\tcv := {go_type}(v)\n\t\t\t{camel} = &cv\n\t\t}}\n"
                    )
                }
            }
        } else {
            let default = |fallback: &str| {
                qp.default_value
                    .as_ref()
                    .map(default_value_to_go)
                    .unwrap_or_else(|| fallback.to_string())
            };
            match inner {
                Type::Int => {
                    self.server_needs_strconv = true;
                    let default = default("0");
                    format!(
                        "{camel} := int64({default})\n\t\tif v := r.URL.Query().Get(\"{name}\"); v != \"\" {{\n\t\t\t{camel}, _ = strconv.ParseInt(v, 10, 64)\n\t\t}}\n"
                    )
                }
                Type::Float => {
                    self.server_needs_strconv = true;
                    let default = default("0");
                    format!(
                        "{camel} := float64({default})\n\t\tif v := r.URL.Query().Get(\"{name}\"); v != \"\" {{\n\t\t\t{camel}, _ = strconv.ParseFloat(v, 64)\n\t\t}}\n"
                    )
                }
                Type::Bool => {
                    self.server_needs_strconv = true;
                    let default = default("false");
                    format!(
                        "{camel} := {default}\n\t\tif v := r.URL.Query().Get(\"{name}\"); v != \"\" {{\n\t\t\t{camel}, _ = strconv.ParseBool(v)\n\t\t}}\n"
                    )
                }
                Type::String => {
                    let default = default("\"\"");
                    format!(
                        "{camel} := {default}\n\t\tif v := r.URL.Query().Get(\"{name}\"); v != \"\" {{\n\t\t\t{camel} = v\n\t\t}}\n"
                    )
                }
                other => {
                    // Enum / named string-backed type: convert the raw value.
                    let go_type = type_to_go(other);
                    format!("{camel} := {go_type}(r.URL.Query().Get(\"{name}\"))\n")
                }
            }
        };

        self.server_out.push_str("\t\t");
        self.server_out.push_str(&body);
    }

    /// Emits server-side parsing for a single REQUEST header into a local whose
    /// Go type matches the handler signature, parallel to [`Self::emit_query_param_parse`]
    /// but reading from `r.Header.Get("<wire_name>")` (the exact wire name from
    /// sema — never recomputed). Required headers parse into a value (declared
    /// default else Go zero value); optional `Option<T>` headers parse into a
    /// `*T` that stays nil when the header is absent or malformed.
    fn emit_header_param_parse(&mut self, h: &HeaderParamInfo) {
        let camel = to_camel(&h.name);
        let wire = &h.wire_name;
        let (optional, inner) = query_param_shape(&h.ty);

        let body = if optional {
            match inner {
                Type::Int => {
                    self.server_needs_strconv = true;
                    format!(
                        "var {camel} *int64\n\t\tif v := r.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t\tif n, err := strconv.ParseInt(v, 10, 64); err == nil {{\n\t\t\t\t{camel} = &n\n\t\t\t}}\n\t\t}}\n"
                    )
                }
                Type::Float => {
                    self.server_needs_strconv = true;
                    format!(
                        "var {camel} *float64\n\t\tif v := r.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t\tif n, err := strconv.ParseFloat(v, 64); err == nil {{\n\t\t\t\t{camel} = &n\n\t\t\t}}\n\t\t}}\n"
                    )
                }
                Type::Bool => {
                    self.server_needs_strconv = true;
                    format!(
                        "var {camel} *bool\n\t\tif v := r.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t\tif b, err := strconv.ParseBool(v); err == nil {{\n\t\t\t\t{camel} = &b\n\t\t\t}}\n\t\t}}\n"
                    )
                }
                Type::String => {
                    format!(
                        "var {camel} *string\n\t\tif v := r.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t\t{camel} = &v\n\t\t}}\n"
                    )
                }
                other => {
                    let go_type = type_to_go(other);
                    format!(
                        "var {camel} *{go_type}\n\t\tif v := r.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t\tcv := {go_type}(v)\n\t\t\t{camel} = &cv\n\t\t}}\n"
                    )
                }
            }
        } else {
            let default = |fallback: &str| {
                h.default_value
                    .as_ref()
                    .map(default_value_to_go)
                    .unwrap_or_else(|| fallback.to_string())
            };
            match inner {
                Type::Int => {
                    self.server_needs_strconv = true;
                    let default = default("0");
                    format!(
                        "{camel} := int64({default})\n\t\tif v := r.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t\t{camel}, _ = strconv.ParseInt(v, 10, 64)\n\t\t}}\n"
                    )
                }
                Type::Float => {
                    self.server_needs_strconv = true;
                    let default = default("0");
                    format!(
                        "{camel} := float64({default})\n\t\tif v := r.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t\t{camel}, _ = strconv.ParseFloat(v, 64)\n\t\t}}\n"
                    )
                }
                Type::Bool => {
                    self.server_needs_strconv = true;
                    let default = default("false");
                    format!(
                        "{camel} := {default}\n\t\tif v := r.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t\t{camel}, _ = strconv.ParseBool(v)\n\t\t}}\n"
                    )
                }
                Type::String => {
                    let default = default("\"\"");
                    format!(
                        "{camel} := {default}\n\t\tif v := r.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t\t{camel} = v\n\t\t}}\n"
                    )
                }
                other => {
                    let go_type = type_to_go(other);
                    format!("{camel} := {go_type}(r.Header.Get(\"{wire}\"))\n")
                }
            }
        };

        self.server_out.push_str("\t\t");
        self.server_out.push_str(&body);
    }

    /// Emits server-side writing of one RESPONSE header from the envelope field
    /// `result.<PascalName>` onto `w.Header()`. Non-string values are stringified
    /// like query params; optional (`*T`) headers are nil-guarded and
    /// dereferenced. Uses the exact wire name from sema.
    fn emit_response_header_set(&mut self, h: &HeaderParamInfo) {
        let field = to_pascal_case(&h.name);
        let wire = &h.wire_name;
        let (optional, inner) = query_param_shape(&h.ty);
        let value_expr = if optional {
            format!("*result.{field}")
        } else {
            format!("result.{field}")
        };
        let str_expr = header_string_expr(inner, &value_expr, &mut self.server_needs_strconv);
        let set_expr = format!("w.Header().Set(\"{wire}\", {str_expr})");
        if optional {
            self.server_out.push_str(&format!(
                "\t\tif result.{field} != nil {{\n\t\t\t{set_expr}\n\t\t}}\n"
            ));
        } else {
            self.server_out.push_str(&format!("\t\t{set_expr}\n"));
        }
    }
}

// ── Helper functions ─────────────────────────────────────────────────

/// Converts a Phoenix `Type` to a Go type string.
/// Renders `body` as one or more Go line comments, each prefixed with `prefix`
/// (e.g. `"// "` or `"\t// "`). A multi-line doc comment (the lexer joins its
/// lines with `\n`) gets EVERY line prefixed, so continuation lines stay
/// commented instead of leaking into the file as code. Trailing whitespace is
/// trimmed per line so an empty line renders as a bare `//` (gofmt-clean).
fn render_line_comment(prefix: &str, body: &str) -> String {
    let mut out = String::new();
    for line in body.split('\n') {
        out.push_str(format!("{prefix}{line}").trim_end());
        out.push('\n');
    }
    out
}

/// Renders the Go type for a derived body field and reports whether it is a
/// nil-able pointer.
///
/// A `partial`-applied field becomes a pointer so it can be omitted, but an
/// already-`Option<T>` field is *already* rendered as `*T` by [`type_to_go`], so
/// applying `partial` to it must NOT produce `**T`. Centralizing this here keeps
/// the struct-field rendering ([`GoGenerator::emit_derived_type`]) and the body
/// `Validate()` deref/nil-guard ([`GoGenerator::emit_body_validate_method`]) in
/// lock-step — the pointer-ness they each compute can never drift.
fn derived_field_go_type(f: &DerivedField) -> (String, bool) {
    let is_option = matches!(&f.ty, Type::Generic(name, _) if name == "Option");
    let is_ptr = f.optional || is_option;
    // `type_to_go` already renders `Option<T>` as `*T`; only add a pointer for a
    // `partial`-applied non-Option field so an optional Option stays a single `*T`.
    let go_type = if f.optional && !is_option {
        format!("*{}", type_to_go(&f.ty))
    } else {
        type_to_go(&f.ty)
    };
    (go_type, is_ptr)
}

/// The output sinks [`render_validate_fn`] writes through: the buffer it appends
/// the `Validate()` method to, plus the two import flags it raises — `fmt` always
/// (the error path uses `fmt.Errorf`) and `strings` only when some constraint
/// calls a `strings` helper. Bundling what were three positional `&mut` params
/// into one named handle keeps the call sites readable and labels the two
/// otherwise-interchangeable `&mut bool` flags. Held as three disjoint
/// `&mut self.field` borrows at the call site, so it coexists with the immutable
/// borrow `emit_validate_method` keeps on `self.check_result` via `fields`.
struct ValidateSink<'a> {
    out: &'a mut String,
    needs_fmt: &'a mut bool,
    needs_strings: &'a mut bool,
}

/// Renders a `func (s {type_name}) Validate() error` whose body checks every
/// constrained field, then `return nil`. `fields` lists each constrained field
/// as `(name, constraint, is_ptr)`: an `is_ptr` field is rendered as a Go
/// pointer (either `partial`-applied or already `Option<T>`, both `*T`), so its
/// check is nil-guarded and `self` is dereferenced inside the constraint
/// expression; a plain field is checked directly. Shared by the source-struct
/// validator ([`GoGenerator::emit_validate_method`]) and the derived-body
/// validator ([`GoGenerator::emit_body_validate_method`]) so the two can never
/// drift. Callers must invoke this only with a non-empty `fields` — an empty
/// `Validate()` would needlessly pull in the `fmt` import.
fn render_validate_fn(sink: ValidateSink<'_>, type_name: &str, fields: &[(&str, &Expr, bool)]) {
    let ValidateSink {
        out,
        needs_fmt,
        needs_strings,
    } = sink;
    *needs_fmt = true;
    out.push_str(&format!(
        "// Validate checks all field constraints for {type_name}.\n"
    ));
    out.push_str(&format!("func (s {type_name}) Validate() error {{\n"));

    for (name, constraint, is_ptr) in fields {
        if constraint_needs_strings(constraint) {
            *needs_strings = true;
        }
        // `constraint_expr_to_go` already wraps binary expressions in
        // parentheses; strip the redundant outer pair so the `!(...)` guard
        // matches gofmt's canonical output (no doubled parens).
        let go_expr = strip_outer_parens(&constraint_expr_to_go(constraint, name, *is_ptr));
        if *is_ptr {
            out.push_str(&format!(
                "\tif s.{} != nil && !({}) {{\n\t\treturn fmt.Errorf(\"{}: constraint violated\")\n\t}}\n",
                to_pascal_case(name), go_expr, name
            ));
        } else {
            out.push_str(&format!(
                "\tif !({}) {{\n\t\treturn fmt.Errorf(\"{}: constraint violated\")\n\t}}\n",
                go_expr, name
            ));
        }
    }

    out.push_str("\treturn nil\n}\n\n");
}

/// Reports whether `ty` is `File` or `Option<File>` — the two field shapes that
/// carry a binary part and therefore need `*multipart.FileHeader` (and the
/// `mime/multipart` import). `List<File>` etc. are rejected by sema, so the only
/// generic wrapper to look through is `Option`.
fn type_contains_file(ty: &Type) -> bool {
    match ty {
        Type::File => true,
        Type::Generic(name, args) if name == "Option" && args.len() == 1 => {
            type_contains_file(&args[0])
        }
        _ => false,
    }
}

fn type_to_go(ty: &Type) -> String {
    match ty {
        Type::Int => "int64".to_string(),
        Type::Float => "float64".to_string(),
        Type::String => "string".to_string(),
        Type::Bool => "bool".to_string(),
        // A `File` body field is a binary upload/download. In Go the server-side
        // type is `*multipart.FileHeader`; multipart parse/assembly and binary
        // responses live in the body-codegen path. This is the field type.
        Type::File => "*multipart.FileHeader".to_string(),
        Type::Void => "".to_string(),
        Type::Named(name) => name.clone(),
        Type::Generic(name, args) if name == "List" && args.len() == 1 => {
            format!("[]{}", type_to_go(&args[0]))
        }
        Type::Generic(name, args) if name == "Map" && args.len() == 2 => {
            format!("map[{}]{}", type_to_go(&args[0]), type_to_go(&args[1]))
        }
        // `Option<File>` is already a nullable `*multipart.FileHeader`; a second
        // pointer (`**`) would be both non-idiomatic and a compile error against
        // `r.FormFile`'s `*multipart.FileHeader` result. Every other `Option<T>`
        // adds the usual single pointer.
        Type::Generic(name, args)
            if name == "Option" && args.len() == 1 && matches!(args[0], Type::File) =>
        {
            type_to_go(&args[0])
        }
        Type::Generic(name, args) if name == "Option" && args.len() == 1 => {
            format!("*{}", type_to_go(&args[0]))
        }
        // Trait objects map to a Go interface named after the trait.  The
        // interface itself is not emitted by Phoenix codegen today — callers
        // supplying `dyn Trait` fields must define the matching Go interface
        // in hand-written code.  Parallel to the TS/Python behavior.
        Type::Dyn(name) => name.clone(),
        _ => "interface{}".to_string(),
    }
}

/// Renders a complete `type NAME struct { ... }` declaration (trailing blank
/// line included) with gofmt-faithful field alignment.
///
/// An empty field set collapses to `type NAME struct{}` — gofmt rewrites the
/// multi-line `struct {\n}` form to that, so emitting it directly keeps
/// `gofmt -l` empty for fieldless structs (e.g. a body type with every field
/// omitted, or a struct sema couldn't resolve).
fn render_struct(name: &str, rows: &[(String, String, String)]) -> String {
    if rows.is_empty() {
        return format!("type {name} struct{{}}\n\n");
    }
    format!("type {name} struct {{\n{}}}\n\n", align_struct_fields(rows))
}

/// Renders Go struct fields as gofmt-aligned rows.
///
/// Each row is `(name, go_type, json_tag)`. gofmt aligns columns within a
/// contiguous field block by padding the name and type columns with spaces to
/// the block maximum (plus one separating space), with a single leading tab for
/// the struct-body indent. Emitting the exact spacing here keeps `gofmt -l`
/// empty without shelling out to gofmt.
fn align_struct_fields(rows: &[(String, String, String)]) -> String {
    let max_name = rows.iter().map(|(n, _, _)| n.len()).max().unwrap_or(0);
    let max_type = rows.iter().map(|(_, t, _)| t.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (name, ty, tag) in rows {
        out.push_str(&format!(
            "\t{name:<nw$} {ty:<tw$} {tag}\n",
            nw = max_name,
            tw = max_type
        ));
    }
    out
}

/// Strips a single balanced outer pair of parentheses from `expr`, if the whole
/// string is wrapped in one. Used to avoid emitting doubled parens like
/// `!((expr))` that gofmt would rewrite.
///
/// Assumes `expr` is parenthesis-balanced (it always is — the only caller feeds
/// it [`constraint_expr_to_go`] output). The leading-`(`/trailing-`)` guard plus
/// that invariant mean the depth counter never decrements below zero, so the
/// scan cannot underflow on the inputs this is ever given.
fn strip_outer_parens(expr: &str) -> String {
    let bytes = expr.as_bytes();
    if bytes.first() != Some(&b'(') || bytes.last() != Some(&b')') {
        return expr.to_string();
    }
    // Verify the opening paren matches the closing paren (balanced wrap).
    let mut depth = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    // The first '(' closes at index i; only strip if that's the
                    // final character (i.e. the whole expression is wrapped).
                    if i == bytes.len() - 1 {
                        return expr[1..expr.len() - 1].to_string();
                    }
                    return expr.to_string();
                }
            }
            _ => {}
        }
    }
    expr.to_string()
}

/// Inspects a query-param type and returns `(optional, inner)`.
///
/// For `Option<T>` this returns `(true, &T)`; for any other type `(false, ty)`.
/// Optional query params are represented as `*T` in client/handler signatures
/// and parsed into a nil-able pointer on the server.
fn query_param_shape(ty: &Type) -> (bool, &Type) {
    match ty {
        Type::Generic(name, args) if name == "Option" && args.len() == 1 => (true, &args[0]),
        other => (false, other),
    }
}

/// The generated envelope type name for an endpoint that declares response
/// headers: `<PascalEndpoint>Result` (e.g. `getPost` → `GetPostResult`). Used
/// for the types.go struct, the handler return, and the client return. Headers
/// reuse the query-param `query_param_shape` for optionality (`Option<T>` →
/// `*T`), so the wire/stringify logic is shared between query params and headers.
fn header_result_type_name(ep: &EndpointInfo) -> String {
    format!("{}Result", to_pascal_case(&ep.name))
}

/// The generated pagination-envelope type name for an endpoint that declares a
/// `pagination { }` block: `<PascalEndpoint>Page` (e.g. `listPosts` →
/// `ListPostsPage`). Used for the types.go struct, the handler return, the client
/// return, and the server encode target. Distinct from the response-headers
/// `<Endpoint>Result` envelope (the two are mutually exclusive per sema).
fn page_type_name(ep: &EndpointInfo) -> String {
    format!("{}Page", to_pascal_case(&ep.name))
}

/// Renders a Go expression that stringifies a header value for the wire,
/// mirroring how query params convert `int64`/`float64`/`bool` to `string`.
/// `value_expr` is the already-dereferenced value (e.g. `*x` for an optional).
/// Returns the string expression; sets `needs_strconv` when a `strconv` helper
/// is used. Named (enum) types are string-backed, so `string(v)` suffices.
fn header_string_expr(inner: &Type, value_expr: &str, needs_strconv: &mut bool) -> String {
    match inner {
        Type::Int => {
            *needs_strconv = true;
            format!("strconv.FormatInt({value_expr}, 10)")
        }
        Type::Float => {
            *needs_strconv = true;
            format!("strconv.FormatFloat({value_expr}, 'f', -1, 64)")
        }
        Type::Bool => {
            *needs_strconv = true;
            format!("strconv.FormatBool({value_expr})")
        }
        Type::String => value_expr.to_string(),
        _ => format!("string({value_expr})"),
    }
}

/// Converts a camelCase identifier to PascalCase (Go exported name).
fn to_pascal_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Returns a camelCase identifier (Go unexported / parameter name).
/// Phoenix identifiers are already camelCase, so this is mostly identity.
fn to_camel(s: &str) -> String {
    s.to_string()
}

use crate::capitalize;

/// Builds a Go `fmt.Sprintf` format string and args from a Phoenix URL pattern.
///
/// Returns `(format_pattern, sprintf_args)`.
/// `/api/users/{id}` → `("/api/users/%d", ", id")`
fn build_go_url(path: &str, params: &[String]) -> (String, String) {
    let mut format_str = String::new();
    let mut in_brace = false;

    for c in path.chars() {
        if c == '{' {
            in_brace = true;
            format_str.push_str("%s"); // path params are strings
        } else if c == '}' {
            in_brace = false;
        } else if !in_brace {
            format_str.push(c);
        }
    }

    let args = if params.is_empty() {
        String::new()
    } else {
        format!(
            ", {}",
            params
                .iter()
                .map(|p| to_camel(p))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    (format_str, args)
}

/// Converts a `DefaultValue` to a Go literal.
fn default_value_to_go(val: &DefaultValue) -> String {
    match val {
        DefaultValue::Int(v) => v.to_string(),
        DefaultValue::Float(v) => v.to_string(),
        DefaultValue::String(v) => format!("\"{}\"", v),
        DefaultValue::Bool(true) => "true".to_string(),
        DefaultValue::Bool(false) => "false".to_string(),
    }
}

/// Recursively converts a Phoenix constraint `Expr` to a Go expression string.
///
/// Replaces `self` with the struct field accessor (`s.FieldName`), translates
/// `self.length` to `len(s.FieldName)`, and `self.contains(arg)` to
/// `strings.Contains(s.FieldName, arg)`.
///
/// When `deref` is true (for `Option` fields), the field is a pointer and
/// accesses are dereferenced (e.g., `*s.FieldName`).
fn constraint_expr_to_go(
    expr: &phoenix_parser::ast::Expr,
    field_name: &str,
    deref: bool,
) -> String {
    use phoenix_parser::ast::{BinaryOp, Expr, LiteralKind, UnaryOp};

    match expr {
        Expr::Ident(ident) if ident.name == "self" => {
            let accessor = format!("s.{}", to_pascal_case(field_name));
            if deref {
                format!("(*{})", accessor)
            } else {
                accessor
            }
        }
        Expr::Ident(ident) => ident.name.clone(),
        Expr::Literal(lit) => match &lit.kind {
            LiteralKind::Int(v) => v.to_string(),
            LiteralKind::Float(v) => v.to_string(),
            LiteralKind::String(v) => format!("\"{}\"", v),
            LiteralKind::Bool(v) => v.to_string(),
        },
        Expr::Binary(bin) => {
            let left = constraint_expr_to_go(&bin.left, field_name, deref);
            let right = constraint_expr_to_go(&bin.right, field_name, deref);
            let op = match bin.op {
                BinaryOp::Add => "+",
                BinaryOp::Sub => "-",
                BinaryOp::Mul => "*",
                BinaryOp::Div => "/",
                BinaryOp::Mod => "%",
                BinaryOp::Eq => "==",
                BinaryOp::NotEq => "!=",
                BinaryOp::Lt => "<",
                BinaryOp::Gt => ">",
                BinaryOp::LtEq => "<=",
                BinaryOp::GtEq => ">=",
                BinaryOp::And => "&&",
                BinaryOp::Or => "||",
            };
            format!("({left} {op} {right})")
        }
        Expr::Unary(un) => {
            let operand = constraint_expr_to_go(&un.operand, field_name, deref);
            match un.op {
                UnaryOp::Neg => format!("(-{operand})"),
                UnaryOp::Not => format!("(!{operand})"),
            }
        }
        // self.length → len(s.FieldName)
        Expr::FieldAccess(fa) if fa.field == "length" && is_self_ident_go(&fa.object) => {
            let accessor = format!("s.{}", to_pascal_case(field_name));
            if deref {
                format!("len(*{})", accessor)
            } else {
                format!("len({})", accessor)
            }
        }
        Expr::FieldAccess(fa) => {
            let object = constraint_expr_to_go(&fa.object, field_name, deref);
            format!("{object}.{}", fa.field)
        }
        // self.contains(arg) → strings.Contains(s.FieldName, arg)
        Expr::MethodCall(mc) if mc.method == "contains" && is_self_ident_go(&mc.object) => {
            let accessor = format!("s.{}", to_pascal_case(field_name));
            let args: Vec<String> = mc
                .args
                .iter()
                .map(|a| constraint_expr_to_go(a, field_name, deref))
                .collect();
            if deref {
                format!("strings.Contains(*{}, {})", accessor, args.join(", "))
            } else {
                format!("strings.Contains({}, {})", accessor, args.join(", "))
            }
        }
        Expr::MethodCall(mc) => {
            let object = constraint_expr_to_go(&mc.object, field_name, deref);
            let args: Vec<String> = mc
                .args
                .iter()
                .map(|a| constraint_expr_to_go(a, field_name, deref))
                .collect();
            format!("{object}.{}({})", mc.method, args.join(", "))
        }
        _ => "true".to_string(),
    }
}

/// Returns true if the expression is the `self` identifier.
fn is_self_ident_go(expr: &phoenix_parser::ast::Expr) -> bool {
    matches!(expr, phoenix_parser::ast::Expr::Ident(i) if i.name == "self")
}

/// Returns true if any part of a constraint expression uses a method call
/// that requires the `"strings"` import (e.g., `contains`).
fn constraint_needs_strings(expr: &phoenix_parser::ast::Expr) -> bool {
    use phoenix_parser::ast::Expr;
    match expr {
        Expr::Binary(bin) => {
            constraint_needs_strings(&bin.left) || constraint_needs_strings(&bin.right)
        }
        Expr::MethodCall(mc) if mc.method == "contains" => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
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
}
