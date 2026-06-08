//! Semantic validation for endpoint declarations.
//!
//! Validates that endpoint types, field references, and HTTP semantics are
//! correct.  Produces [`EndpointInfo`] with all types resolved.

use crate::checker::{
    Checker, DefaultValue, DerivedField, EndpointInfo, HeaderParamInfo, QueryParamInfo,
    ResolvedDerivedType, header_wire_name,
};
use crate::types::Type;
use phoenix_parser::api_version::normalize_api_version;
use phoenix_parser::ast::{
    DerivedType, EndpointDecl, Expr, HeaderParam, HttpMethod, Literal, LiteralKind, TypeExpr,
    TypeModifier,
};

impl Checker {
    /// Type-checks an endpoint declaration and, if valid, adds a resolved
    /// [`EndpointInfo`] to `self.endpoints`.
    ///
    /// Validates:
    /// - Endpoint name is unique (no two endpoints share the same name)
    /// - Response type exists (must be a known struct, enum, or built-in)
    /// - Body base type exists and is a struct (required for omit/pick/partial)
    /// - All field names in `omit`/`pick`/`partial` modifiers exist on the base struct
    /// - `body` is not used on GET or DELETE endpoints
    /// - Error variant names are unique within the endpoint
    /// - Error status codes are in the 400–599 range
    /// - Query parameter types resolve successfully
    /// - Query parameter default values match their declared types
    /// - Request-header default values match their declared types
    /// - Response headers do not declare a default value
    /// - Request-header local names do not collide with path/query params or each other
    /// - Response-header local names do not collide with each other or the `body` field
    /// - No two headers in the same direction resolve to the same wire name
    pub(crate) fn check_endpoint(&mut self, ep: &EndpointDecl) {
        // Check for duplicate endpoint names. `insert` returns `false` when the
        // name was already present, giving us the duplicate flag and recording
        // the name for later endpoints in a single O(1) operation.
        let is_duplicate_name = !self.endpoint_names.insert(ep.name.clone());
        if is_duplicate_name {
            self.error(format!("duplicate endpoint name `{}`", ep.name), ep.span);
        }

        // Apply the API-version prefix (from an `api version "v1" { ... }` block)
        // to the path, so everything downstream — path-param extraction, the
        // resolved `EndpointInfo.path` consumed by every generator's URL/route
        // building, and the OpenAPI paths key — sees the final, prefixed path.
        // The version string is used literally as a leading path segment, with
        // exactly one `/` at the seam regardless of how the author wrote it
        // (`"v1"` and `"/v1"` are equivalent).
        let resolved_path = apply_version_prefix(ep.api_version.as_deref(), &ep.path);

        self.check_route_collision(ep, &resolved_path, is_duplicate_name);

        // Extract path parameters from URL pattern: "/api/users/{id}" -> ["id"]
        let path_params = extract_path_params(&resolved_path);

        // Validate: body not allowed on GET or DELETE
        if ep.body.is_some() && matches!(ep.method, HttpMethod::Get | HttpMethod::Delete) {
            self.error(
                format!(
                    "endpoint `{}`: `body` is not allowed on {:?} endpoints",
                    ep.name, ep.method
                ),
                ep.span,
            );
        }

        // Resolve response type. A response struct may be a file-bearing
        // (body-only) struct — that is a binary download — so the response
        // position permits it (`file_bearing_struct_allowed`). Rule 3 then
        // requires such a response struct to hold EXACTLY one field, of type
        // `File`, and nothing else (a binary stream cannot be multiplexed with
        // JSON fields). See `docs/design-decisions.md` (multipart).
        let response = ep.response.as_ref().map(|te| {
            let prev = self.file_bearing_struct_allowed;
            self.file_bearing_struct_allowed = true;
            let ty = self.resolve_type_expr(te);
            self.file_bearing_struct_allowed = prev;
            if ty == Type::Error {
                self.error(
                    format!("endpoint `{}`: unknown response type", ep.name),
                    ep.span,
                );
            }
            ty
        });

        // Rule 3 (response/download): a file-bearing response struct must be a
        // pure binary download — exactly one field, of type `File`.
        let mut response_is_binary = false;
        if let Some(Type::Named(name)) = &response
            && let Some(si) = self.lookup_struct(name)
            && si.is_file_bearing
        {
            let only_one_file =
                si.fields.len() == 1 && si.fields.first().is_some_and(|f| f.ty == Type::File);
            if only_one_file {
                response_is_binary = true;
            } else {
                self.error(
                    format!(
                        "endpoint `{}`: a `File`-bearing response struct (`{}`) must contain exactly one field of type `File` and nothing else (binary download)",
                        ep.name, name
                    ),
                    ep.span,
                );
            }
        }

        // Resolve body type with modifiers. The request-body path looks the
        // base struct up by name (it does not call `resolve_type_expr`), so a
        // file-bearing body struct is accepted here without the
        // `file_bearing_struct_allowed` gate. A request body may mix `File`
        // fields with scalars (multipart/form-data); see the field-type rule
        // below for what a multipart body's *non-file* fields may be.
        let body = ep
            .body
            .as_ref()
            .and_then(|dt| self.resolve_derived_type(&ep.name, dt));

        // The request body is multipart iff, after omit/pick/partial, any
        // surviving field carries a `File`. Type-determined, not a heuristic —
        // a `File` cannot be JSON-serialized.
        let body_is_multipart = body
            .as_ref()
            .is_some_and(|b| b.fields.iter().any(|f| Self::field_type_is_file(&f.ty)));

        // Rule (multipart bodies): a `multipart/form-data` part is text on the
        // wire, so every *non-file* field of a multipart body must be a scalar
        // (`Int`/`Float`/`Bool`/`String`) or `Option<scalar>`. A `List`, `Map`,
        // nested struct, or enum field cannot be form-encoded and would emit
        // broken client/server code (httpx `data=` / FastAPI `Form(...)` /
        // `FormData`), so reject it here rather than mis-generate. See
        // `docs/design-decisions.md` (multipart section).
        if body_is_multipart && let Some(b) = body.as_ref() {
            for f in &b.fields {
                if !Self::is_multipart_field_type(&f.ty) {
                    self.error(
                        format!(
                            "endpoint `{}`: field `{}` of a multipart (file-upload) body must be a `File`, a scalar (`Int`/`Float`/`Bool`/`String`), or an `Option` of one of those — `{}` cannot be sent as a form field",
                            ep.name, f.name, f.ty
                        ),
                        f.span,
                    );
                }
            }
        }

        // Validate error variants
        let mut seen_errors = std::collections::HashSet::new();
        let mut errors = Vec::new();
        for ev in &ep.errors {
            if !seen_errors.insert(&ev.name) {
                self.error(
                    format!(
                        "endpoint `{}`: duplicate error variant `{}`",
                        ep.name, ev.name
                    ),
                    ev.span,
                );
            }
            if !(400..=599).contains(&ev.status_code) {
                self.error(
                    format!(
                        "endpoint `{}`: status code {} is not a client/server error (expected 400–599)",
                        ep.name, ev.status_code
                    ),
                    ev.span,
                );
            }
            errors.push((ev.name.clone(), ev.status_code));
        }

        // Resolve query parameters
        let query_params: Vec<QueryParamInfo> = ep
            .query_params
            .iter()
            .map(|qp| {
                let ty = self.resolve_type_expr(&qp.type_annotation);
                let default_value = qp.default_value.as_ref().and_then(extract_default_value);

                // Validate default value type matches declared type
                if let Some(ref default) = default_value {
                    let default_matches = matches!(
                        (default, &ty),
                        (DefaultValue::Int(_), Type::Int)
                            | (DefaultValue::Float(_), Type::Float)
                            | (DefaultValue::String(_), Type::String)
                            | (DefaultValue::Bool(_), Type::Bool)
                    );
                    if !default_matches && !ty.is_error() {
                        self.error(
                            format!(
                                "endpoint `{}`: default value for query param `{}` does not match type `{}`",
                                ep.name, qp.name, ty
                            ),
                            qp.span,
                        );
                    }
                }

                QueryParamInfo {
                    name: qp.name.clone(),
                    ty,
                    has_default: qp.default_value.is_some(),
                    default_value,
                }
            })
            .collect();

        // Resolve request and response headers. The per-header rules differ
        // slightly: request headers may carry a default; response headers may
        // not (they are set by the handler, never received — see `resolve_header`).
        let headers: Vec<HeaderParamInfo> = ep
            .headers
            .iter()
            .map(|h| self.resolve_header(ep, h, false))
            .collect();
        let response_headers: Vec<HeaderParamInfo> = ep
            .response_headers
            .iter()
            .map(|h| self.resolve_header(ep, h, true))
            .collect();

        // A binary download streams raw bytes as its whole response body; the
        // generated targets return a stream/blob/`Response` for it, with no
        // `<Endpoint>Result` envelope to carry typed response-header fields.
        // Combining the two has no coherent generated shape (and produces
        // contradictory codegen), so reject it here rather than emit broken code.
        if response_is_binary && let Some(first) = ep.response_headers.first() {
            self.error(
                format!(
                    "endpoint `{}`: a binary-download response (a single-`File` response struct) cannot also declare response headers — the response body is the raw file stream, with no envelope to carry header fields",
                    ep.name
                ),
                first.span,
            );
        }

        // Request headers share the generated parameter scope with path and
        // query params, so a duplicate local name would emit two parameters of
        // the same name (a compile error in the generated Go/TS/Python). Check
        // each request header against the path/query names and the other headers.
        let mut input_names: std::collections::HashSet<&str> =
            path_params.iter().map(String::as_str).collect();
        for qp in &ep.query_params {
            input_names.insert(qp.name.as_str());
        }
        for h in &ep.headers {
            if !input_names.insert(h.name.as_str()) {
                self.error(
                    format!(
                        "endpoint `{}`: request header `{}` collides with another endpoint input (path param, query param, or header) of the same name",
                        ep.name, h.name
                    ),
                    h.span,
                );
            }
        }

        // Response header local names become fields on the generated
        // `<Endpoint>Result` envelope (alongside the envelope's `body` field), so
        // they must be distinct from each other. (They cannot collide with `body`
        // itself: `body` is a reserved keyword and so cannot be a header name.)
        let mut response_field_names: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        for h in &ep.response_headers {
            if !response_field_names.insert(h.name.as_str()) {
                self.error(
                    format!(
                        "endpoint `{}`: response header `{}` is declared more than once",
                        ep.name, h.name
                    ),
                    h.span,
                );
            }
        }

        // Two headers that resolve to the same on-the-wire name (auto-derived or
        // explicit) would silently overwrite each other on send and read the same
        // value on parse. HTTP header names are case-insensitive, so collisions
        // are checked case-insensitively. Request and response headers are
        // different directions and share no namespace, so each is checked alone.
        self.check_header_wire_name_uniqueness(&ep.name, &headers, &ep.headers, "request");
        self.check_header_wire_name_uniqueness(
            &ep.name,
            &response_headers,
            &ep.response_headers,
            "response",
        );

        self.endpoints.push(EndpointInfo {
            name: ep.name.clone(),
            method: ep.method,
            path: resolved_path,
            path_params,
            query_params,
            headers,
            body,
            response,
            response_headers,
            errors,
            doc_comment: ep.doc_comment.clone(),
            body_is_multipart,
            response_is_binary,
        });
    }

