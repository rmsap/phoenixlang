//! OpenAPI 3.1 specification generation for Phoenix Gen.
//!
//! Generates a JSON-formatted OpenAPI 3.1 spec from a parsed and type-checked
//! Phoenix program. Maps Phoenix structs to JSON Schema component schemas,
//! endpoints to path operations, and error variants to HTTP response status codes.

use phoenix_parser::ast::{
    Declaration, EnumDecl, LiteralKind, PaginationMode, Program, StructDecl,
};
use phoenix_sema::Analysis;
use phoenix_sema::checker::{DefaultValue, EndpointInfo, PaginationInfo};
use phoenix_sema::types::Type;
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

use crate::capitalize;

/// Generates an OpenAPI 3.1 JSON specification from a Phoenix program.
///
/// The output is a pretty-printed JSON string representing a complete OpenAPI
/// document with paths, operations, parameters, request bodies, responses,
/// and component schemas.
pub fn generate_openapi(program: &Program, check_result: &Analysis) -> String {
    let mut schemas = Map::new();

    // Emit component schemas for structs and enums
    for decl in &program.declarations {
        match decl {
            Declaration::Struct(s) => {
                if let Some(info) = check_result.module.struct_info_by_name(&s.name) {
                    schemas.insert(s.name.clone(), struct_to_schema(s, info));
                }
            }
            Declaration::Enum(e) => {
                schemas.insert(e.name.clone(), enum_to_schema(e));
            }
            _ => {}
        }
    }

    // Emit derived body schemas for endpoints. A multipart (file-upload) body is
    // inlined into the operation as a `multipart/form-data` schema rather than
    // referenced as a component, so it gets no `{name}Body` component schema.
    for ep in &check_result.endpoints {
        if let Some(ref body) = ep.body
            && !ep.body_is_multipart
        {
            let type_name = format!("{}Body", capitalize(&ep.name));
            schemas.insert(type_name, derived_type_to_schema(body));
        }
        // A paginated endpoint's 200 body is the `<Endpoint>Page` envelope object,
        // emitted as a named component schema (mirroring the `<Endpoint>Body`
        // precedent) and `$ref`d from the response.
        if let Some(ref pagination) = ep.pagination {
            let type_name = format!("{}Page", capitalize(&ep.name));
            schemas.insert(type_name, pagination_page_schema(pagination));
        }
    }

    let paths = build_paths(&check_result.endpoints);

    // Drop component schemas no operation references. A component is emitted for
    // every declared struct/enum, but some are never `$ref`d — a struct used only
    // as a binary-download response (rendered `application/octet-stream`), a
    // multipart body (inlined as `multipart/form-data`), or a plain JSON body (the
    // operation `$ref`s the derived `<Endpoint>Body` instead of the source struct).
    // redocly flags those as `no-unused-components`; pruning keeps the spec
    // reference-clean without changing any referenced schema.
    prune_unreferenced_schemas(&paths, &mut schemas);

    let spec = json!({
        "openapi": "3.1.0",
        "info": {
            "title": "API",
            "version": "1.0.0"
        },
        "servers": [
            { "url": "/" }
        ],
        "paths": paths,
        "components": {
            "schemas": schemas,
        },
    });

    serde_json::to_string_pretty(&spec).expect("OpenAPI spec serialization should not fail")
}

/// Removes component schemas not reachable (transitively via `$ref`) from the
/// operations in `paths`. Roots are the schema names referenced anywhere in
/// `paths`; the reachable set is grown by following each kept schema's own
/// `$ref`s (a struct's fields can reference other structs/enums), then anything
/// outside it is dropped. Mirrors redocly's `no-unused-components` reachability,
/// so an emitted-but-unreferenced schema (binary/multipart/plain-JSON-body source
/// struct) no longer lingers in `components.schemas`.
///
/// Rooting only in `paths` is valid because `components.schemas` is the only
/// component section this generator emits (the spec has no shared
/// `parameters`/`requestBodies`/`responses`/`securitySchemes` that could `$ref` a
/// schema from outside `paths`). If such a section is ever added, its `$ref`s must
/// be folded into the root set here, or it would silently drop still-referenced
/// schemas.
fn prune_unreferenced_schemas(paths: &Value, schemas: &mut Map<String, Value>) {
    // Collects every `#/components/schemas/<name>` `$ref` reachable under `v`. The
    // only `$ref`s this generator emits point at `#/components/schemas/`; any other
    // `$ref` shape is simply not collected (it can't name a component schema).
    fn collect_schema_refs(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(map) => {
                for (k, val) in map {
                    if k == "$ref"
                        && let Value::String(s) = val
                        && let Some(name) = s.strip_prefix("#/components/schemas/")
                    {
                        out.push(name.to_string());
                    } else {
                        collect_schema_refs(val, out);
                    }
                }
            }
            Value::Array(arr) => arr.iter().for_each(|e| collect_schema_refs(e, out)),
            _ => {}
        }
    }

    // A spec with no operations at all (a types-only spec / the struct-only unit
    // fixtures) has nothing to prune *against* — the schemas are the whole point,
    // so keep them rather than emptying `components.schemas`. Gate on `paths`
    // being empty, NOT on the root set: a spec that *has* operations but happens
    // to `$ref` no schema (e.g. only binary/void endpoints) should still prune its
    // unreferenced structs — otherwise they'd re-trip `no-unused-components`.
    if paths.as_object().is_none_or(Map::is_empty) {
        return;
    }

    let mut roots = Vec::new();
    collect_schema_refs(paths, &mut roots);

    let mut reachable: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = Vec::new();
    for r in roots {
        if reachable.insert(r.clone()) {
            queue.push(r);
        }
    }
    while let Some(name) = queue.pop() {
        if let Some(schema) = schemas.get(&name) {
            let mut nested = Vec::new();
            collect_schema_refs(schema, &mut nested);
            for r in nested {
                if reachable.insert(r.clone()) {
                    queue.push(r);
                }
            }
        }
    }
    schemas.retain(|name, _| reachable.contains(name));
}

