//! OpenAPI 3.1 specification generation for Phoenix Gen.
//!
//! Generates a JSON-formatted OpenAPI 3.1 spec from a parsed and type-checked
//! Phoenix program. Maps Phoenix structs to JSON Schema component schemas,
//! endpoints to path operations, and error variants to HTTP response status codes.

use phoenix_parser::ast::{Declaration, EnumDecl, LiteralKind, Program, StructDecl};
use phoenix_sema::Analysis;
use phoenix_sema::checker::{DefaultValue, EndpointInfo};
use phoenix_sema::types::Type;
use serde_json::{Map, Value, json};

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

    // Emit derived body schemas for endpoints
    for ep in &check_result.endpoints {
        if let Some(ref body) = ep.body {
            let type_name = format!("{}Body", capitalize(&ep.name));
            schemas.insert(type_name, derived_type_to_schema(body));
        }
    }

    let paths = build_paths(&check_result.endpoints);

    let spec = json!({
        "openapi": "3.1.0",
        "info": {
            "title": "API",
            "version": "1.0.0"
        },
        "paths": paths,
        "components": {
            "schemas": schemas,
        },
    });

    serde_json::to_string_pretty(&spec).expect("OpenAPI spec serialization should not fail")
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

    if !parameters.is_empty() {
        op.insert("parameters".to_string(), Value::Array(parameters));
    }

    // Request body
    if ep.body.is_some() {
        let body_type = format!("{}Body", capitalize(&ep.name));
        op.insert(
            "requestBody".to_string(),
            json!({
                "required": true,
                "content": {
                    "application/json": {
                        "schema": {
                            "$ref": format!("#/components/schemas/{}", body_type)
                        }
                    }
                }
            }),
        );
    }

    // Responses
    let mut responses: Map<String, Value> = Map::new();

    if let Some(ref resp_type) = ep.response {
        responses.insert(
            "200".to_string(),
            json!({
                "description": "Successful response",
                "content": {
                    "application/json": {
                        "schema": type_to_json_schema(resp_type)
                    }
                }
            }),
        );
    } else {
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

/// Converts a Phoenix `Type` to a JSON Schema value.
fn type_to_json_schema(ty: &Type) -> Value {
    match ty {
        Type::Int => json!({ "type": "integer" }),
        Type::Float => json!({ "type": "number" }),
        Type::String => json!({ "type": "string" }),
        Type::Bool => json!({ "type": "boolean" }),
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
        let mut field_schema = type_to_json_schema(&f.ty);
        if let Some(ref constraint) = f.constraint
            && let Some(obj) = field_schema.as_object_mut()
        {
            extract_schema_constraints(constraint, obj);
        }
        properties.insert(f.name.clone(), field_schema);
        if !f.optional {
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
    use phoenix_parser::ast::{EnumVariant, NamedType, TypeExpr};
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
    Int id
    String name
    Option<String> bio
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
    Int id
    String name
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
struct User { Int id  String name  String email }
endpoint createUser: POST "/api/users" {
    body User omit { id }
    response User
}
"#,
        );
        insta::assert_snapshot!("openapi_derived_body_omit", spec);
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
    Int price where self >= 0 && self <= 10000
    String name
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
    String username where self.length >= 3 && self.length <= 50
    String bio
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
struct Item { Int id  String name }
endpoint listItems: GET "/api/items" {
    query {
        Int page = 1
        Int limit = 20
        Option<String> search
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
struct User { Int id  String name }
endpoint getUser: GET "/api/users/{id}" {
    response User
    error { NotFound(404) }
}
"#,
        );
        insta::assert_snapshot!("openapi_path_params_errors", spec);
    }
}
