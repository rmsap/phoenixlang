//! Semantic validation for endpoint declarations.
//!
//! Validates that endpoint types, field references, and HTTP semantics are
//! correct.  Produces [`EndpointInfo`] with all types resolved.

use crate::checker::{
    Checker, DefaultValue, DerivedField, EndpointInfo, HeaderParamInfo, QueryParamInfo,
    ResolvedDerivedType, header_wire_name,
};
use crate::types::Type;
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
        // Check for duplicate endpoint names
        if self.endpoints.iter().any(|e| e.name == ep.name) {
            self.error(format!("duplicate endpoint name `{}`", ep.name), ep.span);
        }

        // Extract path parameters from URL pattern: "/api/users/{id}" -> ["id"]
        let path_params = extract_path_params(&ep.path);

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

        // Resolve response type
        let response = ep.response.as_ref().map(|te| {
            let ty = self.resolve_type_expr(te);
            if ty == Type::Error {
                self.error(
                    format!("endpoint `{}`: unknown response type", ep.name),
                    ep.span,
                );
            }
            ty
        });

        // Resolve body type with modifiers
        let body = ep
            .body
            .as_ref()
            .and_then(|dt| self.resolve_derived_type(&ep.name, dt));

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
            path: ep.path.clone(),
            path_params,
            query_params,
            headers,
            body,
            response,
            response_headers,
            errors,
            doc_comment: ep.doc_comment.clone(),
        });
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
    use phoenix_parser::parser;

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
}