/// Builds the `paths` object from endpoint definitions.
fn build_paths(endpoints: &[EndpointInfo]) -> Value {
    let mut paths: Map<String, Value> = Map::new();

    for ep in endpoints {
        let method = ep.method.as_lower_str();

        let operation = build_operation(ep);

        let path_entry = paths
            .entry(ep.path.clone())
            .or_insert_with(|| Value::Object(Map::new()));

        if let Some(map) = path_entry.as_object_mut() {
            map.insert(method.to_string(), operation);
        }
    }

    Value::Object(paths)
}

/// Builds an OpenAPI operation object for a single endpoint.
fn build_operation(ep: &EndpointInfo) -> Value {
    let mut op: Map<String, Value> = Map::new();
    op.insert("operationId".to_string(), json!(ep.name));

    // A summary is required by common OpenAPI linters (redocly's
    // `operation-summary`). Derive it from the doc comment's first line when
    // present, otherwise fall back to the endpoint name.
    let summary = ep
        .doc_comment
        .as_ref()
        .and_then(|doc| doc.lines().next())
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .unwrap_or_else(|| ep.name.clone());
    op.insert("summary".to_string(), json!(summary));

    if let Some(ref doc) = ep.doc_comment {
        op.insert("description".to_string(), json!(doc));
    }

    // Parameters: path params + query params
    let mut parameters = Vec::new();

    for pp in &ep.path_params {
        parameters.push(json!({
            "name": pp,
            "in": "path",
            "required": true,
            "schema": { "type": "string" }
        }));
    }

    for qp in &ep.query_params {
        let is_option = matches!(&qp.ty, Type::Generic(name, _) if name == "Option");
        let required = !qp.has_default && !is_option;
        let mut schema = type_to_json_schema(&qp.ty);

        if let Some(ref default) = qp.default_value
            && let Some(schema_map) = schema.as_object_mut()
        {
            schema_map.insert("default".to_string(), default_to_json(default));
        }

        parameters.push(json!({
            "name": qp.name,
            "in": "query",
            "required": required,
            "schema": schema
        }));
    }

    // Request headers: emit one `in: header` parameter per declared request
    // header, keyed by its exact wire name. Optionality mirrors query params
    // (an `Option<T>` type or a default makes the header optional).
    for hp in &ep.headers {
        let is_option = matches!(&hp.ty, Type::Generic(name, _) if name == "Option");
        let required = !hp.has_default && !is_option;
        let mut schema = type_to_json_schema(&hp.ty);

        if let Some(ref default) = hp.default_value
            && let Some(schema_map) = schema.as_object_mut()
        {
            schema_map.insert("default".to_string(), default_to_json(default));
        }

        parameters.push(json!({
            "name": hp.wire_name,
            "in": "header",
            "required": required,
            "schema": schema
        }));
    }

    if !parameters.is_empty() {
        op.insert("parameters".to_string(), Value::Array(parameters));
    }

    // Request body. A multipart (file-upload) body emits an inline
    // `multipart/form-data` object schema (File fields → `format: binary`);
    // a plain JSON body references its `{name}Body` component schema.
    if let Some(ref body) = ep.body {
        let content = if ep.body_is_multipart {
            json!({ "multipart/form-data": { "schema": derived_type_to_schema(body) } })
        } else {
            let body_type = format!("{}Body", capitalize(&ep.name));
            json!({
                "application/json": {
                    "schema": { "$ref": format!("#/components/schemas/{}", body_type) }
                }
            })
        };
        op.insert(
            "requestBody".to_string(),
            json!({ "required": true, "content": content }),
        );
    }

    // Responses
    let mut responses: Map<String, Value> = Map::new();

    // Response headers: a map of wire name → header object, attached to the
    // 200 success response. The grammar only allows response headers via an
    // inline `response <Type> headers { ... }` block, so they can never occur
    // without a response body — the bodyless 204 branch below never sees them.
    // Only present when the endpoint declares response headers.
    let response_headers = if ep.response_headers.is_empty() {
        None
    } else {
        let mut headers_map: Map<String, Value> = Map::new();
        for hp in &ep.response_headers {
            // Mirror the request-header `required` computation. Response headers
            // carry no default (sema rejects one), so optionality is `Option<T>`
            // only: a bare type is a header the server always sets (required),
            // an `Option<T>` is one it may omit.
            let is_option = matches!(&hp.ty, Type::Generic(name, _) if name == "Option");
            headers_map.insert(
                hp.wire_name.clone(),
                json!({
                    "required": !is_option,
                    "schema": type_to_json_schema(&hp.ty)
                }),
            );
        }
        Some(Value::Object(headers_map))
    };

    if !ep.response_statuses.is_empty() {
        // Multi-status endpoint (`response { <status>[: Type] ... }`): emit one
        // entry per declared success status. Typed statuses share one body type
        // (sema decision 1), so they all get the same JSON schema; a typeless
        // status (e.g. `204`) carries no `content`. Mutually exclusive with
        // binary/pagination/response-headers shaping (sema rejects the combos),
        // so none of those branches co-occur here.
        for rs in &ep.response_statuses {
            let entry = match &rs.ty {
                Some(ty) => json!({
                    "description": "Successful response",
                    "content": {
                        "application/json": { "schema": type_to_json_schema(ty) }
                    }
                }),
                None => json!({ "description": typeless_status_description(rs.status) }),
            };
            responses.insert(rs.status.to_string(), entry);
        }
    } else if let Some(ref resp_type) = ep.response {
        // A binary download (single-`File` response struct) streams raw bytes:
        // `application/octet-stream` with a `format: binary` schema. Otherwise the
        // success body is the JSON-serialized response type.
        let content = if ep.response_is_binary {
            json!({
                "application/octet-stream": {
                    "schema": { "type": "string", "format": "binary" }
                }
            })
        } else if ep.pagination.is_some() {
            // The 200 body is the `<Endpoint>Page` envelope object (a bare
            // `List<T>` response becomes the page wrapper), referenced as a
            // named component schema.
            let page_type = format!("{}Page", capitalize(&ep.name));
            json!({
                "application/json": {
                    "schema": { "$ref": format!("#/components/schemas/{}", page_type) }
                }
            })
        } else {
            json!({ "application/json": { "schema": type_to_json_schema(resp_type) } })
        };
        let mut success = json!({
            "description": "Successful response",
            "content": content
        });
        if let Some(ref headers) = response_headers
            && let Some(obj) = success.as_object_mut()
        {
            obj.insert("headers".to_string(), headers.clone());
        }
        responses.insert("200".to_string(), success);
    } else {
        // No body → 204, and (per the grammar) no response headers to attach.
        responses.insert("204".to_string(), json!({ "description": "No content" }));
    }

    for (name, code) in &ep.errors {
        responses.insert(
            code.to_string(),
            json!({
                "description": name,
                "content": {
                    "application/json": {
                        "schema": {
                            "type": "object",
                            "properties": {
                                "error": { "type": "string" }
                            }
                        }
                    }
                }
            }),
        );
    }

    op.insert("responses".to_string(), Value::Object(responses));
    Value::Object(op)
}

