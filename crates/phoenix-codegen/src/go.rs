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

/// The HTTP router the generated `server.go` targets. Only the router wiring in
/// `server.go` differs between frameworks; `types.go`, `client.go`, and
/// `handlers.go` are framework-independent.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum GoServerFramework {
    /// The standard library `net/http` `ServeMux` (the default, backward-compatible
    /// target; uses Go 1.22+ method-pattern routing).
    #[default]
    NetHttp,
    /// A `github.com/go-chi/chi/v5` router (`chi.Router`).
    Chi,
}

/// Generates Go code from a parsed and type-checked Phoenix program, targeting
/// the default (`net/http`) server framework.
pub fn generate_go(program: &Program, check_result: &Analysis) -> GoFiles {
    generate_go_with(program, check_result, GoServerFramework::NetHttp)
}

/// Like [`generate_go`], but emits `server.go` for the chosen
/// [`GoServerFramework`]. The other three files are identical regardless of
/// framework.
pub fn generate_go_with(
    program: &Program,
    check_result: &Analysis,
    framework: GoServerFramework,
) -> GoFiles {
    let generator = GoGenerator::new(check_result, framework);
    generator.generate(program)
}

/// Returns `preferred` if it is free of `taken`, else `preferred` with enough
/// trailing `_`s appended to clear it. Phoenix identifiers share Go's
/// `[A-Za-z_][A-Za-z0-9_]*` shape, so no fixed name is collision-proof; this is
/// how every generated Go local dodges the user's parameter identifiers.
fn pick_free_local(preferred: &str, taken: &BTreeSet<String>) -> String {
    let mut name = preferred.to_string();
    while taken.contains(&name) {
        name.push('_');
    }
    name
}

/// The parameter identifiers a generated local must avoid colliding with: path
/// params, the body, query params, and request headers — all emitted verbatim
/// via `to_camel` (which is the identity). The client and the server both
/// uniquify their locals against this same set, so they share one source of
/// truth here rather than rebuilding it independently.
fn endpoint_param_idents(ep: &EndpointInfo) -> BTreeSet<String> {
    let mut taken = BTreeSet::new();
    for pp in &ep.path_params {
        taken.insert(to_camel(pp));
    }
    if ep.body.is_some() {
        taken.insert("body".to_string());
    }
    for qp in &ep.query_params {
        taken.insert(to_camel(&qp.name));
    }
    for h in &ep.headers {
        taken.insert(to_camel(&h.name));
    }
    taken
}

/// Names for the function-scoped locals a client method emits, each derived to
/// avoid colliding with the method's parameter identifiers.
///
/// Every generated Go client method declares a handful of locals around the
/// HTTP round-trip — `u` (the URL), a `url.Values` query builder, `data` (the
/// marshaled body), `req`/`resp`, the decoded `result`, and `buf`/`writer` for
/// multipart uploads. They share Go's function scope with the method's
/// parameters, whose names are the user's Phoenix identifiers verbatim
/// (`to_camel` is the identity). A generated local whose fixed name equals a
/// parameter is a redeclare — `q := url.Values{}` beside a `q` query param is
/// "no new variables on left side of :=", and `var result T` beside a `result`
/// param is "result redeclared in this block" — and the generated Go won't
/// compile. `q` (a search query), `u`, `data`, `req`, `resp`, `result` are all
/// reachable param names. Phoenix identifiers share Go's `[A-Za-z_][A-Za-z0-9_]*`
/// shape, so no fixed name is collision-proof; each local is derived to dodge
/// this method's parameter identifiers (and the other locals already chosen).
///
/// `err` is deliberately NOT uniquified: every `err` site is a `x, err :=` with
/// a fresh `x` (or an `if`-init in a nested scope), so colliding with an `err`
/// parameter reuses it legally rather than redeclaring — and threading a renamed
/// `err` through every error check would be pure churn. When nothing collides —
/// the overwhelmingly common case — every field keeps its natural name, so
/// existing output is byte-for-byte unchanged.
///
/// `recv` is the method's receiver (`c` by default, as in `func (c *ApiClient)`),
/// referenced as `c.BaseURL` / `c.Client.Do`. It shares the function scope with
/// the parameters too, so a param named `c` (a cursor/count param is plausible)
/// would shadow the receiver — `c.BaseURL` would then read the param, not the
/// client. It is uniquified here like every other local. (This is the client's
/// only fixed generated identifier; the server's `w`/`r`/`h`/`mux` are the
/// analogous fixed identifiers there and are NOT yet uniquified — see
/// `emit_server_route`.)
struct ClientLocals {
    recv: String,
    url: String,
    query: String,
    data: String,
    req: String,
    resp: String,
    result: String,
    buf: String,
    writer: String,
}

impl ClientLocals {
    fn new(ep: &EndpointInfo) -> Self {
        // The parameter identifiers a generated local must avoid (shared with the
        // server route emitter — see [`endpoint_param_idents`]).
        let mut taken = endpoint_param_idents(ep);
        // Every local is derived eagerly, even ones this endpoint won't emit (a
        // bodyless GET still reserves `data`/`buf`/`writer`). A reserved-but-unused
        // name never reaches the output, so this can't widen the diff; deriving the
        // full set up front keeps the struct a plain value rather than threading
        // endpoint shape through every emit site.
        //
        // Pick the preferred name if free, else append `_` until it is; reserve
        // each choice so two locals can't land on the same fallback.
        let mut pick = |preferred: &str| -> String {
            let name = pick_free_local(preferred, &taken);
            taken.insert(name.clone());
            name
        };
        ClientLocals {
            recv: pick("c"),
            url: pick("u"),
            query: pick("q"),
            data: pick("data"),
            req: pick("req"),
            resp: pick("resp"),
            result: pick("result"),
            buf: pick("buf"),
            writer: pick("writer"),
        }
    }
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
    /// Whether types.go needs a `"regexp"` import + the package-level `uuidRe`
    /// var (a `Uuid` field's `Validate()` checks its RFC 4122 format with it).
    types_needs_regexp: bool,
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
    /// The server framework `server.go` targets.
    framework: GoServerFramework,
}