    /// Reports a route collision: two endpoints whose method and resolved path
    /// *pattern* (path-param names ignored, see [`route_signature`]) coincide
    /// match the same incoming requests and would conflict at the router. This
    /// catches accidental duplicates among top-level endpoints as well as a
    /// versioned endpoint colliding with a hand-written `/vX/...` path.
    ///
    /// Distinct names can still resolve to the same route, so this is checked
    /// separately from the duplicate-name check. The first endpoint to claim a
    /// signature owns it (recorded in `route_signatures`); later collisions are
    /// reported against it. When the name itself is already a duplicate
    /// (`is_duplicate_name`), the route diagnostic is suppressed — that is one
    /// mistake, and reporting it twice is just noise.
    fn check_route_collision(
        &mut self,
        ep: &EndpointDecl,
        resolved_path: &str,
        is_duplicate_name: bool,
    ) {
        let route = route_signature(ep.method, resolved_path);
        match self.route_signatures.get(&route) {
            Some(other) => {
                if !is_duplicate_name {
                    self.error(
                        format!(
                            "endpoint `{}`: route `{} {}` conflicts with endpoint `{}`",
                            ep.name,
                            ep.method.as_upper_str(),
                            resolved_path,
                            other
                        ),
                        ep.span,
                    );
                }
            }
            None => {
                self.route_signatures.insert(route, ep.name.clone());
            }
        }
    }

    /// Resolves a single endpoint header into a [`HeaderParamInfo`].
    ///
    /// Computes the on-the-wire HTTP header name: the explicit `as "..."`
    /// override when present, otherwise the Title-Case-Kebab auto-transform of
    /// the identifier (see [`header_wire_name`]).
    ///
    /// `is_response` selects the default-value rule. A **request** header may
    /// carry a default (applied when the request omits it); its value is
    /// type-checked against the declared type, mirroring query-param checking. A
    /// **response** header is *set by the handler*, never received, so a default
    /// is meaningless — it is rejected with a diagnostic and dropped, rather than
    /// silently ignored.
    fn resolve_header(
        &mut self,
        ep: &EndpointDecl,
        h: &HeaderParam,
        is_response: bool,
    ) -> HeaderParamInfo {
        let ty = self.resolve_type_expr(&h.type_annotation);

        let wire_name = h
            .wire_name
            .clone()
            .unwrap_or_else(|| header_wire_name(&h.name));

        if is_response {
            if h.default_value.is_some() {
                self.error(
                    format!(
                        "endpoint `{}`: response header `{}` cannot have a default value (response headers are set by the handler, never received)",
                        ep.name, h.name
                    ),
                    h.span,
                );
            }
            return HeaderParamInfo {
                name: h.name.clone(),
                wire_name,
                ty,
                has_default: false,
                default_value: None,
            };
        }

        let default_value = h.default_value.as_ref().and_then(extract_default_value);

        if let Some(ref default) = default_value {
            let default_matches = matches!(
                (default, &ty),
                (DefaultValue::Int(_), Type::Int)
                    | (DefaultValue::Float(_), Type::Float)
                    | (DefaultValue::String(_), Type::String)
                    | (DefaultValue::Bool(_), Type::Bool)
            );
            if !default_matches && !ty.is_error() {
                self.error(
                    format!(
                        "endpoint `{}`: default value for header `{}` does not match type `{}`",
                        ep.name, h.name, ty
                    ),
                    h.span,
                );
            }
        }

        HeaderParamInfo {
            name: h.name.clone(),
            wire_name,
            ty,
            has_default: h.default_value.is_some(),
            default_value,
        }
    }