/// Description for a TYPELESS multi-status entry: the standard HTTP reason
/// phrase where one exists for the 2xx code (a typeless `202` reads better as
/// "Accepted" than "No content"), falling back to "No content" — the absence
/// of a body is the entry's defining feature. Sema restricts the block to
/// 200..=299, so only 2xx codes can reach this.
fn typeless_status_description(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        205 => "Reset Content",
        206 => "Partial Content",
        _ => "No content",
    }
}

/// Converts a Phoenix `Type` to a JSON Schema value.
fn type_to_json_schema(ty: &Type) -> Value {
    match ty {
        Type::Int => json!({ "type": "integer" }),
        Type::Float => json!({ "type": "number" }),
        Type::String => json!({ "type": "string" }),
        Type::Bool => json!({ "type": "boolean" }),
        // A `File` body field is binary content: OpenAPI represents it as a
        // string with `format: binary` (used inside `multipart/form-data` request
        // bodies and binary responses, wired up in the body-codegen path).
        Type::File => json!({ "type": "string", "format": "binary" }),
        // A `DateTime` is an RFC 3339 instant: an OpenAPI string with the
        // standard `date-time` format. See `docs/design-decisions.md`.
        Type::DateTime => json!({ "type": "string", "format": "date-time" }),
        Type::Void => json!({}),
        Type::Named(name) => json!({ "$ref": format!("#/components/schemas/{}", name) }),
        Type::Generic(name, args) if name == "List" && args.len() == 1 => {
            json!({ "type": "array", "items": type_to_json_schema(&args[0]) })
        }
        Type::Generic(name, args) if name == "Map" && args.len() == 2 => {
            json!({ "type": "object", "additionalProperties": type_to_json_schema(&args[1]) })
        }
        Type::Generic(name, args) if name == "Option" && args.len() == 1 => {
            let inner = type_to_json_schema(&args[0]);
            // OpenAPI 3.1 uses anyOf for nullable
            json!({ "anyOf": [inner, { "type": "null" }] })
        }
        _ => json!({}),
    }
}

/// Converts a struct declaration to a JSON Schema `object`.
fn struct_to_schema(s: &StructDecl, info: &phoenix_sema::checker::StructInfo) -> Value {
    let mut properties: Map<String, Value> = Map::new();
    let mut required = Vec::new();

    for f in &info.fields {
        let is_option = matches!(&f.ty, Type::Generic(name, _) if name == "Option");
        let mut field_schema = type_to_json_schema(&f.ty);
        if let Some(ref constraint) = f.constraint
            && let Some(obj) = field_schema.as_object_mut()
        {
            extract_schema_constraints(constraint, obj);
        }
        properties.insert(f.name.clone(), field_schema);
        if !is_option {
            required.push(json!(f.name));
        }
    }

    let mut map: Map<String, Value> = Map::new();
    map.insert("type".to_string(), json!("object"));
    map.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        map.insert("required".to_string(), Value::Array(required));
    }
    if let Some(ref doc) = s.doc_comment {
        map.insert("description".to_string(), json!(doc));
    }
    Value::Object(map)
}

/// Converts a simple enum to a JSON Schema string enum, or a tagged union
/// to `oneOf` with a discriminator.
fn enum_to_schema(e: &EnumDecl) -> Value {
    let all_unit = e.variants.iter().all(|v| v.fields.is_empty());
    if all_unit {
        let values: Vec<Value> = e.variants.iter().map(|v| json!(v.name)).collect();
        let mut map: Map<String, Value> = Map::new();
        map.insert("type".to_string(), json!("string"));
        map.insert("enum".to_string(), Value::Array(values));
        if let Some(ref doc) = e.doc_comment {
            map.insert("description".to_string(), json!(doc));
        }
        Value::Object(map)
    } else {
        let one_of: Vec<Value> = e
            .variants
            .iter()
            .map(|v| {
                if v.fields.is_empty() {
                    json!({
                        "type": "object",
                        "properties": { "tag": { "type": "string", "const": v.name } },
                        "required": ["tag"]
                    })
                } else {
                    json!({
                        "type": "object",
                        "properties": {
                            "tag": { "type": "string", "const": v.name },
                            "value": {}
                        },
                        "required": ["tag", "value"]
                    })
                }
            })
            .collect();

        json!({ "oneOf": one_of, "discriminator": { "propertyName": "tag" } })
    }
}

/// Converts a resolved derived type to a JSON Schema object.
fn derived_type_to_schema(body: &phoenix_sema::checker::ResolvedDerivedType) -> Value {
    let mut properties: Map<String, Value> = Map::new();
    let mut required = Vec::new();

    for f in &body.fields {
        // A field is optional if `partial` made it so (`f.optional`) or its type is
        // `Option<T>`; only genuinely required fields land in the `required` array.
        let is_option = matches!(&f.ty, Type::Generic(name, _) if name == "Option");
        let mut field_schema = type_to_json_schema(&f.ty);
        if let Some(ref constraint) = f.constraint
            && let Some(obj) = field_schema.as_object_mut()
        {
            extract_schema_constraints(constraint, obj);
        }
        properties.insert(f.name.clone(), field_schema);
        if !f.optional && !is_option {
            required.push(json!(f.name));
        }
    }

    let mut map: Map<String, Value> = Map::new();
    map.insert("type".to_string(), json!("object"));
    map.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        map.insert("required".to_string(), Value::Array(required));
    }
    Value::Object(map)
}

