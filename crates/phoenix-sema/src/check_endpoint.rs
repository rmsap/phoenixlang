//! Semantic validation for endpoint declarations.
//!
//! Validates that endpoint types, field references, and HTTP semantics are
//! correct.  Produces [`EndpointInfo`] with all types resolved.

use crate::checker::{
    Checker, DefaultValue, DerivedField, EndpointInfo, QueryParamInfo, ResolvedDerivedType,
};
use crate::types::Type;
use phoenix_parser::ast::{
    DerivedType, EndpointDecl, Expr, HttpMethod, Literal, LiteralKind, TypeExpr, TypeModifier,
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

        self.endpoints.push(EndpointInfo {
            name: ep.name.clone(),
            method: ep.method,
            path: ep.path.clone(),
            path_params,
            query_params,
            body,
            response,
            errors,
            doc_comment: ep.doc_comment.clone(),
        });
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

        let struct_info = match self.structs.get(&base_name) {
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
                String name where self.length > 0 and self.length <= 100
                String email
                Int age where self >= 0 and self <= 150
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