    /// Reports a diagnostic when two headers in the same direction resolve to the
    /// same on-the-wire name. `resolved` and `ast` are the parallel resolved/AST
    /// header lists (same order, 1:1); the AST entry supplies the span. HTTP
    /// header names are case-insensitive, so the comparison is too. `direction`
    /// is `"request"` or `"response"` for the message.
    fn check_header_wire_name_uniqueness(
        &mut self,
        ep_name: &str,
        resolved: &[HeaderParamInfo],
        ast: &[HeaderParam],
        direction: &str,
    ) {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (info, h) in resolved.iter().zip(ast.iter()) {
            if !seen.insert(info.wire_name.to_ascii_lowercase()) {
                self.error(
                    format!(
                        "endpoint `{}`: {} header wire name `{}` is declared by more than one header",
                        ep_name, direction, info.wire_name
                    ),
                    h.span,
                );
            }
        }
    }

    /// Resolves a derived type (base type + omit/pick/partial modifiers) into
    /// a flat list of fields with their types and optionality.
    fn resolve_derived_type(
        &mut self,
        endpoint_name: &str,
        dt: &DerivedType,
    ) -> Option<ResolvedDerivedType> {
        // Resolve the base type — must be a struct
        let base_name = match &dt.base_type {
            TypeExpr::Named(n) => n.name.clone(),
            TypeExpr::Generic(g) => g.name.clone(),
            _ => {
                self.error(
                    format!("endpoint `{endpoint_name}`: body base type must be a struct name"),
                    dt.span,
                );
                return None;
            }
        };

        let struct_info = match self.lookup_struct(&base_name) {
            Some(info) => info,
            None => {
                self.error(
                    format!(
                        "endpoint `{endpoint_name}`: unknown struct `{base_name}` in body type"
                    ),
                    dt.span,
                );
                return None;
            }
        };

        // Start with all fields from the struct, propagating constraints
        let mut fields: Vec<DerivedField> = struct_info
            .fields
            .iter()
            .map(|f| DerivedField {
                name: f.name.clone(),
                ty: f.ty.clone(),
                optional: false,
                constraint: f.constraint.clone(),
                span: f.definition_span,
            })
            .collect();

        // Apply modifiers left-to-right
        for modifier in &dt.modifiers {
            match modifier {
                TypeModifier::Omit {
                    fields: omit_fields,
                    span,
                } => {
                    self.validate_field_names(
                        endpoint_name,
                        &base_name,
                        omit_fields,
                        &fields,
                        *span,
                    );
                    fields.retain(|f| !omit_fields.contains(&f.name));
                }
                TypeModifier::Pick {
                    fields: pick_fields,
                    span,
                } => {
                    self.validate_field_names(
                        endpoint_name,
                        &base_name,
                        pick_fields,
                        &fields,
                        *span,
                    );
                    fields.retain(|f| pick_fields.contains(&f.name));
                }
                TypeModifier::Partial {
                    fields: partial_fields,
                    span,
                } => {
                    if let Some(field_names) = partial_fields {
                        for field_name in field_names {
                            if let Some(f) = fields.iter_mut().find(|f| &f.name == field_name) {
                                f.optional = true;
                            } else {
                                self.error(
                                    format!(
                                        "endpoint `{endpoint_name}`: field `{field_name}` does not exist on struct `{base_name}`"
                                    ),
                                    *span,
                                );
                            }
                        }
                    } else {
                        for f in &mut fields {
                            f.optional = true;
                        }
                    }
                }
            }
        }

        Some(ResolvedDerivedType {
            base_type: base_name,
            fields,
        })
    }

    /// Reports an error for each name in `names` that does not appear in `fields`.
    fn validate_field_names(
        &mut self,
        endpoint_name: &str,
        struct_name: &str,
        names: &[String],
        fields: &[DerivedField],
        span: phoenix_common::span::Span,
    ) {
        for name in names {
            if !fields.iter().any(|f| &f.name == name) {
                self.error(
                    format!(
                        "endpoint `{endpoint_name}`: field `{name}` does not exist on struct `{struct_name}`"
                    ),
                    span,
                );
            }
        }
    }
}

/// Extracts path parameter names from a URL pattern by scanning for
/// `{name}` segments.
///
/// Returns parameter names in the order they appear in the path.
/// Empty braces (`{}`) are silently skipped.
///
/// # Examples
///
/// ```text
/// "/api/users"                         -> []
/// "/api/users/{id}"                    -> ["id"]
/// "/api/users/{id}/posts/{postId}"     -> ["id", "postId"]
/// ```
fn extract_path_params(path: &str) -> Vec<String> {
    let mut params = Vec::new();
    let mut chars = path.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut name = String::new();
            for inner in chars.by_ref() {
                if inner == '}' {
                    break;
                }
                name.push(inner);
            }
            if !name.is_empty() {
                params.push(name);
            }
        }
    }
    params
}

/// Produces a routing signature for collision detection: the HTTP method
/// paired with the path, but with every `{param}` placeholder collapsed to a
/// bare `{}` so that routes differing only in path-parameter *names*
/// (`GET /posts/{id}` vs `GET /posts/{slug}`) are recognized as the same route
/// — they match the same incoming URLs and so would conflict at the router.
///
/// ```text
/// (Get, "/posts/{id}")    -> "GET /posts/{}"
/// (Get, "/posts/{slug}")  -> "GET /posts/{}"   (same signature)
/// (Post, "/posts/{id}")   -> "POST /posts/{}"  (differs by method)
/// ```
///
/// This is exact-*pattern* equality only — it does NOT detect a parameter
/// segment overlapping a literal one. `GET /posts/{id}` and `GET /posts/tagged`
/// produce different signatures and so are not flagged here, even though a
/// request to `/posts/tagged` is ambiguous at runtime (it matches both, with
/// the winner decided by the target router's precedence rules). Catching that
/// class of ambiguity is out of scope; this check only rejects routes that are
/// pattern-identical.
fn route_signature(method: HttpMethod, path: &str) -> String {
    let mut normalized = String::with_capacity(path.len());
    let mut chars = path.chars();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            // Consume the param name up to and including the closing `}`
            // (mirroring `extract_path_params`) and emit a bare placeholder.
            for inner in chars.by_ref() {
                if inner == '}' {
                    break;
                }
            }
            normalized.push_str("{}");
        } else {
            normalized.push(ch);
        }
    }
    format!("{} {normalized}", method.as_upper_str())
}