/// Builds the `<Endpoint>Page` envelope object schema for a paginated endpoint.
///
/// The `items` property is always the `List<T>` array (`type: array` with the
/// item type as its `items` schema) and is required. The mode-specific metadata
/// field follows the fixed canonical convention (see `docs/design-decisions.md`,
/// pagination decision 3):
/// - `offset` → `totalCount: { type: integer }`, required.
/// - `cursor` → `nextCursor: Option<String>`, rendered nullable (the same
///   `anyOf: [string, null]` shape any `Option<T>` gets) and omitted from
///   `required` — the Go (`*string`) and Python (`str | None`) servers emit
///   `nextCursor: null` on the last page, so the schema must permit `null`, not
///   just absence.
///
/// The wire field names (`items`, `totalCount`, `nextCursor`) are camelCase,
/// matching the TS and Go targets on the wire. The Python target diverges: it
/// emits no `Field(alias=...)` on any model, so its wire names are snake_case
/// (`total_count`/`next_cursor`) and a Python server is NOT described by this
/// schema — a pre-existing Python wire-name divergence affecting every model,
/// not pagination-specific (see `docs/design-decisions.md`, pagination
/// decision 3).
fn pagination_page_schema(pagination: &PaginationInfo) -> Value {
    let mut properties: Map<String, Value> = Map::new();
    let mut required = vec![json!("items")];

    properties.insert(
        "items".to_string(),
        json!({ "type": "array", "items": type_to_json_schema(&pagination.item_type) }),
    );

    match pagination.mode {
        PaginationMode::Offset => {
            properties.insert("totalCount".to_string(), json!({ "type": "integer" }));
            required.push(json!("totalCount"));
        }
        PaginationMode::Cursor => {
            // `nextCursor` is `Option<String>`: nullable (an absent cursor is
            // serialized as JSON `null` on the last page) and optional (omitted
            // from `required`). Route through `type_to_json_schema` so it gets the
            // exact `anyOf: [string, null]` shape every other `Option<T>` does.
            properties.insert(
                "nextCursor".to_string(),
                type_to_json_schema(&Type::Generic("Option".to_string(), vec![Type::String])),
            );
        }
    }

    let mut map: Map<String, Value> = Map::new();
    map.insert("type".to_string(), json!("object"));
    map.insert("properties".to_string(), Value::Object(properties));
    map.insert("required".to_string(), Value::Array(required));
    Value::Object(map)
}

/// Extracts JSON Schema validation keywords from a Phoenix constraint
/// expression. Pattern-matches common forms (numeric comparisons, string
/// length bounds) and applies the corresponding JSON Schema keywords.
/// Complex expressions that don't match known patterns are silently skipped.
fn extract_schema_constraints(expr: &phoenix_parser::ast::Expr, schema: &mut Map<String, Value>) {
    use phoenix_parser::ast::{BinaryOp, Expr};

    match expr {
        // expr1 and expr2 → recurse on both sides
        Expr::Binary(bin) if bin.op == BinaryOp::And => {
            extract_schema_constraints(&bin.left, schema);
            extract_schema_constraints(&bin.right, schema);
        }
        // self >= N, self <= N, self > N, self < N (numeric constraints)
        Expr::Binary(bin) if is_self_ident(&bin.left) => {
            apply_numeric_constraint(bin, schema);
        }
        // self.length >= N, self.length <= N, etc. (string length constraints)
        Expr::Binary(bin) if is_self_length(&bin.left) => {
            apply_length_constraint(bin, schema);
        }
        _ => {}
    }
}

/// Returns true if the expression is the `self` identifier.
fn is_self_ident(expr: &phoenix_parser::ast::Expr) -> bool {
    matches!(expr, phoenix_parser::ast::Expr::Ident(i) if i.name == "self")
}

/// Returns true if the expression is `self.length`.
fn is_self_length(expr: &phoenix_parser::ast::Expr) -> bool {
    matches!(
        expr,
        phoenix_parser::ast::Expr::FieldAccess(fa) if fa.field == "length" && is_self_ident(&fa.object)
    )
}

/// Applies numeric constraints (minimum/maximum) from `self op N`.
fn apply_numeric_constraint(
    bin: &phoenix_parser::ast::BinaryExpr,
    schema: &mut Map<String, Value>,
) {
    use phoenix_parser::ast::{BinaryOp, Expr};
    if let Expr::Literal(lit) = &bin.right {
        let n = match &lit.kind {
            LiteralKind::Int(v) => json!(v),
            LiteralKind::Float(v) => json!(v),
            _ => return,
        };
        match bin.op {
            BinaryOp::GtEq => {
                schema.insert("minimum".into(), n);
            }
            BinaryOp::LtEq => {
                schema.insert("maximum".into(), n);
            }
            BinaryOp::Gt => {
                schema.insert("exclusiveMinimum".into(), n);
            }
            BinaryOp::Lt => {
                schema.insert("exclusiveMaximum".into(), n);
            }
            _ => {}
        }
    }
}

/// Applies string length constraints (minLength/maxLength) from `self.length op N`.
fn apply_length_constraint(bin: &phoenix_parser::ast::BinaryExpr, schema: &mut Map<String, Value>) {
    use phoenix_parser::ast::{BinaryOp, Expr};
    if let Expr::Literal(lit) = &bin.right
        && let LiteralKind::Int(n) = &lit.kind
    {
        match bin.op {
            BinaryOp::GtEq => {
                schema.insert("minLength".into(), json!(n));
            }
            BinaryOp::LtEq => {
                schema.insert("maxLength".into(), json!(n));
            }
            BinaryOp::Gt => {
                schema.insert("minLength".into(), json!(n + 1));
            }
            BinaryOp::Lt => {
                schema.insert("maxLength".into(), json!(n - 1));
            }
            _ => {}
        }
    }
}