impl<'a> GoGenerator<'a> {
    /// Creates a new Go generator with the given semantic analysis results.
    fn new(check_result: &'a Analysis, framework: GoServerFramework) -> Self {
        Self {
            check_result,
            framework,
            types_out: String::new(),
            client_out: String::new(),
            handlers_out: String::new(),
            server_out: String::new(),
            emitted_derived_types: BTreeSet::new(),
            types_needs_fmt: false,
            types_needs_strings: false,
            types_needs_regexp: false,
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
            self.emit_multi_status_envelope(ep);
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
            // `time.Time` (a `DateTime` field) is the only thing that needs
            // `time`, so its presence in the rendered body is the exact import
            // condition — simpler and drift-proof vs. a flag set at every
            // `type_to_go` call site.
            if go_body_uses_time(&types_body) {
                imports.push("\"time\"");
            }
            // `regexp` is needed only by the `uuidRe` var a `Uuid` field's
            // `Validate()` uses.
            if self.types_needs_regexp {
                imports.push("\"regexp\"");
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
        // The shared RFC 4122 matcher, compiled once, used by every `Uuid`
        // field's `Validate()` check. Emitted only when some struct/body has one.
        if self.types_needs_regexp {
            self.types_out.push_str(
                "var uuidRe = regexp.MustCompile(`^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$`)\n\n",
            );
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
            // See the types.go note: `time.Time` in the body is the exact
            // condition for the `time` import (a `DateTime` param/field/return).
            if go_body_uses_time(&client_body) {
                imports.push("\"time\"");
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

        // Compose handlers.go with header and conditional imports: `io` for a
        // binary-download handler return (`io.Reader`), `time` for a `DateTime`
        // in any handler signature (`time.Time` in the body is the exact
        // condition — see the types.go note).
        let handlers_body = std::mem::take(&mut self.handlers_out);
        self.handlers_out
            .push_str("// Generated by Phoenix Gen — implement the handler methods below.\n\n");
        self.handlers_out.push_str("package api\n\n");
        {
            let mut imports: Vec<&str> = Vec::new();
            if self.handlers_needs_io {
                imports.push("\"io\"");
            }
            if go_body_uses_time(&handlers_body) {
                imports.push("\"time\"");
            }
            imports.sort_unstable();
            if !imports.is_empty() {
                self.handlers_out.push_str(&format!(
                    "import (\n{}\n)\n\n",
                    imports
                        .iter()
                        .map(|i| format!("\t{i}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
        }
        self.handlers_out.push_str(&handlers_body);

        // ── server.go (body — header/imports composed at end) ───────
        // Router type/constructor/variable per framework. Only this wiring and the
        // per-route registration differ; the route bodies (decode → handle →
        // respond) are identical net/http code, since chi handlers are ordinary
        // `http.HandlerFunc`s. chi.NewRouter() returns a `chi.Router` (itself an
        // `http.Handler`), so callers mount it exactly like a `*http.ServeMux`.
        let (router_ty, router_ctor, router_var) = match self.framework {
            GoServerFramework::NetHttp => ("*http.ServeMux", "http.NewServeMux()", "mux"),
            GoServerFramework::Chi => ("chi.Router", "chi.NewRouter()", "router"),
        };
        let router_doc = match self.framework {
            GoServerFramework::NetHttp => "an http.ServeMux",
            GoServerFramework::Chi => "a chi.Router",
        };
        self.server_out.push_str(&format!(
            "// NewRouter creates {router_doc} that routes requests to the given handlers.\n",
        ));
        self.server_out
            .push_str(&format!("func NewRouter(h Handlers) {router_ty} {{\n"));
        self.server_out
            .push_str(&format!("\t{router_var} := {router_ctor}\n"));

        for ep in &self.check_result.endpoints {
            self.emit_server_route(ep);
        }

        self.server_out
            .push_str(&format!("\treturn {router_var}\n}}\n"));

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
            // Standard-library imports form the first group. `net/http` is always
            // used (the router and `http.Error`); the rest only when emitted.
            let mut std_imports = vec!["\"net/http\""];
            if self.server_needs_io {
                std_imports.push("\"io\"");
            }
            if self.server_needs_json {
                std_imports.push("\"encoding/json\"");
            }
            if self.server_needs_strconv {
                std_imports.push("\"strconv\"");
            }
            if self.server_needs_strings {
                std_imports.push("\"strings\"");
            }
            // See the types.go note: `time.Time` in the body is the exact
            // condition for the `time` import (e.g. a `DateTime` query param
            // parsed via `time.Parse`).
            if go_body_uses_time(&server_body) {
                std_imports.push("\"time\"");
            }
            std_imports.sort_unstable();

            // chi is the only third-party import (and only for the chi framework).
            // Keeping it in its own blank-line-separated group makes the output
            // stable under goimports/gci as well as gofmt, so a downstream
            // formatter run leaves the checked-in file byte-for-byte untouched.
            let mut third_party: Vec<&str> = Vec::new();
            if matches!(self.framework, GoServerFramework::Chi) {
                third_party.push("\"github.com/go-chi/chi/v5\"");
            }

            let render_group = |group: &[&str]| {
                group
                    .iter()
                    .map(|i| format!("\t{i}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let mut groups = vec![render_group(&std_imports)];
            if !third_party.is_empty() {
                groups.push(render_group(&third_party));
            }
            self.server_out
                .push_str(&format!("import (\n{}\n)\n\n", groups.join("\n\n")));
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
                &format!(
                    "{} is {}{}",
                    s.name,
                    doc.to_lowercase(),
                    doc_terminator(doc)
                ),
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
                &format!(
                    "{} is {}{}",
                    e.name,
                    doc.to_lowercase(),
                    doc_terminator(doc)
                ),
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

    /// Emits the generated `<Endpoint>Response` multi-status envelope struct for an
    /// endpoint that declares a `response { }` block (`response_statuses`
    /// non-empty). The handler returns, and the client observes, this envelope
    /// instead of the bare body:
    /// - `Status int` — the actual HTTP status (handler sets it, server writes it,
    ///   client reads it).
    /// - `Body *T` — the shared body type as an Option (pointer, `omitempty`),
    ///   present only when the block declares at least one typed status (`ep.response`
    ///   is `Some`). An all-typeless block (e.g. `response { 202  204 }`) has no `T`,
    ///   so the envelope is just `{ Status int }` with no `Body` field.
    ///
    /// This mirrors the response-header `<Endpoint>Result` / pagination
    /// `<Endpoint>Page` envelope machinery; all three are mutually exclusive (sema
    /// rejects the combinations), so an endpoint never emits more than one.
    /// Endpoints WITHOUT a `response { }` block emit nothing and keep returning the
    /// bare body unchanged.
    fn emit_multi_status_envelope(&mut self, ep: &EndpointInfo) {
        if ep.response_statuses.is_empty() {
            return;
        }
        let type_name = multi_status_type_name(ep);
        if !self.emitted_derived_types.insert(type_name.clone()) {
            return;
        }
        // `Status` is Go `int` — the one place a Phoenix-design `Int` does not
        // render as `int64` (`type_to_go`). Deliberate: it mirrors
        // `http.Response.StatusCode int`, so handlers and clients assign it
        // without casts, and the JSON wire form is identical either way.
        let mut rows: Vec<(String, String, String)> = vec![(
            "Status".to_string(),
            "int".to_string(),
            "`json:\"status\"`".to_string(),
        )];
        if let Some(body_type) = ep.response.as_ref().map(type_to_go) {
            rows.push((
                "Body".to_string(),
                format!("*{body_type}"),
                "`json:\"body,omitempty\"`".to_string(),
            ));
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
        // A `Uuid`/`Option<Uuid>` body field gets a format check. Pointer-ness
        // comes from `derived_field_go_type` (a `partial`-applied `Uuid` is `*string`),
        // so the nil-guard can't disagree with the rendered field type.
        let uuid_fields: Vec<(&str, bool)> = body
            .fields
            .iter()
            .filter(|f| is_uuid_field(&f.ty))
            .map(|f| (f.name.as_str(), derived_field_go_type(f).1))
            .collect();
        if fields.is_empty() && uuid_fields.is_empty() {
            return;
        }
        render_validate_fn(
            ValidateSink {
                out: &mut self.types_out,
                needs_fmt: &mut self.types_needs_fmt,
                needs_strings: &mut self.types_needs_strings,
                needs_regexp: &mut self.types_needs_regexp,
            },
            type_name,
            &fields,
            &uuid_fields,
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
        let uuid_fields: Vec<(&str, bool)> = info
            .fields
            .iter()
            .filter_map(|f| uuid_field_shape(&f.ty).map(|is_ptr| (f.name.as_str(), is_ptr)))
            .collect();
        if fields.is_empty() && uuid_fields.is_empty() {
            return;
        }
        render_validate_fn(
            ValidateSink {
                out: &mut self.types_out,
                needs_fmt: &mut self.types_needs_fmt,
                needs_strings: &mut self.types_needs_strings,
                needs_regexp: &mut self.types_needs_regexp,
            },
            &s.name,
            &fields,
            &uuid_fields,
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
        // A multi-status endpoint returns the `<Endpoint>Response` envelope
        // (handler-chosen status + optional shared body) instead of the bare body.
        let is_multi_status = !ep.response_statuses.is_empty();
        let multi_status_type = multi_status_type_name(ep);

        // Names for the method's function-scoped locals, each derived to avoid
        // colliding with this endpoint's parameter identifiers (see
        // [`ClientLocals`]). The common no-collision case leaves every name
        // unchanged.
        let locals = ClientLocals::new(ep);

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
        let returns_value = is_multi_status || has_resp_headers || response_type.is_some();

        // Return type. A binary download returns the raw response stream
        // (`io.ReadCloser`) the caller drains and closes; with response headers
        // the method returns the envelope pointer; otherwise the bare response.
        let return_sig = if is_multi_status {
            format!("(*{}, error)", multi_status_type)
        } else if ep.response_is_binary {
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
                &format!(
                    "{} {}{}",
                    method_name,
                    doc.to_lowercase(),
                    doc_terminator(doc)
                ),
            ));
        }
        self.client_out.push_str(&format!(
            "func ({} *ApiClient) {}({}) {} {{\n",
            locals.recv, method_name, params_str, return_sig
        ));

        // Build URL. Every method formats its URL with `fmt.Sprintf` and (below)
        // wraps HTTP errors with `fmt.Errorf`, so emitting any method needs `fmt`.
        self.client_needs_fmt = true;
        let url_expr = build_go_url(&ep.path, &ep.path_params);
        let url = &locals.url;
        let recv = &locals.recv;
        self.client_out.push_str(&format!(
            "\t{url} := fmt.Sprintf(\"%s{}\", {recv}.BaseURL{})\n",
            url_expr.0, url_expr.1
        ));

        // Query params
        if !ep.query_params.is_empty() {
            self.client_needs_url = true;
            // The query builder's `url.Values` local (`q` by default) is one of
            // the method's function-scoped locals uniquified against the
            // parameter names — see [`ClientLocals`].
            let qv = &locals.query;
            self.client_out
                .push_str(&format!("\t{qv} := url.Values{{}}\n"));
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
                            "{qv}.Set(\"{}\", strconv.FormatInt({}, 10))",
                            qp.name, value_expr
                        )
                    }
                    Type::Float => {
                        self.client_needs_strconv = true;
                        format!(
                            "{qv}.Set(\"{}\", strconv.FormatFloat({}, 'f', -1, 64))",
                            qp.name, value_expr
                        )
                    }
                    Type::Bool => {
                        self.client_needs_strconv = true;
                        format!(
                            "{qv}.Set(\"{}\", strconv.FormatBool({}))",
                            qp.name, value_expr
                        )
                    }
                    Type::String => format!("{qv}.Set(\"{}\", {})", qp.name, value_expr),
                    // A `DateTime` goes on the wire as RFC 3339; `fmt.Sprint`
                    // would emit Go's default time layout, which the server's
                    // `time.Parse(time.RFC3339, …)` could not read back. A
                    // dereferenced optional (`*x`) is parenthesized so `.Format`
                    // binds to the value, not `*(x.Format(...))`.
                    Type::DateTime => {
                        let recv = if optional {
                            format!("(*{name})")
                        } else {
                            name.clone()
                        };
                        format!("{qv}.Set(\"{}\", {recv}.Format(time.RFC3339))", qp.name)
                    }
                    _ => format!("{qv}.Set(\"{}\", fmt.Sprint({}))", qp.name, value_expr),
                };
                if optional {
                    self.client_out
                        .push_str(&format!("\tif {name} != nil {{\n\t\t{set_expr}\n\t}}\n"));
                } else {
                    self.client_out.push_str(&format!("\t{set_expr}\n"));
                }
            }
            self.client_out
                .push_str(&format!("\t{url} += \"?\" + {qv}.Encode()\n"));
        }

        // Build request. A JSON request body pulls in `bytes` (for the reader) and
        // `encoding/json` (to marshal); a JSON response pulls in `encoding/json` to
        // decode. A binary download decodes nothing (the caller drains the stream),
        // so it never sets the json flag. Track each so the import block carries
        // only what's used.
        if response_type.is_some() && !ep.response_is_binary {
            self.client_needs_json = true;
        }
        // The error-return arity must track `returns_value` in every branch: a
        // body-carrying endpoint (JSON or multipart) with no response returns
        // bare `error` (so `return nil, err` would not compile), while an
        // all-typeless multi-status block returns the envelope pair despite
        // having no response type.
        let err_ret = if returns_value { "nil, err" } else { "err" };
        if ep.body_is_multipart {
            self.emit_client_multipart_body(ep, http_method, err_ret, &locals);
        } else if ep.body.is_some() {
            self.client_needs_bytes = true;
            self.client_needs_json = true;
            let data = &locals.data;
            let req = &locals.req;
            self.client_out
                .push_str(&format!("\t{data}, err := json.Marshal(body)\n"));
            self.client_out.push_str(&format!(
                "\tif err != nil {{\n\t\treturn {}\n\t}}\n",
                err_ret
            ));
            self.client_out.push_str(&format!(
                "\t{req}, err := http.NewRequest(\"{}\", {url}, bytes.NewReader({data}))\n",
                http_method
            ));
            self.client_out.push_str(&format!(
                "\tif err != nil {{\n\t\treturn {}\n\t}}\n",
                err_ret
            ));
            self.client_out.push_str(&format!(
                "\t{req}.Header.Set(\"Content-Type\", \"application/json\")\n"
            ));
        } else {
            let req = &locals.req;
            self.client_out.push_str(&format!(
                "\t{req}, err := http.NewRequest(\"{}\", {url}, nil)\n",
                http_method
            ));
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
            let set_expr = format!(
                "{}.Header.Set(\"{}\", {})",
                locals.req, h.wire_name, str_expr
            );
            if optional {
                self.client_out
                    .push_str(&format!("\tif {name} != nil {{\n\t\t{set_expr}\n\t}}\n"));
            } else {
                self.client_out.push_str(&format!("\t{set_expr}\n"));
            }
        }

        // Execute
        let err_ret = if returns_value { "nil, " } else { "" };
        let resp = &locals.resp;
        self.client_out.push_str(&format!(
            "\t{resp}, err := {}.Client.Do({})\n",
            locals.recv, locals.req
        ));
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
                "\tif {resp}.StatusCode >= 400 {{\n\t\t_ = {resp}.Body.Close()\n\t\treturn {err_ret}fmt.Errorf(\"HTTP %d\", {resp}.StatusCode)\n\t}}\n",
            ));
        } else {
            self.client_out
                .push_str(&format!("\tdefer {resp}.Body.Close()\n"));
            self.client_out.push_str(&format!(
                "\tif {resp}.StatusCode >= 400 {{\n\t\treturn {err_ret}fmt.Errorf(\"HTTP %d\", {resp}.StatusCode)\n\t}}\n",
            ));
        }

        // Decode response. A binary download returns the raw stream; with response
        // headers, decode the body into the envelope's `Body` field, then read each
        // header from `resp.Header`.
        if is_multi_status {
            // Build the `<Endpoint>Response` envelope: the handler-chosen status
            // comes from the HTTP response; the optional shared body is decoded into
            // `*T` only when the response actually carries one. An all-typeless block
            // (no `T`) has no `Body` field, so just record the status.
            let result = &locals.result;
            self.client_out.push_str(&format!(
                "\t{result} := {}{{Status: {resp}.StatusCode}}\n",
                multi_status_type
            ));
            if let Some(ref rt) = response_type {
                self.client_needs_json = true;
                // A typeless status (e.g. 204) sends no body; only decode when the
                // response reports a non-empty body. `ContentLength == 0` covers the
                // explicit empty case; `io.EOF` from the decoder covers a streamed
                // empty body (chunked / unknown length).
                self.client_needs_io = true;
                self.client_out
                    .push_str(&format!("\tif {resp}.ContentLength != 0 {{\n"));
                self.client_out.push_str(&format!("\t\tvar body {}\n", rt));
                self.client_out.push_str(&format!(
                    "\t\tif err := json.NewDecoder({resp}.Body).Decode(&body); err != nil && err != io.EOF {{\n\t\t\treturn nil, err\n\t\t}} else if err == nil {{\n\t\t\t{result}.Body = &body\n\t\t}}\n",
                ));
                self.client_out.push_str("\t}\n");
            }
            self.client_out
                .push_str(&format!("\treturn &{result}, nil\n"));
        } else if ep.response_is_binary {
            self.client_needs_io = true;
            self.client_out
                .push_str(&format!("\treturn {resp}.Body, nil\n"));
        } else if has_resp_headers {
            self.client_needs_json = true;
            let result = &locals.result;
            self.client_out
                .push_str(&format!("\tvar {result} {}\n", result_type));
            self.client_out.push_str(&format!(
                "\tif err := json.NewDecoder({resp}.Body).Decode(&{result}.Body); err != nil {{\n\t\treturn nil, err\n\t}}\n",
            ));
            for h in &ep.response_headers {
                self.emit_client_response_header_read(h, &locals);
            }
            self.client_out
                .push_str(&format!("\treturn &{result}, nil\n"));
        } else if is_paginated {
            // The whole response body is the page object (`{items, <metadata>}`),
            // decoded into the `<Endpoint>Page` envelope like any struct response.
            self.client_needs_json = true;
            let result = &locals.result;
            self.client_out
                .push_str(&format!("\tvar {result} {}\n", page_type));
            self.client_out.push_str(&format!(
                "\tif err := json.NewDecoder({resp}.Body).Decode(&{result}); err != nil {{\n\t\treturn nil, err\n\t}}\n",
            ));
            self.client_out
                .push_str(&format!("\treturn &{result}, nil\n"));
        } else if let Some(ref rt) = response_type {
            let result = &locals.result;
            self.client_out
                .push_str(&format!("\tvar {result} {}\n", rt));
            self.client_out.push_str(&format!(
                "\tif err := json.NewDecoder({resp}.Body).Decode(&{result}); err != nil {{\n\t\treturn {err_ret}err\n\t}}\n",
            ));
            self.client_out
                .push_str(&format!("\treturn &{result}, nil\n"));
        } else {
            self.client_out.push_str("\treturn nil\n");
        }

        self.client_out.push_str("}\n");
    }

    /// Emits the client-side multipart request build for an upload endpoint: a
    /// `multipart.Writer` over a `bytes.Buffer`, a `CreateFormFile` part per file
    /// field (copying `FileUpload.Content`) and a `WriteField` per scalar field
    /// (stringified like query params / headers). The request `Content-Type` is
    /// the writer's `FormDataContentType()` (carries the boundary). Every error
    /// path returns via `err_ret`, which the caller derives from `returns_value`
    /// — a multipart upload with no `response` is legal (see the
    /// `multipart_upload_no_response` test) and its client method returns bare
    /// `error`, so a hardcoded `nil, err` would not compile there.
    fn emit_client_multipart_body(
        &mut self,
        ep: &EndpointInfo,
        http_method: &str,
        err_ret: &str,
        locals: &ClientLocals,
    ) {
        let Some(ref body) = ep.body else { return };
        self.client_needs_bytes = true;
        self.client_needs_multipart = true;
        self.client_needs_io = true;

        // `buf`/`writer`/`req` and the URL local are uniquified against the
        // parameter names (see [`ClientLocals`]); the per-file `part` lives in
        // its own block and only ever shadows.
        let buf = &locals.buf;
        let writer = &locals.writer;
        self.client_out
            .push_str(&format!("\tvar {buf} bytes.Buffer\n"));
        self.client_out
            .push_str(&format!("\t{writer} := multipart.NewWriter(&{buf})\n"));

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
                    "\t\tpart, err := {writer}.CreateFormFile(\"{wire}\", body.{field}.Filename)\n\t\tif err != nil {{\n\t\t\treturn {err_ret}\n\t\t}}\n\t\tif _, err := io.Copy(part, body.{field}.Content); err != nil {{\n\t\t\treturn {err_ret}\n\t\t}}\n"
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
                    "{i}if err := {writer}.WriteField(\"{wire}\", {str_expr}); err != nil {{\n{i}\treturn {err_ret}\n{i}}}\n"
                );
                if optional {
                    self.client_out
                        .push_str(&format!("\tif body.{field} != nil {{\n{write}\t}}\n"));
                } else {
                    self.client_out.push_str(&write);
                }
            }
        }

        let req = &locals.req;
        self.client_out.push_str(&format!(
            "\tif err := {writer}.Close(); err != nil {{\n\t\treturn {err_ret}\n\t}}\n"
        ));
        self.client_out.push_str(&format!(
            "\t{req}, err := http.NewRequest(\"{http_method}\", {}, &{buf})\n",
            locals.url
        ));
        self.client_out
            .push_str(&format!("\tif err != nil {{\n\t\treturn {err_ret}\n\t}}\n"));
        self.client_out.push_str(&format!(
            "\t{req}.Header.Set(\"Content-Type\", {writer}.FormDataContentType())\n"
        ));
    }

    /// Emits client-side parsing of one response header from `resp.Header` into
    /// the envelope field `result.<PascalName>`. String headers are assigned
    /// directly; numeric/bool are parsed; optional (`Option<T>`) headers parse
    /// into a `*T` left nil when the header is absent — mirroring the server-side
    /// request-header parse and the query-param parse.
    fn emit_client_response_header_read(&mut self, h: &HeaderParamInfo, locals: &ClientLocals) {
        let field = to_pascal_case(&h.name);
        let wire = &h.wire_name;
        // `resp`/`result` are the method's uniquified locals (see
        // [`ClientLocals`]); the per-header `v`/`n`/`b`/`cv` live in their own
        // `if`-init block and only ever shadow.
        let resp = &locals.resp;
        let result = &locals.result;
        let (optional, inner) = query_param_shape(&h.ty);
        let body = if optional {
            match inner {
                Type::Int => {
                    self.client_needs_strconv = true;
                    format!(
                        "if v := {resp}.Header.Get(\"{wire}\"); v != \"\" {{\n\t\tif n, err := strconv.ParseInt(v, 10, 64); err == nil {{\n\t\t\t{result}.{field} = &n\n\t\t}}\n\t}}\n"
                    )
                }
                Type::Float => {
                    self.client_needs_strconv = true;
                    format!(
                        "if v := {resp}.Header.Get(\"{wire}\"); v != \"\" {{\n\t\tif n, err := strconv.ParseFloat(v, 64); err == nil {{\n\t\t\t{result}.{field} = &n\n\t\t}}\n\t}}\n"
                    )
                }
                Type::Bool => {
                    self.client_needs_strconv = true;
                    format!(
                        "if v := {resp}.Header.Get(\"{wire}\"); v != \"\" {{\n\t\tif b, err := strconv.ParseBool(v); err == nil {{\n\t\t\t{result}.{field} = &b\n\t\t}}\n\t}}\n"
                    )
                }
                Type::String => {
                    format!(
                        "if v := {resp}.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t{result}.{field} = &v\n\t}}\n"
                    )
                }
                Type::DateTime => {
                    format!(
                        "if v := {resp}.Header.Get(\"{wire}\"); v != \"\" {{\n\t\tif t, err := time.Parse(time.RFC3339, v); err == nil {{\n\t\t\t{result}.{field} = &t\n\t\t}}\n\t}}\n"
                    )
                }
                other => {
                    let go_type = type_to_go(other);
                    format!(
                        "if v := {resp}.Header.Get(\"{wire}\"); v != \"\" {{\n\t\tcv := {go_type}(v)\n\t\t{result}.{field} = &cv\n\t}}\n"
                    )
                }
            }
        } else {
            match inner {
                Type::Int => {
                    self.client_needs_strconv = true;
                    format!(
                        "if v := {resp}.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t{result}.{field}, _ = strconv.ParseInt(v, 10, 64)\n\t}}\n"
                    )
                }
                Type::Float => {
                    self.client_needs_strconv = true;
                    format!(
                        "if v := {resp}.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t{result}.{field}, _ = strconv.ParseFloat(v, 64)\n\t}}\n"
                    )
                }
                Type::Bool => {
                    self.client_needs_strconv = true;
                    format!(
                        "if v := {resp}.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t{result}.{field}, _ = strconv.ParseBool(v)\n\t}}\n"
                    )
                }
                Type::String => {
                    format!("{result}.{field} = {resp}.Header.Get(\"{wire}\")\n")
                }
                Type::DateTime => {
                    format!(
                        "if v := {resp}.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t{result}.{field}, _ = time.Parse(time.RFC3339, v)\n\t}}\n"
                    )
                }
                other => {
                    let go_type = type_to_go(other);
                    format!("{result}.{field} = {go_type}({resp}.Header.Get(\"{wire}\"))\n")
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
        let return_type = if !ep.response_statuses.is_empty() {
            // A multi-status endpoint's handler supplies the `<Endpoint>Response`
            // envelope (handler-chosen status + optional shared body).
            format!("(*{}, error)", multi_status_type_name(ep))
        } else if ep.response_is_binary {
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
                &format!(
                    "{} {}{}",
                    method_name,
                    doc.to_lowercase(),
                    doc_terminator(doc)
                ),
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

        // Route registration differs by framework; the handler closure body that
        // follows is identical. net/http uses a Go 1.22+ "METHOD /path" pattern on
        // `mux.HandleFunc`; chi takes the method and path separately via
        // `router.MethodFunc` (its path syntax `{id}` matches Phoenix's verbatim).
        match self.framework {
            GoServerFramework::NetHttp => self.server_out.push_str(&format!(
                "\tmux.HandleFunc(\"{} {}\", func(w http.ResponseWriter, r *http.Request) {{\n",
                method, ep.path
            )),
            GoServerFramework::Chi => self.server_out.push_str(&format!(
                "\trouter.MethodFunc(\"{}\", \"{}\", func(w http.ResponseWriter, r *http.Request) {{\n",
                method, ep.path
            )),
        }

        // Parse path params. net/http reads them off `r.PathValue`; chi off
        // `chi.URLParam(r, ...)`.
        for pp in &ep.path_params {
            let accessor = match self.framework {
                GoServerFramework::NetHttp => format!("r.PathValue(\"{pp}\")"),
                GoServerFramework::Chi => format!("chi.URLParam(r, \"{pp}\")"),
            };
            self.server_out.push_str(&format!(
                "\t\t{camel} := {accessor}\n",
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
            // `Validate()` exists (and is worth calling) when the body has a
            // constrained field OR a `Uuid` field (whose format it checks). This
            // condition must track `emit_body_validate_method`'s emit gate so we
            // never call a method that wasn't generated, nor skip one that was.
            let has_uuid_field = body.fields.iter().any(|f| is_uuid_field(&f.ty));
            if body.fields.iter().any(|f| f.constraint.is_some()) || has_uuid_field {
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

        // The handler-result local (`result`) shares the closure's scope with the
        // path/query/header input locals, which carry the parameter names verbatim
        // (`to_camel` is the identity). Derive it so `result, err := h.X(...)` can't
        // redeclare a same-named input — a `result` query param would otherwise turn
        // `result, err := …` into an assignment to the `*string` query local
        // ("cannot use … as *string"). Mirrors the client-side [`ClientLocals`].
        //
        // NOT covered here: the closure's *fixed* identifiers — `w`
        // (`http.ResponseWriter`), `r` (`*http.Request`), and the captured `h`
        // (`Handlers`) / `mux` from `NewRouter`. A parameter named `w`/`r`/`h`/`mux`
        // shadows or redeclares one of those (`w := r.PathValue("w")` beside the
        // `w` writer is "no new variables on left side of :="). Uniquifying them
        // would mean threading renamed `w`/`r`/`h` through every server emit site
        // (~6 helpers, ~40 sites); deferred as a separate edge until a real schema
        // needs it. The client's only fixed identifier — the receiver `c` — IS
        // uniquified (see [`ClientLocals`]) because it costs three sites.
        //
        // Uniquified against the same parameter-identifier set the client uses
        // (see [`endpoint_param_idents`]).
        let taken = endpoint_param_idents(ep);
        let result = pick_free_local("result", &taken);

        // Error mapping uses `strings.Contains`; encoding a response uses
        // `encoding/json`. Record both so the import block stays minimal.
        if !ep.errors.is_empty() {
            self.server_needs_strings = true;
        }
        if !ep.response_statuses.is_empty() {
            // Multi-status: the handler returns the `<Endpoint>Response` envelope
            // carrying the chosen status + optional shared body. The server writes
            // that status (not a hardcoded 200/204) and JSON-encodes the body only
            // when present (a typeless status — or an all-typeless block — leaves
            // `result.Body` nil and writes a bodyless response).
            self.server_out.push_str(&format!(
                "\t\t{result}, err := h.{}({})\n",
                handler_name, args_str
            ));
            self.server_out.push_str("\t\tif err != nil {\n");
            self.emit_server_error_mapping(ep);
            // A `(nil, nil)` return is a handler bug Go's type system can't
            // prevent; without this guard `result.Status` below panics the
            // route. Same guard as the binary and response-header paths.
            self.server_out.push_str(&format!(
                "\t\tif {result} == nil {{\n\t\t\thttp.Error(w, \"handler returned nil result\", http.StatusInternalServerError)\n\t\t\treturn\n\t\t}}\n",
            ));
            // Validate the handler-chosen envelope against the DECLARED contract
            // before writing it — all three mismatches are handler bugs, reported
            // as a 500 instead of written to the wire (mirrors the TS/Python
            // servers):
            // - an undeclared status (a zero-value envelope would make
            //   `WriteHeader(0)` panic, and a 4xx smuggled through the success
            //   envelope would bypass `error { }`);
            // - a body paired with a typeless status (`net/http` only suppresses
            //   body writes on 1xx/204/304 — on e.g. a typeless 202 or 205 the
            //   body WOULD hit the wire, and the content-guarded client would
            //   parse it, silently violating the contract);
            // - a nil body paired with a typed status (the contract — and the
            //   OpenAPI spec — promise a body there).
            // One switch covers all three: the typed arm requires a body, the
            // typeless arm forbids one, default is undeclared. An all-typeless
            // block has no `Body` field, so its single arm is bare membership.
            let typed: Vec<String> = ep
                .response_statuses
                .iter()
                .filter(|rs| rs.ty.is_some())
                .map(|rs| rs.status.to_string())
                .collect();
            let typeless: Vec<String> = ep
                .response_statuses
                .iter()
                .filter(|rs| rs.ty.is_none())
                .map(|rs| rs.status.to_string())
                .collect();
            self.server_out
                .push_str(&format!("\t\tswitch {result}.Status {{\n"));
            if ep.response.is_some() {
                self.server_out.push_str(&format!(
                    "\t\tcase {}:\n\t\t\tif {result}.Body == nil {{\n\t\t\t\thttp.Error(w, \"handler returned no body for a typed status\", http.StatusInternalServerError)\n\t\t\t\treturn\n\t\t\t}}\n",
                    typed.join(", ")
                ));
                if !typeless.is_empty() {
                    self.server_out.push_str(&format!(
                        "\t\tcase {}:\n\t\t\tif {result}.Body != nil {{\n\t\t\t\thttp.Error(w, \"handler returned a body for a bodyless status\", http.StatusInternalServerError)\n\t\t\t\treturn\n\t\t\t}}\n",
                        typeless.join(", ")
                    ));
                }
            } else {
                self.server_out
                    .push_str(&format!("\t\tcase {}:\n", typeless.join(", ")));
            }
            self.server_out.push_str(
                "\t\tdefault:\n\t\t\thttp.Error(w, \"handler returned undeclared status\", http.StatusInternalServerError)\n\t\t\treturn\n\t\t}\n",
            );
            if ep.response.is_some() {
                // The block declares at least one typed status, so the envelope has
                // a `Body *T`. Set the content type, write the status, then encode
                // the body when the handler supplied one.
                self.server_needs_json = true;
                self.server_out.push_str(&format!(
                    "\t\tif {result}.Body != nil {{\n\t\t\tw.Header().Set(\"Content-Type\", \"application/json\")\n\t\t\tw.WriteHeader({result}.Status)\n\t\t\tjson.NewEncoder(w).Encode({result}.Body)\n\t\t}} else {{\n\t\t\tw.WriteHeader({result}.Status)\n\t\t}}\n",
                ));
            } else {
                // All-typeless block: no `Body` field, just write the status.
                self.server_out
                    .push_str(&format!("\t\tw.WriteHeader({result}.Status)\n"));
            }
        } else if ep.response_is_binary {
            // Binary download: the handler returns an `io.Reader`; stream it to
            // the wire as `application/octet-stream` (no JSON encoding).
            self.server_needs_io = true;
            self.server_out.push_str(&format!(
                "\t\t{result}, err := h.{}({})\n",
                handler_name, args_str
            ));
            self.server_out.push_str("\t\tif err != nil {\n");
            self.emit_server_error_mapping(ep);
            // A `(nil, nil)` return is a handler bug Go's type system can't
            // prevent; `io.Copy` from a nil reader would panic the route.
            self.server_out.push_str(&format!(
                "\t\tif {result} == nil {{\n\t\t\thttp.Error(w, \"handler returned nil result\", http.StatusInternalServerError)\n\t\t\treturn\n\t\t}}\n",
            ));
            self.server_out
                .push_str("\t\tw.Header().Set(\"Content-Type\", \"application/octet-stream\")\n");
            // The status line and headers are already committed, so a streaming
            // failure here is unrecoverable — discard the error explicitly.
            self.server_out
                .push_str(&format!("\t\t_, _ = io.Copy(w, {result})\n"));
        } else if ep.response.is_some() {
            self.server_needs_json = true;
            self.server_out.push_str(&format!(
                "\t\t{result}, err := h.{}({})\n",
                handler_name, args_str
            ));
            self.server_out.push_str("\t\tif err != nil {\n");
            self.emit_server_error_mapping(ep);
            // A `(nil, nil)` return is a handler bug Go's type system can't
            // prevent; the header reads below deref the envelope pointer and
            // would panic the route. The plain-response case needs no guard —
            // `Encode` renders a nil pointer as `null` without panicking.
            if has_resp_headers {
                self.server_out.push_str(&format!(
                    "\t\tif {result} == nil {{\n\t\t\thttp.Error(w, \"handler returned nil result\", http.StatusInternalServerError)\n\t\t\treturn\n\t\t}}\n",
                ));
            }
            // Response headers: set each on `w.Header()` (stringified, optional
            // guarded) before the body is encoded. With an envelope the body
            // lives in `result.Body`; otherwise `result` is the body itself.
            for h in &ep.response_headers {
                self.emit_response_header_set(h, &result);
            }
            self.server_out
                .push_str("\t\tw.Header().Set(\"Content-Type\", \"application/json\")\n");
            let encode_target = if has_resp_headers {
                format!("{result}.Body")
            } else {
                result.clone()
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
            self.emit_server_error_mapping(ep);
            self.server_out
                .push_str("\t\tw.WriteHeader(http.StatusNoContent)\n");
        }

        self.server_out.push_str("\t})\n");
    }

    /// Emits the interior of a server route's `if err != nil { ... }` block,
    /// shared by every route shape (multi-status, binary, plain response, no
    /// response): one `strings.Contains` check per declared `error { }`
    /// variant answering its mapped status, then the 500 fallback. The caller
    /// has already opened the `if`; this closes it.
    fn emit_server_error_mapping(&mut self, ep: &EndpointInfo) {
        for (name, code) in &ep.errors {
            self.server_out.push_str(&format!(
                "\t\t\tif strings.Contains(err.Error(), \"{name}\") {{\n\t\t\t\thttp.Error(w, \"{name}\", {code})\n\t\t\t\treturn\n\t\t\t}}\n"
            ));
        }
        self.server_out
            .push_str("\t\t\thttp.Error(w, err.Error(), http.StatusInternalServerError)\n");
        self.server_out.push_str("\t\t\treturn\n\t\t}\n");
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
                Type::DateTime => {
                    format!(
                        "var {camel} *time.Time\n\t\tif v := r.URL.Query().Get(\"{name}\"); v != \"\" {{\n\t\t\tif t, err := time.Parse(time.RFC3339, v); err == nil {{\n\t\t\t\t{camel} = &t\n\t\t\t}}\n\t\t}}\n"
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
                // This is the required-param seed (overwritten when the query
                // carries a value). The other scalars seed from `default(...)`,
                // but a `DateTime` can't carry a default (sema accepts only
                // Int/Float/Bool/String defaults), so it seeds from the
                // `time.Time{}` zero instead.
                Type::DateTime => {
                    format!(
                        "{camel} := time.Time{{}}\n\t\tif v := r.URL.Query().Get(\"{name}\"); v != \"\" {{\n\t\t\t{camel}, _ = time.Parse(time.RFC3339, v)\n\t\t}}\n"
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
                Type::DateTime => {
                    format!(
                        "var {camel} *time.Time\n\t\tif v := r.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t\tif t, err := time.Parse(time.RFC3339, v); err == nil {{\n\t\t\t\t{camel} = &t\n\t\t\t}}\n\t\t}}\n"
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
                // Required-header seed (overwritten when present). A `DateTime`
                // can't carry a default (sema accepts only Int/Float/Bool/String
                // defaults), so it seeds from the `time.Time{}` zero rather than
                // from `default(...)` like the other scalars.
                Type::DateTime => {
                    format!(
                        "{camel} := time.Time{{}}\n\t\tif v := r.Header.Get(\"{wire}\"); v != \"\" {{\n\t\t\t{camel}, _ = time.Parse(time.RFC3339, v)\n\t\t}}\n"
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
    fn emit_response_header_set(&mut self, h: &HeaderParamInfo, result: &str) {
        let field = to_pascal_case(&h.name);
        let wire = &h.wire_name;
        let (optional, inner) = query_param_shape(&h.ty);
        let value_expr = if optional {
            format!("*{result}.{field}")
        } else {
            format!("{result}.{field}")
        };
        let str_expr = header_string_expr(inner, &value_expr, &mut self.server_needs_strconv);
        let set_expr = format!("w.Header().Set(\"{wire}\", {str_expr})");
        if optional {
            self.server_out.push_str(&format!(
                "\t\tif {result}.{field} != nil {{\n\t\t\t{set_expr}\n\t\t}}\n"
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
        // Strip each line's own leading whitespace before applying the prefix. A
        // multi-line doc comment carries the visual indentation of its source
        // `/** … *  continuation */` into `body` (the ` * ` leader is stripped but
        // the author's alignment spaces survive). Emitted after the `// ` prefix
        // that indent becomes `//  continuation` (two+ spaces), which gofmt 1.19+
        // reads as an indented *code block* and rewrites — inserting a blank `//`
        // and re-tabbing the body, so `gofmt -l` flags the file. The prefix already
        // carries any intended Go-level indentation (e.g. the leading `\t` for an
        // interface-method doc); the comment text itself must sit flush against it.
        // Tradeoff: this also flattens any *authored* indentation inside the doc
        // text (a nested list, an aligned table) — gofmt would reject preserving
        // it anyway, so doc prose can't rely on leading-whitespace layout.
        let line = line.trim_start();
        out.push_str(format!("{prefix}{line}").trim_end());
        out.push('\n');
    }
    out
}

/// Returns the sentence-ending period to append after a doc comment, or `""` when
/// the comment already ends in `.`/`!`/`?`. The Go doc renderers add a period so a
/// bare phrase reads as a sentence; without this guard a doc that already ends in
/// punctuation would render a doubled `..` (sloppy, and flagged by doc-style
/// linters). Checks the trimmed tail so trailing whitespace/newlines don't hide
/// the punctuation.
fn doc_terminator(doc: &str) -> &'static str {
    match doc.trim_end().chars().last() {
        Some('.') | Some('!') | Some('?') => "",
        _ => ".",
    }
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
    needs_regexp: &'a mut bool,
}

/// Renders a `func (s {type_name}) Validate() error` whose body checks every
/// constrained field, then each `Uuid`/`Option<Uuid>` field's RFC 4122 format,
/// then `return nil`. `fields` lists each constrained field as
/// `(name, constraint, is_ptr)`: an `is_ptr` field is rendered as a Go pointer
/// (either `partial`-applied or already `Option<T>`, both `*T`), so its check is
/// nil-guarded and `self` is dereferenced inside the constraint expression; a
/// plain field is checked directly. `uuid_fields` lists each `(name, is_ptr)`
/// uuid field, format-checked against `uuidRe` (nil-guarded when `is_ptr`).
/// Shared by the source-struct validator
/// ([`GoGenerator::emit_validate_method`]) and the derived-body validator
/// ([`GoGenerator::emit_body_validate_method`]) so the two can never drift.
/// Callers must invoke this only when `fields` OR `uuid_fields` is non-empty — an
/// empty `Validate()` would needlessly pull in the `fmt` import.
fn render_validate_fn(
    sink: ValidateSink<'_>,
    type_name: &str,
    fields: &[(&str, &Expr, bool)],
    uuid_fields: &[(&str, bool)],
) {
    let ValidateSink {
        out,
        needs_fmt,
        needs_strings,
        needs_regexp,
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

    for (name, is_ptr) in uuid_fields {
        *needs_regexp = true;
        let field = to_pascal_case(name);
        if *is_ptr {
            out.push_str(&format!(
                "\tif s.{field} != nil && !uuidRe.MatchString(*s.{field}) {{\n\t\treturn fmt.Errorf(\"{name}: invalid uuid\")\n\t}}\n",
            ));
        } else {
            out.push_str(&format!(
                "\tif !uuidRe.MatchString(s.{field}) {{\n\t\treturn fmt.Errorf(\"{name}: invalid uuid\")\n\t}}\n",
            ));
        }
    }

    out.push_str("\treturn nil\n}\n\n");
}

/// Whether `ty` is a directly-validatable `Uuid` field for Go's `Validate()`: a
/// bare `Uuid` (`string`) or an `Option<Uuid>` (`*string`), returning the
/// `is_ptr` flag. `List<Uuid>`/`Map<_, Uuid>` element validation is not emitted
/// (Go is the documented weak link for uuid validation).
fn uuid_field_shape(ty: &Type) -> Option<bool> {
    match ty {
        Type::Uuid => Some(false),
        Type::Generic(name, args)
            if name == "Option" && args.len() == 1 && matches!(args[0], Type::Uuid) =>
        {
            Some(true)
        }
        _ => None,
    }
}

/// Whether `ty` is a `Uuid`/`Option<Uuid>` field that the generated `Validate()`
/// format-checks — the single source of truth for that rule (derived from
/// [`uuid_field_shape`]). Used where only the presence matters, not the pointer
/// shape: the body validator's field filter (whose pointer-ness comes from
/// `derived_field_go_type` instead) and the server's "should I call `Validate()`"
/// gate, which must agree with the body validator's emit gate.
fn is_uuid_field(ty: &Type) -> bool {
    uuid_field_shape(ty).is_some()
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
        // A `DateTime` is an RFC 3339 instant. Go's `time.Time` round-trips
        // exactly that via its JSON marshalling (RFC 3339), so it needs the
        // `time` import wherever it appears. See `docs/design-decisions.md`
        // (DateTime & UUID scalar types).
        Type::DateTime => "time.Time".to_string(),
        // A `Uuid` is a hyphenated RFC 4122 string. Go has no stdlib UUID type and
        // we add no dependency, so it maps to `string`; format is checked in the
        // generated `Validate()` (see `emit_validate_method`). It needs no special
        // (de)serialization — it IS a string on the wire and in memory.
        Type::Uuid => "string".to_string(),
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

/// Whether a rendered Go file body references the `time` package, and thus needs
/// the `"time"` import. Every `DateTime` use lands on one of two tokens:
/// `time.Time` (a field/param/return type) or `time.RFC3339` (the format/parse
/// layout, e.g. `t.Format(time.RFC3339)` / `time.Parse(time.RFC3339, v)` — the
/// latter has no `time.Time` literal). Matching these exact tokens (rather than a
/// bare `time.`) avoids a false positive on, say, a URL path like `/uptime.html`
/// that would otherwise force an unused import and fail `go build` — and avoids
/// the false *positive* a bare `time.` would also hit on prose like a doc comment
/// ending in "…at request time.".
///
/// RESIDUAL FALSE POSITIVE: a user-authored doc comment whose text literally
/// contains `time.Time` or `time.RFC3339` (these tokens are rendered into the
/// body verbatim) would still force an unused `time` import and break `go build`.
/// That is a deliberately accepted near-impossibility — no realistic prose hits
/// the dotted-identifier tokens, only the bare word "time" — and the only
/// alternative (parsing the rendered Go to skip comment/string spans) is far more
/// machinery than the risk warrants.
///
/// INVARIANT: this is the single gate for the `time` import. Any future codegen
/// path that emits another `time`-package reference (`time.Duration`,
/// `time.Now()`, …) MUST add that token here, or the import will silently go
/// missing and the output won't compile. Today the generator emits no other
/// `time.*` token, so these two are exhaustive.
fn go_body_uses_time(body: &str) -> bool {
    body.contains("time.Time") || body.contains("time.RFC3339")
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

/// The generated multi-status-envelope type name for an endpoint that declares a
/// `response { }` block (`response_statuses` non-empty): `<PascalEndpoint>Response`
/// (e.g. `createUser` → `CreateUserResponse`). Used for the types.go struct, the
/// handler return, the client return, and the server status/body wiring. Distinct
/// from the response-headers `<Endpoint>Result` and pagination `<Endpoint>Page`
/// envelopes (all three are mutually exclusive per sema).
fn multi_status_type_name(ep: &EndpointInfo) -> String {
    format!("{}Response", to_pascal_case(&ep.name))
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
        // A `DateTime` (`time.Time`) goes on the wire as an RFC 3339 string. A
        // dereferenced optional (`*x`) must be parenthesized: `*x.Format(...)`
        // parses as `*(x.Format(...))` and would fail to compile.
        Type::DateTime => {
            let recv = if value_expr.starts_with('*') {
                format!("({value_expr})")
            } else {
                value_expr.to_string()
            };
            format!("{recv}.Format(time.RFC3339)")
        }
        _ => format!("string({value_expr})"),
    }
}

/// Converts a camelCase identifier to PascalCase (Go exported name).
///
/// Phoenix identifiers are already camelCase, so "PascalCase" reduces to
/// capitalizing the first character — delegate to the shared `capitalize` so
/// generated type names (notably the `<Endpoint>{Result,Page,Response}`
/// envelopes) stay in lockstep with sema's envelope-collision check. If Go
/// ever needs real word-splitting here, the envelope names must keep using
/// `capitalize` or that check goes blind for Go.
fn to_pascal_case(s: &str) -> String {
    capitalize(s)
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
#[path = "go_tests.rs"]
mod tests;