/// Prepends the API-version prefix to an endpoint path.
///
/// The version string (from an `api version "..." { }` block) is treated as a
/// literal leading path segment. The author may write it with or without a
/// leading slash (`"v1"` ≡ `"/v1"`); the path may likewise start with or without
/// one. The result always has exactly one `/` at each seam and a single leading
/// `/`. With no version (`None`), the path is returned unchanged.
///
/// ```text
/// (Some("v1"),  "/posts")  -> "/v1/posts"
/// (Some("/v1"), "/posts")  -> "/v1/posts"
/// (Some("v1"),  "posts")   -> "/v1/posts"
/// (None,        "/posts")  -> "/posts"
/// ```
fn apply_version_prefix(version: Option<&str>, path: &str) -> String {
    match version {
        None => path.to_string(),
        Some(v) => {
            // Normalize via the shared helper so the seam/whitespace handling
            // matches exactly what the parser validated (`" v1 "` -> `v1`). The
            // parser already rejects versions that are empty once normalized.
            let v = normalize_api_version(v);
            let p = path.trim_start_matches('/');
            // An empty path would otherwise yield a trailing-slash seam
            // (`/v1/`). Endpoints always carry a non-empty path today, but keep
            // the helper total.
            if p.is_empty() {
                format!("/{v}")
            } else {
                format!("/{v}/{p}")
            }
        }
    }
}