/// Converts a [`DefaultValue`] to a JSON value.
fn default_to_json(val: &DefaultValue) -> Value {
    match val {
        DefaultValue::Int(v) => json!(v),
        DefaultValue::Float(v) => json!(v),
        DefaultValue::String(v) => json!(v),
        DefaultValue::Bool(v) => json!(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_common::span::{SourceId, Span};
    use phoenix_lexer::lexer::tokenize;
    use phoenix_parser::ast::{EnumVariant, NamedType, TypeExpr, Visibility};
    use phoenix_parser::parser;
    use phoenix_sema::checker;

    /// Helper: parse and type-check a Phoenix source string, then generate the
    /// OpenAPI spec. Returns the full JSON string.
    fn generate_from_source(source: &str) -> String {
        let tokens = tokenize(source, SourceId(0));
        let (program, parse_errors) = parser::parse(&tokens);
        assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
        let result = checker::check(&program);
        assert!(
            result.diagnostics.is_empty(),
            "check errors: {:?}",
            result.diagnostics
        );
        generate_openapi(&program, &result)
    }

    // ── type_to_json_schema tests ──────────────────────────────────

    #[test]
    fn primitive_type_schemas() {
        let int_schema = type_to_json_schema(&Type::Int);
        let float_schema = type_to_json_schema(&Type::Float);
        let string_schema = type_to_json_schema(&Type::String);
        let bool_schema = type_to_json_schema(&Type::Bool);

        let combined = serde_json::json!({
            "int": int_schema,
            "float": float_schema,
            "string": string_schema,
            "bool": bool_schema,
        });
        insta::assert_snapshot!(
            "openapi_primitive_types",
            serde_json::to_string_pretty(&combined).unwrap()
        );
    }

    #[test]
    fn named_type_schema() {
        let schema = type_to_json_schema(&Type::Named("User".to_string()));
        insta::assert_snapshot!(
            "openapi_named_type",
            serde_json::to_string_pretty(&schema).unwrap()
        );
    }

    #[test]
    fn list_type_schema() {
        let schema = type_to_json_schema(&Type::Generic("List".to_string(), vec![Type::String]));
        insta::assert_snapshot!(
            "openapi_list_type",
            serde_json::to_string_pretty(&schema).unwrap()
        );
    }

    #[test]
    fn map_type_schema() {
        let schema = type_to_json_schema(&Type::Generic(
            "Map".to_string(),
            vec![Type::String, Type::Int],
        ));
        insta::assert_snapshot!(
            "openapi_map_type",
            serde_json::to_string_pretty(&schema).unwrap()
        );
    }

    #[test]
    fn option_type_schema() {
        let schema = type_to_json_schema(&Type::Generic("Option".to_string(), vec![Type::Int]));
        insta::assert_snapshot!(
            "openapi_option_type",
            serde_json::to_string_pretty(&schema).unwrap()
        );
    }

    // ── enum_to_schema tests ───────────────────────────────────────

    #[test]
    fn simple_enum_string_schema() {
        let spec = generate_from_source("enum Role { Admin  Editor  Viewer }");
        insta::assert_snapshot!("openapi_simple_enum", spec);
    }

    #[test]
    fn complex_enum_tagged_union() {
        let e = EnumDecl {
            name: "Shape".to_string(),
            name_span: Span::BUILTIN,
            type_params: vec![],
            variants: vec![
                EnumVariant {
                    name: "Circle".to_string(),
                    fields: vec![TypeExpr::Named(NamedType {
                        name: "Float".to_string(),
                        span: Span::BUILTIN,
                    })],
                    span: Span::BUILTIN,
                },
                EnumVariant {
                    name: "Square".to_string(),
                    fields: vec![TypeExpr::Named(NamedType {
                        name: "Float".to_string(),
                        span: Span::BUILTIN,
                    })],
                    span: Span::BUILTIN,
                },
                EnumVariant {
                    name: "Unit".to_string(),
                    fields: vec![],
                    span: Span::BUILTIN,
                },
            ],
            methods: vec![],
            trait_impls: vec![],
            doc_comment: None,
            visibility: Visibility::Private,
            span: Span::BUILTIN,
        };
        let schema = enum_to_schema(&e);
        insta::assert_snapshot!(
            "openapi_complex_enum",
            serde_json::to_string_pretty(&schema).unwrap()
        );
    }

    // ── struct_to_schema tests ─────────────────────────────────────

    #[test]
    fn struct_required_and_optional_fields() {
        let spec = generate_from_source(
            r#"
struct User {
    id: Int
    name: String
    bio: Option<String>
}
"#,
        );
        insta::assert_snapshot!("openapi_struct_required_optional", spec);
    }

    #[test]
    fn struct_with_doc_comment() {
        let spec = generate_from_source(
            r#"
/** A registered user account */
struct User {
    id: Int
    name: String
}
"#,
        );
        insta::assert_snapshot!("openapi_struct_doc_comment", spec);
    }

    // ── derived_type_to_schema tests ───────────────────────────────

    #[test]
    fn derived_type_body_omit() {
        let spec = generate_from_source(
            r#"
struct User { id: Int  name: String  email: String }
endpoint createUser: POST "/api/users" {
    body User omit { id }
    response User
}
"#,
        );
        insta::assert_snapshot!("openapi_derived_body_omit", spec);
    }

    #[test]
    fn derived_type_body_option_field_not_required() {
        // An `Option<T>` field of a (plain JSON) request body must NOT land in the
        // schema's `required` array — only genuinely required fields do. Regression
        // guard for the `is_option` exclusion in `derived_type_to_schema`.
        let spec = generate_from_source(
            r#"
struct Note { title: String  body: Option<String>  priority: Int }
endpoint createNote: POST "/api/notes" {
    body Note
    response Note
}
"#,
        );
        // The body schema requires `title` and `priority` but not the optional `body`.
        let v: serde_json::Value = serde_json::from_str(&spec).unwrap();
        let required = &v["components"]["schemas"]["CreateNoteBody"]["required"];
        assert_eq!(
            required,
            &serde_json::json!(["title", "priority"]),
            "Option field must be excluded from required:\n{spec}"
        );
        insta::assert_snapshot!("openapi_derived_body_option_field", spec);
    }

    // ── default_to_json tests ──────────────────────────────────────

    #[test]
    fn default_values_all_types() {
        let int_val = default_to_json(&DefaultValue::Int(42));
        let float_val = default_to_json(&DefaultValue::Float(3.125));
        let string_val = default_to_json(&DefaultValue::String("hello".to_string()));
        let bool_val = default_to_json(&DefaultValue::Bool(true));

        let combined = serde_json::json!({
            "int": int_val,
            "float": float_val,
            "string": string_val,
            "bool": bool_val,
        });
        insta::assert_snapshot!(
            "openapi_default_values",
            serde_json::to_string_pretty(&combined).unwrap()
        );
    }

    // ── extract_schema_constraints tests ───────────────────────────

    #[test]
    fn numeric_min_max_constraints() {
        let spec = generate_from_source(
            r#"
struct Product {
    price: Int where self >= 0 && self <= 10000
    name: String
}
"#,
        );
        insta::assert_snapshot!("openapi_numeric_constraints", spec);
    }

    #[test]
    fn string_length_constraints() {
        let spec = generate_from_source(
            r#"
struct Profile {
    username: String where self.length >= 3 && self.length <= 50
    bio: String
}
"#,
        );
        insta::assert_snapshot!("openapi_string_length_constraints", spec);
    }

    // ── full endpoint tests ────────────────────────────────────────

    #[test]
    fn endpoint_with_query_params_and_defaults() {
        let spec = generate_from_source(
            r#"
struct Item { id: Int  name: String }
endpoint listItems: GET "/api/items" {
    query {
        page: Int = 1
        limit: Int = 20
        search: Option<String>
    }
    response List<Item>
}
"#,
        );
        insta::assert_snapshot!("openapi_query_params_defaults", spec);
    }

    #[test]
    fn endpoint_with_path_params_and_errors() {
        let spec = generate_from_source(
            r#"
struct User { id: Int  name: String }
endpoint getUser: GET "/api/users/{id}" {
    response User
    error { NotFound(404) }
}
"#,
        );
        insta::assert_snapshot!("openapi_path_params_errors", spec);
    }

    // ── header tests ───────────────────────────────────────────────

    #[test]
    fn endpoint_with_request_header() {
        // A required request header emits an `in: header` parameter keyed by its
        // auto-derived wire name (idempotencyKey → Idempotency-Key) with
        // required: true.
        let spec = generate_from_source(
            r#"
struct Order { id: Int }
endpoint createOrder: POST "/api/orders" {
    headers {
        idempotencyKey: String
    }
    response Order
}
"#,
        );
        insta::assert_snapshot!("openapi_request_header", spec);
    }

    #[test]
    fn endpoint_with_request_header_as_override() {
        // An explicit `as "..."` override pins the exact wire name verbatim.
        let spec = generate_from_source(
            r#"
struct Order { id: Int }
endpoint createOrder: POST "/api/orders" {
    headers {
        rateLimit: String as "X-RateLimit-Limit"
    }
    response Order
}
"#,
        );
        insta::assert_snapshot!("openapi_request_header_as_override", spec);
    }

    #[test]
    fn endpoint_with_optional_and_required_request_headers() {
        // Optionality mirrors query params: Option<T> or a default → optional
        // (required: false); a bare required type → required: true.
        let spec = generate_from_source(
            r#"
struct Order { id: Int }
endpoint createOrder: POST "/api/orders" {
    headers {
        idempotencyKey: String
        traceId: Option<String>
    }
    response Order
}
"#,
        );
        insta::assert_snapshot!("openapi_request_headers_optional_required", spec);
    }

    #[test]
    fn endpoint_with_response_header() {
        // A response header attaches a `headers` map (keyed by wire name) to the
        // success (200) response object.
        let spec = generate_from_source(
            r#"
struct Post { id: Int }
endpoint getPost: GET "/api/posts/{id}" {
    response Post headers { ratelimitRemaining: Int as "X-RateLimit-Remaining" }
}
"#,
        );
        insta::assert_snapshot!("openapi_response_header", spec);
    }

    #[test]
    fn endpoint_with_defaulted_request_header() {
        // A request header with a literal default carries `required: false` and a
        // `default` baked into its schema (the `default_to_json` insertion path).
        let spec = generate_from_source(
            r#"
struct Order { id: Int }
endpoint createOrder: POST "/api/orders" {
    headers { maxStale: Int = 60 }
    response Order
}
"#,
        );
        let parsed: Value = serde_json::from_str(&spec).expect("spec is valid JSON");
        let params = parsed["paths"]["/api/orders"]["post"]["parameters"]
            .as_array()
            .expect("operation has parameters");
        let header = params
            .iter()
            .find(|p| p["in"] == json!("header") && p["name"] == json!("Max-Stale"))
            .expect("Max-Stale header parameter present");
        assert_eq!(
            header["required"],
            json!(false),
            "a defaulted header is not required:\n{}",
            spec
        );
        assert_eq!(
            header["schema"]["default"],
            json!(60),
            "the default must be baked into the schema:\n{}",
            spec
        );
    }

    // ── multipart / file-upload + binary download tests ────────────

    #[test]
    fn endpoint_with_multipart_request_body() {
        // A request body whose struct contains a `File` field is multipart: the
        // request body emits an inline `multipart/form-data` object schema (the
        // File field as `type: string, format: binary`, scalars normally, with a
        // `required` array), and no `{name}Body` component schema is generated.
        let spec = generate_from_source(
            r#"
struct AvatarUpload {
    avatar: File
    caption: String
    alt: Option<String>
}
endpoint uploadAvatar: POST "/api/avatar" {
    body AvatarUpload
}
"#,
        );
        insta::assert_snapshot!("openapi_multipart_request_body", spec);
    }

    #[test]
    fn endpoint_with_binary_response() {
        // A response whose struct is a single `File` field is a binary download:
        // the 200 response content becomes `application/octet-stream` with a
        // `format: binary` schema instead of `application/json`.
        let spec = generate_from_source(
            r#"
struct Doc { data: File }
endpoint downloadDoc: GET "/api/doc/{id}" {
    response Doc
}
"#,
        );
        insta::assert_snapshot!("openapi_binary_response", spec);
    }

    /// Component schemas no operation references are pruned (matching redocly's
    /// `no-unused-components`). A struct used only as a binary-download response
    /// (octet-stream, never `$ref`d) or only as a JSON request body (the operation
    /// `$ref`s the derived `<Endpoint>Body`, not the source struct) is dropped,
    /// while a struct an operation actually `$ref`s — and the derived body
    /// component itself — are kept. Regression for the unused-component gap.
    #[test]
    fn unreferenced_component_schemas_are_pruned() {
        let spec = generate_from_source(
            r#"
struct Item { id: Int  name: String }
struct Blob { data: File }
struct CreateItemReq { name: String }
endpoint getItem: GET "/items/{id}" { response Item }
endpoint downloadBlob: GET "/blobs/{id}" { response Blob }
endpoint createItem: POST "/items" { body CreateItemReq  response Item }
"#,
        );
        // Referenced by getItem/createItem responses -> kept.
        assert!(
            spec.contains("\"Item\":"),
            "a referenced struct must stay in components:\n{spec}"
        );
        // The derived body the createItem operation actually `$ref`s -> kept.
        assert!(
            spec.contains("\"CreateItemBody\":"),
            "the derived <Endpoint>Body component must stay:\n{spec}"
        );
        // Only a binary-download response -> octet-stream, never `$ref`d -> pruned.
        assert!(
            !spec.contains("\"Blob\""),
            "a binary-only response struct must be pruned:\n{spec}"
        );
        // Only a JSON-body source -> operation `$ref`s CreateItemBody -> pruned.
        assert!(
            !spec.contains("\"CreateItemReq\""),
            "a JSON-body-only source struct must be pruned:\n{spec}"
        );
    }

    /// Reachability is transitive: a struct no operation references directly is
    /// kept when a referenced struct reaches it through a field `$ref`. Here only
    /// `Order` is named by an operation; `Order` has a `customer: Customer` field
    /// and `Customer` an `address: Address` field, so both survive the prune via
    /// the BFS over each kept schema's own `$ref`s — while a sibling struct nothing
    /// reaches is still dropped. Guards the queue-following branch of the prune.
    #[test]
    fn transitively_referenced_component_schemas_are_kept() {
        let spec = generate_from_source(
            r#"
struct Address { street: String  city: String }
struct Customer { id: Int  address: Address }
struct Order { id: Int  customer: Customer }
struct Orphan { note: String }
endpoint getOrder: GET "/orders/{id}" { response Order }
"#,
        );
        // Directly referenced by the operation response.
        assert!(
            spec.contains("\"Order\":"),
            "the directly-referenced struct must stay:\n{spec}"
        );
        // Reached only via Order.customer -> kept transitively.
        assert!(
            spec.contains("\"Customer\":"),
            "a struct reached via a field `$ref` must stay (transitive):\n{spec}"
        );
        // Reached only via Customer.address (two hops) -> kept transitively.
        assert!(
            spec.contains("\"Address\":"),
            "a struct reached via a two-hop field `$ref` must stay:\n{spec}"
        );
        // Reached by nothing -> pruned, even alongside the transitive keeps.
        assert!(
            !spec.contains("\"Orphan\""),
            "a struct nothing reaches must still be pruned:\n{spec}"
        );
    }

    /// Pruning is gated on the *operation set* being empty, not on the *root ref
    /// set*: a spec that has operations but `$ref`s no schema (here, a lone
    /// binary-download endpoint) must still prune its unreferenced struct, or that
    /// struct re-trips redocly's `no-unused-components`. The earlier "keep all when
    /// no roots" guard wrongly kept it. A types-only spec (no operations) is the
    /// case that legitimately keeps every schema — exercised by the unit fixtures.
    #[test]
    fn operations_present_but_no_refs_still_prunes() {
        let spec = generate_from_source(
            r#"
struct Blob { data: File }
endpoint downloadBlob: GET "/blobs/{id}" { response Blob }
"#,
        );
        // The only operation streams octet-stream and never `$ref`s `Blob`, so the
        // schema is unreferenced and must be pruned despite a non-empty `paths`.
        assert!(
            !spec.contains("\"Blob\""),
            "an unreferenced struct must be pruned even when operations exist:\n{spec}"
        );
    }

    /// An `api version` block prefixes the `paths` key in the generated OpenAPI
    /// document, just as it does the client URL and server route. The sema layer
    /// resolves the path before any generator runs, so this pins that the
    /// OpenAPI emitter — the one consumer the roundtrip harness does not
    /// exercise at runtime — keys off the prefixed path and never the bare one.
    #[test]
    fn api_version_prefixes_openapi_path() {
        let spec = generate_from_source(
            r#"
struct Post { id: Int }
api version "v2" {
    endpoint listTaggedPosts: GET "/api/posts/tagged/{tag}" { response Post }
}
"#,
        );
        assert!(
            spec.contains("/v2/api/posts/tagged/{tag}"),
            "OpenAPI paths key should carry the version prefix, got: {spec}"
        );
        // The unprefixed path must not also appear as its own key.
        assert!(
            !spec.contains("\"/api/posts/tagged/{tag}\""),
            "OpenAPI should not also emit the unprefixed path, got: {spec}"
        );
    }

    // ── pagination tests ───────────────────────────────────────────

    #[test]
    fn offset_pagination_page_schema() {
        // An offset-paginated endpoint's 200 body is the `<Endpoint>Page` envelope
        // object (`{ items: array, totalCount: integer }`, both required),
        // emitted as a named component and `$ref`d from the response. The bare
        // `List<Post>` no longer appears as the response schema.
        let spec = generate_from_source(
            r#"
struct Post { id: Int  title: String }
endpoint listPosts: GET "/api/posts" {
    response List<Post>
    pagination { offset }
}
"#,
        );
        let v: Value = serde_json::from_str(&spec).expect("spec is valid JSON");

        // The 200 response references the page component.
        let resp_schema = &v["paths"]["/api/posts"]["get"]["responses"]["200"]["content"]["application/json"]
            ["schema"];
        assert_eq!(
            resp_schema,
            &json!({ "$ref": "#/components/schemas/ListPostsPage" }),
            "200 response should $ref the page component:\n{spec}"
        );

        // The page component is an object with items (array of Post $refs) and a
        // required totalCount integer.
        let page = &v["components"]["schemas"]["ListPostsPage"];
        assert_eq!(page["type"], json!("object"));
        assert_eq!(
            page["properties"]["items"],
            json!({ "type": "array", "items": { "$ref": "#/components/schemas/Post" } }),
            "items must be an array of the element schema:\n{spec}"
        );
        assert_eq!(
            page["properties"]["totalCount"],
            json!({ "type": "integer" })
        );
        assert_eq!(page["required"], json!(["items", "totalCount"]));

        insta::assert_snapshot!("openapi_pagination_offset", spec);
    }

    #[test]
    fn cursor_pagination_page_schema() {
        // A cursor-paginated endpoint's page envelope carries an optional,
        // nullable `nextCursor` (rendered `anyOf: [string, null]`, present in
        // properties, omitted from required — the server emits `null` on the last
        // page) and a required `items` array.
        let spec = generate_from_source(
            r#"
struct Post { id: Int  title: String }
endpoint listPosts: GET "/api/posts" {
    response List<Post>
    pagination { cursor }
}
"#,
        );
        let v: Value = serde_json::from_str(&spec).expect("spec is valid JSON");

        let resp_schema = &v["paths"]["/api/posts"]["get"]["responses"]["200"]["content"]["application/json"]
            ["schema"];
        assert_eq!(
            resp_schema,
            &json!({ "$ref": "#/components/schemas/ListPostsPage" }),
            "200 response should $ref the page component:\n{spec}"
        );

        let page = &v["components"]["schemas"]["ListPostsPage"];
        assert_eq!(page["type"], json!("object"));
        assert_eq!(
            page["properties"]["items"],
            json!({ "type": "array", "items": { "$ref": "#/components/schemas/Post" } }),
            "items must be an array of the element schema:\n{spec}"
        );
        assert_eq!(
            page["properties"]["nextCursor"],
            json!({ "anyOf": [{ "type": "string" }, { "type": "null" }] }),
            "nextCursor must be nullable (it serializes to null on the last page):\n{spec}"
        );
        assert_eq!(
            page["required"],
            json!(["items"]),
            "nextCursor must be optional (excluded from required):\n{spec}"
        );

        insta::assert_snapshot!("openapi_pagination_cursor", spec);
    }

    #[test]
    fn plain_list_response_is_unchanged_by_pagination() {
        // A non-paginated `List<T>` response stays a bare array; no `<Endpoint>Page`
        // component is emitted. Guards the byte-for-byte-unchanged invariant.
        let spec = generate_from_source(
            r#"
struct Post { id: Int  title: String }
endpoint listPosts: GET "/api/posts" {
    response List<Post>
}
"#,
        );
        let v: Value = serde_json::from_str(&spec).expect("spec is valid JSON");

        let resp_schema = &v["paths"]["/api/posts"]["get"]["responses"]["200"]["content"]["application/json"]
            ["schema"];
        assert_eq!(
            resp_schema,
            &json!({ "type": "array", "items": { "$ref": "#/components/schemas/Post" } }),
            "plain list response must remain a bare array:\n{spec}"
        );
        assert!(
            v["components"]["schemas"]["ListPostsPage"].is_null(),
            "no page component should be emitted for a non-paginated endpoint:\n{spec}"
        );
    }

    // ── multi-status response tests ────────────────────────────────

    #[test]
    fn multi_status_shared_body() {
        // `response { 200: User  201: User }`: two entries in the operation's
        // `responses` map, each with the SAME (shared) `User` schema. Native
        // OpenAPI — no envelope.
        let spec = generate_from_source(
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
        let v: Value = serde_json::from_str(&spec).expect("spec is valid JSON");
        let responses = &v["paths"]["/api/users/{id}"]["put"]["responses"];
        let user_ref = json!({ "$ref": "#/components/schemas/User" });
        assert_eq!(
            responses["200"]["content"]["application/json"]["schema"], user_ref,
            "200 must carry the shared User schema:\n{spec}"
        );
        assert_eq!(
            responses["201"]["content"]["application/json"]["schema"], user_ref,
            "201 must carry the SAME shared User schema:\n{spec}"
        );
        insta::assert_snapshot!("openapi_multi_status_shared_body", spec);
    }

    #[test]
    fn multi_status_typed_and_typeless() {
        // `response { 200: User  204 }`: the typed status carries the `User`
        // schema; the typeless `204` has a description but NO `content` (no body).
        let spec = generate_from_source(
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
        let v: Value = serde_json::from_str(&spec).expect("spec is valid JSON");
        let responses = &v["paths"]["/api/users/{id}"]["put"]["responses"];
        assert_eq!(
            responses["200"]["content"]["application/json"]["schema"],
            json!({ "$ref": "#/components/schemas/User" }),
            "200 must carry the User schema:\n{spec}"
        );
        assert!(
            responses["204"]["content"].is_null() && responses["204"]["description"].is_string(),
            "typeless 204 must have a description and NO content:\n{spec}"
        );
        insta::assert_snapshot!("openapi_multi_status_mixed", spec);
    }

    #[test]
    fn multi_status_all_typeless() {
        // `response { 202  204 }`: both entries are typeless — description only,
        // no content on either.
        let spec = generate_from_source(
            r#"
endpoint enqueueJob: POST "/api/jobs" {
    response {
        202
        204
    }
}
"#,
        );
        let v: Value = serde_json::from_str(&spec).expect("spec is valid JSON");
        let responses = &v["paths"]["/api/jobs"]["post"]["responses"];
        assert!(
            responses["202"]["content"].is_null()
                && responses["202"]["description"].is_string()
                && responses["204"]["content"].is_null()
                && responses["204"]["description"].is_string(),
            "all-typeless entries must each be description-only with no content:\n{spec}"
        );
        insta::assert_snapshot!("openapi_multi_status_all_typeless", spec);
    }

    #[test]
    fn multi_status_and_error_block_coexist() {
        // Multi-status entries and `error { }` variants land in ONE `responses`
        // map. The key ranges are disjoint by construction (sema: block statuses
        // are 2xx-only, error variants 400-599), so each insert must survive the
        // other — pin all three keys on the same operation.
        let spec = generate_from_source(
            r#"
struct User { id: Int  name: String }
endpoint updateUser: PUT "/api/users/{id}" {
    response {
        200: User
        204
    }
    error { NotFound(404) }
}
"#,
        );
        let v: Value = serde_json::from_str(&spec).expect("spec is valid JSON");
        let responses = &v["paths"]["/api/users/{id}"]["put"]["responses"];
        assert_eq!(
            responses["200"]["content"]["application/json"]["schema"],
            json!({ "$ref": "#/components/schemas/User" }),
            "200 must carry the User schema:\n{spec}"
        );
        assert!(
            responses["204"]["content"].is_null() && responses["204"]["description"].is_string(),
            "typeless 204 must survive alongside the error entry:\n{spec}"
        );
        assert!(
            responses["404"].is_object(),
            "the error variant's 404 must survive alongside the multi-status entries:\n{spec}"
        );
    }
}
