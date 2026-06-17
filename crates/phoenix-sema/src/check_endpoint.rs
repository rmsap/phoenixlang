//! Semantic validation for endpoint declarations.
//!
//! Validates that endpoint types, field references, and HTTP semantics are
//! correct.  Produces [`EndpointInfo`] with all types resolved.

use crate::checker::{
    Checker, DefaultValue, DerivedField, EndpointInfo, HeaderParamInfo, PaginationInfo,
    QueryParamInfo, ResolvedDerivedType, ResponseStatusInfo, header_wire_name,
};
use crate::types::Type;
use phoenix_common::capitalize;
use phoenix_parser::api_version::normalize_api_version;
use phoenix_parser::ast::{
    DerivedType, EndpointDecl, Expr, HeaderParam, HttpMethod, Literal, LiteralKind, TypeExpr,
    TypeModifier,
};
use std::collections::HashSet;

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

        let exported_name_collides_with = self.check_exported_name_collision(ep, is_duplicate_name);

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
        let mut response = ep.response.as_ref().map(|te| {
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

        // Multi-status block form: `response { 200: User  201: User  204 }`.
        // Resolve and validate the block, then mirror the shared body type `T`
        // into `response` so downstream "what is the success body type" reads
        // keep working. `response_statuses` being non-empty is what signals
        // multi-status to codegen; `is_multi_status` gates the binary/pagination
        // resolution below so a block-form endpoint is never also flagged binary
        // or paginatable (decision 6 / decision 4). See
        // `docs/design-decisions.md` (multi-status responses design).
        let is_multi_status = !ep.response_statuses.is_empty();
        let response_statuses = self.check_response_statuses(ep);
        if is_multi_status {
            // Shared body type `T` (the first VALID typed entry; all valid typed
            // entries are validated equal in `check_response_statuses`, and an
            // entry that failed to resolve is skipped here the same way it is
            // skipped there). `None` when the block has only typeless statuses
            // (e.g. `response { 202  204 }`). Only the valid/None distinction is
            // ever observable downstream: a `Type::Error` entry always comes with
            // a diagnostic, and codegen never runs on a failed check.
            response = response_statuses
                .iter()
                .find_map(|rs| rs.ty.clone().filter(|t| *t != Type::Error));
        }

        // Rule 3 (response/download): a file-bearing response struct must be a
        // pure binary download — exactly one field, of type `File`. Multi-status
        // bodies are JSON-only (`check_response_statuses` rejects a file-bearing
        // struct with a targeted error and resolves it to `Type::Error`), so a
        // multi-status endpoint is never a binary download — skip the check for
        // the block form so the shared-`T` mirrored into `response` above can
        // never be misread as a binary download.
        let mut response_is_binary = false;
        if !is_multi_status
            && let Some(Type::Named(name)) = &response
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

        // Pagination. A `pagination { offset|cursor }` block requires the
        // response to be a bare `List<T>`: the generated `<Endpoint>Page` envelope
        // wraps that list (`items: List<T>`) plus a mode-specific metadata field.
        // `Option<List<T>>` is rejected — a paginated call always returns a page
        // (emptiness is `items: []`), so a null page is meaningless. See
        // `docs/design-decisions.md` (pagination section).
        // Precedence: a multi-status block + pagination is reported as the
        // combo rejection below (decision 4), NOT as the "pagination requires a
        // `List<T>` response" error — the shared-`T` mirrored into `response`
        // for a multi-status block is not a `List<T>`, so running the pagination
        // resolution here would emit a confusing "requires List" diagnostic.
        // Skip pagination resolution entirely for the block form; the dedicated
        // combo error fires unconditionally when `ep.pagination` is set.
        let pagination = if is_multi_status {
            None
        } else {
            ep.pagination.and_then(|mode| {
                let item_type = match &response {
                    Some(Type::Generic(name, args)) if name == "List" && args.len() == 1 => {
                        Some(args[0].clone())
                    }
                    _ => None,
                };
                match item_type {
                    Some(item_type) => Some(PaginationInfo { mode, item_type }),
                    None => {
                        self.error(
                            format!(
                                "endpoint `{}`: `pagination` requires the response to be a `List<T>` (got {}); a paginated response wraps a list — `Option<List<T>>` and non-list responses are not allowed",
                                ep.name,
                                response
                                    .as_ref()
                                    .map(|t| t.to_string())
                                    .unwrap_or_else(|| "no response".to_string())
                            ),
                            ep.span,
                        );
                        None
                    }
                }
            })
        };

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
        let mut seen_errors = HashSet::new();
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

                // `Money` is a composite (`{ amount, currency }`) with no scalar
                // URL encoding, so it cannot ride in the query string — reject it
                // here rather than emit a dangling type reference (the per-target
                // `Money` definition is gated on body/field/response use only).
                if ty.mentions_money() {
                    self.error(
                        format!(
                            "endpoint `{}`: query param `{}` cannot be a `Money` (a composite `{{ amount, currency }}` is not URL-encodable) — carry it in the request body, or pass the amount and currency as separate scalar params",
                            ep.name, qp.name
                        ),
                        qp.span,
                    );
                }

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

        // Pagination and response headers both wrap the handler's single return
        // value in a generated envelope (`<Endpoint>Page` vs `<Endpoint>Result`),
        // and a handler has exactly one return slot — so the two envelope types
        // cannot coexist. Reject the combination here. (On the wire they are
        // orthogonal — pagination metadata rides in the body, headers in HTTP
        // headers — so this is purely a return-type-shape limitation. Future
        // options, nest vs flat-merge, are recorded in `docs/design-decisions.md`
        // pagination decision 7 / `docs/known-issues.md`.)
        if pagination.is_some()
            && let Some(first) = ep.response_headers.first()
        {
            self.error(
                format!(
                    "endpoint `{}`: `pagination` and response headers cannot be combined — both wrap the response in a generated envelope and a handler has one return value. Carry pagination metadata as response headers instead, or drop one. See docs/known-issues.md.",
                    ep.name
                ),
                first.span,
            );
        }

        // Multi-status (`response { ... }`) wraps the handler's return value in a
        // generated `<Endpoint>Response` envelope, exactly as response headers
        // wrap it in `<Endpoint>Result` and pagination in `<Endpoint>Page`. One
        // return slot holds one envelope, so multi-status is mutually exclusive
        // with both (decision 4). NOTE: the parser already rejects an inline
        // `headers { ... }` after a response block with its own targeted error
        // (and discards the block), so a block-form endpoint can never populate
        // `response_headers`. This first check is therefore
        // defensive/unreachable via the current grammar; kept as cheap
        // insurance. See `docs/known-issues.md`.
        if is_multi_status && let Some(first) = ep.response_headers.first() {
            self.error(
                format!(
                    "endpoint `{}`: a multi-status `response {{ }}` block cannot also declare response headers — both wrap the return value in a generated envelope and a handler has one return value. See docs/known-issues.md",
                    ep.name
                ),
                first.span,
            );
        }
        if is_multi_status && ep.pagination.is_some() {
            let span = ep
                .response_statuses
                .first()
                .map(|rs| rs.span)
                .unwrap_or(ep.span);
            self.error(
                format!(
                    "endpoint `{}`: a multi-status `response {{ }}` block cannot also be paginated — both wrap the return value in a generated envelope. See docs/known-issues.md",
                    ep.name
                ),
                span,
            );
        }

        self.check_generated_type_collisions(
            ep,
            is_duplicate_name,
            exported_name_collides_with.as_deref(),
            is_multi_status,
            body.is_some(),
            body_is_multipart,
        );

        // Request headers share the generated parameter scope with path and
        // query params, so a duplicate local name would emit two parameters of
        // the same name (a compile error in the generated Go/TS/Python). Check
        // each request header against the path/query names and the other headers.
        let mut input_names: HashSet<&str> = path_params.iter().map(String::as_str).collect();
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
        let mut response_field_names: HashSet<&str> = HashSet::new();
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
            response_statuses,
            response_headers,
            errors,
            doc_comment: ep.doc_comment.clone(),
            body_is_multipart,
            response_is_binary,
            pagination,
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

    /// Rejects an endpoint whose *exported* name — `capitalize(name)`, the
    /// shared rule codegen builds exported identifiers with — is already
    /// claimed by another endpoint. Endpoint names are unique only
    /// case-sensitively, so `getUser` and `GetUser` are both distinct names;
    /// but Go derives the client method, server method, and handler-interface
    /// method from the exported form, so that pair emits two `GetUser`
    /// methods on one struct — a Go compile error regardless of what else
    /// either endpoint declares. (TS/Python keep the name as written and are
    /// unaffected, but sema is target-agnostic, matching how `ClientBody` is
    /// reserved on every target.) The predicate is exported-name equality,
    /// not full case-insensitivity: `getUser` vs `getuSer` export as
    /// `GetUser` vs `GetuSer` — distinct Go methods, no collision, accepted.
    ///
    /// Returns the owning endpoint's name on collision so
    /// `check_generated_type_collisions` can treat the pair as
    /// already-reported: equal exported names imply equal generated *type*
    /// names (every generated type name is `exported + suffix`), so this
    /// check subsumes all same-stem type collisions, leaving the type map to
    /// catch the cross-stem `Body`/`ClientBody` overlap.
    ///
    /// An exact duplicate name is skipped entirely: it would trivially hit
    /// its own first declaration's entry, and that is one mistake already
    /// reported as the duplicate-name error.
    fn check_exported_name_collision(
        &mut self,
        ep: &EndpointDecl,
        is_duplicate_name: bool,
    ) -> Option<String> {
        if is_duplicate_name {
            return None;
        }
        let exported = capitalize(&ep.name);
        match self.endpoint_exported_names.get(&exported).cloned() {
            Some(other) => {
                self.error(
                    format!(
                        "endpoint `{}` collides with endpoint `{}`: both export the generated name `{}` (the Go client/handler method name), which declares the same method twice in the generated output; rename one of the endpoints",
                        ep.name, other, exported
                    ),
                    ep.span,
                );
                Some(other)
            }
            None => {
                self.endpoint_exported_names
                    .insert(exported, ep.name.clone());
                None
            }
        }
    }

    /// Reports generated-type name collisions. An endpoint declaration
    /// synthesizes types in the generated output — the envelopes
    /// `<Endpoint>Result` (response headers), `<Endpoint>Page` (pagination),
    /// and `<Endpoint>Response` (multi-status, all three mutually exclusive),
    /// plus the request-body types `<Endpoint>Body` (any `body` clause; not
    /// exclusive with the envelopes) and `<Endpoint>ClientBody` (Go only,
    /// multipart bodies), and the fixed-name multipart helper `FileUpload`
    /// (Go only, shared by every multipart endpoint). Each such name must not
    /// already be taken, or the generated output declares the same type
    /// twice. Go/TS surface a compile error, but Python silently redefines
    /// the class (last wins), a quiet miscompile — and for
    /// endpoint-vs-endpoint `Body` collisions codegen's
    /// `emitted_derived_types` dedupe is first-wins in every backend, so the
    /// second endpoint silently binds to the first one's struct. Two
    /// claimants are possible per name: a user-defined struct/enum, and
    /// another endpoint generating the same name. For the latter, same-stem
    /// pairs (equal exported names, e.g. `getUser`/`GetUser`) are already
    /// rejected by `check_exported_name_collision`, so the live
    /// endpoint-vs-endpoint case is the one cross-stem suffix overlap:
    /// `"ClientBody"` ends with `"Body"`, so `upload` (multipart) and
    /// `uploadClient` (any body) both generate `UploadClientBody`; no other
    /// suffix pair overlaps. Names are built with the same shared
    /// `capitalize` codegen uses, so this check cannot drift from the
    /// generators. Caveat: `lookup_struct`/`lookup_enum` resolve in the
    /// endpoint's module scope while `generated_type_names` is global
    /// (mirroring `route_signatures`) — if endpoints ever live in non-entry
    /// modules, a same-named type in a sibling module would be missed here.
    /// See `docs/design-decisions.md` (generated-type-name collision check).
    fn check_generated_type_collisions(
        &mut self,
        ep: &EndpointDecl,
        is_duplicate_name: bool,
        exported_name_collides_with: Option<&str>,
        is_multi_status: bool,
        has_body: bool,
        body_is_multipart: bool,
    ) {
        // The `else if` chain claims at most one envelope: the exclusivity
        // checks in `check_endpoint` (which run before this) reject any
        // endpoint combining response headers, pagination, or multi-status,
        // so under-claiming on an (already rejected) combination is harmless.
        // If those rules ever relax, the chain must claim every declared
        // envelope.
        let mut generated_claims: Vec<(&'static str, &'static str)> = Vec::new();
        if !ep.response_headers.is_empty() {
            generated_claims.push(("Result", "an envelope"));
        } else if ep.pagination.is_some() {
            generated_claims.push(("Page", "an envelope"));
        } else if is_multi_status {
            generated_claims.push(("Response", "an envelope"));
        }
        if has_body {
            generated_claims.push(("Body", "a request-body"));
            if body_is_multipart {
                generated_claims.push(("ClientBody", "a request-body"));
            }
        }
        // At most one endpoint-vs-endpoint diagnostic per colliding endpoint
        // *pair*: a colliding pair is one mistake even when several of its
        // claimed names collide (e.g. both endpoints declare a body and
        // multi-status). Suppression is per pair rather than per endpoint
        // because a cross-stem `Body`/`ClientBody` collision involves a
        // different other endpoint than a same-stem one, and those are two
        // distinct mistakes. An exported-name collision seeds the list: that
        // pair was already reported by `check_exported_name_collision`, and
        // every same-stem type collision is the same mistake — while a
        // cross-stem collision against a *third* endpoint still reports.
        let mut reported_against: Vec<String> = match exported_name_collides_with {
            Some(other) => vec![other.to_string()],
            None => Vec::new(),
        };
        // When the endpoint name itself is a duplicate or exported-name
        // collision, the user-type diagnostic is suppressed too — that name
        // clash is the mistake to fix first. For names the owning declaration
        // also claimed, the clash was already reported there; for a name only
        // this endpoint claims (the colliding endpoints declare different
        // features, e.g. headers vs body), the clash *cascades*: it surfaces
        // on the recompile after the rename — which may well fix it, since
        // the rename changes every generated name. Same deliberate-cascade
        // discipline as the duplicate-multipart `FileUpload` corner below;
        // pinned by `exported_name_collision_with_differing_features_cascades`.
        let suppress_user_type_reports = is_duplicate_name || exported_name_collides_with.is_some();
        let exported = capitalize(&ep.name);
        for (suffix, kind) in generated_claims {
            let generated_name = exported.clone() + suffix;
            if !suppress_user_type_reports
                && (self.lookup_struct(&generated_name).is_some()
                    || self.lookup_enum(&generated_name).is_some())
            {
                self.error(
                    format!(
                        "endpoint `{}` generates {} type `{}` that collides with a user-defined type of the same name; rename the user type (the generated name is `{{Endpoint}}{}`)",
                        ep.name, kind, generated_name, suffix
                    ),
                    ep.span,
                );
            }
            // Endpoint-vs-endpoint: the first endpoint to claim a name owns it
            // (recorded in `generated_type_names`); later collisions are
            // reported against it. Same-stem hits land on the seeded
            // exported-name collider and are suppressed as already-reported,
            // so the live case is cross-stem: `"ClientBody"` ends with
            // `"Body"`, so `upload` (multipart) and `uploadClient` (any body)
            // both generate `UploadClientBody` — a collision only the map
            // catches. All five suffixes still claim entries, keeping the map
            // self-defending if a future suffix introduces a new overlap.
            match self.generated_type_names.get(&generated_name).cloned() {
                Some(other) => {
                    if !is_duplicate_name && !reported_against.contains(&other) {
                        reported_against.push(other.clone());
                        self.error(
                            format!(
                                "endpoint `{}` generates {} type `{}` that collides with the type generated for endpoint `{}`; rename one of the endpoints",
                                ep.name, kind, generated_name, other
                            ),
                            ep.span,
                        );
                    }
                }
                None => {
                    self.generated_type_names
                        .insert(generated_name, ep.name.clone());
                }
            }
        }

        // The multipart helper `FileUpload` (Go) is a *fixed-name* generated
        // type shared by every multipart endpoint, so two multipart endpoints
        // both needing it is by design, not a collision — it bypasses the
        // endpoint-vs-endpoint reporting above. A user-defined type of that
        // name still duplicates the declaration in generated Go, and that
        // single mistake gets a single diagnostic: the first (non-duplicate)
        // multipart endpoint claims the name in `generated_type_names` and
        // only the claimant reports. The entry cannot trigger a false
        // endpoint-pair collision in the loop above because no
        // `<Endpoint>`+suffix name can equal `FileUpload` — none of the five
        // suffixes is a suffix of it.
        if body_is_multipart
            && !is_duplicate_name
            && !self.generated_type_names.contains_key("FileUpload")
        {
            self.generated_type_names
                .insert("FileUpload".to_string(), ep.name.clone());
            if self.lookup_struct("FileUpload").is_some()
                || self.lookup_enum("FileUpload").is_some()
            {
                self.error(
                    format!(
                        "endpoint `{}`: a multipart (file-upload) body generates the helper type `FileUpload`, which collides with a user-defined type of the same name; rename the user type",
                        ep.name
                    ),
                    ep.span,
                );
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

        // `Money` is a composite (`{ amount, currency }`) with no scalar header
        // encoding, so it cannot ride in an HTTP header — reject it here (request
        // or response) rather than emit a dangling type reference (the per-target
        // `Money` definition is gated on body/field/response use only).
        if ty.mentions_money() {
            let direction = if is_response { "response " } else { "" };
            self.error(
                format!(
                    "endpoint `{}`: {direction}header `{}` cannot be a `Money` (a composite `{{ amount, currency }}` is not header-encodable) — carry it in the body, or use separate scalar headers for the amount and currency",
                    ep.name, h.name
                ),
                h.span,
            );
        }

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
        let mut seen: HashSet<String> = HashSet::new();
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

    /// Resolves and validates a multi-status `response { ... }` block,
    /// returning one [`ResponseStatusInfo`] per declared status.
    ///
    /// Returns an empty `Vec` for the bare `response <T>` form (or no response),
    /// in which case the caller leaves `EndpointInfo.response` as the single
    /// source of truth and no envelope is generated.
    ///
    /// Validates (see `docs/design-decisions.md`, multi-status responses design,
    /// decisions 1 and 2):
    /// - Each entry's body type resolves (`resolve_type_expr` reports unknown
    ///   types itself); typeless entries (`204`) carry `ty: None`. A
    ///   file-bearing struct (a binary download) is rejected with a targeted
    ///   message — multi-status bodies are JSON-only.
    /// - Each typed entry must name a STRUCT type: `List<T>`, scalars,
    ///   `Option<T>`, enums, etc. are rejected (the bare `response <Type>` form
    ///   keeps supporting them). The envelope's `body` slot serializes through
    ///   the struct machinery in every target — Python in particular emits
    ///   `T.model_validate(...)` / `body.model_dump_json()`, which only exist on
    ///   pydantic models, so a non-struct `T` would generate code that fails at
    ///   runtime. Relaxing this later is additive.
    /// - All typed entries share ONE body type (decision 1 / Option A — no
    ///   unions).
    /// - Every status is in the success range 200..=299 (failures belong in the
    ///   `error { }` block).
    /// - Bodyless statuses (204, 205) must be typeless — HTTP forbids a body on
    ///   them, and generated servers would silently drop one.
    /// - Status codes are unique within the block.
    ///
    /// An empty `response { }` never reaches here as a non-bare block: the
    /// parser reports it and yields an empty list, which the `is_empty` early
    /// return treats the same as the bare form.
    fn check_response_statuses(&mut self, ep: &EndpointDecl) -> Vec<ResponseStatusInfo> {
        if ep.response_statuses.is_empty() {
            return Vec::new();
        }

        let mut resolved: Vec<ResponseStatusInfo> = Vec::with_capacity(ep.response_statuses.len());
        let mut seen_statuses: HashSet<u16> = HashSet::new();
        // The shared body type `T`: the first typed entry's resolved type. Every
        // later typed entry must equal it (decision 1). Seeded only from entries
        // that RESOLVED — an entry rejected above (unknown type, file-bearing,
        // non-struct) must not pin `shared_ty` to `Type::Error` and thereby
        // suppress a genuine mismatch between the valid entries around it
        // (e.g. `200: Bogus  201: User  202: Receipt` must report both the
        // unknown `Bogus` AND the User/Receipt mismatch in one pass).
        let mut shared_ty: Option<Type> = None;

        for rs in &ep.response_statuses {
            // 2xx-only: failures belong in `error { }` (decision 2).
            if !(200..=299).contains(&rs.status) {
                self.error(
                    format!(
                        "endpoint `{}`: response status {} is not a success code (2xx); failures belong in the `error {{ }}` block",
                        ep.name, rs.status
                    ),
                    rs.span,
                );
            }

            // Bodyless statuses: HTTP (RFC 9110) forbids a body on 204 (No
            // Content) and 205 (Reset Content), and the generated servers
            // could not honor a typed entry either way: on a 204, Go's
            // net/http and Express silently drop body writes; on a 205
            // (which neither framework suppresses) the body would hit the
            // wire as an illegal response. Reject the typed entry rather
            // than generating a contract the wire cannot honor.
            if matches!(rs.status, 204 | 205) && rs.ty.is_some() {
                self.error(
                    format!(
                        "endpoint `{}`: status {} cannot declare a body type — HTTP forbids a body on {} responses; list it typeless (`{}`)",
                        ep.name, rs.status, rs.status, rs.status
                    ),
                    rs.span,
                );
            }

            // Unique statuses.
            if !seen_statuses.insert(rs.status) {
                self.error(
                    format!(
                        "endpoint `{}`: duplicate response status {}",
                        ep.name, rs.status
                    ),
                    rs.span,
                );
            }

            // Resolve the body type. Unknown types error inside
            // `resolve_type_expr` (returning `Type::Error`) — adding a second
            // diagnostic here would be a cascade, so don't. Resolve with the
            // file-bearing gate up (like the bare response path) and reject a
            // file-bearing struct AFTER resolution with a targeted message:
            // letting the gate stay down would surface the generic "body-only
            // type usable only in `body`/`response` position" error, which is
            // confusing here — the type IS in response position; the actual
            // problem is that multi-status bodies are JSON-only.
            let ty = rs.ty.as_ref().map(|te| {
                let prev = self.file_bearing_struct_allowed;
                self.file_bearing_struct_allowed = true;
                let resolved_ty = self.resolve_type_expr(te);
                self.file_bearing_struct_allowed = prev;
                if resolved_ty == Type::Error {
                    return Type::Error;
                }
                // `(name, is_file_bearing)` when the entry names a struct;
                // cloned out of the lookup so the borrow ends before `error`.
                let struct_info = match &resolved_ty {
                    Type::Named(name) => self
                        .lookup_struct(name)
                        .map(|si| (name.clone(), si.is_file_bearing)),
                    _ => None,
                };
                match struct_info {
                    Some((name, true)) => {
                        self.error(
                            format!(
                                "endpoint `{}`: response status {} cannot carry file-bearing struct `{}` — a multi-status `response {{ }}` block is JSON-only; use the bare `response <Type>` form for a binary download",
                                ep.name, rs.status, name
                            ),
                            rs.span,
                        );
                        Type::Error
                    }
                    Some((_, false)) => resolved_ty,
                    // Non-struct entry (`List<T>`, a scalar, `Option<T>`, an
                    // enum, ...): the envelope's `body` slot serializes through
                    // the struct machinery in every target (Python emits
                    // `T.model_validate(...)` / `body.model_dump_json()`, which
                    // only exist on pydantic models), so reject it here instead
                    // of generating code that fails at runtime.
                    None => {
                        self.error(
                            format!(
                                "endpoint `{}`: response status {} body type must be a named struct (got `{}`) — a multi-status `response {{ }}` block carries one struct shape; use the bare `response <Type>` form for list or scalar responses",
                                ep.name, rs.status, resolved_ty
                            ),
                            rs.span,
                        );
                        Type::Error
                    }
                }
            });

            // Shared body type (decision 1): every typed entry must match the
            // first VALID typed entry. Typeless entries are exempt, and an
            // entry that failed to resolve (`Type::Error`, already errored
            // above) neither seeds nor compares — its own diagnostic stands and
            // it must not cascade here or mask the valid entries' mismatches.
            if let Some(this_ty) = &ty
                && *this_ty != Type::Error
            {
                match &shared_ty {
                    None => shared_ty = Some(this_ty.clone()),
                    Some(first_ty) => {
                        if first_ty != this_ty {
                            self.error(
                                format!(
                                    "endpoint `{}`: all typed response statuses must share one body type (Option A); found `{}` and `{}` — use separate endpoints or `error {{ }}` for different shapes",
                                    ep.name, first_ty, this_ty
                                ),
                                rs.span,
                            );
                        }
                    }
                }
            }

            resolved.push(ResponseStatusInfo {
                status: rs.status,
                ty,
            });
        }

        resolved
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
    use crate::types::Type;

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
            struct Post { id: Int }
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
            struct Post { id: Int }
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
            struct Post { id: Int }
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
            struct Post { id: Int }
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
            struct Post { id: Int }
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
            struct Post { id: Int }
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
            struct Post { id: Int }
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
            struct Post { id: Int }
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
            struct Post { id: Int }
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
            struct Post { id: Int }
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
            struct Post { id: Int }
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
            struct Post { id: Int }
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
            struct User { id: Int  name: String }
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
            struct User { id: Int  name: String }
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
            struct User { id: Int  name: String }
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
            struct User { id: Int  name: String }
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
            struct User { id: Int  name: String }
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
            struct User { id: Int  name: String  email: String }
            endpoint createUser: POST "/api/users" {
                body User omit { id }
                response User
                query {
                    notify: Bool = true
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
            struct User { id: Int }
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
            struct User { id: Int  name: String }
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
            struct User { id: Int  name: String }
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
            struct User { id: Int  name: String }
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
            struct User { id: Int  name: String }
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
            struct User { id: Int  name: String }
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
            struct User { id: Int  name: String }
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
            struct User { id: Int  name: String  email: String }
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
            struct User { id: Int  name: String  email: String }
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
            struct User { id: Int }
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
            struct User { id: Int }
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
            struct User { id: Int }
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
            struct User { id: Int }
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
            struct User { id: Int  name: String }
            endpoint listUsers: GET "/api/users" {
                query {
                    page: Int = 1
                    limit: Int = 20
                    search: String = "default"
                    active: Bool = true
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
            struct User { id: Int }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    authorization: String
                    idempotencyKey: String
                    xRequestId: Option<String>
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
            struct User { id: Int }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    rateLimit: String as "X-RateLimit-Limit"
                    etag: String as "ETag"
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
            struct Post { id: Int }
            endpoint getPost: GET "/api/posts/{id}" {
                response Post headers {
                    ratelimitRemaining: Int as "X-RateLimit-Remaining"
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
            struct User { id: Int }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    retries: Int = "nope"
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
            struct User { id: Int }
            endpoint getUser: GET "/api/users/{id}" {
                response User headers {
                    ratelimitRemaining: Int = 100
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
            struct User { id: Int }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    xRequestId: String
                    tracing: String as "X-Request-Id"
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
            struct User { id: Int }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    a: String as "X-Trace"
                    b: String as "x-trace"
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
            struct Post { id: Int }
            endpoint getPost: GET "/api/posts/{id}" {
                response Post headers {
                    rateLimit: Int as "X-Limit"
                    ceiling: Int as "x-limit"
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
            struct User { id: Int }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    id: String as "X-Id"
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
            struct User { id: Int }
            endpoint listUsers: GET "/api/users" {
                query {
                    trace: Option<String>
                }
                headers {
                    trace: String as "X-Trace"
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
            struct User { id: Int }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    token: String as "X-A"
                    token: String as "X-B"
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
            struct User { id: Int }
            endpoint getUser: GET "/api/users/{id}" {
                response User headers {
                    rate: Int as "X-A"
                    rate: Int as "X-B"
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
            struct User { id: Int }
            endpoint listUsers: GET "/api/users" {
                query {
                    page: Int = "not_a_number"
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
            struct User { id: Int }
            endpoint listUsers: GET "/api/users" {
                query {
                    active: Int = true
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
            struct User { id: Int }
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
            struct Payload { data: String }

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
            struct User { id: Int  name: String  email: String  age: Int }
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
            struct User { id: Int  name: String }
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
            struct User { id: Int  name: String }
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
                id: Int
                name: String where self.length > 0 && self.length <= 100
                email: String
                age: Int where self >= 0 && self <= 150
                bio: Option<String>
            }

            endpoint createUser: POST "/api/users" {
                body User omit { id, bio }
                response User
                query {
                    notify: Bool = true
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
            struct AvatarUpload { avatar: File  caption: String }
            struct UploadResult { url: String }
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
            struct Doc { data: File }
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
            struct MaybeUpload { avatar: Option<File>  caption: String }
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
            struct AvatarUpload { avatar: File  caption: String }
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
            struct AvatarUpload { avatar: File  caption: String }
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
            struct Doc { data: File }
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
            struct MaybeUpload { avatar: Option<File>  caption: String }
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
            struct User { id: Int }
            endpoint getUser: GET "/api/users/{id}" {
                query {
                    f: File
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
            struct User { id: Int }
            endpoint getUser: GET "/api/users/{id}" {
                headers {
                    token: File
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
            struct Gallery { photos: List<File> }
            "#,
            "`File` is only allowed",
        );
    }

    // REJECT: File inside `Map<String, File>` as a field.
    #[test]
    fn file_reject_map_of_file_field() {
        assert_has_error(
            r#"
            struct Bucket { blobs: Map<String, File> }
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
            struct AvatarUpload { avatar: File  caption: String }
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
            struct AvatarUpload { avatar: File }
            struct Profile { upload: AvatarUpload  name: String }
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
            struct Profile { upload: AvatarUpload  name: String }
            struct AvatarUpload { avatar: File }
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
            struct AvatarUpload { avatar: File  caption: String }
            "#,
            "body-only type",
        );
    }

    // REJECT: a RESPONSE that is a file-bearing struct mixing File + scalars.
    #[test]
    fn file_reject_response_mixed_body() {
        assert_has_error(
            r#"
            struct Bad { data: File  name: String }
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
            struct TwoFiles { a: File  b: File }
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
            struct Doc { data: File }
            endpoint download: GET "/api/doc/{id}" {
                response Doc headers {
                    etag: String as "ETag"
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
            struct Doc { data: File }
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
            struct Doc { data: File }
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
            struct Upload { avatar: File  rotation: Int  crop: Bool  caption: Option<String> }
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
            struct Upload { avatar: File  tags: List<String> }
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
            struct Meta { key: String }
            struct Upload { avatar: File  meta: Meta }
            endpoint upload: POST "/api/upload" {
                body Upload
            }
            "#,
            "cannot be sent as a form field",
        );
    }

    // REJECT: a multipart body with a `DateTime` field (and `Option<DateTime>`).
    // `DateTime` is a first-class scalar everywhere *except* a multipart form
    // field: `is_multipart_field_type`'s whitelist is `Int`/`Float`/`Bool`/
    // `String`/`File` only, so a timestamp in a file-upload body must error at
    // check time rather than mis-generate the per-target form encode/parse. This
    // is the documented "deferred: DateTime as a multipart form field" contract;
    // see `docs/design-decisions.md` (DateTime & UUID scalar types).
    #[test]
    fn file_reject_multipart_datetime_field() {
        assert_has_error(
            r#"
            struct Upload { avatar: File  capturedAt: DateTime }
            endpoint upload: POST "/api/upload" {
                body Upload
            }
            "#,
            "cannot be sent as a form field",
        );
        assert_has_error(
            r#"
            struct Upload { avatar: File  capturedAt: Option<DateTime> }
            endpoint upload: POST "/api/upload" {
                body Upload
            }
            "#,
            "cannot be sent as a form field",
        );
    }

    // ── `Money` position restriction ─────────────────────────────────
    // `Money` is a composite `{ amount, currency }` with no scalar URL/header
    // encoding, so it is legal only in struct/body fields and responses — never a
    // query param or header. Each target gates its one-time `Money` definition on
    // body/field/response use, so an un-rejected query/header `Money` would emit a
    // dangling type reference (non-compiling Go/Python, dangling OpenAPI `$ref`).
    // See `docs/design-decisions.md` (Money type).

    // REJECT: a bare `Money` query param.
    #[test]
    fn money_reject_query_param() {
        assert_has_error(
            r#"
            endpoint search: GET "/search" {
                query { price: Money }
                response String
            }
            "#,
            "cannot be a `Money`",
        );
    }

    // REJECT: a `Money` reached through a generic (`Option`/`List`) query param.
    #[test]
    fn money_reject_query_param_nested() {
        assert_has_error(
            r#"
            endpoint search: GET "/search" {
                query { price: Option<Money> }
                response String
            }
            "#,
            "cannot be a `Money`",
        );
        assert_has_error(
            r#"
            endpoint search: GET "/search" {
                query { prices: List<Money> }
                response String
            }
            "#,
            "cannot be a `Money`",
        );
    }

    // REJECT: a `Money` request header and a `Money` response header.
    #[test]
    fn money_reject_headers() {
        assert_has_error(
            r#"
            endpoint pay: POST "/pay" {
                headers { amount: Money }
                response String
            }
            "#,
            "header `amount` cannot be a `Money`",
        );
        assert_has_error(
            r#"
            endpoint pay: POST "/pay" {
                response String headers { balance: Money }
            }
            "#,
            "response header `balance` cannot be a `Money`",
        );
    }

    // ACCEPT: `Money` in struct/body fields and as a response is fine (the legal
    // positions) — the restriction is query/header-only.
    #[test]
    fn money_accept_body_field_and_response() {
        assert_no_errors(
            r#"
            struct Invoice { total: Money }
            endpoint create: POST "/invoices" {
                body Invoice
                response Money
            }
            "#,
        );
    }

    // ── Pagination ──────────────────────────────────────────────────

    #[test]
    fn pagination_offset_on_list_resolves() {
        use phoenix_parser::ast::PaginationMode;
        let ep = first_endpoint(
            r#"
            struct Post { id: Int }
            endpoint listPosts: GET "/posts" {
                response List<Post>
                pagination { offset }
            }
            "#,
        );
        let pg = ep.pagination.expect("pagination should resolve");
        assert!(matches!(pg.mode, PaginationMode::Offset));
        // The item type is the list element (`Post`).
        assert!(matches!(pg.item_type, crate::types::Type::Named(ref n) if n.ends_with("Post")));
    }

    #[test]
    fn pagination_cursor_on_list_resolves() {
        use phoenix_parser::ast::PaginationMode;
        let ep = first_endpoint(
            r#"
            struct Post { id: Int }
            endpoint listPosts: GET "/posts" {
                response List<Post>
                pagination { cursor }
            }
            "#,
        );
        let pg = ep.pagination.expect("pagination should resolve");
        assert!(matches!(pg.mode, PaginationMode::Cursor));
    }

    #[test]
    fn pagination_absent_is_none() {
        let ep = first_endpoint(
            r#"
            struct Post { id: Int }
            endpoint listPosts: GET "/posts" { response List<Post> }
            "#,
        );
        assert!(ep.pagination.is_none());
    }

    #[test]
    fn pagination_rejects_non_list_response() {
        assert_has_error(
            r#"
            struct Post { id: Int }
            endpoint getPost: GET "/posts/{id}" {
                response Post
                pagination { offset }
            }
            "#,
            "requires the response to be a `List<T>`",
        );
    }

    #[test]
    fn pagination_rejects_option_list_response() {
        assert_has_error(
            r#"
            struct Post { id: Int }
            endpoint listPosts: GET "/posts" {
                response Option<List<Post>>
                pagination { offset }
            }
            "#,
            "requires the response to be a `List<T>`",
        );
    }

    #[test]
    fn pagination_rejects_combination_with_response_headers() {
        assert_has_error(
            r#"
            struct Post { id: Int }
            endpoint listPosts: GET "/posts" {
                response List<Post> headers { totalCount: Int as "X-Total" }
                pagination { offset }
            }
            "#,
            "cannot be combined",
        );
    }

    // ---- Multi-status `response { ... }` block (decisions 1, 2, 4, 6) ----

    #[test]
    fn multi_status_two_typed_same_type_accepts() {
        let src = r#"
            struct User { id: Int }
            endpoint createUser: POST "/users" {
                response { 200: User  201: User }
            }
        "#;
        assert_no_errors(src);
        let ep = first_endpoint(src);
        assert_eq!(ep.response_statuses.len(), 2);
        assert_eq!(ep.response_statuses[0].status, 200);
        assert_eq!(
            ep.response_statuses[0].ty,
            Some(Type::Named("User".to_string()))
        );
        assert_eq!(ep.response_statuses[1].status, 201);
        assert_eq!(
            ep.response_statuses[1].ty,
            Some(Type::Named("User".to_string()))
        );
        // The shared body type `T` is mirrored into `response`.
        assert_eq!(ep.response, Some(Type::Named("User".to_string())));
    }

    #[test]
    fn multi_status_typed_and_typeless_accepts() {
        let src = r#"
            struct User { id: Int }
            endpoint createUser: POST "/users" {
                response { 200: User  204 }
            }
        "#;
        assert_no_errors(src);
        let ep = first_endpoint(src);
        assert_eq!(ep.response_statuses.len(), 2);
        assert_eq!(ep.response_statuses[0].status, 200);
        assert_eq!(
            ep.response_statuses[0].ty,
            Some(Type::Named("User".to_string()))
        );
        assert_eq!(ep.response_statuses[1].status, 204);
        assert_eq!(ep.response_statuses[1].ty, None);
        // Shared `T` still comes from the one typed entry.
        assert_eq!(ep.response, Some(Type::Named("User".to_string())));
    }

    #[test]
    fn multi_status_typeless_first_mirrors_shared_type() {
        // The shared-`T` mirror scans for the FIRST TYPED entry (`find_map`),
        // not the first entry: a typeless status listed before the typed one
        // must still mirror `T` into `response`.
        let src = r#"
            struct User { id: Int }
            endpoint createUser: POST "/users" {
                response { 204  200: User }
            }
        "#;
        assert_no_errors(src);
        let ep = first_endpoint(src);
        assert_eq!(ep.response_statuses.len(), 2);
        assert_eq!(ep.response_statuses[0].status, 204);
        assert_eq!(ep.response_statuses[0].ty, None);
        assert_eq!(ep.response_statuses[1].status, 200);
        assert_eq!(
            ep.response_statuses[1].ty,
            Some(Type::Named("User".to_string()))
        );
        assert_eq!(ep.response, Some(Type::Named("User".to_string())));
    }

    #[test]
    fn multi_status_all_typeless_accepts() {
        let src = r#"
            endpoint accept: POST "/jobs" {
                response { 202  204 }
            }
        "#;
        assert_no_errors(src);
        let ep = first_endpoint(src);
        assert_eq!(ep.response_statuses.len(), 2);
        assert_eq!(ep.response_statuses[0].status, 202);
        assert_eq!(ep.response_statuses[0].ty, None);
        assert_eq!(ep.response_statuses[1].status, 204);
        assert_eq!(ep.response_statuses[1].ty, None);
        // No typed entry → no shared body type.
        assert_eq!(ep.response, None);
    }

    #[test]
    fn bare_response_leaves_statuses_empty() {
        // Regression: the common bare-response case is untouched.
        let src = r#"
            struct User { id: Int }
            endpoint getUser: GET "/users/{id}" {
                response User
            }
        "#;
        assert_no_errors(src);
        let ep = first_endpoint(src);
        assert!(ep.response_statuses.is_empty());
        assert_eq!(ep.response, Some(Type::Named("User".to_string())));
    }

    #[test]
    fn multi_status_differing_body_types_rejected() {
        assert_has_error(
            r#"
            struct User { id: Int }
            struct Receipt { id: Int }
            endpoint createUser: POST "/users" {
                response { 200: User  201: Receipt }
            }
            "#,
            "must share one body type",
        );
    }

    #[test]
    fn multi_status_non_2xx_rejected() {
        assert_has_error(
            r#"
            struct User { id: Int }
            struct NotFound { id: Int }
            endpoint getUser: GET "/users/{id}" {
                response { 200: User  404: NotFound }
            }
            "#,
            "not a success code",
        );
    }

    #[test]
    fn multi_status_duplicate_status_rejected() {
        assert_has_error(
            r#"
            struct User { id: Int }
            endpoint getUser: GET "/users/{id}" {
                response { 200: User  200: User }
            }
            "#,
            "duplicate response status",
        );
    }

    #[test]
    fn multi_status_with_request_headers_allowed() {
        // Request headers are orthogonal to multi-status (they don't wrap the
        // return value), so this combination is FINE. NOTE: the multi-status +
        // RESPONSE-headers combo never reaches sema — the parser rejects an
        // inline `headers { ... }` after a response block with its own targeted
        // error and discards the block. The sema check for it is therefore
        // defensive; this test instead pins that a standalone (request)
        // `headers` block coexists cleanly with a multi-status block.
        let src = r#"
            struct User { id: Int }
            endpoint createUser: POST "/users" {
                headers { x: String }
                response { 200: User  201: User }
            }
        "#;
        assert_no_errors(src);
        let ep = first_endpoint(src);
        assert_eq!(ep.headers.len(), 1);
        assert_eq!(ep.response_statuses.len(), 2);
        assert!(ep.response_headers.is_empty());
    }

    #[test]
    fn multi_status_typed_204_rejected() {
        // HTTP forbids a body on 204; a typed entry would generate a server
        // that silently drops the handler-supplied body.
        assert_has_error(
            r#"
            struct User { id: Int }
            endpoint deleteUser: DELETE "/users/{id}" {
                response { 204: User }
            }
            "#,
            "cannot declare a body type",
        );
    }

    #[test]
    fn multi_status_typed_205_rejected_typeless_accepted() {
        // 205 (Reset Content) is bodyless like 204: a typed entry is rejected,
        // a typeless one accepted. 205 matters at the codegen layer — neither
        // Express nor Go's net/http auto-suppresses a body on it (unlike 204),
        // so the generated servers' body-shape guard is the only protection;
        // sema letting a typeless 205 through is what makes that path live.
        assert_has_error(
            r#"
            struct User { id: Int }
            endpoint resetUser: PUT "/users/{id}" {
                response { 205: User }
            }
            "#,
            "cannot declare a body type",
        );
        let src = r#"
            struct User { id: Int }
            endpoint resetUser: PUT "/users/{id}" {
                response { 200: User  205 }
            }
        "#;
        assert_no_errors(src);
        let ep = first_endpoint(src);
        assert_eq!(ep.response_statuses.len(), 2);
        assert_eq!(ep.response_statuses[1].status, 205);
        assert_eq!(ep.response_statuses[1].ty, None);
    }

    #[test]
    fn multi_status_unknown_type_single_diagnostic() {
        // `resolve_type_expr` reports the unknown type itself; the block path
        // must not add a second "unknown response type" diagnostic on top.
        let errors = check_source(
            r#"
            endpoint getThing: GET "/things/{id}" {
                response { 200: Bogus  204 }
            }
            "#,
        );
        assert_eq!(
            errors.len(),
            1,
            "an unknown type must produce exactly one diagnostic, got: {errors:?}"
        );
        assert!(
            errors[0].contains("unknown type `Bogus`"),
            "the one diagnostic should be resolve_type_expr's, got: {errors:?}"
        );
    }

    #[test]
    fn multi_status_unknown_first_type_does_not_mask_mismatch() {
        // `shared_ty` is seeded from the first VALID typed entry: an unresolved
        // first entry (`200: Bogus`) must not pin it to `Type::Error` and
        // thereby suppress the genuine User-vs-Receipt mismatch behind it —
        // both errors must surface in ONE pass, not one per fix-recompile.
        let errors = check_source(
            r#"
            struct User { id: Int }
            struct Receipt { id: Int }
            endpoint createUser: POST "/users" {
                response { 200: Bogus  201: User  202: Receipt }
            }
            "#,
        );
        assert!(
            errors.iter().any(|e| e.contains("unknown type `Bogus`")),
            "the unknown first entry keeps its own diagnostic: {errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("must share one body type")
                && e.contains("`User`")
                && e.contains("`Receipt`")),
            "the User/Receipt mismatch must not be masked by the unknown first entry: {errors:?}"
        );
    }

    #[test]
    fn multi_status_file_bearing_struct_rejected() {
        // A file-bearing struct is a binary download, which only the bare
        // `response <Type>` form supports; the block form must reject it with
        // the targeted JSON-only message, not the generic "body-only type"
        // error (the type IS in response position) and not an "unknown type"
        // cascade.
        let errors = check_source(
            r#"
            struct Doc { data: File }
            endpoint getDoc: GET "/docs/{id}" {
                response { 200: Doc  204 }
            }
            "#,
        );
        assert!(
            errors.iter().any(|e| e.contains("JSON-only")),
            "expected the targeted file-bearing rejection, got: {errors:?}"
        );
        assert!(
            !errors.iter().any(|e| e.contains("body-only type")),
            "the generic body-only-position error is misleading here: {errors:?}"
        );
        assert!(
            !errors.iter().any(|e| e.contains("unknown")),
            "no unknown-type cascade expected: {errors:?}"
        );
    }

    #[test]
    fn multi_status_list_body_rejected() {
        // A `List<T>` body is bare-form-only: the envelope's `body` slot
        // serializes through the struct machinery (Python emits
        // `T.model_validate(...)` / `body.model_dump_json()`, which only exist
        // on pydantic models), so a non-struct entry must be rejected here
        // instead of generating Python that fails at runtime. Both entries name
        // the same list type, so there must also be NO shared-type cascade on
        // top of the per-entry rejections.
        let errors = check_source(
            r#"
            struct Post { id: Int }
            endpoint syncPosts: POST "/posts/sync" {
                response { 200: List<Post>  201: List<Post> }
            }
            "#,
        );
        assert!(
            errors.iter().any(|e| e.contains("must be a named struct")),
            "expected the non-struct body rejection, got: {errors:?}"
        );
        assert!(
            !errors
                .iter()
                .any(|e| e.contains("must share one body type")),
            "rejected entries must not also cascade the shared-type error: {errors:?}"
        );
    }

    #[test]
    fn multi_status_scalar_body_rejected() {
        // Same restriction for a scalar body (`200: String`).
        assert_has_error(
            r#"
            endpoint getStatus: GET "/status" {
                response { 200: String  204 }
            }
            "#,
            "must be a named struct",
        );
    }

    #[test]
    fn multi_status_datetime_body_rejected() {
        // `DateTime` is a first-class scalar, but a multi-status `response { }`
        // body is still struct-only: the envelope's `body` slot serializes
        // through the pydantic struct machinery (`T.model_validate(...)`), which
        // a bare `datetime` has no method for. So a bare `DateTime` status body
        // is impossible input — it errors here rather than reaching codegen — and
        // a `DateTime` instant must use the bare `response DateTime` form (which
        // IS supported and round-trip-tested). This pins the new scalar to the
        // same contract the `String` case above asserts.
        assert_has_error(
            r#"
            endpoint getPublishedAt: GET "/published" {
                response { 200: DateTime  204 }
            }
            "#,
            "must be a named struct",
        );
    }

    #[test]
    fn multi_status_with_pagination_rejected() {
        // Precedence: the combo error fires, NOT a confusing "pagination
        // requires a `List<T>` response" error.
        let errors = check_source(
            r#"
            struct User { id: Int }
            endpoint listUsers: GET "/users" {
                response { 200: User  201: User }
                pagination { offset }
            }
            "#,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("cannot also be paginated")),
            "expected the multi-status+pagination combo error, got: {errors:?}"
        );
        assert!(
            !errors
                .iter()
                .any(|e| e.contains("requires the response to be a `List<T>`")),
            "should not surface the confusing pagination-requires-List error: {errors:?}"
        );
    }

    // ── Generated-envelope name collisions ──────────────────────────

    #[test]
    fn envelope_collision_multi_status_response_rejected() {
        assert_has_error(
            r#"
            struct User { id: Int }
            struct UpsertUserResponse { x: Int }
            endpoint upsertUser: PUT "/u/{id}" {
                response { 200: User  201: User }
            }
            "#,
            "collides with a user-defined type",
        );
    }

    #[test]
    fn envelope_collision_pagination_page_rejected() {
        assert_has_error(
            r#"
            struct Post { id: Int }
            struct ListPostsPage { x: Int }
            endpoint listPosts: GET "/p" {
                response List<Post>
                pagination { offset }
            }
            "#,
            "collides with a user-defined type",
        );
    }

    #[test]
    fn envelope_collision_response_headers_result_rejected() {
        assert_has_error(
            r#"
            struct User { id: Int }
            struct GetUserResult { x: Int }
            endpoint getUser: GET "/u/{id}" {
                response User headers { total: Int as "X-Total" }
            }
            "#,
            "collides with a user-defined type",
        );
    }

    #[test]
    fn envelope_collision_with_enum_rejected() {
        // The collision check covers enums too, not just structs.
        assert_has_error(
            r#"
            struct Post { id: Int }
            enum ListPostsPage { A  B }
            endpoint listPosts: GET "/p" {
                response List<Post>
                pagination { offset }
            }
            "#,
            "collides with a user-defined type",
        );
    }

    #[test]
    fn envelope_name_without_the_feature_is_not_a_collision() {
        // `ListPostsPage` only collides when the endpoint actually declares
        // pagination; a like-named struct alongside a plain endpoint is fine.
        assert_no_errors(
            r#"
            struct Post { id: Int }
            struct ListPostsPage { x: Int }
            endpoint listPosts: GET "/p" {
                response List<Post>
            }
            "#,
        );
    }

    #[test]
    fn envelope_collision_between_two_endpoints_reported_at_name_level() {
        // `getUser` and `GetUser` collide on the exported name itself (Go
        // method/handler), so the pair is rejected there — and the same-stem
        // `GetUserResult` type collision is the same mistake, suppressed.
        let errors = check_source(
            r#"
            struct User { id: Int }
            endpoint getUser: GET "/u/{id}" {
                response User headers { total: Int as "X-Total" }
            }
            endpoint GetUser: GET "/uu/{id}" {
                response User headers { total: Int as "X-Total" }
            }
            "#,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("both export the generated name `GetUser`")),
            "expected the exported-name collision error: {errors:?}"
        );
        assert!(
            !errors
                .iter()
                .any(|e| e.contains("collides with the type generated")),
            "the same-stem type collision is the same mistake and should be suppressed: {errors:?}"
        );
    }

    #[test]
    fn exported_name_collision_rejected_without_any_generated_types() {
        // The exported-name collision is independent of envelopes and bodies:
        // two plain endpoints whose names differ only in first-letter case
        // emit two `GetUser` methods on the generated Go client struct (and
        // two identical handler-interface methods) even though neither
        // generates a single type.
        assert_has_error(
            r#"
            struct User { id: Int }
            endpoint getUser: GET "/u/{id}" {
                response User
            }
            endpoint GetUser: GET "/uu/{id}" {
                response User
            }
            "#,
            "both export the generated name `GetUser`",
        );
    }

    #[test]
    fn exported_name_predicate_is_capitalize_equality_not_case_insensitivity() {
        // `getUser` and `getuSer` are case-insensitively equal but export as
        // `GetUser` vs `GetuSer` — distinct Go methods, no collision. The
        // check must not over-reject by comparing full-lowercased names.
        assert_no_errors(
            r#"
            struct User { id: Int }
            endpoint getUser: GET "/u/{id}" {
                response User
            }
            endpoint getuSer: GET "/uu/{id}" {
                response User
            }
            "#,
        );
    }

    #[test]
    fn distinct_endpoint_envelopes_do_not_collide() {
        assert_no_errors(
            r#"
            struct User { id: Int }
            endpoint getUser: GET "/u/{id}" {
                response User headers { total: Int as "X-Total" }
            }
            endpoint getAdmin: GET "/a/{id}" {
                response User headers { total: Int as "X-Total" }
            }
            "#,
        );
    }

    #[test]
    fn duplicate_endpoint_name_does_not_also_report_envelope_collision() {
        // A literally duplicated endpoint name is one mistake; it gets the
        // duplicate-name diagnostic — not a second envelope-collision one,
        // and not an exported-name collision against its own first
        // declaration either.
        let errors = check_source(
            r#"
            struct User { id: Int }
            endpoint getUser: GET "/u/{id}" {
                response User headers { total: Int as "X-Total" }
            }
            endpoint getUser: GET "/uu/{id}" {
                response User headers { total: Int as "X-Total" }
            }
            "#,
        );
        assert!(
            errors.iter().any(|e| e.contains("duplicate endpoint name")),
            "expected the duplicate-name error: {errors:?}"
        );
        assert!(
            !errors
                .iter()
                .any(|e| e.contains("collides with the type generated")),
            "duplicate name should not also surface a generated-type collision: {errors:?}"
        );
        assert!(
            !errors
                .iter()
                .any(|e| e.contains("both export the generated name")),
            "duplicate name should not also surface an exported-name collision: {errors:?}"
        );
    }

    #[test]
    fn duplicate_endpoint_name_reports_user_type_collision_once() {
        // Both declarations of a duplicated endpoint name predict the same
        // generated names, but only the first (non-duplicate) one should report
        // the clash against a user type — the second is the duplicate-name
        // mistake, already diagnosed as such.
        let errors = check_source(
            r#"
            struct User { id: Int }
            struct GetUserResult { x: Int }
            endpoint getUser: GET "/u/{id}" {
                response User headers { total: Int as "X-Total" }
            }
            endpoint getUser: GET "/uu/{id}" {
                response User headers { total: Int as "X-Total" }
            }
            "#,
        );
        let user_type_collisions = errors
            .iter()
            .filter(|e| e.contains("collides with a user-defined type"))
            .count();
        assert_eq!(
            user_type_collisions, 1,
            "expected exactly one user-type collision error: {errors:?}"
        );
    }

    #[test]
    fn exported_name_collision_reports_user_type_collision_once() {
        // Same discipline as the duplicate-name case: `getUser` and `GetUser`
        // predict the identical generated names, so the user-type clash
        // against `GetUserResult` is reported once, by the endpoint that owns
        // the exported name — the second endpoint's name collision is the
        // mistake already diagnosed.
        let errors = check_source(
            r#"
            struct User { id: Int }
            struct GetUserResult { x: Int }
            endpoint getUser: GET "/u/{id}" {
                response User headers { total: Int as "X-Total" }
            }
            endpoint GetUser: GET "/uu/{id}" {
                response User headers { total: Int as "X-Total" }
            }
            "#,
        );
        let user_type_collisions = errors
            .iter()
            .filter(|e| e.contains("collides with a user-defined type"))
            .count();
        assert_eq!(
            user_type_collisions, 1,
            "expected exactly one user-type collision error: {errors:?}"
        );
    }

    #[test]
    fn exported_name_collision_with_differing_features_cascades() {
        // Deliberate cascade: when the colliding endpoints declare *different*
        // features, the suppressed endpoint may claim a generated name the
        // owner never did — here `GetUser` (body) claims `GetUserBody`, which
        // `getUser` (headers) does not — so its clash with the user struct
        // goes unreported this compile. The exported-name collision is the
        // mistake to fix first, and renaming the endpoint changes every
        // generated name, so the clash may not even survive the fix; if it
        // does, it surfaces on the recompile. This pins that the corner stays
        // a single name-level diagnostic — not a double report, not a crash.
        let errors = check_source(
            r#"
            struct User { id: Int }
            struct GetUserBody { x: Int }
            endpoint getUser: GET "/u/{id}" {
                response User headers { total: Int as "X-Total" }
            }
            endpoint GetUser: POST "/uu/{id}" {
                body User
            }
            "#,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("both export the generated name `GetUser`")),
            "expected the exported-name collision error: {errors:?}"
        );
        assert!(
            !errors
                .iter()
                .any(|e| e.contains("collides with a user-defined type")),
            "the suppressed endpoint's `GetUserBody` clash should cascade, not report: {errors:?}"
        );
    }

    // ── Generated request-body type (`<Endpoint>Body` / `<Endpoint>ClientBody`) ──

    #[test]
    fn body_collision_with_user_type_rejected() {
        // Any `body` clause generates `<Endpoint>Body` in every backend.
        assert_has_error(
            r#"
            struct User { id: Int }
            struct CreateUserBody { x: Int }
            endpoint createUser: POST "/u" {
                body User
            }
            "#,
            "collides with a user-defined type",
        );
    }

    #[test]
    fn client_body_collision_with_user_type_rejected() {
        // A multipart body additionally generates `<Endpoint>ClientBody` (Go).
        assert_has_error(
            r#"
            struct AvatarUpload { avatar: File  caption: String }
            struct UploadAvatarClientBody { x: Int }
            endpoint uploadAvatar: POST "/api/avatar" {
                body AvatarUpload
            }
            "#,
            "collides with a user-defined type",
        );
    }

    #[test]
    fn body_name_without_a_body_is_not_a_collision() {
        // `CreateUserBody` only collides when the endpoint actually declares a
        // body; a like-named struct alongside a bodyless endpoint is fine.
        assert_no_errors(
            r#"
            struct User { id: Int }
            struct CreateUserBody { x: Int }
            endpoint createUser: POST "/u" {
                response User
            }
            "#,
        );
    }

    #[test]
    fn client_body_name_with_json_body_is_not_a_collision() {
        // `<Endpoint>ClientBody` is only generated for multipart bodies; a
        // plain JSON body does not claim it.
        assert_no_errors(
            r#"
            struct User { id: Int }
            struct CreateUserClientBody { x: Int }
            endpoint createUser: POST "/u" {
                body User
            }
            "#,
        );
    }

    #[test]
    fn body_collision_between_two_endpoints_rejected() {
        // Without rejection the second endpoint silently binds to the first
        // one's Body struct (codegen's `emitted_derived_types` dedupe is
        // first-wins in every backend). The pair is caught at the
        // exported-name level — same-stem names collide on the Go
        // method/handler name before any type is even considered.
        assert_has_error(
            r#"
            struct User { id: Int }
            endpoint createUser: POST "/u" {
                body User
            }
            endpoint CreateUser: POST "/uu" {
                body User
            }
            "#,
            "both export the generated name `CreateUser`",
        );
    }

    #[test]
    fn body_claimed_alongside_envelope() {
        // A body is not mutually exclusive with the envelopes: an endpoint with
        // both multi-status and a body claims `<Endpoint>Response` AND
        // `<Endpoint>Body`, and a user type colliding with either is rejected.
        assert_has_error(
            r#"
            struct User { id: Int }
            struct UpsertUserBody { x: Int }
            endpoint upsertUser: PUT "/u/{id}" {
                body User
                response { 200: User  201: User }
            }
            "#,
            "collides with a user-defined type",
        );
    }

    #[test]
    fn case_colliding_endpoints_report_one_collision() {
        // Both endpoints claim two colliding type names each
        // (`UpsertUserResponse` and `UpsertUserBody`) on top of the colliding
        // exported name, but the endpoint-name pair is one mistake — exactly
        // one diagnostic, at the name level, with every same-stem type
        // collision suppressed as the same mistake.
        let errors = check_source(
            r#"
            struct User { id: Int }
            endpoint upsertUser: PUT "/u/{id}" {
                body User
                response { 200: User  201: User }
            }
            endpoint UpsertUser: PUT "/uu/{id}" {
                body User
                response { 200: User  201: User }
            }
            "#,
        );
        let name_collisions = errors
            .iter()
            .filter(|e| e.contains("both export the generated name"))
            .count();
        assert_eq!(
            name_collisions, 1,
            "expected exactly one exported-name collision error: {errors:?}"
        );
        assert!(
            !errors
                .iter()
                .any(|e| e.contains("collides with the type generated")),
            "same-stem type collisions are the same mistake and should be suppressed: {errors:?}"
        );
    }

    #[test]
    fn case_colliding_paginated_endpoints_report_one_collision() {
        // The `Page` envelope variant of the same discipline: the pair is
        // rejected at the exported-name level and the same-stem
        // `ListPostsPage` type collision is suppressed.
        let errors = check_source(
            r#"
            struct Post { id: Int }
            endpoint listPosts: GET "/p" {
                response List<Post>
                pagination { offset }
            }
            endpoint ListPosts: GET "/pp" {
                response List<Post>
                pagination { offset }
            }
            "#,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("both export the generated name `ListPosts`")),
            "expected the exported-name collision error: {errors:?}"
        );
        assert!(
            !errors
                .iter()
                .any(|e| e.contains("collides with the type generated")),
            "same-stem type collisions are the same mistake and should be suppressed: {errors:?}"
        );
    }

    #[test]
    fn body_collision_cross_stem_client_body_rejected() {
        // `"ClientBody"` ends with `"Body"`, so the two suffixes collide across
        // *different* stems: `upload` (multipart) generates `UploadClientBody`,
        // and so does `uploadClient` (any body). In Go the second emission is
        // silently skipped by the first-wins dedupe, binding one endpoint to
        // the other's struct.
        assert_has_error(
            r#"
            struct FileDoc { data: File }
            struct Note { text: String }
            endpoint upload: POST "/up" {
                body FileDoc
            }
            endpoint uploadClient: POST "/upc" {
                body Note
            }
            "#,
            "collides with the type generated for endpoint `upload`",
        );
    }

    #[test]
    fn body_collision_cross_stem_client_body_reverse_order_rejected() {
        // Same cross-stem collision, opposite declaration order: here the
        // plain `Body` claims the name first and the multipart endpoint's
        // `ClientBody` is the late claimant.
        assert_has_error(
            r#"
            struct FileDoc { data: File }
            struct Note { text: String }
            endpoint uploadClient: POST "/upc" {
                body Note
            }
            endpoint upload: POST "/up" {
                body FileDoc
            }
            "#,
            "collides with the type generated for endpoint `uploadClient`",
        );
    }

    #[test]
    fn body_collision_two_distinct_endpoint_pairs_two_diagnostics() {
        // One endpoint can collide with two *different* endpoints — same-stem
        // on the exported name (`upload` vs `Upload`) and cross-stem on
        // `ClientBody` (`upload` vs `uploadClient`). Those are two distinct
        // mistakes, so the seeded per-pair suppression must report both: the
        // exported-name diagnostic for the first pair, and the type-collision
        // diagnostic for the second — suppressing only the same-stem
        // `UploadBody` hit, which the name-level error already covers.
        let errors = check_source(
            r#"
            struct FileDoc { data: File }
            struct Note { text: String }
            endpoint Upload: POST "/a" {
                body Note
            }
            endpoint uploadClient: POST "/b" {
                body Note
            }
            endpoint upload: POST "/c" {
                body FileDoc
            }
            "#,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("both export the generated name `Upload`")),
            "expected the same-stem pair to be reported at the name level: {errors:?}"
        );
        let generated_collisions: Vec<_> = errors
            .iter()
            .filter(|e| e.contains("collides with the type generated"))
            .collect();
        assert_eq!(
            generated_collisions.len(),
            1,
            "expected exactly one type-collision diagnostic, for the cross-stem pair: {errors:?}"
        );
        assert!(
            generated_collisions[0].contains("for endpoint `uploadClient`"),
            "expected the cross-stem `ClientBody` pair to be reported: {errors:?}"
        );
    }

    #[test]
    fn collision_with_user_type_declared_after_endpoint_rejected() {
        // The registration pass collects every struct before any endpoint is
        // checked, so a colliding type declared *after* the endpoint must
        // still be found — this pins the two-pass guarantee the
        // `lookup_struct` call relies on.
        assert_has_error(
            r#"
            struct User { id: Int }
            endpoint upsertUser: PUT "/u/{id}" {
                response { 200: User  201: User }
            }
            struct UpsertUserResponse { x: Int }
            "#,
            "collides with a user-defined type",
        );
    }

    // ── Fixed-name multipart helper (`FileUpload`) ──────────────────

    #[test]
    fn file_upload_helper_collision_with_user_type_rejected() {
        // Any multipart endpoint generates the fixed-name `FileUpload` helper
        // struct (Go), so a user type of that name duplicates the declaration
        // in the generated output.
        assert_has_error(
            r#"
            struct FileUpload { x: Int }
            struct Doc { data: File }
            endpoint upload: POST "/up" {
                body Doc
            }
            "#,
            "helper type `FileUpload`",
        );
    }

    #[test]
    fn file_upload_name_without_multipart_is_not_a_collision() {
        // `FileUpload` is only generated for multipart bodies; alongside a
        // plain JSON body the name is free.
        assert_no_errors(
            r#"
            struct FileUpload { x: Int }
            struct Note { text: String }
            endpoint create: POST "/n" {
                body Note
            }
            "#,
        );
    }

    #[test]
    fn file_upload_collision_reported_once_across_multipart_endpoints() {
        // The helper is emitted once and shared by every multipart endpoint,
        // so a user `FileUpload` is one mistake — one diagnostic, at the
        // first multipart endpoint, not one per endpoint.
        let errors = check_source(
            r#"
            struct FileUpload { x: Int }
            struct Doc { data: File }
            endpoint upload: POST "/a" {
                body Doc
            }
            endpoint uploadTwo: POST "/b" {
                body Doc
            }
            "#,
        );
        let helper_collisions = errors
            .iter()
            .filter(|e| e.contains("helper type `FileUpload`"))
            .count();
        assert_eq!(
            helper_collisions, 1,
            "expected exactly one FileUpload collision error: {errors:?}"
        );
    }

    #[test]
    fn file_upload_collision_suppressed_when_only_multipart_endpoint_is_a_duplicate() {
        // Deliberate cascade: when the only multipart endpoint carries a
        // duplicated name, its `FileUpload` claim is suppressed along with
        // its other diagnostics — the duplicate name is the mistake to fix
        // first, and the helper clash surfaces on the recompile after the
        // rename. This pins that the corner stays a cascade, not a crash or
        // a double report.
        let errors = check_source(
            r#"
            struct FileUpload { x: Int }
            struct Note { text: String }
            struct Doc { data: File }
            endpoint upload: POST "/a" {
                body Note
            }
            endpoint upload: POST "/b" {
                body Doc
            }
            "#,
        );
        assert!(
            errors.iter().any(|e| e.contains("duplicate endpoint name")),
            "expected the duplicate-name error: {errors:?}"
        );
        assert!(
            !errors
                .iter()
                .any(|e| e.contains("helper type `FileUpload`")),
            "the duplicate's FileUpload clash should cascade, not report: {errors:?}"
        );
    }
}