/// Extracts a [`DefaultValue`] from a literal AST expression.
///
/// Returns `None` for non-literal expressions (which should not appear as
/// query parameter defaults in well-formed Phoenix schemas).
fn extract_default_value(expr: &Expr) -> Option<DefaultValue> {
    match expr {
        Expr::Literal(Literal {
            kind: LiteralKind::Int(v),
            ..
        }) => Some(DefaultValue::Int(*v)),
        Expr::Literal(Literal {
            kind: LiteralKind::Float(v),
            ..
        }) => Some(DefaultValue::Float(*v)),
        Expr::Literal(Literal {
            kind: LiteralKind::String(v),
            ..
        }) => Some(DefaultValue::String(v.clone())),
        Expr::Literal(Literal {
            kind: LiteralKind::Bool(v),
            ..
        }) => Some(DefaultValue::Bool(*v)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use phoenix_common::diagnostics::Severity;
    use phoenix_common::span::SourceId;
    use phoenix_lexer::lexer::tokenize;
    use phoenix_parser::ast::HttpMethod;
    use phoenix_parser::parser;

    use super::{apply_version_prefix, route_signature};
    use crate::checker::check;

    /// Lex, parse, and type-check `source`, returning only the error-level
    /// diagnostic messages.
    fn check_source(source: &str) -> Vec<String> {
        let tokens = tokenize(source, SourceId(0));
        let (program, parse_errors) = parser::parse(&tokens);
        assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
        let result = check(&program);
        result
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| d.message.clone())
            .collect()
    }

    fn assert_no_errors(source: &str) {
        let errors = check_source(source);
        assert!(errors.is_empty(), "unexpected errors: {:?}", errors);
    }

    /// Lex/parse/check `source` and return the first endpoint's resolved info.
    fn first_endpoint(source: &str) -> crate::checker::EndpointInfo {
        let tokens = tokenize(source, SourceId(0));
        let (program, parse_errors) = parser::parse(&tokens);
        assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
        let result = check(&program);
        result
            .endpoints
            .into_iter()
            .next()
            .expect("expected at least one endpoint")
    }

    fn assert_has_error(source: &str, expected_fragment: &str) {
        let errors = check_source(source);
        assert!(
            errors.iter().any(|e| e.contains(expected_fragment)),
            "expected an error containing {:?}, but got: {:?}",
            expected_fragment,
            errors
        );
    }

    /// Returns the resolved path of the endpoint named `name`.
    fn endpoint_path(source: &str, name: &str) -> String {
        let tokens = tokenize(source, SourceId(0));
        let (program, parse_errors) = parser::parse(&tokens);
        assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
        let result = check(&program);
        result
            .endpoints
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("no endpoint named {name}"))
            .path
            .clone()
    }

    #[test]
    fn api_version_prefixes_path() {
        let src = r#"
            struct Post { Int id }
            api version "v1" {
                endpoint listPosts: GET "/posts" { response Post }
                endpoint getPost: GET "/posts/{id}" { response Post }
            }
        "#;
        assert_eq!(endpoint_path(src, "listPosts"), "/v1/posts");
        assert_eq!(endpoint_path(src, "getPost"), "/v1/posts/{id}");
    }

    #[test]
    fn api_version_slash_forms_normalize() {
        // `"/v1"` prefix and a path without a leading slash both normalize to a
        // single seam slash.
        let src = r#"
            struct Post { Int id }
            api version "/v1" {
                endpoint p: GET "posts" { response Post }
            }
        "#;
        assert_eq!(endpoint_path(src, "p"), "/v1/posts");
    }

    #[test]
    fn api_version_path_params_extracted_after_prefix() {
        // The version prefix has no params; path params are still extracted.
        let src = r#"
            struct Post { Int id }
            api version "v1" {
                endpoint getPost: GET "/posts/{id}" { response Post }
            }
        "#;
        let ep = first_endpoint(src);
        assert_eq!(ep.path, "/v1/posts/{id}");
        assert_eq!(ep.path_params, vec!["id".to_string()]);
    }

    #[test]
    fn multiple_api_version_blocks_and_unversioned() {
        let src = r#"
            struct Post { Int id }
            api version "v1" {
                endpoint a: GET "/posts" { response Post }
            }
            api version "v2" {
                endpoint b: GET "/posts" { response Post }
            }
            endpoint health: GET "/health" { response Post }
        "#;
        assert_eq!(endpoint_path(src, "a"), "/v1/posts");
        assert_eq!(endpoint_path(src, "b"), "/v2/posts");
        assert_eq!(endpoint_path(src, "health"), "/health");
    }

    #[test]
    fn api_version_duplicate_endpoint_name_still_rejected() {
        // Endpoint names are globally unique even across version blocks.
        assert_has_error(
            r#"
            struct Post { Int id }
            api version "v1" { endpoint dup: GET "/a" { response Post } }
            api version "v2" { endpoint dup: GET "/b" { response Post } }
            "#,
            "duplicate endpoint name",
        );
    }

    #[test]
    fn apply_version_prefix_normalizes_seams() {
        // Exactly one `/` at the seam regardless of how either side is written.
        assert_eq!(apply_version_prefix(Some("v1"), "/posts"), "/v1/posts");
        assert_eq!(apply_version_prefix(Some("/v1"), "/posts"), "/v1/posts");
        assert_eq!(apply_version_prefix(Some("v1"), "posts"), "/v1/posts");
        assert_eq!(apply_version_prefix(Some("/v1/"), "posts"), "/v1/posts");
        // Surrounding whitespace is trimmed defensively (the parser rejects
        // empty-after-trim versions, but a stray-spaced one must not leak in).
        assert_eq!(apply_version_prefix(Some(" v1 "), "/posts"), "/v1/posts");
        // A multi-segment prefix (internal `/` is allowed) keeps its inner
        // separator; only the outer seams are normalized to a single `/`.
        assert_eq!(
            apply_version_prefix(Some("v1/beta"), "/posts"),
            "/v1/beta/posts"
        );
        assert_eq!(
            apply_version_prefix(Some("/v1/beta/"), "posts"),
            "/v1/beta/posts"
        );
        // No version leaves the path untouched.
        assert_eq!(apply_version_prefix(None, "/posts"), "/posts");
        // A degenerate empty/slash-only path yields the bare prefix, never a
        // trailing-slash seam (`/v1/`).
        assert_eq!(apply_version_prefix(Some("v1"), ""), "/v1");
        assert_eq!(apply_version_prefix(Some("v1"), "/"), "/v1");
    }

    #[test]
    fn api_version_multi_segment_prefix() {
        // A version string may itself contain `/` to declare a multi-segment
        // prefix; it splices in verbatim ahead of the endpoint path.
        let src = r#"
            struct Post { Int id }
            api version "v1/beta" {
                endpoint getPost: GET "/posts/{id}" { response Post }
            }
        "#;
        let ep = first_endpoint(src);
        assert_eq!(ep.path, "/v1/beta/posts/{id}");
        assert_eq!(ep.path_params, vec!["id".to_string()]);
    }

    // ── Route-collision detection ───────────────────────────────────────

    #[test]
    fn route_signature_normalizes_path_param_names() {
        // Same position, different param name -> same signature.
        assert_eq!(
            route_signature(HttpMethod::Get, "/posts/{id}"),
            route_signature(HttpMethod::Get, "/posts/{slug}"),
        );
        // Different method -> different signature.
        assert_ne!(
            route_signature(HttpMethod::Get, "/posts/{id}"),
            route_signature(HttpMethod::Post, "/posts/{id}"),
        );
        // Different static segment -> different signature.
        assert_ne!(
            route_signature(HttpMethod::Get, "/posts/{id}"),
            route_signature(HttpMethod::Get, "/users/{id}"),
        );
    }

    #[test]
    fn duplicate_route_rejected() {
        // Two endpoints with distinct names but the same method + path collide.
        assert_has_error(
            r#"
            struct Post { Int id }
            endpoint listPosts: GET "/posts" { response Post }
            endpoint allPosts: GET "/posts" { response Post }
            "#,
            "conflicts with endpoint `listPosts`",
        );
    }

    #[test]
    fn route_collision_ignores_path_param_names() {
        // `/posts/{id}` and `/posts/{slug}` match the same URLs -> collision.
        assert_has_error(
            r#"
            struct Post { Int id }
            endpoint getById: GET "/posts/{id}" { response Post }
            endpoint getBySlug: GET "/posts/{slug}" { response Post }
            "#,
            "conflicts with endpoint `getById`",
        );
    }

    #[test]
    fn same_path_different_method_ok() {
        // A shared path with distinct methods is a normal REST pattern.
        assert_no_errors(
            r#"
            struct Post { Int id }
            endpoint listPosts: GET "/posts" { response Post }
            endpoint createPost: POST "/posts" { response Post }
            "#,
        );
    }

    #[test]
    fn versioned_route_collides_with_handwritten_prefix() {
        // A versioned endpoint resolving to `/v2/posts` collides with a
        // top-level endpoint whose path was written out as `/v2/posts`.
        assert_has_error(
            r#"
            struct Post { Int id }
            api version "v2" {
                endpoint listV2: GET "/posts" { response Post }
            }
            endpoint handwritten: GET "/v2/posts" { response Post }
            "#,
            "conflicts with endpoint `listV2`",
        );
    }

    #[test]
    fn same_path_under_different_versions_ok() {
        // The same path under different version prefixes resolves to distinct
        // routes (`/v1/posts` vs `/v2/posts`) and must not collide.
        assert_no_errors(
            r#"
            struct Post { Int id }
            api version "v1" {
                endpoint listV1: GET "/posts" { response Post }
            }
            api version "v2" {
                endpoint listV2: GET "/posts" { response Post }
            }
            "#,
        );
    }

    #[test]
    fn duplicate_name_suppresses_route_conflict() {
        // Two endpoints sharing both a name AND a route are one mistake: only
        // the duplicate-name error fires, not also a redundant route conflict.
        let errors = check_source(
            r#"
            struct Post { Int id }
            endpoint listPosts: GET "/posts" { response Post }
            endpoint listPosts: GET "/posts" { response Post }
            "#,
        );
        assert!(
            errors.iter().any(|e| e.contains("duplicate endpoint name")),
            "expected a duplicate-name error, got: {:?}",
            errors
        );
        assert!(
            !errors.iter().any(|e| e.contains("conflicts with endpoint")),
            "route conflict should be suppressed when the name is a duplicate, got: {:?}",
            errors
        );
    }

    // ── Valid endpoint declarations ─────────────────────────────────────

    #[test]
    fn valid_get_endpoint_with_response() {
        assert_no_errors(
            r#"
            struct User { Int id  String name }
            endpoint getUser: GET "/api/users/{id}" {
                response User
            }
            "#,
        );
    }

    #[test]
    fn valid_post_endpoint_with_body_and_response() {
        assert_no_errors(
            r#"
            struct User { Int id  String name }
            endpoint createUser: POST "/api/users" {
                body User
                response User
            }
            "#,
        );
    }

    #[test]
    fn valid_put_endpoint() {
        assert_no_errors(
            r#"
            struct User { Int id  String name }
            endpoint updateUser: PUT "/api/users/{id}" {
                body User
                response User
            }
            "#,
        );
    }

    #[test]
    fn valid_patch_endpoint() {
        assert_no_errors(
            r#"
            struct User { Int id  String name }
            endpoint patchUser: PATCH "/api/users/{id}" {
                body User
                response User
            }
            "#,
        );
    }

    #[test]
    fn valid_delete_endpoint_no_body() {
        assert_no_errors(
            r#"
            struct User { Int id  String name }
            endpoint deleteUser: DELETE "/api/users/{id}" {
                response User
            }
            "#,
        );
    }

    #[test]
    fn valid_endpoint_with_all_sections() {
        assert_no_errors(
            r#"
            struct User { Int id  String name  String email }
            endpoint createUser: POST "/api/users" {
                body User omit { id }
                response User
                query {
                    Bool notify = true
                }
                error {
                    Conflict(409)
                    ValidationError(400)
                }
            }
            "#,
        );
    }

    #[test]
    fn valid_endpoint_empty_block() {
        assert_no_errors(
            r#"
            endpoint healthCheck: GET "/health" {
            }
            "#,
        );
    }

    // ── Duplicate endpoint names ────────────────────────────────────────

    #[test]
    fn duplicate_endpoint_name() {
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint getUser: GET "/api/users/{id}" {
                response User
            }
            endpoint getUser: GET "/api/users/{id}" {
                response User
            }
            "#,
            "duplicate endpoint name `getUser`",
        );
    }

    // ── Body not allowed on GET / DELETE ────────────────────────────────

    #[test]
    fn body_on_get_endpoint() {
        assert_has_error(
            r#"
            struct User { Int id  String name }
            endpoint getUser: GET "/api/users/{id}" {
                body User
                response User
            }
            "#,
            "`body` is not allowed on Get endpoints",
        );
    }

    #[test]
    fn body_on_delete_endpoint() {
        assert_has_error(
            r#"
            struct User { Int id  String name }
            endpoint deleteUser: DELETE "/api/users/{id}" {
                body User
            }
            "#,
            "`body` is not allowed on Delete endpoints",
        );
    }

    // ── Unknown response type ──────────────────────────────────────────

    #[test]
    fn unknown_response_type() {
        assert_has_error(
            r#"
            endpoint getUser: GET "/api/users/{id}" {
                response NonexistentType
            }
            "#,
            "unknown response type",
        );
    }

    // ── Unknown body struct ────────────────────────────────────────────

    #[test]
    fn unknown_body_struct() {
        assert_has_error(
            r#"
            endpoint createUser: POST "/api/users" {
                body UnknownStruct
            }
            "#,
            "unknown struct `UnknownStruct` in body type",
        );
    }

    // ── Omit modifier with invalid field ────────────────────────────────

    #[test]
    fn omit_nonexistent_field() {
        assert_has_error(
            r#"
            struct User { Int id  String name }
            endpoint createUser: POST "/api/users" {
                body User omit { nonexistent }
                response User
            }
            "#,
            "field `nonexistent` does not exist on struct `User`",
        );
    }

    // ── Pick modifier with invalid field ────────────────────────────────

    #[test]
    fn pick_nonexistent_field() {
        assert_has_error(
            r#"
            struct User { Int id  String name }
            endpoint createUser: POST "/api/users" {
                body User pick { nonexistent }
                response User
            }
            "#,
            "field `nonexistent` does not exist on struct `User`",
        );
    }

    // ── Partial modifier with invalid field ─────────────────────────────

    #[test]
    fn partial_nonexistent_field() {
        assert_has_error(
            r#"
            struct User { Int id  String name }
            endpoint patchUser: PATCH "/api/users/{id}" {
                body User partial { nonexistent }
                response User
            }
            "#,
            "field `nonexistent` does not exist on struct `User`",
        );
    }

    #[test]
    fn valid_partial_all_fields() {
        assert_no_errors(
            r#"
            struct User { Int id  String name }
            endpoint patchUser: PATCH "/api/users/{id}" {
                body User partial
                response User
            }
            "#,
        );
    }

    // ── Chained modifiers ───────────────────────────────────────────────

    #[test]
    fn valid_omit_then_partial() {
        assert_no_errors(
            r#"
            struct User { Int id  String name  String email }
            endpoint patchUser: PATCH "/api/users/{id}" {
                body User omit { id } partial
                response User
            }
            "#,
        );
    }

    #[test]
    fn omit_then_partial_nonexistent_field() {
        // After omit { id }, the remaining fields are name and email.
        // Trying to make "id" partial should fail because it was already removed.
        assert_has_error(
            r#"
            struct User { Int id  String name  String email }
            endpoint patchUser: PATCH "/api/users/{id}" {
                body User omit { id } partial { id }
                response User
            }
            "#,
            "field `id` does not exist on struct `User`",
        );
    }

    // ── Error variant validation ────────────────────────────────────────

    #[test]
    fn duplicate_error_variant() {
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint createUser: POST "/api/users" {
                body User
                error {
                    NotFound(404)
                    NotFound(404)
                }
            }
            "#,
            "duplicate error variant `NotFound`",
        );
    }

    #[test]
    fn error_status_code_below_400() {
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint createUser: POST "/api/users" {
                body User
                error {
                    Ok(200)
                }
            }
            "#,
            "status code 200 is not a client/server error",
        );
    }

    #[test]
    fn error_status_code_above_599() {
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint createUser: POST "/api/users" {
                body User
                error {
                    CustomError(600)
                }
            }
            "#,
            "status code 600 is not a client/server error",
        );
    }

    #[test]
    fn valid_error_status_codes_at_boundaries() {
        assert_no_errors(
            r#"
            struct User { Int id }
            endpoint createUser: POST "/api/users" {
                body User
                error {
                    BadRequest(400)
                    InternalError(599)
                }
            }
            "#,
        );
    }

    // ── Query parameter validation ──────────────────────────────────────

    #[test]
    fn valid_query_params_with_defaults() {
        assert_no_errors(
            r#"
            struct User { Int id  String name }
            endpoint listUsers: GET "/api/users" {
                query {
                    Int page = 1
                    Int limit = 20
                    String search = "default"
                    Bool active = true
                }
                response User
            }
            "#,
        );
    }

    #[test]
    fn header_wire_name_auto_transform() {
        // Auto-derived wire names: camelCase identifier -> Title-Case-Kebab.
        let ep = first_endpoint(
            r#"
            struct User { Int id }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    String authorization
                    String idempotencyKey
                    Option<String> xRequestId
                }
                response User
            }
            "#,
        );
        let wire: Vec<(&str, &str)> = ep
            .headers
            .iter()
            .map(|h| (h.name.as_str(), h.wire_name.as_str()))
            .collect();
        assert_eq!(
            wire,
            vec![
                ("authorization", "Authorization"),
                ("idempotencyKey", "Idempotency-Key"),
                ("xRequestId", "X-Request-Id"),
            ]
        );
    }

    #[test]
    fn header_wire_name_explicit_override() {
        // An `as "..."` override is taken verbatim, bypassing the transform.
        let ep = first_endpoint(
            r#"
            struct User { Int id }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    String rateLimit as "X-RateLimit-Limit"
                    String etag as "ETag"
                }
                response User
            }
            "#,
        );
        let wire: Vec<(&str, &str)> = ep
            .headers
            .iter()
            .map(|h| (h.name.as_str(), h.wire_name.as_str()))
            .collect();
        assert_eq!(
            wire,
            vec![("rateLimit", "X-RateLimit-Limit"), ("etag", "ETag")]
        );
    }

    #[test]
    fn response_headers_resolved() {
        let ep = first_endpoint(
            r#"
            struct Post { Int id }
            endpoint getPost: GET "/api/posts/{id}" {
                response Post headers {
                    Int ratelimitRemaining as "X-RateLimit-Remaining"
                }
            }
            "#,
        );
        assert_eq!(ep.headers.len(), 0);
        assert_eq!(ep.response_headers.len(), 1);
        assert_eq!(ep.response_headers[0].name, "ratelimitRemaining");
        assert_eq!(ep.response_headers[0].wire_name, "X-RateLimit-Remaining");
    }

    #[test]
    fn header_default_type_mismatch() {
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    Int retries = "nope"
                }
                response User
            }
            "#,
            "default value for header `retries` does not match type",
        );
    }

    #[test]
    fn response_header_default_rejected() {
        // A response header is set by the handler, never received, so a `= default`
        // is meaningless and rejected (not silently ignored).
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint getUser: GET "/api/users/{id}" {
                response User headers {
                    Int ratelimitRemaining = 100
                }
            }
            "#,
            "response header `ratelimitRemaining` cannot have a default value",
        );
    }

    #[test]
    fn request_header_wire_name_collision() {
        // Two request headers resolving to the same wire name (here an auto-derived
        // `X-Request-Id` and an explicit override of the same) would silently
        // overwrite each other on the wire.
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    String xRequestId
                    String tracing as "X-Request-Id"
                }
                response User
            }
            "#,
            "request header wire name `X-Request-Id` is declared by more than one header",
        );
    }

    #[test]
    fn request_header_wire_name_collision_case_insensitive() {
        // HTTP header names are case-insensitive, so `X-Trace` and `x-trace`
        // collide.
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    String a as "X-Trace"
                    String b as "x-trace"
                }
                response User
            }
            "#,
            "is declared by more than one header",
        );
    }

    #[test]
    fn response_header_wire_name_collision() {
        // The wire-name uniqueness check runs per direction; this exercises the
        // RESPONSE branch with two distinct local names colliding on the same
        // wire name (case-insensitively, since HTTP header names are). Two
        // response headers resolving to the same wire name would overwrite each
        // other on send and read the same value on parse.
        assert_has_error(
            r#"
            struct Post { Int id }
            endpoint getPost: GET "/api/posts/{id}" {
                response Post headers {
                    Int rateLimit as "X-Limit"
                    Int ceiling as "x-limit"
                }
            }
            "#,
            // The diagnostic names the colliding (second) header's wire name verbatim.
            "response header wire name `x-limit` is declared by more than one header",
        );
    }

    #[test]
    fn request_header_collides_with_path_param() {
        // A request header local name that duplicates a path param would emit two
        // generated parameters of the same name.
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    String id as "X-Id"
                }
                response User
            }
            "#,
            "request header `id` collides with another endpoint input",
        );
    }

    #[test]
    fn request_header_collides_with_query_param() {
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint listUsers: GET "/api/users" {
                query {
                    Option<String> trace
                }
                headers {
                    String trace as "X-Trace"
                }
                response User
            }
            "#,
            "request header `trace` collides with another endpoint input",
        );
    }

    #[test]
    fn request_header_duplicate_local_name() {
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    String token as "X-A"
                    String token as "X-B"
                }
                response User
            }
            "#,
            "request header `token` collides with another endpoint input",
        );
    }

    #[test]
    fn response_header_duplicate_local_name() {
        // Two response headers with the same local name would emit two envelope
        // fields of the same name.
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint getUser: GET "/api/users/{id}" {
                response User headers {
                    Int rate as "X-A"
                    Int rate as "X-B"
                }
            }
            "#,
            "response header `rate` is declared more than once",
        );
    }

    #[test]
    fn query_param_default_type_mismatch() {
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint listUsers: GET "/api/users" {
                query {
                    Int page = "not_a_number"
                }
                response User
            }
            "#,
            "default value for query param `page` does not match type",
        );
    }

    #[test]
    fn query_param_bool_default_on_int_type() {
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint listUsers: GET "/api/users" {
                query {
                    Int active = true
                }
                response User
            }
            "#,
            "default value for query param `active` does not match type",
        );
    }

    // ── Path parameter extraction (verified via resolved endpoint info) ─

    #[test]
    fn path_params_are_extracted() {
        let source = r#"
            struct User { Int id }
            endpoint getUser: GET "/api/users/{id}/posts/{postId}" {
                response User
            }
        "#;
        let tokens = tokenize(source, SourceId(0));
        let (program, parse_errors) = parser::parse(&tokens);
        assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
        let result = check(&program);
        assert!(
            result
                .diagnostics
                .iter()
                .all(|d| d.severity != Severity::Error),
            "unexpected errors: {:?}",
            result.diagnostics
        );
        let ep = result
            .endpoints
            .iter()
            .find(|e| e.name == "getUser")
            .expect("endpoint not found");
        assert_eq!(ep.path_params, vec!["id".to_string(), "postId".to_string()]);
    }

    // ── Body on POST/PUT/PATCH is fine ──────────────────────────────────

    #[test]
    fn body_allowed_on_post_put_patch() {
        // POST, PUT, and PATCH should all accept body without errors.
        assert_no_errors(
            r#"
            struct Payload { String data }

            endpoint postIt: POST "/a" {
                body Payload
            }

            endpoint putIt: PUT "/b" {
                body Payload
            }

            endpoint patchIt: PATCH "/c" {
                body Payload
            }
            "#,
        );
    }

    // ── Pick modifier keeps only specified fields ───────────────────────

    #[test]
    fn valid_pick_subset() {
        assert_no_errors(
            r#"
            struct User { Int id  String name  String email  Int age }
            endpoint createUser: POST "/api/users" {
                body User pick { name, email }
                response User
            }
            "#,
        );
    }

    // ── Doc comment does not affect validation ──────────────────────────

    #[test]
    fn endpoint_with_doc_comment() {
        assert_no_errors(
            r#"
            struct User { Int id  String name }
            /** Retrieves a single user by ID. */
            endpoint getUser: GET "/api/users/{id}" {
                response User
            }
            "#,
        );
    }

    // ── Multiple endpoints with unique names are fine ────────────────────

    #[test]
    fn multiple_distinct_endpoints() {
        assert_no_errors(
            r#"
            struct User { Int id  String name }
            endpoint listUsers: GET "/api/users" {
                response User
            }
            endpoint getUser: GET "/api/users/{id}" {
                response User
            }
            endpoint createUser: POST "/api/users" {
                body User
                response User
            }
            "#,
        );
    }

    // ── Integration: full realistic endpoint ────────────────────────────

    #[test]
    fn realistic_full_endpoint() {
        assert_no_errors(
            r#"
            struct User {
                Int id
                String name where self.length > 0 && self.length <= 100
                String email
                Int age where self >= 0 && self <= 150
                Option<String> bio
            }

            endpoint createUser: POST "/api/users" {
                body User omit { id, bio }
                response User
                query {
                    Bool notify = true
                }
                error {
                    ValidationError(400)
                    Conflict(409)
                }
            }
            "#,
        );
    }

    // ── File primitive: body-only / multipart / binary-download rules ────
    //
    // `File` is an endpoint-transport-only type: legal ONLY as the direct
    // field of a struct used as an endpoint request/response body. See
    // `docs/design-decisions.md` (multipart / file-upload section).

    // ACCEPT: request body mixing a File + scalars (multipart upload).
    #[test]
    fn file_accept_request_body_mixed() {
        assert_no_errors(
            r#"
            struct AvatarUpload { File avatar  String caption }
            struct UploadResult { String url }
            endpoint uploadAvatar: POST "/api/avatar" {
                body AvatarUpload
                response UploadResult
            }
            "#,
        );
    }

    // ACCEPT: response body that is a single-File struct (binary download).
    #[test]
    fn file_accept_response_single_file() {
        assert_no_errors(
            r#"
            struct Doc { File data }
            endpoint download: GET "/api/doc/{id}" {
                response Doc
            }
            "#,
        );
    }

    // ACCEPT: `Option<File>` as a struct field (optional file upload).
    #[test]
    fn file_accept_optional_file_field() {
        assert_no_errors(
            r#"
            struct MaybeUpload { Option<File> avatar  String caption }
            endpoint upload: POST "/api/maybe" {
                body MaybeUpload
            }
            "#,
        );
    }

    // FLAGS: request body with a File field is multipart.
    #[test]
    fn file_flag_body_is_multipart() {
        let ep = first_endpoint(
            r#"
            struct AvatarUpload { File avatar  String caption }
            endpoint uploadAvatar: POST "/api/avatar" {
                body AvatarUpload
            }
            "#,
        );
        assert!(ep.body_is_multipart, "body should be multipart");
        assert!(!ep.response_is_binary);
    }

    // FLAGS: omitting the File field makes the body plain JSON again.
    #[test]
    fn file_flag_body_multipart_cleared_by_omit() {
        let ep = first_endpoint(
            r#"
            struct AvatarUpload { File avatar  String caption }
            endpoint uploadAvatar: POST "/api/avatar" {
                body AvatarUpload omit { avatar }
            }
            "#,
        );
        assert!(
            !ep.body_is_multipart,
            "omitting the only File field clears multipart"
        );
    }

    // FLAGS: single-File response struct is a binary download.
    #[test]
    fn file_flag_response_is_binary() {
        let ep = first_endpoint(
            r#"
            struct Doc { File data }
            endpoint download: GET "/api/doc/{id}" {
                response Doc
            }
            "#,
        );
        assert!(ep.response_is_binary, "response should be binary");
        assert!(!ep.body_is_multipart);
    }

    // FLAGS: Option<File> body field still counts as multipart.
    #[test]
    fn file_flag_optional_file_body_is_multipart() {
        let ep = first_endpoint(
            r#"
            struct MaybeUpload { Option<File> avatar  String caption }
            endpoint upload: POST "/api/maybe" {
                body MaybeUpload
            }
            "#,
        );
        assert!(ep.body_is_multipart);
    }

    // REJECT: File as a function parameter.
    #[test]
    fn file_reject_function_param() {
        assert_has_error(
            r#"
            function f(x: File) -> Int { return 0 }
            "#,
            "`File` is only allowed",
        );
    }

    // REJECT: File as a function return type.
    #[test]
    fn file_reject_function_return() {
        assert_has_error(
            r#"
            function g() -> File { return 0 }
            "#,
            "`File` is only allowed",
        );
    }

    // REJECT: File as a `let` binding type.
    #[test]
    fn file_reject_let_binding() {
        assert_has_error(
            r#"
            function h() -> Int {
                let x: File = 0
                return 0
            }
            "#,
            "`File` is only allowed",
        );
    }

    // REJECT: File as a query parameter type.
    #[test]
    fn file_reject_query_param() {
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint getUser: GET "/api/users/{id}" {
                query {
                    File f
                }
                response User
            }
            "#,
            "`File` is only allowed",
        );
    }

    // REJECT: File as a header type.
    #[test]
    fn file_reject_header() {
        assert_has_error(
            r#"
            struct User { Int id }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    File token
                }
                response User
            }
            "#,
            "`File` is only allowed",
        );
    }

    // REJECT: File as an enum variant payload.
    #[test]
    fn file_reject_enum_variant_payload() {
        assert_has_error(
            r#"
            enum Wrapper { Holds(File)  Empty }
            "#,
            "`File` is only allowed",
        );
    }

    // REJECT: File inside a generic argument (`List<File>`) — even as a field.
    #[test]
    fn file_reject_list_of_file_field() {
        assert_has_error(
            r#"
            struct Gallery { List<File> photos }
            "#,
            "`File` is only allowed",
        );
    }

    // REJECT: File inside `Map<String, File>` as a field.
    #[test]
    fn file_reject_map_of_file_field() {
        assert_has_error(
            r#"
            struct Bucket { Map<String, File> blobs }
            "#,
            "`File` is only allowed",
        );
    }

    // REJECT: File as a type-alias target.
    #[test]
    fn file_reject_type_alias_target() {
        assert_has_error(
            r#"
            type Blob = File
            "#,
            "`File` is only allowed",
        );
    }

    // REJECT: a file-bearing struct used as a regular function parameter.
    #[test]
    fn file_reject_bearing_struct_as_param() {
        assert_has_error(
            r#"
            struct AvatarUpload { File avatar  String caption }
            function f(a: AvatarUpload) -> Int { return 0 }
            "#,
            "body-only type",
        );
    }

    // REJECT: a file-bearing struct nested as a field of another struct.
    #[test]
    fn file_reject_bearing_struct_nested() {
        assert_has_error(
            r#"
            struct AvatarUpload { File avatar }
            struct Profile { AvatarUpload upload  String name }
            "#,
            "body-only type",
        );
    }

    // REJECT: same, but the file-bearing struct is declared AFTER the struct that
    // nests it. `is_file_bearing` is computed in the pre-registration pass from
    // the raw field annotations, so the rejection does not depend on declaration
    // order (a stale `false` placeholder would otherwise let this slip through).
    #[test]
    fn file_reject_bearing_struct_nested_forward_ref() {
        assert_has_error(
            r#"
            struct Profile { AvatarUpload upload  String name }
            struct AvatarUpload { File avatar }
            "#,
            "body-only type",
        );
    }

    // REJECT: same ordering hazard for a function param typed as a file-bearing
    // struct declared later in the file.
    #[test]
    fn file_reject_bearing_struct_as_param_forward_ref() {
        assert_has_error(
            r#"
            function f(a: AvatarUpload) -> Int { return 0 }
            struct AvatarUpload { File avatar  String caption }
            "#,
            "body-only type",
        );
    }

    // REJECT: a RESPONSE that is a file-bearing struct mixing File + scalars.
    #[test]
    fn file_reject_response_mixed_body() {
        assert_has_error(
            r#"
            struct Bad { File data  String name }
            endpoint download: GET "/api/bad/{id}" {
                response Bad
            }
            "#,
            "exactly one field of type `File`",
        );
    }

    // REJECT: a RESPONSE file-bearing struct with multiple File fields.
    #[test]
    fn file_reject_response_multiple_files() {
        assert_has_error(
            r#"
            struct TwoFiles { File a  File b }
            endpoint download: GET "/api/two/{id}" {
                response TwoFiles
            }
            "#,
            "exactly one field of type `File`",
        );
    }

    // REJECT: a binary download (single-`File` response struct) that also
    // declares response headers. A binary response body is the raw file stream
    // with no `<Endpoint>Result` envelope to carry typed header fields, so the
    // two cannot be combined — the per-target codegen has no coherent shape for
    // it. See `docs/design-decisions.md` (multipart, direction asymmetry).
    #[test]
    fn file_reject_binary_response_with_response_headers() {
        assert_has_error(
            r#"
            struct Doc { File data }
            endpoint download: GET "/api/doc/{id}" {
                response Doc headers {
                    String etag as "ETag"
                }
            }
            "#,
            "cannot also declare response headers",
        );
    }

    // REJECT: a file-bearing struct nested inside a generic in RESPONSE position
    // (`List<Doc>`). The response-position allowance must not leak through
    // generic args — a `File` cannot be JSON-serialized inside a list.
    #[test]
    fn file_reject_bearing_struct_in_list_response() {
        assert_has_error(
            r#"
            struct Doc { File data }
            endpoint download: GET "/api/docs" {
                response List<Doc>
            }
            "#,
            "body-only type",
        );
    }

    // REJECT: same leak via `Option<Doc>` in response position.
    #[test]
    fn file_reject_bearing_struct_in_option_response() {
        assert_has_error(
            r#"
            struct Doc { File data }
            endpoint download: GET "/api/docs/{id}" {
                response Option<Doc>
            }
            "#,
            "body-only type",
        );
    }

    // ACCEPT: a multipart body whose non-file fields are scalars / Option<scalar>.
    #[test]
    fn file_accept_multipart_scalar_fields() {
        assert_no_errors(
            r#"
            struct Upload { File avatar  Int rotation  Bool crop  Option<String> caption }
            endpoint upload: POST "/api/upload" {
                body Upload
            }
            "#,
        );
    }

    // REJECT: a multipart body with a `List<String>` field (no form encoding).
    #[test]
    fn file_reject_multipart_list_field() {
        assert_has_error(
            r#"
            struct Upload { File avatar  List<String> tags }
            endpoint upload: POST "/api/upload" {
                body Upload
            }
            "#,
            "cannot be sent as a form field",
        );
    }

    // REJECT: a multipart body with a nested-struct field (no form encoding).
    #[test]
    fn file_reject_multipart_nested_struct_field() {
        assert_has_error(
            r#"
            struct Meta { String key }
            struct Upload { File avatar  Meta meta }
            endpoint upload: POST "/api/upload" {
                body Upload
            }
            "#,
            "cannot be sent as a form field",
        );
    }
}
