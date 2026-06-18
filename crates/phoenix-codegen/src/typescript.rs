//! TypeScript code generation for Phoenix Gen.
//!
//! Generates four files from a Phoenix program:
//! - **types.ts** — TypeScript interfaces for structs, string unions for enums,
//!   derived type aliases for endpoint body types, and error type definitions.
//! - **client.ts** — a fetch-based client SDK with typed async functions for
//!   each endpoint, including query parameter handling and typed error responses.
//! - **handlers.ts** — a `Handlers` interface that server implementations must
//!   satisfy, with typed method signatures for each endpoint.
//! - **server.ts** — an Express-compatible router that wires HTTP routes to
//!   handler methods with automatic parameter parsing and error mapping.

use std::collections::BTreeSet;

use phoenix_parser::ast::{Declaration, EnumDecl, PaginationMode, Program, StructDecl, TypeExpr};
use phoenix_sema::Analysis;
use phoenix_sema::checker::{DefaultValue, EndpointInfo, ResolvedDerivedType};
use phoenix_sema::types::Type;

mod format;
use format::*;

/// The output of TypeScript code generation: four file contents.
pub struct GeneratedFiles {
    /// Content for `types.ts` — interfaces, enums, derived types, error types.
    pub types: String,
    /// Content for `client.ts` — fetch-based client SDK.
    pub client: String,
    /// Content for `handlers.ts` — server handler interface.
    pub handlers: String,
    /// Content for `server.ts` — Express-compatible router wiring.
    pub server: String,
}

/// The HTTP server framework the generated `server.ts` targets. Only the server
/// router differs between frameworks; `types.ts`, `client.ts`, and `handlers.ts`
/// are framework-independent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TsServerFramework {
    /// Express-compatible `Router` (the default, backward-compatible target).
    Express,
    /// A Fastify plugin (`FastifyPluginCallback`) registering one route per endpoint.
    Fastify,
}

/// Generates TypeScript code from a parsed and type-checked Phoenix program,
/// targeting the default (Express) server framework.
///
/// Iterates over declarations in source order to produce deterministic,
/// diff-friendly output.
pub fn generate_typescript(program: &Program, check_result: &Analysis) -> GeneratedFiles {
    generate_typescript_with(program, check_result, TsServerFramework::Express)
}

/// Like [`generate_typescript`], but emits `server.ts` for the chosen
/// [`TsServerFramework`]. The other three files are identical regardless of
/// framework.
pub fn generate_typescript_with(
    program: &Program,
    check_result: &Analysis,
    framework: TsServerFramework,
) -> GeneratedFiles {
    let generator = TsGenerator::new(check_result, framework);
    generator.generate(program)
}

/// Internal TypeScript code generator.
///
/// Accumulates generated code for `types.ts`, `client.ts`, `handlers.ts`,
/// and `server.ts` in separate string buffers, using the semantic analysis
/// results to resolve types and build correct signatures.
struct TsGenerator<'a> {
    /// Semantic analysis results providing resolved types, struct info, and
    /// endpoint definitions.
    check_result: &'a Analysis,
    /// Accumulated output for `types.ts`.
    types_out: String,
    /// Accumulated output for `client.ts`.
    client_out: String,
    /// Accumulated output for `handlers.ts`.
    handlers_out: String,
    /// Accumulated output for `server.ts`.
    server_out: String,
    /// Tracks which derived type names have been emitted to avoid duplicates.
    emitted_derived_types: BTreeSet<String>,
    /// Whether the `ValidationError` class has been emitted.
    emitted_validation_error: bool,
    /// Bare names of structs that transitively contain a `DateTime`, so a decoded
    /// JSON value needs a `revive<Name>` pass to turn ISO strings back into
    /// `Date`s. Computed once at the top of [`Self::generate`]. See
    /// `docs/design-decisions.md` (DateTime & UUID scalar types).
    revivable_structs: BTreeSet<String>,
    /// Names of simple (unit-variant) enums used in a query param or request
    /// header, so the server decode path validates the wire string into the
    /// branded union via a generated `parse<Enum>`. Computed once in
    /// [`Self::generate`].
    param_enums: BTreeSet<String>,
    /// The server framework `server.ts` targets.
    framework: TsServerFramework,
}

impl<'a> TsGenerator<'a> {
    /// Creates a new TypeScript generator with the given semantic analysis results.
    fn new(check_result: &'a Analysis, framework: TsServerFramework) -> Self {
        Self {
            check_result,
            types_out: String::new(),
            client_out: String::new(),
            handlers_out: String::new(),
            server_out: String::new(),
            emitted_derived_types: BTreeSet::new(),
            emitted_validation_error: false,
            revivable_structs: BTreeSet::new(),
            param_enums: BTreeSet::new(),
            framework,
        }
    }

    /// Generates TypeScript `types.ts`, `client.ts`, and `handlers.ts` from the
    /// program AST.
    fn generate(mut self, program: &Program) -> GeneratedFiles {
        self.revivable_structs = compute_revivable_structs(self.check_result, program);
        self.param_enums = crate::param_enum_names(program, self.check_result);

        // Generate types.ts
        self.types_out
            .push_str("// Generated by Phoenix Gen — do not edit manually.\n\n");

        // The branded-scalar aliases + `parse*` validators (`Uuid`, `Decimal`),
        // before the types/revivers that reference them. `Money` forces the
        // `Decimal` helpers too: `reviveMoney` calls `parseDecimal` and the `Money`
        // interface's `amount` is a `Decimal`, even if no field is a bare `Decimal`.
        let uses_money = self.schema_uses_scalar(program, &Type::Money);
        for (target, _, _) in ts_branded_scalars() {
            let used = self.schema_uses_scalar(program, &target)
                || (target == Type::Decimal && uses_money);
            if used {
                self.emit_branded_helper(&target);
            }
        }
        // The composite `Money` interface + `reviveMoney` + ISO-4217 code set,
        // after the `Decimal` helpers it depends on.
        if uses_money {
            self.emit_money_helper();
        }

        for decl in &program.declarations {
            match decl {
                // A file-bearing (body-only) struct never appears as a normal TS
                // value: as a multipart request body it is exploded into the
                // derived `<Endpoint>Body` (its `File` fields read off a
                // `Record<string, Blob>`), and as a binary response it is streamed
                // as a `Buffer`/`Blob`. Emitting a bare `interface` for it would be
                // dead, exported surface, so skip it — matching the Go/Python
                // generators, which likewise skip the file-bearing base struct.
                Declaration::Struct(s) if self.struct_is_file_bearing(&s.name) => {}
                Declaration::Struct(s) => self.emit_struct(s),
                Declaration::Enum(e) => self.emit_enum(e),
                _ => {}
            }
        }

        // Emit struct-level validation functions (skip file-bearing structs: no
        // interface is emitted for them, and their constraints are validated on
        // the derived body path, not here).
        for decl in &program.declarations {
            if let Declaration::Struct(s) = decl
                && !self.struct_is_file_bearing(&s.name)
            {
                self.emit_struct_validation(s);
            }
        }

        // Emit a `parse<Enum>` validator for each simple enum used in a query/
        // request-header param (the server decode path validates the wire string).
        self.emit_enum_param_helpers(program);

        // Emit derived types and error types for endpoints
        for ep in &self.check_result.endpoints {
            self.emit_endpoint_derived_type(ep);
        }
        for ep in &self.check_result.endpoints {
            self.emit_endpoint_response_projection_type(ep);
        }
        for ep in &self.check_result.endpoints {
            self.emit_endpoint_result_type(ep);
        }
        for ep in &self.check_result.endpoints {
            self.emit_endpoint_page_type(ep);
        }
        for ep in &self.check_result.endpoints {
            self.emit_endpoint_response_type(ep);
        }
        for ep in &self.check_result.endpoints {
            self.emit_endpoint_error_types(ep);
        }

        // Emit validation functions for endpoints with constrained body fields
        for ep in &self.check_result.endpoints {
            self.emit_validation_function(ep);
        }

        // Emit one `revive<Struct>` per Date-bearing struct (used client-side to
        // turn decoded ISO strings back into `Date`s).
        self.emit_reviver_functions(program);

        // Generate client.ts
        self.client_out
            .push_str("// Generated by Phoenix Gen — do not edit manually.\n\n");
        self.emit_client_imports();
        self.emit_client_preamble();

        for (i, ep) in self.check_result.endpoints.iter().enumerate() {
            // One blank line between methods (none before the first).
            if i > 0 {
                self.client_out.push('\n');
            }
            self.emit_client_function(ep);
        }

        // Close the `api` object literal (assigned to a const → needs `;`).
        self.client_out.push_str("};\n");

        // Generate handlers.ts
        self.handlers_out
            .push_str("// Generated by Phoenix Gen — implement the handler methods below.\n\n");
        self.emit_handler_imports();
        self.handlers_out.push_str("export interface Handlers {\n");

        for ep in &self.check_result.endpoints {
            self.emit_handler_method(ep);
        }

        self.handlers_out.push_str("}\n");

        // Generate server.ts for the chosen framework. Only this file varies;
        // types/client/handlers above are framework-independent.
        self.server_out
            .push_str("// Generated by Phoenix Gen — do not edit manually.\n\n");
        match self.framework {
            TsServerFramework::Express => {
                self.emit_server_imports();
                self.emit_server_router();
            }
            TsServerFramework::Fastify => {
                self.emit_fastify_imports();
                self.emit_fastify_router();
            }
        }

        GeneratedFiles {
            types: finalize(self.types_out),
            client: finalize(self.client_out),
            handlers: finalize(self.handlers_out),
            server: finalize(self.server_out),
        }
    }

    // ── Type emission ────────────────────────────────────────────────

    /// Whether the named struct is file-bearing (body-only): it has a direct
    /// `File`/`Option<File>` field and so is legal only in endpoint
    /// `body`/`response` position. Such a struct emits no standalone interface or
    /// validator (see the type-emission loop in `generate`).
    fn struct_is_file_bearing(&self, name: &str) -> bool {
        self.check_result
            .module
            .struct_info_by_name(name)
            .is_some_and(|si| si.is_file_bearing)
    }

    /// Emits a TypeScript `export interface` for a Phoenix struct.
    fn emit_struct(&mut self, s: &StructDecl) {
        if let Some(ref doc) = s.doc_comment {
            self.types_out.push_str(&render_jsdoc("", doc));
        }
        self.types_out
            .push_str(&format!("export interface {} {{\n", s.name));

        if let Some(info) = self.check_result.module.struct_info_by_name(&s.name) {
            for f in &info.fields {
                let ts_type = type_to_ts(&f.ty);
                let optional = matches!(&f.ty, Type::Generic(name, _) if name == "Option");
                if optional {
                    self.types_out
                        .push_str(&format!("  {}?: {};\n", f.name, ts_type));
                } else {
                    self.types_out
                        .push_str(&format!("  {}: {};\n", f.name, ts_type));
                }
            }
        }

        self.types_out.push_str("}\n\n");
    }

    /// Emits a `validate{StructName}` function for a struct with constrained fields.
    fn emit_struct_validation(&mut self, s: &StructDecl) {
        let Some(info) = self.check_result.module.struct_info_by_name(&s.name) else {
            return;
        };
        if !info.fields.iter().any(|f| f.constraint.is_some()) {
            return;
        }

        // Emit ValidationError class once
        self.ensure_validation_error();

        emit_validate_signature(&mut self.types_out, &s.name);
        let vfields: Vec<ValidationField> = info
            .fields
            .iter()
            .map(|f| validation_field(&f.name, &f.ty, f.constraint.as_ref(), false))
            .collect();
        emit_validation_body(&mut self.types_out, &vfields);

        self.types_out
            .push_str(&format!("  return obj as unknown as {};\n", s.name));
        self.types_out.push_str("}\n\n");
    }

    /// Whether the schema references the branded scalar `target` anywhere a
    /// generated TS file would name its alias or call its `parse*`: struct fields,
    /// body fields, query/request-header/response-header params, the response type,
    /// or a pagination item type. Gates the one-time alias/`parse*` emission.
    fn schema_uses_scalar(&self, program: &Program, target: &Type) -> bool {
        let in_struct = program.declarations.iter().any(|d| {
            matches!(d, Declaration::Struct(s)
                if self
                    .check_result
                    .module
                    .struct_info_by_name(&s.name)
                    .is_some_and(|si| si.fields.iter().any(|f| type_mentions(&f.ty, target))))
        });
        let in_ep = self.check_result.endpoints.iter().any(|ep| {
            ep.query_params.iter().any(|q| type_mentions(&q.ty, target))
                || ep.headers.iter().any(|h| type_mentions(&h.ty, target))
                || ep
                    .response_headers
                    .iter()
                    .any(|h| type_mentions(&h.ty, target))
                || ep
                    .response
                    .as_ref()
                    .is_some_and(|t| type_mentions(t, target))
                || ep
                    .body
                    .as_ref()
                    .is_some_and(|b| b.fields.iter().any(|f| type_mentions(&f.ty, target)))
                || ep
                    .pagination
                    .as_ref()
                    .is_some_and(|p| type_mentions(&p.item_type, target))
        });
        in_struct || in_ep
    }

    /// Emits, into `types.ts`, a `<ENUM>_VALUES` array and a `parse<Enum>`
    /// validator for each simple enum used in a query/request-header param. The
    /// server decode path calls `parse<Enum>` to turn the wire string into the
    /// branded union, throwing `ValidationError` (→ 400) on an unknown variant.
    fn emit_enum_param_helpers(&mut self, program: &Program) {
        if self.param_enums.is_empty() {
            return;
        }
        self.ensure_validation_error();
        for decl in &program.declarations {
            let Declaration::Enum(e) = decl else { continue };
            if !self.param_enums.contains(&e.name) {
                continue;
            }
            let name = &e.name;
            let const_name = format!("{}_VALUES", to_screaming_snake(name));
            // The values array: one line if it fits ≤ 80 cols, else one variant
            // per line with a trailing comma (matching Prettier).
            let inline = e
                .variants
                .iter()
                .map(|v| format!("\"{}\"", v.name))
                .collect::<Vec<_>>()
                .join(", ");
            let one_line = format!("const {const_name}: readonly {name}[] = [{inline}];");
            if one_line.len() <= PRINT_WIDTH {
                self.types_out.push_str(&one_line);
                self.types_out.push_str("\n\n");
            } else {
                self.types_out
                    .push_str(&format!("const {const_name}: readonly {name}[] = [\n"));
                for v in &e.variants {
                    self.types_out.push_str(&format!("  \"{}\",\n", v.name));
                }
                self.types_out.push_str("];\n\n");
            }
            self.types_out.push_str(&format!(
                "/** Validates that `value` is a `{name}`, throwing otherwise. Run by\n \
                 * the server query/header decode path. */\n\
                 export function parse{name}(value: string): {name} {{\n  \
                 if (({const_name} as readonly string[]).includes(value)) {{\n    \
                 return value as {name};\n  \
                 }}\n  \
                 throw new ValidationError(`invalid {name}: ${{value}}`);\n}}\n\n"
            ));
        }
    }

    /// Emits the `ValidationError` class into `types.ts` exactly once.
    fn ensure_validation_error(&mut self) {
        if self.emitted_validation_error {
            return;
        }
        self.types_out
            .push_str("export class ValidationError extends Error {\n");
        self.types_out
            .push_str("  constructor(message: string) {\n");
        self.types_out.push_str("    super(message);\n");
        self.types_out
            .push_str("    this.name = \"ValidationError\";\n");
        self.types_out.push_str("  }\n");
        self.types_out.push_str("}\n\n");
        self.emitted_validation_error = true;
    }

    /// Emits a branded-scalar alias + its `parse*` validator into types.ts (once,
    /// when the schema uses that scalar). The `parse*` checks the value's format
    /// against a regex and brands the string; the decode path runs it on every
    /// such value read off the wire (response bodies/headers client-side, request
    /// bodies/params server-side). No native JS type exists to revive into, so the
    /// brand + runtime check is how a decoded value differs from a bare string.
    fn emit_branded_helper(&mut self, target: &Type) {
        let (alias, parse) = branded_scalar(target).expect("branded scalar");
        let (brand_field, re_const, re_literal, noun, err) = match target {
            Type::Uuid => (
                "__uuidBrand",
                "UUID_RE",
                "/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i",
                "UUID string (RFC 4122)",
                "invalid UUID",
            ),
            Type::Decimal => (
                "__decimalBrand",
                "DECIMAL_RE",
                r"/^-?[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?$/",
                "decimal string (exact, base-10)",
                "invalid decimal",
            ),
            other => unreachable!("not a branded scalar: {other}"),
        };
        self.types_out.push_str(&format!(
            "/** A {noun}, branded so a bare `string` cannot be used as\n \
             * one without going through `{parse}`. */\n\
             export type {alias} = string & {{ readonly {brand_field}: unique symbol }};\n\n"
        ));
        // Prettier keeps `const RE = <literal>;` on one line if it fits, else
        // breaks after `=`. The UUID literal overflows 80; the decimal one fits.
        let const_line = format!("const {re_const} = {re_literal};");
        if const_line.len() <= PRINT_WIDTH {
            self.types_out.push_str(&const_line);
            self.types_out.push_str("\n\n");
        } else {
            self.types_out
                .push_str(&format!("const {re_const} =\n  {re_literal};\n\n"));
        }
        self.types_out.push_str(&format!(
            "/** Validates the format and brands the value as `{alias}`, throwing\n \
             * otherwise. Run by the generated decode path on each wire `{alias}`. */\n\
             export function {parse}(value: string): {alias} {{\n  \
             if (!{re_const}.test(value)) {{\n    \
             throw new Error(`{err}: ${{value}}`);\n  }}\n  \
             return value as {alias};\n}}\n\n"
        ));
    }

    /// Emits the composite `Money` built-in into types.ts: the ISO-4217
    /// `CURRENCY_CODES` set, the `Money` interface (`{ amount: Decimal; currency:
    /// string }`), and `reviveMoney` (validates+rebuilds amount via `parseDecimal`
    /// and the currency against the set). Emitted after the `Decimal` helpers it
    /// calls. Slots into the revival pipeline like a struct reviver.
    fn emit_money_helper(&mut self) {
        self.types_out
            .push_str("const CURRENCY_CODES = new Set([\n");
        for code in crate::iso4217::ISO_4217_CODES {
            self.types_out.push_str(&format!("  \"{code}\",\n"));
        }
        self.types_out.push_str("]);\n\n");
        self.types_out.push_str(
            "/** A monetary amount: an exact `Decimal` plus an ISO-4217 currency code. */\n\
             export interface Money {\n  \
             amount: Decimal;\n  \
             currency: string;\n}\n\n\
             /** Validates a decoded `Money` (amount format + ISO-4217 currency) and\n \
             * brands its amount, throwing otherwise. */\n\
             export function reviveMoney(o: Money): Money {\n  \
             o.amount = parseDecimal(o.amount);\n  \
             if (!CURRENCY_CODES.has(o.currency)) {\n    \
             throw new Error(`invalid ISO 4217 currency code: ${o.currency}`);\n  }\n  \
             return o;\n}\n\n",
        );
    }

    /// Emits one `export function revive<Struct>(o: Struct): Struct` per
    /// Date-bearing struct (those in [`Self::revivable_structs`]), plus one
    /// `revive<Endpoint>Body` per Date-bearing request body. `JSON.parse`
    /// decodes a `DateTime` field to a string, so both the client (over a decoded
    /// response) and the server (over a decoded request body) run the matching
    /// reviver to reconstruct `Date`s in place. Revivers live in types.ts (full
    /// type scope, no extra imports) and call each other for nested structs; the
    /// client imports the response leaf revivers and the server the body revivers.
    fn emit_reviver_functions(&mut self, program: &Program) {
        for decl in &program.declarations {
            let Declaration::Struct(s) = decl else {
                continue;
            };
            if !self.revivable_structs.contains(&s.name) {
                continue;
            }
            let Some(info) = self.check_result.module.struct_info_by_name(&s.name) else {
                continue;
            };
            let name = &s.name;
            push_reviver_signature(&mut self.types_out, &reviver_name(name), name);
            for f in &info.fields {
                emit_field_revival(&mut self.types_out, &f.name, &f.ty, &self.revivable_structs);
            }
            self.types_out.push_str("  return o;\n}\n\n");
        }

        // Request-body revivers. A `JSON.parse`d request body (`express.json()` /
        // Fastify's parser) decodes a `DateTime` field to a string, so the server
        // runs `revive<Endpoint>Body` over the cast/validated body before handing
        // it to the handler — otherwise the handler's `Date`-typed field is a raw
        // string at runtime. The body is a derived type (`<Endpoint>Body`), so its
        // reviver is keyed on the resolved body fields rather than a struct decl.
        for ep in &self.check_result.endpoints {
            if !body_needs_revival(ep, &self.revivable_structs) {
                continue;
            }
            let Some(ref body) = ep.body else { continue };
            let name = format!("{}Body", capitalize(&ep.name));
            push_reviver_signature(&mut self.types_out, &reviver_name(&name), &name);
            for f in &body.fields {
                // A `partial` body marks a field optional via `f.optional` without
                // wrapping its type in `Option`, so synthesize that wrapper to get
                // the `!= null` guard (an absent field must not become `new
                // Date(undefined)`).
                let ty = if f.optional && !matches!(&f.ty, Type::Generic(n, _) if n == "Option") {
                    Type::Generic("Option".to_string(), vec![f.ty.clone()])
                } else {
                    f.ty.clone()
                };
                emit_field_revival(&mut self.types_out, &f.name, &ty, &self.revivable_structs);
            }
            self.types_out.push_str("  return o;\n}\n\n");
        }

        // Response-projection revivers. A projected `<Endpoint>Response` is a
        // generated type (not a struct decl), so — like the body revivers above —
        // its reviver is keyed on the resolved projected fields. The client runs it
        // over the decoded JSON when the projection reaches a revivable scalar.
        for ep in &self.check_result.endpoints {
            let Some(ref proj) = ep.response_projection else {
                continue;
            };
            let name = format!("{}Response", capitalize(&ep.name));
            if !self.revivable_structs.contains(&name) {
                continue;
            }
            push_reviver_signature(&mut self.types_out, &reviver_name(&name), &name);
            for f in &proj.fields {
                let ty = if f.optional && !matches!(&f.ty, Type::Generic(n, _) if n == "Option") {
                    Type::Generic("Option".to_string(), vec![f.ty.clone()])
                } else {
                    f.ty.clone()
                };
                emit_field_revival(&mut self.types_out, &f.name, &ty, &self.revivable_structs);
            }
            self.types_out.push_str("  return o;\n}\n\n");
        }
    }

    /// Emits a TypeScript string union type for a Phoenix enum (simple variants only).
    fn emit_enum(&mut self, e: &EnumDecl) {
        if let Some(ref doc) = e.doc_comment {
            self.types_out.push_str(&render_jsdoc("", doc));
        }

        let all_unit = e.variants.iter().all(|v| v.fields.is_empty());
        if all_unit {
            // Simple enum → string union
            let variants: Vec<String> = e
                .variants
                .iter()
                .map(|v| format!("\"{}\"", v.name))
                .collect();
            emit_union_type_alias(&mut self.types_out, &e.name, &variants);
        } else {
            // Tagged union → discriminated union of `{ tag; value }` objects.
            // Routed through `emit_union_type_alias` so the members get the same
            // Prettier-correct layout as string unions (one line if it fits, else
            // a leading-`|` member per line) and a `;` terminator.
            let members: Vec<String> = e
                .variants
                .iter()
                .map(|v| {
                    if v.fields.is_empty() {
                        format!("{{ tag: \"{}\" }}", v.name)
                    } else if v.fields.len() == 1 {
                        let ts = type_expr_to_ts(&v.fields[0]);
                        format!("{{ tag: \"{}\"; value: {} }}", v.name, ts)
                    } else {
                        let ts: Vec<String> = v.fields.iter().map(type_expr_to_ts).collect();
                        format!("{{ tag: \"{}\"; value: [{}] }}", v.name, ts.join(", "))
                    }
                })
                .collect();
            emit_union_type_alias(&mut self.types_out, &e.name, &members);
        }
    }

    /// Emits a derived type alias for an endpoint body (e.g., `Omit<User, "id">`).
    fn emit_endpoint_derived_type(&mut self, ep: &EndpointInfo) {
        let Some(ref body) = ep.body else { return };
        let type_name = format!("{}Body", capitalize(&ep.name));

        if !self.emitted_derived_types.insert(type_name.clone()) {
            return;
        }

        let ts_type = derived_type_to_ts(body);
        self.types_out
            .push_str(&format!("export type {} = {};\n\n", type_name, ts_type));
    }

    /// Emits the `<Endpoint>Response` type alias for an endpoint with an inline
    /// response projection (`response Struct pick/omit/partial`, incl. a `List<…>`
    /// element), mirroring [`Self::emit_endpoint_derived_type`] for the response
    /// side. The client revives it (see [`Self::emit_reviver_functions`]) when its
    /// fields reach a `DateTime`/`Uuid`/`Decimal`/`Money`.
    fn emit_endpoint_response_projection_type(&mut self, ep: &EndpointInfo) {
        let Some(ref proj) = ep.response_projection else {
            return;
        };
        let type_name = format!("{}Response", capitalize(&ep.name));
        if !self.emitted_derived_types.insert(type_name.clone()) {
            return;
        }
        let ts_type = derived_type_to_ts(proj);
        self.types_out
            .push_str(&format!("export type {} = {};\n\n", type_name, ts_type));
    }

    /// Emits the response-header envelope type for an endpoint that declares
    /// response headers (e.g. `interface GetPostResult { body: Post;
    /// ratelimitRemaining: number; }`). Endpoints without response headers emit
    /// nothing here and keep returning their bare response type.
    fn emit_endpoint_result_type(&mut self, ep: &EndpointInfo) {
        if ep.response_headers.is_empty() {
            return;
        }
        let name = result_type_name(ep);
        let body_type = ep
            .response
            .as_ref()
            .map(type_to_ts)
            .unwrap_or_else(|| "void".to_string());

        self.types_out
            .push_str(&format!("export interface {name} {{\n"));
        self.types_out.push_str(&format!("  body: {body_type};\n"));
        for h in &ep.response_headers {
            let ts_ty = type_to_ts(&h.ty);
            if is_header_option(h) {
                self.types_out
                    .push_str(&format!("  {}?: {};\n", h.name, ts_ty));
            } else {
                self.types_out
                    .push_str(&format!("  {}: {};\n", h.name, ts_ty));
            }
        }
        self.types_out.push_str("}\n\n");
    }

    /// Emits the pagination envelope type for an endpoint that declares a
    /// `pagination { }` block. The envelope wraps the bare `List<T>` response in
    /// a typed page object with mode-specific metadata:
    /// - **offset** → `interface ListPostsPage { items: Post[]; totalCount: number; }`
    /// - **cursor** → `interface ListPostsPage { items: Post[]; nextCursor?: string; }`
    ///
    /// Endpoints without pagination emit nothing here and keep returning their
    /// bare response type. Pagination and response headers are mutually exclusive
    /// (sema rejects the combination), so this never collides with the
    /// `<Endpoint>Result` envelope.
    fn emit_endpoint_page_type(&mut self, ep: &EndpointInfo) {
        let Some(ref pag) = ep.pagination else {
            return;
        };
        let name = page_type_name(ep);
        let item_type = type_to_ts(&pag.item_type);

        self.types_out
            .push_str(&format!("export interface {name} {{\n"));
        self.types_out
            .push_str(&format!("  items: {item_type}[];\n"));
        match pag.mode {
            PaginationMode::Offset => {
                self.types_out.push_str("  totalCount: number;\n");
            }
            // `nextCursor` is null/absent on the last page — render it optional,
            // matching how the response-header envelope renders an optional field.
            PaginationMode::Cursor => {
                self.types_out.push_str("  nextCursor?: string;\n");
            }
        }
        self.types_out.push_str("}\n\n");
    }

    /// Emits the multi-status envelope type for an endpoint that declares a
    /// `response { }` block (`response_statuses` non-empty). The handler returns,
    /// and the client observes, this envelope instead of the bare body:
    /// ```typescript
    /// export interface UpsertUserResponse {
    ///   status: number;
    ///   body?: User;
    /// }
    /// ```
    /// - `status` is the actual HTTP status (handler sets it, server writes it,
    ///   client reads it).
    /// - `body?: T` is the shared body type as an optional field, present only when
    ///   the block declares at least one typed status (`ep.response` is `Some`). An
    ///   all-typeless block (e.g. `response { 202  204 }`) has no `T`, so the
    ///   envelope is just `{ status: number }` with no `body` field.
    ///
    /// Endpoints without a `response { }` block emit nothing here and keep
    /// returning their bare response type unchanged.
    fn emit_endpoint_response_type(&mut self, ep: &EndpointInfo) {
        if ep.response_statuses.is_empty() {
            return;
        }
        let name = multi_status_type_name(ep);
        self.types_out
            .push_str(&format!("export interface {name} {{\n"));
        self.types_out.push_str("  status: number;\n");
        if let Some(ref resp) = ep.response {
            let body_type = type_to_ts(resp);
            self.types_out.push_str(&format!("  body?: {body_type};\n"));
        }
        self.types_out.push_str("}\n\n");
    }

    /// Emits error type definitions for an endpoint's error variants.
    ///
    /// For an endpoint with `error { NotFound(404), Conflict(409) }`, produces:
    /// ```typescript
    /// export type CreateUserError = "NotFound" | "Conflict";
    /// export const CreateUserErrors = { NotFound: 404, Conflict: 409 } as const;
    /// ```
    fn emit_endpoint_error_types(&mut self, ep: &EndpointInfo) {
        if ep.errors.is_empty() {
            return;
        }

        let type_name = format!("{}Error", capitalize(&ep.name));

        // String union of error variant names
        let variants: Vec<String> = ep
            .errors
            .iter()
            .map(|(name, _)| format!("\"{name}\""))
            .collect();
        emit_union_type_alias(&mut self.types_out, &type_name, &variants);

        // Const object mapping variant names to status codes
        let entries: Vec<String> = ep
            .errors
            .iter()
            .map(|(name, code)| format!("  {name}: {code}"))
            .collect();
        self.types_out.push_str(&format!(
            "export const {}Errors = {{\n{},\n}} as const;\n\n",
            capitalize(&ep.name),
            entries.join(",\n")
        ));
    }

    /// Emits a `ValidationError` class and a typed `validate*Body` function for
    /// an endpoint whose body has at least one constrained field.
    fn emit_validation_function(&mut self, ep: &EndpointInfo) {
        let Some(ref body) = ep.body else { return };
        let has_constraints = body.fields.iter().any(|f| f.constraint.is_some());
        if !has_constraints {
            return;
        }

        // Emit ValidationError class once
        self.ensure_validation_error();

        let type_name = format!("{}Body", capitalize(&ep.name));
        emit_validate_signature(&mut self.types_out, &type_name);
        let vfields: Vec<ValidationField> = body
            .fields
            .iter()
            .map(|f| validation_field(&f.name, &f.ty, f.constraint.as_ref(), f.optional))
            .collect();
        emit_validation_body(&mut self.types_out, &vfields);

        self.types_out
            .push_str(&format!("  return obj as unknown as {};\n", type_name));
        self.types_out.push_str("}\n\n");
    }

    // ── Client emission ──────────────────────────────────────────────

    /// Emits import statements for the client file, collecting all user-defined
    /// type names referenced by endpoint signatures.
    fn emit_client_imports(&mut self) {
        let mut type_imports = BTreeSet::new();

        for ep in &self.check_result.endpoints {
            if ep.body.is_some() {
                type_imports.insert(format!("{}Body", capitalize(&ep.name)));
            }
            if !ep.response_headers.is_empty() {
                // The method returns the envelope type, so import it — AND fall
                // through to import the bare body type too: the client still
                // casts the decoded JSON to it (`(await response.json()) as
                // <Body>`) before wrapping it in the envelope, so both names are
                // referenced in this file.
                type_imports.insert(result_type_name(ep));
            }
            if ep.pagination.is_some() {
                // The method returns (and casts the decoded JSON to) the page
                // envelope. The bare item type is NOT referenced in the client
                // (the response below is read as the whole page object), so the
                // `collect_import_names` on the `List<T>` response is skipped for
                // paginated endpoints — import only the page type here.
                type_imports.insert(page_type_name(ep));
            }
            if !ep.response_statuses.is_empty() {
                // The method returns the `<Endpoint>Response` envelope, so import
                // it — AND fall through to import the bare body type below: the
                // client still casts the parsed JSON to it
                // (`JSON.parse(responseText) as <T>`) before wrapping it in the
                // envelope's `body`, so both names are referenced here.
                type_imports.insert(multi_status_type_name(ep));
            }
            // A binary-download response is returned as a `Blob`, so the response
            // struct's name is never referenced in the client — don't import it.
            // A paginated response is read as the page envelope (imported above);
            // the bare `List<T>` element type is not referenced — skip it too.
            if !ep.response_is_binary
                && ep.pagination.is_none()
                && let Some(ref resp) = ep.response
            {
                collect_import_names(resp, &mut type_imports);
            }
            // Query/request-header param types name enums used in the method
            // signature; response-header types name enums the client casts on read
            // (and that appear in the result envelope). `collect_import_names` only
            // adds user `Named` types (enums/structs), skipping builtins — branded
            // scalars are imported by the dedicated loop below.
            for q in &ep.query_params {
                collect_import_names(&q.ty, &mut type_imports);
            }
            for h in ep.headers.iter().chain(ep.response_headers.iter()) {
                collect_import_names(&h.ty, &mut type_imports);
            }
        }

        // Value imports for the reviver functions the client calls. The decode
        // site for an endpoint revives its payload type — `pagination.item_type`
        // for a paginated endpoint, otherwise the bare response/body type — so
        // import the leaf struct's reviver (nested-struct revivers are called from
        // within types.ts and need no import here). Binary downloads decode to a
        // `Blob`, so they revive nothing.
        let mut reviver_imports = BTreeSet::new();
        for ep in &self.check_result.endpoints {
            if ep.response_is_binary {
                continue;
            }
            let payload = if let Some(ref pag) = ep.pagination {
                Some(&pag.item_type)
            } else {
                ep.response.as_ref()
            };
            if let Some(ty) = payload
                && let Some(reviver) = leaf_struct_reviver(ty, &self.revivable_structs)
            {
                reviver_imports.insert(reviver);
            }
        }

        // A branded scalar (`Uuid`/`Decimal`) is a builtin that, unlike other
        // scalars, needs its alias imported wherever a signature names it
        // (query/request-header params, or a bare `X`/`X[]`/… response/page-item
        // return type). Its `parse*` (a value) is needed wherever the decode path
        // calls it directly: a bare-scalar payload leaf, or a scalar response
        // header. (Struct payloads call their reviver, which calls `parse*` inside
        // types.ts — no client import.)
        for (target, alias, parse) in ts_branded_scalars() {
            for ep in &self.check_result.endpoints {
                let names = ep
                    .query_params
                    .iter()
                    .any(|q| type_mentions(&q.ty, &target))
                    || ep.headers.iter().any(|h| type_mentions(&h.ty, &target));
                let payload = if let Some(ref pag) = ep.pagination {
                    Some(&pag.item_type)
                } else if ep.response_is_binary {
                    None
                } else {
                    ep.response.as_ref()
                };
                if names || payload.is_some_and(|t| type_mentions(t, &target)) {
                    type_imports.insert(alias.to_string());
                }
                if payload.is_some_and(|t| leaf_is(t, &target))
                    || ep
                        .response_headers
                        .iter()
                        .any(|h| type_mentions(&h.ty, &target))
                {
                    reviver_imports.insert(parse.to_string());
                }
            }
        }

        // `Money` (composite builtin): import the `Money` type for a bare-`Money`
        // payload return (`Promise<Money>`/`Money[]`); its `reviveMoney` value
        // import is already collected by `leaf_struct_reviver` above. A struct
        // payload with a `Money` field imports that struct, not `Money`.
        for ep in &self.check_result.endpoints {
            let payload = if let Some(ref pag) = ep.pagination {
                Some(&pag.item_type)
            } else if ep.response_is_binary {
                None
            } else {
                ep.response.as_ref()
            };
            if payload.is_some_and(|t| type_mentions(t, &Type::Money)) {
                type_imports.insert("Money".to_string());
            }
        }

        let has_imports = !type_imports.is_empty() || !reviver_imports.is_empty();
        if !type_imports.is_empty() {
            let joined: Vec<_> = type_imports.into_iter().collect();
            emit_import(&mut self.client_out, "import type", &joined, "./types");
        }
        if !reviver_imports.is_empty() {
            let joined: Vec<_> = reviver_imports.into_iter().collect();
            emit_import(&mut self.client_out, "import", &joined, "./types");
        }
        if has_imports {
            self.client_out.push('\n');
        }
    }

    /// Emits the client preamble: `ApiError` class (if needed), base URL
    /// configuration, and the opening of the `api` object.
    fn emit_client_preamble(&mut self) {
        // Emit ApiError class if any endpoint has error variants
        let has_errors = self
            .check_result
            .endpoints
            .iter()
            .any(|ep| !ep.errors.is_empty());
        if has_errors {
            self.client_out
                .push_str("export class ApiError extends Error {\n");
            self.client_out.push_str("  constructor(\n");
            self.client_out
                .push_str("    public readonly code: string,\n");
            self.client_out
                .push_str("    public readonly status: number,\n");
            self.client_out
                .push_str("    public readonly body: string,\n");
            self.client_out.push_str("  ) {\n");
            self.client_out
                .push_str("    super(`${code} (${String(status)}): ${body}`);\n");
            self.client_out.push_str("    this.name = \"ApiError\";\n");
            self.client_out.push_str("  }\n");
            self.client_out.push_str("}\n\n");
        }

        self.client_out.push_str("export let baseUrl = \"\";\n\n");
        self.client_out
            .push_str("export function setBaseUrl(url: string): void {\n");
        self.client_out.push_str("  baseUrl = url;\n");
        self.client_out.push_str("}\n\n");
        self.client_out.push_str("export const api = {\n");
    }

    /// Emits (when revival is needed) a `const __body = <decode>;` binding and
    /// returns the expression that yields the revived response body. `decode` is
    /// the single-consume decode expression (`(await response.json()) as T`, or a
    /// `JSON.parse(...)` form); `indent` is the leading whitespace for the emitted
    /// binding statement.
    ///
    /// Binding is required for correctness, not cosmetic: a reviver may read its
    /// argument more than once (an `Option<T>` revives as `x != null ? <new x>
    /// : x`), and the decode reads the response body — which can be consumed only
    /// once. Inlining an `await response.json()` decode into such a reviver would
    /// re-`await` an already-read body and throw at runtime. So a revivable
    /// response is always bound to a re-evaluable local first. When the type needs
    /// no revival, returns `decode` unchanged and emits nothing.
    fn bind_and_revive(&mut self, indent: &str, resp: Option<&Type>, decode: &str) -> String {
        match resp.and_then(|ty| ts_revive_expr(ty, "__body", &self.revivable_structs)) {
            Some(revived) => {
                self.client_out
                    .push_str(&format!("{indent}const __body = {decode};\n"));
                revived
            }
            None => decode.to_string(),
        }
    }

    /// Emits a single async client function for an endpoint, including query
    /// parameter construction, body serialization, and typed error handling.
    fn emit_client_function(&mut self, ep: &EndpointInfo) {
        let method = ep.method.as_upper_str();
        let has_resp_headers = !ep.response_headers.is_empty();
        let body_type = ep
            .response
            .as_ref()
            .map(type_to_ts)
            .unwrap_or_else(|| "void".to_string());
        // The method's declared return type: the bare body when there are no
        // response headers (the common case, unchanged), otherwise the typed
        // envelope bundling the body + each response header. A binary-download
        // response (a single-`File` response struct) is read as a `Blob`.
        let is_multi_status = !ep.response_statuses.is_empty();
        let response_type = if ep.response_is_binary {
            "Blob".to_string()
        } else if has_resp_headers {
            result_type_name(ep)
        } else if ep.pagination.is_some() {
            // The bare `List<T>` response is wrapped in the typed page envelope;
            // the client reads the whole page object from the JSON body.
            page_type_name(ep)
        } else if is_multi_status {
            // The bare body is wrapped in the `<Endpoint>Response` envelope
            // carrying the observed status + optional parsed body.
            multi_status_type_name(ep)
        } else {
            body_type.clone()
        };
        let has_query = !ep.query_params.is_empty();
        let has_req_headers = !ep.headers.is_empty();
        // The request-header object param is nullable (`headers?:`) only when
        // EVERY request header is client-optional (default or `Option<T>`); a
        // single required header makes it `headers:`. Mirrors `opts_nullable`.
        let req_headers_nullable = ep.headers.iter().all(is_header_client_optional);
        // `opts` is nullable (`opts?:`) only when EVERY query param is optional; a
        // single required param makes it `opts:`. Computed once here so the
        // signature prefix and the per-param `params.set` access logic below can
        // never disagree about whether `opts` may be undefined.
        let opts_nullable = ep.query_params.iter().all(is_query_param_optional);

        // Build function parameters
        let mut params: Vec<Param> = Vec::new();
        for pp in &ep.path_params {
            params.push(Param::Simple(format!("{pp}: string")));
        }
        if ep.body.is_some() {
            let body_type = format!("{}Body", capitalize(&ep.name));
            params.push(Param::Simple(format!("body: {body_type}")));
        }
        if has_query {
            let fields: Vec<String> = ep
                .query_params
                .iter()
                .map(|qp| {
                    let ts_ty = type_to_ts(&qp.ty);
                    if is_query_param_optional(qp) {
                        format!("{}?: {}", qp.name, ts_ty)
                    } else {
                        format!("{}: {}", qp.name, ts_ty)
                    }
                })
                .collect();
            // A fully-optional query bag renders as `opts: {...} = {}` (a default),
            // not `opts?: {...}`. Both are omittable, but a defaulted param — unlike
            // a `?:` one — may legally precede a required parameter, so `opts` keeps
            // its slot ahead of a required `headers` without tripping TS1016.
            params.push(Param::Object {
                prefix: "opts: ".to_string(),
                fields,
                default_empty: opts_nullable,
            });
        }
        if has_req_headers {
            let fields: Vec<String> = ep
                .headers
                .iter()
                .map(|h| {
                    let ts_ty = type_to_ts(&h.ty);
                    if is_header_client_optional(h) {
                        format!("{}?: {}", h.name, ts_ty)
                    } else {
                        format!("{}: {}", h.name, ts_ty)
                    }
                })
                .collect();
            // Same as `opts`: a fully-optional header bag renders as
            // `headers: {...} = {}` (defaulted, omittable) rather than `headers?:`,
            // so it can sit in a stable slot without violating TS1016.
            params.push(Param::Object {
                prefix: "headers: ".to_string(),
                fields,
                default_empty: req_headers_nullable,
            });
        }

        // Doc comment
        if let Some(ref doc) = ep.doc_comment {
            self.client_out.push_str(&render_jsdoc("  ", doc));
        }

        // Function signature
        let head = format!("  async {}", ep.name);
        let tail = format!(": Promise<{response_type}> {{");
        self.client_out
            .push_str(&format_signature(&head, &params, &tail, "  "));

        // Query string construction (only if endpoint has query params)
        if has_query {
            self.client_out
                .push_str("    const params = new URLSearchParams();\n");
            // `opts` is always non-nullable in the emitted client (a fully-optional
            // bag is a `= {}` default, not `opts?:`), so `emit_param_set` only needs
            // each param's own optionality to decide whether to guard with
            // `!== undefined` — it never emits an eslint-flagged redundant chain.
            for qp in &ep.query_params {
                emit_param_set(
                    &mut self.client_out,
                    "    ",
                    &qp.name,
                    is_query_param_optional(qp),
                    matches!(unwrap_option_ts(&qp.ty), Type::DateTime),
                );
            }
            self.client_out
                .push_str("    const query = params.toString();\n");
        }

        // Build URL with path param substitution
        let url_expr = build_url_expr(&ep.path, &ep.path_params);
        let url_arg = if has_query {
            format!("`${{baseUrl}}{url_expr}${{query ? `?${{query}}` : \"\"}}`")
        } else {
            format!("`${{baseUrl}}{url_expr}`")
        };

        // For a multipart body (a body field is a `File`/`Option<File>`), build a
        // `FormData` BEFORE the fetch call: append each file field (the Blob/File
        // value) and each scalar field (`String(...)`). The runtime sets the
        // multipart boundary on `Content-Type` automatically, so we MUST NOT set
        // Content-Type ourselves (doing so breaks the boundary).
        let body_is_multipart = ep.body_is_multipart;
        if body_is_multipart && let Some(ref ep_body) = ep.body {
            self.client_out
                .push_str("    const formData = new FormData();\n");
            for f in &ep_body.fields {
                emit_form_data_append(&mut self.client_out, "    ", &f.name, &f.ty, f.optional);
            }
        }

        // Build the fetch init object body. When the endpoint declares request
        // headers we build a `Headers` instance (so Content-Type and the typed
        // request headers coexist) and reference it from the init; otherwise the
        // common cases stay byte-for-byte unchanged.
        let mut init_lines: Vec<String> = vec![format!("method: \"{method}\"")];
        if has_req_headers {
            self.client_out
                .push_str("    const requestHeaders = new Headers();\n");
            // Only JSON bodies get an explicit Content-Type; multipart bodies let
            // the runtime set the boundary.
            if ep.body.is_some() && !body_is_multipart {
                self.client_out
                    .push_str("    requestHeaders.set(\"Content-Type\", \"application/json\");\n");
            }
            for h in &ep.headers {
                emit_header_set(
                    &mut self.client_out,
                    "    ",
                    "requestHeaders",
                    &h.name,
                    &h.wire_name,
                    is_header_client_optional(h),
                    matches!(unwrap_option_ts(&h.ty), Type::DateTime),
                );
            }
            init_lines.push("headers: requestHeaders".to_string());
            if ep.body.is_some() {
                if body_is_multipart {
                    init_lines.push("body: formData".to_string());
                } else {
                    init_lines.push("body: JSON.stringify(body)".to_string());
                }
            }
        } else if ep.body.is_some() {
            if body_is_multipart {
                init_lines.push("body: formData".to_string());
            } else {
                init_lines.push("headers: { \"Content-Type\": \"application/json\" }".to_string());
                init_lines.push("body: JSON.stringify(body)".to_string());
            }
        }
        emit_fetch_call(&mut self.client_out, "    ", &url_arg, &init_lines);

        // Error handling
        if ep.errors.is_empty() {
            // This thrown path never reads the body, so cancel it before
            // throwing — an unconsumed fetch body holds the underlying
            // connection until GC. The `error { }` branch below needs no
            // cancel: `response.text()` consumes the body. Swallow the cancel
            // result: per the Streams spec, `cancel()` on an already-errored
            // stream returns a REJECTED promise, which would replace the
            // intended status error below with a raw stream error.
            self.client_out.push_str("    if (!response.ok) {\n");
            self.client_out
                .push_str("      await response.body?.cancel().catch(() => undefined);\n");
            self.client_out.push_str(
                "      throw new Error(`${String(response.status)}: ${response.statusText}`);\n",
            );
            self.client_out.push_str("    }\n");
        } else {
            self.client_out.push_str("    if (!response.ok) {\n");
            self.client_out
                .push_str("      const errorBody = await response.text();\n");
            for (name, code) in &ep.errors {
                emit_if_stmt(
                    &mut self.client_out,
                    "      ",
                    &format!("response.status === {code}"),
                    &format!("throw new ApiError(\"{name}\", {code}, errorBody);"),
                );
            }
            self.client_out
                .push_str("      throw new ApiError(\"Unknown\", response.status, errorBody);\n");
            self.client_out.push_str("    }\n");
        }

        // Return. `response.json()` is typed `any`; assert the declared response
        // type so callers get a typed result and strict lint rules (no-unsafe-*)
        // are satisfied.
        if ep.response_is_binary {
            // Binary download: read the raw bytes as a Blob rather than JSON.
            self.client_out
                .push_str("    return await response.blob();\n");
        } else if has_resp_headers {
            // Typed envelope: body read as today, each response header read from
            // the fetch `Response.headers` and coerced into its typed field. The
            // body binding (if any) is emitted before the `return {` so it sits at
            // statement level, not inside the object literal.
            let body_decode = format!("(await response.json()) as {body_type}");
            let body_value = self.bind_and_revive("    ", ep.response.as_ref(), &body_decode);
            self.client_out.push_str("    return {\n");
            self.client_out
                .push_str(&format!("      body: {body_value},\n"));
            for h in &ep.response_headers {
                emit_object_property(
                    &mut self.client_out,
                    "      ",
                    &h.name,
                    &response_header_coercion(h),
                );
            }
            self.client_out.push_str("    };\n");
        } else if is_multi_status {
            // Multi-status envelope: record the observed status; parse the body
            // only when the response actually carries one. The guard is on
            // CONTENT, not status code: ANY typeless status (202, 204, ...)
            // sends an empty body, and `response.json()` throws on empty input
            // — so read the text and parse only when non-empty (mirrors the Go
            // client's ContentLength/EOF guard and the Python client's
            // `response.content` check). An all-typeless block has no `T` and
            // emits just the status. The local is named `responseBody` (not
            // `body`) so it never collides with a `body` request-body parameter
            // on the same method.
            if let Some(ref resp) = ep.response {
                let body_type = type_to_ts(resp);
                self.client_out.push_str("    let responseBody: ");
                self.client_out
                    .push_str(&format!("{body_type} | undefined;\n"));
                self.client_out
                    .push_str("    const responseText = await response.text();\n");
                self.client_out.push_str("    if (responseText) {\n");
                let body_decode = format!("JSON.parse(responseText) as {body_type}");
                let body_value = self.bind_and_revive("      ", Some(resp), &body_decode);
                self.client_out
                    .push_str(&format!("      responseBody = {body_value};\n"));
                self.client_out.push_str("    }\n");
                self.client_out
                    .push_str("    return { status: response.status, body: responseBody };\n");
            } else {
                // All-typeless block: status only. Cancel the unread body so the
                // underlying connection is released for reuse immediately (an
                // unconsumed fetch body holds it until GC). Swallow the cancel
                // result: `cancel()` on an already-errored stream rejects, and a
                // post-headers connection drop must not turn a success into a
                // rejection.
                self.client_out
                    .push_str("    await response.body?.cancel().catch(() => undefined);\n");
                self.client_out
                    .push_str("    return { status: response.status };\n");
            }
        } else if let Some(ref pag) = ep.pagination {
            // Paginated: decode the page envelope, then revive `Date`s in its
            // items in place when the item type carries any.
            let decode = format!("(await response.json()) as {response_type}");
            if let Some(inner) = ts_revive_expr(&pag.item_type, "x", &self.revivable_structs) {
                self.client_out
                    .push_str(&format!("    const pageResult = {decode};\n"));
                // Prettier keeps the `.map((x) => …)` on one line when it fits, else
                // breaks after the arrow (arg one indent deeper, trailing comma).
                let one_line =
                    format!("    pageResult.items = pageResult.items.map((x) => {inner});");
                if one_line.len() <= PRINT_WIDTH {
                    self.client_out.push_str(&one_line);
                    self.client_out.push('\n');
                } else {
                    self.client_out
                        .push_str("    pageResult.items = pageResult.items.map((x) =>\n");
                    self.client_out
                        .push_str(&format!("      {inner},\n    );\n"));
                }
                self.client_out.push_str("    return pageResult;\n");
            } else {
                self.client_out.push_str(&format!("    return {decode};\n"));
            }
        } else if response_type != "void" {
            // Bare body: revive when the response type carries any `Date`. A `Map`
            // revival expands past the print width, so emit through `emit_return`
            // (Prettier-style call wrapping) rather than a flat `return …;`.
            let decode = format!("(await response.json()) as {response_type}");
            let value = self.bind_and_revive("    ", ep.response.as_ref(), &decode);
            emit_return(&mut self.client_out, "    ", &value);
        } else {
            // No response declared: cancel the unread body so the underlying
            // connection is released for reuse immediately (an unconsumed fetch
            // body holds it until GC). Swallow the cancel result: `cancel()` on
            // an already-errored stream rejects, and a post-headers connection
            // drop must not turn a success into a rejection.
            self.client_out
                .push_str("    await response.body?.cancel().catch(() => undefined);\n");
        }

        self.client_out.push_str("  },\n");
    }

    // ── Handler emission ────────────────────────────────────────────

    /// Emits import statements for the handlers file, collecting all
    /// user-defined type names referenced by handler signatures.
    fn emit_handler_imports(&mut self) {
        let mut imports = BTreeSet::new();

        for ep in &self.check_result.endpoints {
            if ep.body.is_some() {
                imports.insert(format!("{}Body", capitalize(&ep.name)));
            }
            // A binary download handler returns a `Buffer` (a Node global), so
            // the response struct's name is never referenced — skip its import.
            if !ep.response_is_binary {
                if !ep.response_headers.is_empty() {
                    // Handler returns the envelope, which already bundles the body.
                    imports.insert(result_type_name(ep));
                } else if ep.pagination.is_some() {
                    // Handler returns the page envelope (which already bundles the
                    // item type via its `items` field); import only the page type.
                    imports.insert(page_type_name(ep));
                } else if !ep.response_statuses.is_empty() {
                    // Handler returns the `<Endpoint>Response` envelope (which
                    // already bundles the optional body via its `body` field);
                    // import only the envelope type.
                    imports.insert(multi_status_type_name(ep));
                } else if let Some(ref resp) = ep.response {
                    collect_import_names(resp, &mut imports);
                }
            }
            // Query/request-header param types name enums the handler signature
            // receives (response-header enums travel inside the imported result
            // envelope, so they need no separate import).
            for q in &ep.query_params {
                collect_import_names(&q.ty, &mut imports);
            }
            for h in &ep.headers {
                collect_import_names(&h.ty, &mut imports);
            }
            // A branded scalar (`Uuid`/`Decimal`) is a builtin the handler
            // signature still names (query/header params, or a bare non-envelope
            // response) — collect its alias like a type import, since
            // `collect_import_names` only picks up `Named`s.
            for (target, alias, _) in ts_branded_scalars() {
                let bare_response = !ep.response_is_binary
                    && ep.response_headers.is_empty()
                    && ep.pagination.is_none()
                    && ep.response_statuses.is_empty()
                    && ep
                        .response
                        .as_ref()
                        .is_some_and(|t| type_mentions(t, &target));
                if ep
                    .query_params
                    .iter()
                    .any(|q| type_mentions(&q.ty, &target))
                    || ep.headers.iter().any(|h| type_mentions(&h.ty, &target))
                    || bare_response
                {
                    imports.insert(alias.to_string());
                }
            }
            // `Money` (composite builtin) named by a bare non-envelope response.
            let bare_money = !ep.response_is_binary
                && ep.response_headers.is_empty()
                && ep.pagination.is_none()
                && ep.response_statuses.is_empty()
                && ep
                    .response
                    .as_ref()
                    .is_some_and(|t| type_mentions(t, &Type::Money));
            if bare_money {
                imports.insert("Money".to_string());
            }
        }

        if !imports.is_empty() {
            let joined: Vec<_> = imports.into_iter().collect();
            emit_import(&mut self.handlers_out, "import type", &joined, "./types");
            self.handlers_out.push('\n');
        }
    }

    /// Emits a single method signature in the `Handlers` interface for an
    /// endpoint.
    ///
    /// Handler methods receive path params as individual arguments, body as a
    /// typed parameter, and query params as a required object (the server
    /// framework applies defaults before calling the handler).
    fn emit_handler_method(&mut self, ep: &EndpointInfo) {
        // With response headers the handler resolves the typed envelope; without
        // them it resolves the bare response type (unchanged common case). A
        // binary-download response is produced by the handler as a `Buffer`
        // (Node's idiomatic byte container; the server writes it to the wire).
        let response_type = if ep.response_is_binary {
            "Buffer".to_string()
        } else if !ep.response_headers.is_empty() {
            result_type_name(ep)
        } else if ep.pagination.is_some() {
            // Handler resolves the typed page envelope instead of the bare list;
            // it supplies the metadata (totalCount / nextCursor) Phoenix can't
            // compute.
            page_type_name(ep)
        } else if !ep.response_statuses.is_empty() {
            // Handler resolves the `<Endpoint>Response` envelope, choosing the
            // status code and supplying the optional body.
            multi_status_type_name(ep)
        } else {
            ep.response
                .as_ref()
                .map(type_to_ts)
                .unwrap_or_else(|| "void".to_string())
        };

        let mut params: Vec<Param> = Vec::new();
        for pp in &ep.path_params {
            params.push(Param::Simple(format!("{pp}: string")));
        }
        if ep.body.is_some() {
            let body_type = format!("{}Body", capitalize(&ep.name));
            params.push(Param::Simple(format!("body: {body_type}")));
        }
        if !ep.query_params.is_empty() {
            let fields: Vec<String> = ep
                .query_params
                .iter()
                .map(|qp| {
                    let ts_ty = type_to_ts(&qp.ty);
                    // In handlers, Option<T> fields are optional; others are required
                    // (defaults have been applied by the framework before the handler).
                    let optional = matches!(&qp.ty, Type::Generic(name, _) if name == "Option");
                    if optional {
                        format!("{}?: {}", qp.name, ts_ty)
                    } else {
                        format!("{}: {}", qp.name, ts_ty)
                    }
                })
                .collect();
            params.push(Param::Object {
                prefix: "query: ".to_string(),
                fields,
                default_empty: false,
            });
        }
        if !ep.headers.is_empty() {
            let fields: Vec<String> = ep
                .headers
                .iter()
                .map(|h| {
                    let ts_ty = type_to_ts(&h.ty);
                    // As with query: Option<T> headers are optional; others are
                    // required (the server applies defaults before the handler).
                    if is_header_option(h) {
                        format!("{}?: {}", h.name, ts_ty)
                    } else {
                        format!("{}: {}", h.name, ts_ty)
                    }
                })
                .collect();
            params.push(Param::Object {
                prefix: "headers: ".to_string(),
                fields,
                default_empty: false,
            });
        }

        if let Some(ref doc) = ep.doc_comment {
            self.handlers_out.push_str(&render_jsdoc("  ", doc));
        }
        let head = format!("  {}", ep.name);
        let tail = format!(": Promise<{response_type}>;");
        self.handlers_out
            .push_str(&format_signature(&head, &params, &tail, "  "));
    }
    // ── Server router emission ─────────────────────────────────────

    /// Emits import statements for the server router file, including validation
    /// function imports for endpoints with constrained body types.
    fn emit_server_imports(&mut self) {
        self.server_out
            .push_str("import { Router } from \"express\";\n");
        self.server_out
            .push_str("import type { Request, Response } from \"express\";\n");
        self.server_out
            .push_str("import type { Handlers } from \"./handlers\";\n");
        self.emit_server_shared_type_imports();
    }

    /// Emits the framework-independent half of the server imports — validation
    /// functions, body-type imports, and the `MultipartRequest` interface — shared
    /// by the Express and Fastify import emitters so the two cannot drift. The
    /// caller emits its framework-specific header imports first.
    fn emit_server_shared_type_imports(&mut self) {
        // Value imports from `./types`: validation functions for endpoints with
        // constrained bodies, plus body revivers for Date-bearing bodies. A
        // multipart body skips validate (it is assembled field-by-field from the
        // request), so it never imports the validate function — only its type; and
        // it never carries a `DateTime`, so it never imports a reviver. All value
        // imports go in ONE statement so two `import {…} from "./types"` lines
        // can't collide.
        let mut value_imports: Vec<String> = Vec::new();
        for ep in &self.check_result.endpoints {
            if let Some(ref body) = ep.body
                && !ep.body_is_multipart
                && body.fields.iter().any(|f| f.constraint.is_some())
            {
                let type_name = format!("{}Body", capitalize(&ep.name));
                value_imports.push(format!("validate{type_name}"));
            }
        }
        // `ValidationError` is referenced by the 400 guard whenever the server
        // validates inbound data: a constrained body, or an enum query/header
        // param whose `parse<Enum>` throws it.
        if !value_imports.is_empty() || !self.param_enums.is_empty() {
            value_imports.insert(0, "ValidationError".to_string());
        }
        for ep in &self.check_result.endpoints {
            if body_needs_revival(ep, &self.revivable_structs) {
                value_imports.push(reviver_name(&format!("{}Body", capitalize(&ep.name))));
            }
        }
        // A branded scalar's `parse*` (a value): the server validates+brands a
        // `Uuid`/`Decimal` query or request-header param through it. (Body values
        // go via the body reviver imported above, which calls `parse*` inside
        // types.ts.) Pushed in `ts_branded_scalars` order for stable output.
        for (target, _, parse) in ts_branded_scalars() {
            if self.check_result.endpoints.iter().any(|ep| {
                ep.query_params
                    .iter()
                    .any(|q| type_mentions(&q.ty, &target))
                    || ep.headers.iter().any(|h| type_mentions(&h.ty, &target))
            }) {
                value_imports.push(parse.to_string());
            }
        }
        // Each simple enum used in a query/request-header param: its `parse<Enum>`
        // validator (BTreeSet → sorted, stable output).
        for name in &self.param_enums {
            value_imports.push(format!("parse{name}"));
        }
        if !value_imports.is_empty() {
            emit_import(&mut self.server_out, "import", &value_imports, "./types");
        }

        // Import the body *type* for endpoints whose body is cast/assembled
        // directly (no validate fn supplies it): JSON bodies without constraints
        // (`req.body as XBody`) and every multipart body (`const body: XBody`).
        let mut body_type_imports: Vec<String> = Vec::new();
        for ep in &self.check_result.endpoints {
            if let Some(ref body) = ep.body
                && (ep.body_is_multipart || !body.fields.iter().any(|f| f.constraint.is_some()))
            {
                body_type_imports.push(format!("{}Body", capitalize(&ep.name)));
            }
        }
        if !body_type_imports.is_empty() {
            emit_import(
                &mut self.server_out,
                "import type",
                &body_type_imports,
                "./types",
            );
        }

        self.server_out.push('\n');

        // When any endpoint has a multipart body, emit a minimal interface
        // describing the shape an upstream multipart middleware (multer, busboy,
        // …) adds to the request: parsed scalar fields on `body`, uploaded files
        // as `Blob` values on `files` (keyed by field name). This adds NO runtime
        // dependency — the user mounts the middleware; we only type the contract.
        if self
            .check_result
            .endpoints
            .iter()
            .any(|ep| ep.body_is_multipart)
        {
            // The adapter example uses the request identifier the routes in THIS
            // file use (`req` for Express, `request` for Fastify) so the snippet
            // reads correctly against the framework being generated.
            let req = match self.framework {
                TsServerFramework::Express => "req",
                TsServerFramework::Fastify => "request",
            };
            self.server_out.push_str(
                "// A multipart middleware (multer, busboy, …) must be mounted upstream; it\n\
                 // populates these fields on the request. Scalar form fields arrive on\n\
                 // `body`; each uploaded file is expected as a `Blob` on `files`, keyed by\n\
                 // field name. NOTE: standard parsers do not produce `Blob`s directly —\n\
                 // multer's memory storage, for example, yields `{ buffer, originalname }`\n\
                 // per field — so mount a tiny adapter that maps each parsed file to a\n\
                 // `Blob`, e.g.:\n\
                 //   const blobFiles: Record<string, Blob> = {};\n",
            );
            self.server_out.push_str(&format!(
                "//   for (const [field, parts] of Object.entries({req}.files ?? {{}}))\n"
            ));
            self.server_out
                .push_str("//     blobFiles[field] = new Blob([parts[0].buffer]);\n");
            self.server_out.push_str(&format!(
                "//   ({req} as unknown as MultipartRequest).files = blobFiles;\n"
            ));
            self.server_out.push_str(
                "// (The original filename is not carried on a `Blob`; see the project docs.)\n\
                 interface MultipartRequest {\n\
                 \x20 body: Record<string, string>;\n\
                 \x20 files: Record<string, Blob>;\n\
                 }\n\n",
            );
        }
    }

    /// Emits the Fastify `server.ts` import header (just `FastifyPluginCallback` —
    /// the plugin's `(app, _opts, done)` and each route's `(request, reply)` are
    /// inferred), then the shared validation/body-type imports.
    fn emit_fastify_imports(&mut self) {
        self.server_out
            .push_str("import type { FastifyPluginCallback } from \"fastify\";\n");
        self.server_out
            .push_str("import type { Handlers } from \"./handlers\";\n");
        self.emit_server_shared_type_imports();
    }

    /// Emits `createRouter(handlers)` returning a `FastifyPluginCallback` that
    /// registers one route per endpoint. Fastify's radix-tree router resolves
    /// static-vs-parametric specificity itself, so — unlike Express — no
    /// most-specific-first ordering is needed; routes are emitted in source order.
    ///
    /// The plugin is the *callback* form (`(app, opts, done) => { … done(); }`)
    /// rather than async: route registration is synchronous, so an `async` plugin
    /// would have no `await` and trip eslint's `require-await`. (The route handlers
    /// themselves stay async — they await the handler call.)
    fn emit_fastify_router(&mut self) {
        self.server_out.push_str(
            "export function createRouter(handlers: Handlers): FastifyPluginCallback {\n",
        );
        self.server_out
            .push_str("  return (app, _opts, done) => {\n");

        let ordered: Vec<&EndpointInfo> = self.check_result.endpoints.iter().collect();
        for ep in ordered {
            self.emit_fastify_route(ep);
        }

        self.server_out.push_str("    done();\n");
        self.server_out.push_str("  };\n");
        self.server_out.push_str("}\n");
    }

    /// Emits a single Fastify route handler for an endpoint, mirroring the Express
    /// route's structure (path/query/body/header decode → handler call → response,
    /// with the same error→status and multi-status guards) but using Fastify's
    /// `(request, reply)` API. Routes nest one level deeper than Express (inside
    /// the plugin's `return (app, _opts, done) => { … }`), hence the wider base
    /// indent.
    fn emit_fastify_route(&mut self, ep: &EndpointInfo) {
        let method = ep.method.as_lower_str();
        // Fastify path params use the same `:id` syntax as Express.
        let path = ep.path.replace('{', ":").replace('}', "");

        // Decide up front whether the `app.X(...)` opener stays inline or breaks
        // across lines: a long path pushes it past the print width, and prettier
        // then wraps the call AND shifts the whole arrow body 2 spaces deeper.
        // This fixes the body's indentation, so it is built at its final depth and
        // the width-sensitive emitters (handler call, record cast, 500s) make
        // correct decisions.
        let inline_opener = format!("    app.{method}(\"{path}\", async (request, reply) => {{");
        let wraps = inline_opener.len() > PRINT_WIDTH;
        // Indents: the route opener sits at 4 (inside the plugin arrow); the try
        // body, statements, and nested blocks sit at 6/8/10, shifted one level (2
        // spaces) deeper when the opener wraps.
        let ti = if wraps { "        " } else { "      " };
        let si = if wraps { "          " } else { "        " };
        let ni = if wraps { "            " } else { "          " };

        let mut body = String::new();
        body.push_str(&format!("{ti}try {{\n"));

        // Request decode (path/body/query/headers) is shared with Express; only
        // the accessor vocabulary differs — see [`emit_route_prelude`].
        let args = emit_route_prelude(
            &mut body,
            ep,
            FASTIFY_DIALECT,
            si,
            ni,
            &self.revivable_structs,
            &self.param_enums,
        );

        // Response + error mapping are framework-independent (only the response
        // verbs differ) — see [`emit_route_response`] / [`emit_route_error_catch`].
        emit_route_response(&mut body, ep, &args, FASTIFY_DIALECT, si, ni);
        emit_route_error_catch(
            &mut body,
            ep,
            FASTIFY_DIALECT,
            ti,
            si,
            ni,
            &self.param_enums,
        );

        // Emit the route. When the inline opener fits, the whole `app.X(...)` call
        // stays on one line above the body; otherwise the call breaks one argument
        // per line (path, then the arrow), with the body two spaces deeper — the
        // form prettier produces (verified in the compile-and-lint phase).
        if !wraps {
            self.server_out.push_str(&inline_opener);
            self.server_out.push('\n');
            self.server_out.push_str(&body);
            self.server_out.push_str("    });\n\n");
        } else {
            self.server_out
                .push_str(&format!("    app.{method}(\n      \"{path}\",\n"));
            self.server_out
                .push_str("      async (request, reply) => {\n");
            self.server_out.push_str(&body);
            self.server_out.push_str("      },\n    );\n\n");
        }
    }

    /// Emits an Express-compatible `createRouter` function that wires each
    /// endpoint to its handler, parsing path/query/body parameters and
    /// mapping errors to HTTP status codes.
    fn emit_server_router(&mut self) {
        self.server_out
            .push_str("export function createRouter(handlers: Handlers): Router {\n");
        self.server_out.push_str("  const router = Router();\n\n");

        // Express matches routes first-registered-wins, so a parametric route
        // (`/api/posts/:id`) registered before a static sibling
        // (`/api/posts/paged`) would shadow it — the static path gets captured as
        // `id = "paged"`. Register more-specific (more-static) routes first so
        // literal segments win, matching the most-specific-wins semantics Go's
        // ServeMux and FastAPI already provide. The sort is stable, so endpoints
        // of equal specificity keep their source order (snapshot-stable).
        let mut ordered: Vec<&EndpointInfo> = self.check_result.endpoints.iter().collect();
        ordered.sort_by_key(|ep| crate::route_specificity_key(&ep.path));

        for ep in ordered {
            self.emit_server_route(ep);
        }

        self.server_out.push_str("  return router;\n");
        self.server_out.push_str("}\n");
    }

    /// Emits a single Express route handler for an endpoint.
    fn emit_server_route(&mut self, ep: &EndpointInfo) {
        let method = ep.method.as_lower_str();

        // Convert Phoenix path params to Express-style: {id} → :id
        let express_path = ep.path.replace('{', ":").replace('}', "");

        // Type the request's path params so `req.params.id` resolves to `string`
        // rather than Express's default `string | string[]`.
        let req_type = if ep.path_params.is_empty() {
            "Request".to_string()
        } else {
            let params = ep
                .path_params
                .iter()
                .map(|pp| format!("{pp}: string"))
                .collect::<Vec<_>>()
                .join("; ");
            format!("Request<{{ {params} }}>")
        };

        // Decide up front whether the `router.X(...)` call stays inline or breaks
        // across lines: a wide `Request<{…}>` (several path params) pushes the
        // opener past the print width. This fixes the arrow body's indentation,
        // so the body is built at its final depth and the width-sensitive
        // emitters (handler call, query coercion) make correct decisions.
        let inline_opener = format!(
            "  router.{method}(\"{express_path}\", async (req: {req_type}, res: Response) => {{"
        );
        let wraps = inline_opener.len() > PRINT_WIDTH;
        // `try`/`catch` indent, statement indent, and nested-block indent. When
        // the call wraps, the whole arrow body sits one level (2 spaces) deeper.
        let ti = if wraps { "      " } else { "    " };
        let si = if wraps { "        " } else { "      " };
        let ni = if wraps { "          " } else { "        " };

        let mut body = String::new();
        body.push_str(&format!("{ti}try {{\n"));

        // Request decode (path/body/query/headers) is shared with Fastify; only
        // the accessor vocabulary differs — see [`emit_route_prelude`].
        let args = emit_route_prelude(
            &mut body,
            ep,
            EXPRESS_DIALECT,
            si,
            ni,
            &self.revivable_structs,
            &self.param_enums,
        );

        // Response + error mapping are framework-independent (only the response
        // verbs differ) — see [`emit_route_response`] / [`emit_route_error_catch`].
        emit_route_response(&mut body, ep, &args, EXPRESS_DIALECT, si, ni);
        emit_route_error_catch(
            &mut body,
            ep,
            EXPRESS_DIALECT,
            ti,
            si,
            ni,
            &self.param_enums,
        );

        // Emit the route. When the inline opener fits, the whole `router.X(...)`
        // call stays on one line above the body; otherwise the call breaks one
        // argument per line, and the arrow function's own parameter list breaks
        // too if it overflows (Prettier style).
        if !wraps {
            self.server_out.push_str(&inline_opener);
            self.server_out.push('\n');
            self.server_out.push_str(&body);
            self.server_out.push_str("  });\n\n");
        } else {
            self.server_out
                .push_str(&format!("  router.{method}(\n    \"{express_path}\",\n"));
            emit_arrow_header(&mut self.server_out, "    ", &req_type);
            self.server_out.push_str(&body);
            self.server_out.push_str("    },\n  );\n\n");
        }
    }
}

// ── Helper functions ─────────────────────────────────────────────────

/// Whether a body field's resolved type is `Option<File>` (an optional upload).
fn field_ty_is_optional_file(ty: &Type) -> bool {
    matches!(ty, Type::Generic(name, args) if name == "Option" && args.len() == 1 && matches!(args[0], Type::File))
}

/// Emits a client-side `formData.append(...)` for one multipart body field.
///
/// File fields append the `Blob`/`File` value directly; scalar fields are
/// stringified (multipart form values are always text on the wire). Any
/// optional field — `Option<T>`, or a `partial`-relaxed field (`optional`) — is
/// guarded so a missing value is omitted entirely rather than appended as the
/// literal string `"undefined"` (which the server would then coerce to `NaN`,
/// `false`, or store verbatim).
fn emit_form_data_append(out: &mut String, indent: &str, name: &str, ty: &Type, optional: bool) {
    // `optional` covers `partial`-relaxed fields; `Option<T>`/`Option<File>` is
    // detected from the type itself.
    let is_optional = optional
        || field_ty_is_optional_file(ty)
        || matches!(ty, Type::Generic(n, _) if n == "Option");
    let value = if matches!(ty, Type::File) || field_ty_is_optional_file(ty) {
        format!("body.{name}")
    } else {
        format!("String(body.{name})")
    };
    if is_optional {
        out.push_str(&format!("{indent}if (body.{name} !== undefined) {{\n"));
        out.push_str(&format!(
            "{indent}  formData.append(\"{name}\", {value});\n"
        ));
        out.push_str(&format!("{indent}}}\n"));
    } else {
        out.push_str(&format!("{indent}formData.append(\"{name}\", {value});\n"));
    }
}

/// Server-side expression extracting one multipart field into its typed body
/// value. File fields read the parsed `Blob` from `multipart.files`; scalar
/// fields read the string from `multipart.body` and coerce to the field type.
///
/// `optional` is true for a `partial`-relaxed or `Option<T>` scalar: an absent
/// value stays `undefined` rather than coercing to `NaN`/`false`, matching the
/// `T | undefined` shape of the optional field in the generated body type.
fn multipart_field_extraction(name: &str, ty: &Type, optional: bool) -> String {
    if matches!(ty, Type::File) || field_ty_is_optional_file(ty) {
        return format!("multipart.files.{name}");
    }
    let is_option = matches!(ty, Type::Generic(n, _) if n == "Option");
    let inner = match ty {
        Type::Generic(n, args) if n == "Option" && args.len() == 1 => &args[0],
        _ => ty,
    };
    let raw = format!("multipart.body.{name}");
    let coerced = match inner {
        Type::Int | Type::Float => format!("Number({raw})"),
        Type::Bool => format!("{raw} === \"true\""),
        _ => raw.clone(),
    };
    if optional || is_option {
        format!("{raw} !== undefined ? {coerced} : undefined")
    } else {
        coerced
    }
}

/// Generates a TypeScript expression that extracts and coerces a query parameter
/// from `{base}.{name}`, applying the default value if the parameter is absent.
/// Each framework supplies its own `base`: Express reads `req.query` directly,
/// while Fastify casts `request.query` to a `Record<string, string | undefined>`
/// local (`rawQuery`). When `raw_is_str_opt` is true the accessor is already
/// typed `string | undefined`, so the `Option<String>` branch drops the
/// now-redundant `as string | undefined` cast that eslint's
/// `no-unnecessary-type-assertion` would reject (the required-`String` cast
/// stays — it narrows away `undefined`); Express passes `false` because
/// `req.query.X` is wider than `string | undefined` (`ParsedQs`).
fn query_param_coercion_with(
    qp: &phoenix_sema::checker::QueryParamInfo,
    base: &str,
    raw_is_str_opt: bool,
    param_enums: &BTreeSet<String>,
) -> String {
    let raw = format!("{base}.{}", qp.name);
    let is_option = matches!(&qp.ty, Type::Generic(name, _) if name == "Option");

    // Determine the coercion expression based on the resolved type
    let coerced = match &qp.ty {
        Type::Int | Type::Float => format!("Number({raw})"),
        Type::Bool => format!("{raw} === \"true\""),
        Type::String => format!("{raw} as string"),
        // `new Date` rejects `undefined`; `raw` is `string | undefined` here (the
        // `as string` of the String branch shows it isn't pre-narrowed), so the
        // cast is necessary, not redundant.
        Type::DateTime => format!("new Date({raw} as string)"),
        // A branded scalar's `parse*` validates + brands; like `new Date` it needs
        // a `string`, and `raw` here is `string | undefined`, so the `as string`
        // cast is required.
        Type::Uuid | Type::Decimal => {
            let parse = branded_scalar(&qp.ty).expect("branded scalar").1;
            format!("{parse}({raw} as string)")
        }
        // A simple enum: validate the wire string into the branded union via the
        // generated `parse<Enum>` (throws ValidationError → 400 on an unknown
        // variant). Like the branded scalars, `raw` is `string | undefined` here.
        Type::Named(n) if param_enums.contains(n) => format!("parse{n}({raw} as string)"),
        Type::Generic(name, args) if name == "Option" && !args.is_empty() => {
            // For Option<T>, coerce the inner type if present
            match &args[0] {
                Type::Int | Type::Float => {
                    format!("{raw} !== undefined ? Number({raw}) : undefined")
                }
                Type::Bool => format!("{raw} !== undefined ? {raw} === \"true\" : undefined"),
                // When `raw` is already typed `string | undefined` (Fastify's
                // cast record), `!== undefined` narrows it to `string`, so no cast
                // — one would be flagged unnecessary. Express's `raw` is the wide
                // `ParsedQs` union, which `!== undefined` does NOT narrow to a
                // `Date`-constructible type, so it needs the `as string` cast.
                Type::DateTime if raw_is_str_opt => {
                    format!("{raw} !== undefined ? new Date({raw}) : undefined")
                }
                Type::DateTime => {
                    format!("{raw} !== undefined ? new Date({raw} as string) : undefined")
                }
                inner @ (Type::Uuid | Type::Decimal) => {
                    let parse = branded_scalar(inner).expect("branded scalar").1;
                    if raw_is_str_opt {
                        format!("{raw} !== undefined ? {parse}({raw}) : undefined")
                    } else {
                        format!("{raw} !== undefined ? {parse}({raw} as string) : undefined")
                    }
                }
                Type::Named(n) if param_enums.contains(n) => {
                    if raw_is_str_opt {
                        format!("{raw} !== undefined ? parse{n}({raw}) : undefined")
                    } else {
                        format!("{raw} !== undefined ? parse{n}({raw} as string) : undefined")
                    }
                }
                _ if raw_is_str_opt => raw.clone(),
                _ => format!("{raw} as string | undefined"),
            }
        }
        _ => format!("{raw} as string"),
    };

    // Apply default value if present
    if is_option {
        return coerced;
    }
    if let Some(ref default) = qp.default_value {
        let default_ts = default_value_to_ts(default);
        format!("{raw} !== undefined ? {coerced} : {default_ts}")
    } else {
        coerced
    }
}

/// Converts a [`DefaultValue`] to a TypeScript literal.
///
/// Unlike the Python generator (which forces a decimal point so a float reads as
/// a `float`), `Float` is rendered with a plain `to_string()` here on purpose:
/// JavaScript has a single `number` type, so `0` and `0.0` are identical and no
/// fix-up is needed. Keep this divergence from `python.rs` intentional.
fn default_value_to_ts(val: &DefaultValue) -> String {
    match val {
        DefaultValue::Int(v) => v.to_string(),
        DefaultValue::Float(v) => v.to_string(),
        DefaultValue::String(v) => format!("\"{}\"", v),
        DefaultValue::Bool(v) => v.to_string(),
        // An enum value is a member of the string-union type, so the default is
        // just the variant string literal — no enum-name qualifier needed.
        DefaultValue::Enum(v) => format!("\"{}\"", v),
    }
}

/// Builds a [`ValidationField`] for a field of resolved type `ty`, unwrapping
/// `Option<T>` so the emitted `typeof` guard narrows the inner primitive (and the
/// field is treated as skippable when absent). `extra_optional` folds in any
/// *other* reason the field may be omitted — a `partial`-applied body field.
///
/// Shared by the struct validator ([`TsGenerator::emit_struct_validation`]) and
/// the derived-body validator ([`TsGenerator::emit_validation_function`]) so
/// their `Option` handling cannot drift. That drift is exactly what made the
/// body validator emit a `.length` access on an un-narrowed `unknown`
/// (TS18047 / TS2339) for a constrained `Option<T>` body field: it skipped the
/// `typeof` narrowing the struct validator applies.
fn validation_field(
    name: &str,
    ty: &Type,
    constraint: Option<&phoenix_parser::ast::Expr>,
    extra_optional: bool,
) -> ValidationField {
    let is_option = matches!(ty, Type::Generic(n, _) if n == "Option");
    let inner_ty = if is_option {
        match ty {
            Type::Generic(_, args) => &args[0],
            _ => ty,
        }
    } else {
        ty
    };
    ValidationField {
        name: name.to_string(),
        optional: extra_optional || is_option,
        ts_typeof: ts_typeof_of(inner_ty),
        constraint_guard: constraint.map(|c| {
            let (code, needs_conjunct_parens) = negated_constraint_to_ts(c, name);
            ConstraintGuard {
                code,
                needs_conjunct_parens,
            }
        }),
    }
}

/// Renders the negated constraint for a validation guard, with minimal
/// parentheses (Prettier style), returning the guard code plus whether it must be
/// wrapped in parens when AND-joined with an optional field's presence check (see
/// [`ValidationField::constraint_guard`]).
///
/// `!` (`PREC_UNARY`) only needs to wrap an operand whose precedence is lower, so
/// a method-call/atom constraint negates as `!obj.x.includes("/")` (no parens —
/// `!` binds looser than member/call) while a comparison negates as
/// `!(obj.x > 5)`. Emitting `!(…)` unconditionally would leave redundant parens
/// that `prettier --check` rejects. These `!`-rooted guards are unary-tight, so
/// they never need conjunct parens.
///
/// A constraint that is itself a `!x` collapses to `x` rather than the double
/// negation `!!x` (which eslint's `no-unnecessary-condition`/`no-extra-boolean-cast`
/// flags). `x` is emitted bare — clean standalone (`if (x)`) and as the right of
/// an `&&` (`if (obj.f !== undefined && x)`) for any `x` that binds at least as
/// tight as `&&`. The one exception is a `||`-rooted `x` (`!(a || b)` → `a || b`):
/// bare, `obj.f !== undefined && a || b` mis-parses as `(… && a) || b`, so the
/// `needs_conjunct_parens` flag is set and the caller wraps it to `(a || b)` in
/// the conjunct position only — wrapping it unconditionally would instead leave
/// the redundant `if ((a || b))` parens prettier rejects in the standalone case.
fn negated_constraint_to_ts(expr: &phoenix_parser::ast::Expr, field_name: &str) -> (String, bool) {
    use phoenix_parser::ast::{Expr, UnaryOp};
    if let Expr::Unary(un) = expr
        && matches!(un.op, UnaryOp::Not)
    {
        // Negating a `!x` constraint is just `x`. It needs conjunct parens exactly
        // when it binds looser than `&&` — i.e. a top-level `||`.
        let (code, prec) = constraint_expr_prec(&un.operand, field_name);
        return (code, prec < PREC_AND);
    }
    let (code, prec) = constraint_expr_prec(expr, field_name);
    let guard = if prec < PREC_UNARY {
        format!("!({code})")
    } else {
        format!("!{code}")
    };
    (guard, false)
}

/// Recursively converts a Phoenix constraint `Expr` to a TypeScript expression
/// string, replacing `self` with `obj.{field_name}` for use in validation
/// functions.
fn constraint_expr_to_ts(expr: &phoenix_parser::ast::Expr, field_name: &str) -> String {
    constraint_expr_prec(expr, field_name).0
}

/// JS operator-precedence tiers used to decide when parentheses are needed.
/// Higher binds tighter. Mirrors the standard JS grammar closely enough for the
/// expressions Phoenix constraints can produce, so output matches Prettier's
/// minimal-parens style.
const PREC_ATOM: u8 = 100;
const PREC_UNARY: u8 = 14;
const PREC_OR: u8 = 3;
const PREC_AND: u8 = 4;

/// Converts a constraint `Expr` to `(code, precedence)`. The precedence lets the
/// caller decide whether to wrap the child in parentheses.
fn constraint_expr_prec(expr: &phoenix_parser::ast::Expr, field_name: &str) -> (String, u8) {
    use phoenix_parser::ast::{BinaryOp, Expr, LiteralKind, UnaryOp};

    // Wraps `child` in parens only if its precedence is below `min`.
    fn paren(child: (String, u8), min: u8) -> String {
        if child.1 < min {
            format!("({})", child.0)
        } else {
            child.0
        }
    }

    match expr {
        Expr::Ident(ident) if ident.name == "self" => (format!("obj.{field_name}"), PREC_ATOM),
        Expr::Ident(ident) => (ident.name.clone(), PREC_ATOM),
        Expr::Literal(lit) => {
            let s = match &lit.kind {
                LiteralKind::Int(v) => v.to_string(),
                LiteralKind::Float(v) => v.to_string(),
                LiteralKind::String(v) => format!("\"{}\"", v),
                LiteralKind::Bool(v) => v.to_string(),
            };
            (s, PREC_ATOM)
        }
        Expr::Binary(bin) => {
            let (op, prec) = match bin.op {
                BinaryOp::Or => ("||", PREC_OR),
                BinaryOp::And => ("&&", PREC_AND),
                BinaryOp::Eq => ("===", 8),
                BinaryOp::NotEq => ("!==", 8),
                BinaryOp::Lt => ("<", 9),
                BinaryOp::Gt => (">", 9),
                BinaryOp::LtEq => ("<=", 9),
                BinaryOp::GtEq => (">=", 9),
                BinaryOp::Add => ("+", 11),
                BinaryOp::Sub => ("-", 11),
                BinaryOp::Mul => ("*", 12),
                BinaryOp::Div => ("/", 12),
                BinaryOp::Mod => ("%", 12),
            };
            // Left-associative: left child needs only >= prec, right child > prec.
            let left = paren(constraint_expr_prec(&bin.left, field_name), prec);
            let right = paren(constraint_expr_prec(&bin.right, field_name), prec + 1);
            (format!("{left} {op} {right}"), prec)
        }
        Expr::Unary(un) => {
            let operand = paren(constraint_expr_prec(&un.operand, field_name), PREC_UNARY);
            let s = match un.op {
                UnaryOp::Neg => format!("-{operand}"),
                UnaryOp::Not => format!("!{operand}"),
            };
            (s, PREC_UNARY)
        }
        Expr::FieldAccess(fa) => {
            let object = paren(constraint_expr_prec(&fa.object, field_name), PREC_ATOM);
            (format!("{object}.{}", fa.field), PREC_ATOM)
        }
        Expr::MethodCall(mc) => {
            let object = paren(constraint_expr_prec(&mc.object, field_name), PREC_ATOM);
            let args: Vec<String> = mc
                .args
                .iter()
                .map(|a| constraint_expr_to_ts(a, field_name))
                .collect();
            // Map Phoenix method names to JS equivalents
            let method = match mc.method.as_str() {
                "contains" => "includes",
                other => other,
            };
            (format!("{object}.{method}({})", args.join(", ")), PREC_ATOM)
        }
        _ => ("true".to_string(), PREC_ATOM),
    }
}

/// Renders `text` as a JSDoc comment at `indent`. A single-line comment stays on
/// one line (`/** text */`); a multi-line doc comment (the lexer joins its lines
/// with `\n`) expands to the block form with each line on its own ` * ` row, so
/// continuation lines stay inside the comment instead of leaking as code. Matches
/// Prettier's JSDoc layout in both cases.
fn render_jsdoc(indent: &str, text: &str) -> String {
    if !text.contains('\n') {
        return format!("{indent}/** {text} */\n");
    }
    let mut out = format!("{indent}/**\n");
    for line in text.split('\n') {
        // Trim per line so an empty (or whitespace-only) doc line renders as a
        // bare ` *` row with no trailing space, matching Prettier — and matching
        // the `trim_end` the Go (`render_line_comment`) and Python
        // (`render_hash_comment`) sibling helpers use.
        out.push_str(format!("{indent} * {line}").trim_end());
        out.push('\n');
    }
    out.push_str(&format!("{indent} */\n"));
    out
}

/// Returns the inner type of an `Option<T>`, or the type unchanged otherwise.
/// Used to decide per-scalar wire handling (e.g. `DateTime` vs `Option<DateTime>`
/// both stringify a `Date` the same way once the optional guard has run).
fn unwrap_option_ts(ty: &Type) -> &Type {
    match ty {
        Type::Generic(name, args) if name == "Option" && args.len() == 1 => &args[0],
        other => other,
    }
}

/// The name of a struct's reviver function (`User` → `reviveUser`).
fn reviver_name(struct_name: &str) -> String {
    format!("revive{}", capitalize(struct_name))
}

/// Emits a reviver's `export function <fn>(o: <ty>): <ty> {` header, wrapping the
/// single parameter onto its own line (Prettier's layout) when the one-line form
/// exceeds the print width — a long type name (e.g. `CreateAccountBody`) pushes
/// it over 80 columns.
fn push_reviver_signature(out: &mut String, fn_name: &str, ty: &str) {
    let one_line = format!("export function {fn_name}(o: {ty}): {ty} {{");
    if one_line.len() <= PRINT_WIDTH {
        out.push_str(&one_line);
        out.push('\n');
    } else {
        out.push_str(&format!(
            "export function {fn_name}(\n  o: {ty},\n): {ty} {{\n"
        ));
    }
}

/// The branded validated-string scalars and their `(TS alias, parse fn)` names.
/// `DateTime` is NOT here — it has a native `Date` and decodes via `new Date`, not
/// a branded alias + `parse*`. The single source of truth for "which scalars get
/// a `type X = string & {…}` alias + `parseX` validator," iterated by the helper
/// emission and the per-file import collection.
fn ts_branded_scalars() -> [(Type, &'static str, &'static str); 2] {
    [
        (Type::Uuid, "Uuid", "parseUuid"),
        (Type::Decimal, "Decimal", "parseDecimal"),
    ]
}

/// If `ty` is a branded scalar, its `(alias, parse fn)` — e.g. `Uuid` →
/// `("Uuid", "parseUuid")`. Drives `type_to_ts` and `ts_revive_expr`.
fn branded_scalar(ty: &Type) -> Option<(&'static str, &'static str)> {
    ts_branded_scalars()
        .into_iter()
        .find(|(t, _, _)| t == ty)
        .map(|(_, alias, parse)| (alias, parse))
}

/// Whether `ty` is (or has a generic arg that is) `target`. Does NOT recurse
/// through `Named` structs — callers scan struct fields directly. Used to decide
/// the branded-alias / `parse*` imports a file needs.
fn type_mentions(ty: &Type, target: &Type) -> bool {
    ty == target
        || matches!(ty, Type::Generic(_, args) if args.iter().any(|a| type_mentions(a, target)))
}

/// Whether peeling `Option`/`List`/`Map` reaches a bare `target` value leaf — i.e.
/// the decode path calls its `parse*` directly (not via a struct reviver), so the
/// file referencing it must import that `parse*`.
fn leaf_is(ty: &Type, target: &Type) -> bool {
    if ty == target {
        return true;
    }
    match ty {
        Type::Generic(n, args) if n == "Option" && args.len() == 1 => leaf_is(&args[0], target),
        Type::Generic(n, args) if n == "List" && args.len() == 1 => leaf_is(&args[0], target),
        Type::Generic(n, args) if n == "Map" && args.len() == 2 => leaf_is(&args[1], target),
        _ => false,
    }
}

/// Computes the set of (bare) struct names that transitively contain a
/// `DateTime` — directly, or via `Option`/`List`/`Map`, or through a field that
/// references another such struct. Fixpoint iteration: a struct joins the set
/// once any of its field types reaches a `DateTime` (where a `Named` field counts
/// only if its struct is already in the set). File-bearing structs are excluded
/// (they emit no interface/value).
fn compute_revivable_structs(check_result: &Analysis, program: &Program) -> BTreeSet<String> {
    let mut structs: Vec<(String, Vec<Type>)> = program
        .declarations
        .iter()
        .filter_map(|d| match d {
            Declaration::Struct(s) => check_result
                .module
                .struct_info_by_name(&s.name)
                .filter(|si| !si.is_file_bearing)
                .map(|si| {
                    (
                        s.name.clone(),
                        si.fields.iter().map(|f| f.ty.clone()).collect(),
                    )
                }),
            _ => None,
        })
        .collect();
    // Projected response structs (`<Endpoint>Response`) are response-side, so they
    // participate in the same revival fixed-point: the client revives a projected
    // response whose fields reach a `DateTime`/`Uuid`/`Decimal`/`Money`.
    for ep in &check_result.endpoints {
        if let Some(ref proj) = ep.response_projection {
            structs.push((
                format!("{}Response", capitalize(&ep.name)),
                proj.fields.iter().map(|f| f.ty.clone()).collect(),
            ));
        }
    }

    let mut set: BTreeSet<String> = BTreeSet::new();
    loop {
        let mut changed = false;
        for (name, field_types) in &structs {
            if set.contains(name) {
                continue;
            }
            if field_types.iter().any(|t| ts_type_needs_revival(t, &set)) {
                set.insert(name.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    set
}

/// Whether an endpoint's request body needs a `Date` revival pass on the server:
/// it is a non-multipart body (multipart fields are scalar-only — never a
/// `DateTime`) with at least one field that reaches a `DateTime` (directly or
/// transitively). Drives both the `revive<Endpoint>Body` emission and its server
/// import/call.
fn body_needs_revival(ep: &EndpointInfo, set: &BTreeSet<String>) -> bool {
    !ep.body_is_multipart
        && ep
            .body
            .as_ref()
            .is_some_and(|b| b.fields.iter().any(|f| ts_type_needs_revival(&f.ty, set)))
}

/// Whether a value of type `ty` needs a `Date` revival pass — it reaches a
/// `DateTime` directly, through `Option`/`List`/`Map`/generic args, or through a
/// `Named` struct already known to be revivable (`set`).
fn ts_type_needs_revival(ty: &Type, set: &BTreeSet<String>) -> bool {
    match ty {
        // `DateTime` → `new Date(...)`; `Uuid`/`Decimal` → `parse*(...)`;
        // `Money` → `reviveMoney(...)`. All are post-decode transforms.
        Type::DateTime | Type::Uuid | Type::Decimal | Type::Money => true,
        Type::Named(name) => set.contains(name),
        Type::Generic(_, args) => args.iter().any(|a| ts_type_needs_revival(a, set)),
        _ => false,
    }
}

/// An expression that yields the revived value of `expr` (typed `ty`), or `None`
/// when `ty` needs no revival. `Date`s are rebuilt with `new Date(...)`; structs
/// delegate to their `revive<Struct>`; collections map/rebuild their elements.
/// `expr` may be re-evaluated (Option/Map), so callers pass a simple accessor.
fn ts_revive_expr(ty: &Type, expr: &str, set: &BTreeSet<String>) -> Option<String> {
    match ty {
        Type::DateTime => Some(format!("new Date({expr})")),
        // A branded scalar validates+brands via its `parse*` (e.g. `parseUuid`
        // checks RFC 4122; `parseDecimal` checks the decimal format).
        Type::Uuid | Type::Decimal => {
            branded_scalar(ty).map(|(_, parse)| format!("{parse}({expr})"))
        }
        // `Money` is a composite: `reviveMoney` validates+rebuilds it (amount via
        // `parseDecimal`, currency via the code set).
        Type::Money => Some(format!("reviveMoney({expr})")),
        Type::Named(name) if set.contains(name) => Some(format!("{}({expr})", reviver_name(name))),
        Type::Generic(n, args) if n == "Option" && args.len() == 1 => {
            ts_revive_expr(&args[0], expr, set)
                .map(|inner| format!("{expr} != null ? {inner} : {expr}"))
        }
        Type::Generic(n, args) if n == "List" && args.len() == 1 => {
            ts_revive_expr(&args[0], "x", set).map(|inner| format!("{expr}.map((x) => {inner})"))
        }
        Type::Generic(n, args) if n == "Map" && args.len() == 2 => {
            ts_revive_expr(&args[1], "v", set).map(|inner| {
                format!("Object.fromEntries(Object.entries({expr}).map(([k, v]) => [k, {inner}]))")
            })
        }
        _ => None,
    }
}

/// Peels `Option`/`List`/`Map` to the value leaf; if that leaf is a revivable
/// struct, returns the reviver name the CLIENT must import (nested struct
/// revivers are called from within types.ts, so they are not imported). A
/// `DateTime` or scalar leaf needs `new Date`/no work, not an imported function.
fn leaf_struct_reviver(ty: &Type, set: &BTreeSet<String>) -> Option<String> {
    match ty {
        Type::Named(name) if set.contains(name) => Some(reviver_name(name)),
        // `Money` is a composite built-in; its reviver is the client import for a
        // bare-`Money` payload (a struct payload imports its own reviver instead).
        Type::Money => Some("reviveMoney".to_string()),
        Type::Generic(n, args) if n == "Option" && args.len() == 1 => {
            leaf_struct_reviver(&args[0], set)
        }
        Type::Generic(n, args) if n == "List" && args.len() == 1 => {
            leaf_struct_reviver(&args[0], set)
        }
        Type::Generic(n, args) if n == "Map" && args.len() == 2 => {
            leaf_struct_reviver(&args[1], set)
        }
        _ => None,
    }
}

/// A revival statement for one field, before layout. `Assign` is a single
/// `target = <expr>;`; `ForEach` keeps a `Map` value-revival loop's header and
/// body separate so the emitter can break between them the way Prettier breaks
/// `for (…) singleStatement;` when the one-line form overflows the print width.
enum ReviveStmt {
    Assign(String),
    ForEach { header: String, body: String },
}

impl ReviveStmt {
    /// The single-line rendering (`<header> <body>` for a loop).
    fn one_line(&self) -> String {
        match self {
            ReviveStmt::Assign(s) => s.clone(),
            ReviveStmt::ForEach { header, body } => format!("{header} {body}"),
        }
    }
}

/// Emits the in-place revival statement(s) for one struct field inside a reviver.
/// A required field becomes `o.f = <expr>;` (or a `for…of` loop for a `Map`); an
/// `Option<...>` field wraps that in an `if (o.f != null)` guard. No-op when the
/// field needs no revival. Layout mirrors Prettier (one line when ≤ 80 cols,
/// breaking a guard or a `for` header onto its own line otherwise).
fn emit_field_revival(out: &mut String, field: &str, ty: &Type, set: &BTreeSet<String>) {
    let target = format!("o.{field}");
    // Peel a single `Option` layer into a `!= null` guard around the inner
    // revival (a struct/derived field is never doubly-optional in practice).
    if let Type::Generic(n, args) = ty
        && n == "Option"
        && args.len() == 1
    {
        if let Some(stmt) = revive_stmt(&args[0], &target, set) {
            let one_line = format!("  if ({target} != null) {}", stmt.one_line());
            if one_line.len() <= PRINT_WIDTH {
                out.push_str(&one_line);
                out.push('\n');
            } else {
                // Guard on its own line; the inner statement indented under it,
                // itself broken (a `for` header/body split) if still too long.
                out.push_str(&format!("  if ({target} != null)\n"));
                push_revive_stmt(out, "    ", &stmt);
            }
        }
        return;
    }
    if let Some(stmt) = revive_stmt(ty, &target, set) {
        push_revive_stmt(out, "  ", &stmt);
    }
}

/// Pushes `stmt` at `indent`, breaking a `ForEach` between its `for` header and
/// body when the one-line form would exceed the print width — the layout Prettier
/// produces for `for (…) singleStatement;`. An `Assign` is always one line (its
/// expression is never long enough to wrap in practice).
fn push_revive_stmt(out: &mut String, indent: &str, stmt: &ReviveStmt) {
    match stmt {
        ReviveStmt::Assign(s) => out.push_str(&format!("{indent}{s}\n")),
        ReviveStmt::ForEach { header, body } => {
            let one_line = format!("{indent}{header} {body}");
            if one_line.len() <= PRINT_WIDTH {
                out.push_str(&one_line);
                out.push('\n');
            } else {
                out.push_str(&format!("{indent}{header}\n{indent}  {body}\n"));
            }
        }
    }
}

/// The statement that revives `target` (typed `ty`) in place: a `for…of` loop for
/// a `Map` value, otherwise a `target = <expr>;` assignment. `None` when `ty`
/// needs no revival.
fn revive_stmt(ty: &Type, target: &str, set: &BTreeSet<String>) -> Option<ReviveStmt> {
    if let Type::Generic(n, args) = ty
        && n == "Map"
        && args.len() == 2
    {
        return ts_revive_expr(&args[1], "v", set).map(|inner| ReviveStmt::ForEach {
            header: format!("for (const [k, v] of Object.entries({target}))"),
            body: format!("{target}[k] = {inner};"),
        });
    }
    ts_revive_expr(ty, target, set).map(|expr| ReviveStmt::Assign(format!("{target} = {expr};")))
}

/// Converts a resolved Phoenix `Type` to a TypeScript type string.
fn type_to_ts(ty: &Type) -> String {
    match ty {
        Type::Int | Type::Float => "number".to_string(),
        Type::String => "string".to_string(),
        Type::Bool => "boolean".to_string(),
        // A `File` field in a body type is a binary upload/download. In TS the
        // wire value is a `Blob` (FormData entry / fetch body). Multipart request
        // assembly and binary response handling live in the body-codegen path
        // (branched on "body contains a File"); this is the field type.
        Type::File => "Blob".to_string(),
        // A `DateTime` is an RFC 3339 instant. JS has a `Date`, but `JSON.parse`
        // never revives one — the decoded field is a string at runtime, so the
        // client emits a recursive revival pass to reconstruct `Date`s at the
        // DateTime field paths. `JSON.stringify` emits ISO strings for the
        // reverse. See `docs/design-decisions.md` (DateTime & UUID scalar types).
        Type::DateTime => "Date".to_string(),
        // A branded `string` alias (`type Uuid = string & {…}` / `Decimal`):
        // distinct from a bare `string` at compile time, but a string at runtime.
        // The decode pass validates+brands via `parse*` (no native JS type to
        // revive into). See `docs/design-decisions.md`.
        Type::Uuid | Type::Decimal => branded_scalar(ty).expect("branded scalar").0.to_string(),
        // A composite built-in (`{ amount: Decimal; currency: string }`); the
        // generated `Money` interface + `reviveMoney` validate it on decode.
        Type::Money => "Money".to_string(),
        Type::Void => "void".to_string(),
        Type::Named(name) => name.clone(),
        Type::Generic(name, args) if name == "List" && args.len() == 1 => {
            format!("{}[]", type_to_ts(&args[0]))
        }
        Type::Generic(name, args) if name == "Map" && args.len() == 2 => {
            format!("Record<{}, {}>", type_to_ts(&args[0]), type_to_ts(&args[1]))
        }
        Type::Generic(name, args) if name == "Option" && args.len() == 1 => {
            format!("{} | undefined", type_to_ts(&args[0]))
        }
        Type::Generic(name, args) => {
            let ts_args: Vec<String> = args.iter().map(type_to_ts).collect();
            format!("{}<{}>", name, ts_args.join(", "))
        }
        Type::Function(params, ret) => {
            let ps: Vec<String> = params
                .iter()
                .enumerate()
                .map(|(i, p)| format!("arg{}: {}", i, type_to_ts(p)))
                .collect();
            format!("({}) => {}", ps.join(", "), type_to_ts(ret))
        }
        Type::TypeVar(name) => name.clone(),
        // Trait objects erase to the trait name on the TS side (structural
        // interface dispatch handles the runtime variance); sharper mappings
        // can replace this when specific traits get per-trait TS shims.
        Type::Dyn(name) => name.clone(),
        // `JsValue` is an executable-language host-FFI type, not a Phoenix Gen
        // schema type. The Gen entry (`emit_target`) rejects any schema that
        // mentions `JsValue` — via `extern js` block or as a field/param type —
        // before codegen runs (`schema_mentions_jsvalue`), so this arm is
        // unreachable in practice. It exists only because `type_to_ts` is an
        // exhaustive match (the Go/Python type-mappers absorb `JsValue` in their
        // `_` arm); map to `unknown` defensively rather than panic.
        Type::JsValue => "unknown".to_string(),
        Type::Error => "unknown".to_string(),
    }
}

/// Converts a Phoenix `TypeExpr` (AST node) to a TypeScript type string.
/// Used for enum variant fields which only have AST-level type info.
fn type_expr_to_ts(te: &TypeExpr) -> String {
    match te {
        TypeExpr::Named(n) => match n.name.as_str() {
            "Int" | "Float" => "number".to_string(),
            "String" => "string".to_string(),
            "Bool" => "boolean".to_string(),
            "Void" => "void".to_string(),
            other => other.to_string(),
        },
        TypeExpr::Generic(g) => {
            let args: Vec<String> = g.type_args.iter().map(type_expr_to_ts).collect();
            match g.name.as_str() {
                "List" if args.len() == 1 => format!("{}[]", args[0]),
                "Option" if args.len() == 1 => format!("{} | undefined", args[0]),
                "Map" if args.len() == 2 => format!("Record<{}, {}>", args[0], args[1]),
                _ => format!("{}<{}>", g.name, args.join(", ")),
            }
        }
        TypeExpr::Function(f) => {
            let ps: Vec<String> = f
                .param_types
                .iter()
                .enumerate()
                .map(|(i, p)| format!("arg{}: {}", i, type_expr_to_ts(p)))
                .collect();
            format!("({}) => {}", ps.join(", "), type_expr_to_ts(&f.return_type))
        }
        TypeExpr::Dyn(d) => d.trait_name.clone(),
    }
}

/// Converts a resolved derived type to a TypeScript type expression.
///
/// Emits an inline object type to faithfully represent the derived fields
/// with their optionality. This covers all modifier combinations (omit,
/// pick, partial, selective partial).
fn derived_type_to_ts(body: &ResolvedDerivedType) -> String {
    let all_optional = !body.fields.is_empty() && body.fields.iter().all(|f| f.optional);

    if all_optional {
        // All optional — emit Partial with inline type for the fields
        let mut parts = Vec::new();
        for f in &body.fields {
            let ts = type_to_ts(&f.ty);
            parts.push(format!("  {}?: {};", f.name, ts));
        }
        return format!("{{\n{}\n}}", parts.join("\n"));
    }

    // Emit inline object type with the actual derived fields
    let mut parts = Vec::new();
    for f in &body.fields {
        let ts = type_to_ts(&f.ty);
        if f.optional {
            parts.push(format!("  {}?: {};", f.name, ts));
        } else {
            parts.push(format!("  {}: {};", f.name, ts));
        }
    }
    format!("{{\n{}\n}}", parts.join("\n"))
}

/// Recursively collects user-defined type names that need to be imported.
///
/// Skips built-in generic wrappers (`List`, `Option`, `Map`, `Result`) since
/// they map to native TypeScript constructs, but descends into their type
/// arguments to find the actual user-defined types (e.g., `List<User>` imports
/// `User`, not `List`).
fn collect_import_names(ty: &Type, imports: &mut BTreeSet<String>) {
    match ty {
        Type::Named(name) => {
            imports.insert(name.clone());
        }
        Type::Generic(name, args) => {
            if !matches!(name.as_str(), "List" | "Option" | "Map" | "Result") {
                imports.insert(name.clone());
            }
            for arg in args {
                collect_import_names(arg, imports);
            }
        }
        _ => {}
    }
}

/// Returns whether a query parameter should be optional in the client function
/// signature (either it has a default value or its type is `Option<T>`).
fn is_query_param_optional(qp: &phoenix_sema::checker::QueryParamInfo) -> bool {
    qp.has_default || matches!(&qp.ty, Type::Generic(name, _) if name == "Option")
}

/// Returns whether a request header is `Option<T>` (i.e. an optional field on
/// the wire and in the handler signature). Defaults make the *client* field
/// optional too, but the handler always receives a value once the server
/// applies the default — mirroring query-param handling.
fn is_header_option(h: &phoenix_sema::checker::HeaderParamInfo) -> bool {
    matches!(&h.ty, Type::Generic(name, _) if name == "Option")
}

/// Returns whether a request header should be optional in the *client* method
/// signature (either it has a default value or its type is `Option<T>`).
fn is_header_client_optional(h: &phoenix_sema::checker::HeaderParamInfo) -> bool {
    h.has_default || is_header_option(h)
}

/// The TypeScript name of the response-header envelope type for an endpoint
/// (e.g. `getPost` → `GetPostResult`). Only emitted/used when the endpoint
/// declares response headers.
fn result_type_name(ep: &EndpointInfo) -> String {
    format!("{}Result", capitalize(&ep.name))
}

/// The TypeScript name of the pagination envelope type for an endpoint
/// (e.g. `listPosts` → `ListPostsPage`). Only emitted/used when the endpoint
/// declares a `pagination { }` block. Distinct from `result_type_name`
/// (pagination and response headers are mutually exclusive — sema rejects the
/// combination).
fn page_type_name(ep: &EndpointInfo) -> String {
    format!("{}Page", capitalize(&ep.name))
}

/// The TypeScript name of the multi-status envelope type for an endpoint
/// (e.g. `upsertUser` → `UpsertUserResponse`). Only emitted/used when the
/// endpoint declares a `response { }` block (`response_statuses` non-empty).
/// Distinct from `result_type_name` / `page_type_name` — multi-status is
/// mutually exclusive with response headers and pagination (sema rejects the
/// combinations), so this never collides with the other envelopes.
fn multi_status_type_name(ep: &EndpointInfo) -> String {
    format!("{}Response", capitalize(&ep.name))
}

/// Emits a `res.status(500).json({ error: "<message>" });` statement at indent
/// `ni`, breaking the member chain the way prettier does once the one-liner
/// exceeds [`PRINT_WIDTH`] (the route indent varies with the opener wrapping,
/// so the guard messages can land on either side of the limit).
fn emit_500_json(body: &mut String, ni: &str, message: &str) {
    let one_line = format!("{ni}res.status(500).json({{ error: \"{message}\" }});");
    if one_line.len() <= PRINT_WIDTH {
        body.push_str(&one_line);
        body.push('\n');
    } else {
        body.push_str(&format!(
            "{ni}res\n{ni}  .status(500)\n{ni}  .json({{ error: \"{message}\" }});\n"
        ));
    }
}

/// Emits a `const {name} = {accessor} as Record<string, string | undefined>;`
/// cast at indent `si` — the Fastify prelude's `rawQuery`/`rawHeaders` locals.
/// When the one-liner exceeds [`PRINT_WIDTH`] the generic type breaks one
/// argument per line (at `ni`, the statement indent + 2), matching prettier; a
/// deep route indent plus a long accessor name can push it over.
fn emit_record_cast(body: &mut String, si: &str, ni: &str, name: &str, accessor: &str) {
    let one_line = format!("{si}const {name} = {accessor} as Record<string, string | undefined>;");
    if one_line.len() <= PRINT_WIDTH {
        body.push_str(&one_line);
        body.push('\n');
    } else {
        body.push_str(&format!("{si}const {name} = {accessor} as Record<\n"));
        body.push_str(&format!("{ni}string,\n"));
        body.push_str(&format!("{ni}string | undefined\n"));
        body.push_str(&format!("{si}>;\n"));
    }
}

/// Emits the Fastify prelude's path-param cast — `const params = request.params
/// as {{ a: string; b: string }};` at indent `si`. When the one-liner exceeds
/// [`PRINT_WIDTH`] the object type breaks one member per line (at `ni`, the
/// statement indent + 2), matching prettier; several path params at a deep
/// (opener-wrapped) route indent can push it over.
fn emit_params_cast(body: &mut String, si: &str, ni: &str, path_params: &[String]) {
    let members = path_params
        .iter()
        .map(|pp| format!("{pp}: string"))
        .collect::<Vec<_>>()
        .join("; ");
    let one_line = format!("{si}const params = request.params as {{ {members} }};");
    if one_line.len() <= PRINT_WIDTH {
        body.push_str(&one_line);
        body.push('\n');
    } else {
        body.push_str(&format!("{si}const params = request.params as {{\n"));
        for pp in path_params {
            body.push_str(&format!("{ni}{pp}: string;\n"));
        }
        body.push_str(&format!("{si}}};\n"));
    }
}

/// Emits an `if (<conds>) {{` opener at indent `si`, joining `conds` with `&&`.
/// When the single-line form exceeds [`PRINT_WIDTH`] it breaks the way prettier
/// does: each operand on its own line at `ni` (statement indent + 2) with the
/// `&&` trailing, and the closing `) {{` back at `si`. The multi-status guard
/// lines grow with the declared-status list, and the route body nests one level
/// deeper for Fastify than Express, so a long-enough status list can push the
/// one-liner over the print width.
fn emit_if_opener(body: &mut String, si: &str, ni: &str, conds: &[&str]) {
    let one_line = format!("{si}if ({}) {{", conds.join(" && "));
    if one_line.len() <= PRINT_WIDTH {
        body.push_str(&one_line);
        body.push('\n');
    } else {
        body.push_str(&format!("{si}if (\n"));
        for (i, cond) in conds.iter().enumerate() {
            let sep = if i + 1 < conds.len() { " &&" } else { "" };
            body.push_str(&format!("{ni}{cond}{sep}\n"));
        }
        body.push_str(&format!("{si}) {{\n"));
    }
}

/// Fastify analogue of [`emit_500_json`]: `reply.status(500).send({ error: … });`,
/// breaking the member chain the way prettier does once the one-liner exceeds
/// [`PRINT_WIDTH`].
fn emit_fastify_500(body: &mut String, ni: &str, message: &str) {
    let one_line = format!("{ni}reply.status(500).send({{ error: \"{message}\" }});");
    if one_line.len() <= PRINT_WIDTH {
        body.push_str(&one_line);
        body.push('\n');
    } else {
        body.push_str(&format!(
            "{ni}reply\n{ni}  .status(500)\n{ni}  .send({{ error: \"{message}\" }});\n"
        ));
    }
}

/// The framework-specific vocabulary the shared route emitters
/// ([`emit_route_prelude`], [`emit_route_response`], [`emit_route_error_catch`])
/// write against. Express and Fastify share the entire route structure — request
/// decode → handler call → respond → error map, including the multi-status
/// envelope guards — and differ only in these few accessors and verbs, so the
/// logic lives once and reads the dialect rather than being copy-pasted per
/// framework (which would let the two drift).
#[derive(Clone, Copy)]
struct Dialect {
    /// The request object identifier: `req` (Express) or `request` (Fastify).
    request: &'static str,
    /// Whether path params need an explicit `request.params as {…}` cast (Fastify,
    /// which types `params` as `unknown`) rather than being typed via the opener's
    /// `Request<{…}>` generic (Express).
    cast_params: bool,
    /// Whether query/header values are read off a `Record<string, string |
    /// undefined>` cast local (Fastify) rather than directly off the request
    /// (Express's `req.query` / `req.header(...)`).
    cast_record: bool,
    /// The response object: `res` (Express) or `reply` (Fastify).
    receiver: &'static str,
    /// The verb that sends a JSON/object body: `json` (Express) or `send`
    /// (Fastify).
    json_verb: &'static str,
    /// The verb that ends a bodyless response: `end` (Express) or `send`
    /// (Fastify).
    empty_verb: &'static str,
    /// The method that sets a response header: `setHeader` (Express) or `header`
    /// (Fastify).
    header_verb: &'static str,
    /// Emits a width-aware `status(500)` error response — [`emit_500_json`] for
    /// Express, [`emit_fastify_500`] for Fastify.
    emit_500: fn(&mut String, &str, &str),
}

const EXPRESS_DIALECT: Dialect = Dialect {
    request: "req",
    cast_params: false,
    cast_record: false,
    receiver: "res",
    json_verb: "json",
    empty_verb: "end",
    header_verb: "setHeader",
    emit_500: emit_500_json,
};

const FASTIFY_DIALECT: Dialect = Dialect {
    request: "request",
    cast_params: true,
    cast_record: true,
    receiver: "reply",
    json_verb: "send",
    empty_verb: "send",
    header_verb: "header",
    emit_500: emit_fastify_500,
};

/// Emits a `const body = {reviver}({decode});` body-revival assignment at indent
/// `si`. When the one-liner exceeds [`PRINT_WIDTH`] the single call argument breaks
/// onto its own line at `si + 2` with a trailing comma, the way prettier does under
/// the pinned `trailingComma: "all"`. One split is assumed to suffice: the only
/// over-width driver here is a long endpoint name in `reviver`, not the (short)
/// `decode` argument (a single call/cast); a `decode` long enough to need a second
/// split would drift — the e2e `prettier --check` gate would catch it.
fn emit_body_revival(body: &mut String, si: &str, reviver: &str, decode: &str) {
    let one_line = format!("{si}const body = {reviver}({decode});");
    if one_line.len() <= PRINT_WIDTH {
        body.push_str(&one_line);
        body.push('\n');
    } else {
        body.push_str(&format!(
            "{si}const body = {reviver}(\n{si}  {decode},\n{si});\n"
        ));
    }
}

/// Emits the request-decode prelude of a route body — path-param, body, query,
/// and header locals — into `body`, returning the handler argument list in
/// declaration order. Shared by the Express and Fastify route emitters, which
/// supply their own `d`: the two differ only in the request-accessor vocabulary
/// it captures (the `req`/`request` identifier, whether path params need an
/// explicit cast, and whether query/headers are read off a `Record` cast). `si`
/// is the statement indent, `ni` the nested-block indent (used by the width-aware
/// casts).
fn emit_route_prelude(
    body: &mut String,
    ep: &EndpointInfo,
    d: Dialect,
    si: &str,
    ni: &str,
    revivable: &BTreeSet<String>,
    param_enums: &BTreeSet<String>,
) -> Vec<String> {
    let req = d.request;
    let mut args = Vec::new();

    // Path params. Fastify types `params` as `unknown`, so cast it once and read
    // each off the local; Express types them via the opener's `Request<{…}>`
    // generic, so `req.params.id` is already `string`.
    if !ep.path_params.is_empty() {
        if d.cast_params {
            emit_params_cast(body, si, ni, &ep.path_params);
        }
        let base = if d.cast_params {
            "params".to_string()
        } else {
            format!("{req}.params")
        };
        for pp in &ep.path_params {
            body.push_str(&format!("{si}const {pp} = {base}.{pp};\n"));
            args.push(pp.clone());
        }
    }

    // Body: multipart (assembled off the MultipartRequest shape), validated, or
    // cast.
    if let Some(ref ep_body) = ep.body {
        let type_name = format!("{}Body", capitalize(&ep.name));
        if ep.body_is_multipart {
            // multipart/form-data body. A multipart middleware (multer, busboy, …)
            // must be mounted upstream: it exposes parsed scalar fields on the
            // request body and the uploaded files as `Blob` values (keyed by field
            // name). We read against the minimal `MultipartRequest` interface
            // (emitted once in the imports) — no runtime dependency is added.
            body.push_str(&format!(
                "{si}const multipart = {req} as unknown as MultipartRequest;\n"
            ));
            body.push_str(&format!("{si}const body: {type_name} = {{\n"));
            for f in &ep_body.fields {
                emit_object_property(
                    body,
                    &format!("{si}  "),
                    &f.name,
                    &multipart_field_extraction(&f.name, &f.ty, f.optional),
                );
            }
            body.push_str(&format!("{si}}};\n"));
        } else {
            // The decode expression: a constrained body is validated, an
            // unconstrained one cast (`req.body` is untyped `any`/`unknown`, so
            // the cast keeps the handler call type-safe under strict `no-unsafe-*`).
            let decode = if ep_body.fields.iter().any(|f| f.constraint.is_some()) {
                format!("validate{type_name}({req}.body)")
            } else {
                format!("{req}.body as {type_name}")
            };
            // A `JSON.parse`d body decodes `DateTime`/`Uuid`/`Decimal`/`Money`
            // fields to strings/plain objects; revive them in place before the
            // handler sees its branded/`Date`-typed fields.
            if body_needs_revival(ep, revivable) {
                emit_body_revival(body, si, &reviver_name(&type_name), &decode);
            } else {
                body.push_str(&format!("{si}const body = {decode};\n"));
            }
        }
        args.push("body".to_string());
    }

    // Query params: coerce + apply defaults. Fastify reads off a
    // `Record<string, string | undefined>` cast local (so the `Option<String>`
    // branch can drop a cast); Express reads `req.query` directly (wider
    // `ParsedQs`, so that branch keeps its `as string | undefined`).
    if !ep.query_params.is_empty() {
        let query_base = if d.cast_record {
            emit_record_cast(body, si, ni, "rawQuery", &format!("{req}.query"));
            "rawQuery".to_string()
        } else {
            format!("{req}.query")
        };
        body.push_str(&format!("{si}const query = {{\n"));
        for qp in &ep.query_params {
            emit_object_property(
                body,
                &format!("{si}  "),
                &qp.name,
                &query_param_coercion_with(qp, &query_base, d.cast_record, param_enums),
            );
        }
        body.push_str(&format!("{si}}};\n"));
        args.push("query".to_string());
    }

    // Request headers: coerce + apply defaults. Fastify lowercases header names
    // and types them `string | string[] | undefined`, so cast to a `string |
    // undefined` record and read each by its lowercased wire name; Express's
    // case-insensitive `req.header("Wire-Name")` is already `string | undefined`,
    // so the exact wire name is safe to pass verbatim.
    if !ep.headers.is_empty() {
        if d.cast_record {
            emit_record_cast(body, si, ni, "rawHeaders", &format!("{req}.headers"));
        }
        body.push_str(&format!("{si}const headers = {{\n"));
        for h in &ep.headers {
            let raw = if d.cast_record {
                format!("rawHeaders[\"{}\"]", h.wire_name.to_lowercase())
            } else {
                format!("{req}.header(\"{}\")", h.wire_name)
            };
            emit_object_property(
                body,
                &format!("{si}  "),
                &h.name,
                &request_header_coercion_raw(h, &raw, param_enums),
            );
        }
        body.push_str(&format!("{si}}};\n"));
        args.push("headers".to_string());
    }

    args
}

/// Emits the response section of a route body — the handler call plus the
/// status/body/header writes — into `body`, shared verbatim by the Express and
/// Fastify route emitters (which supply their own `d`). Covers the five response
/// shapes (binary download, multi-status envelope, response headers, plain body,
/// bodyless 204) with the same envelope-vs-contract guards across frameworks.
/// `si` is the statement indent, `ni` the nested-block indent; `args` is the
/// handler argument list assembled by the caller from the decode prelude.
fn emit_route_response(
    body: &mut String,
    ep: &EndpointInfo,
    args: &[String],
    d: Dialect,
    si: &str,
    ni: &str,
) {
    let recv = d.receiver;
    let call = format!("const result = await handlers.{}", ep.name);
    if ep.response_is_binary {
        // Binary download: the handler returns a `Buffer`; stream it to the wire
        // with a generic binary content type.
        emit_call_stmt(body, si, &call, args, ";");
        body.push_str(&format!(
            "{si}{recv}.{}(\"Content-Type\", \"application/octet-stream\");\n",
            d.header_verb
        ));
        body.push_str(&format!("{si}{recv}.send(result);\n"));
    } else if !ep.response_statuses.is_empty() {
        // Multi-status: the handler returns the `<Endpoint>Response` envelope
        // carrying the chosen status + optional body. Write that status (not a
        // hardcoded 200/204), then encode the body only when present.
        //
        // The handler-chosen envelope is validated against the DECLARED contract
        // first — all three mismatches are handler bugs, reported as a 500 instead
        // of written to the wire (mirrors the Go/Python servers):
        // - an undeclared status (a zero-value envelope would make `res.status(0)`
        //   throw; a 4xx smuggled through the success envelope would bypass
        //   `error { }`);
        // - a body paired with a typeless status (only 204/304 get body
        //   suppression — on e.g. a typeless 202 the body WOULD hit the wire, and
        //   the content-guarded client would parse it, violating the contract);
        // - an undefined body paired with a typed status (the contract — and the
        //   OpenAPI spec — promise a body there).
        // An all-typeless block has no `body` field, so only the membership check
        // applies.
        emit_call_stmt(body, si, &call, args, ";");
        let declared = ep
            .response_statuses
            .iter()
            .map(|rs| rs.status.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        emit_if_opener(
            body,
            si,
            ni,
            &[&format!("![{declared}].includes(result.status)")],
        );
        (d.emit_500)(body, ni, "handler returned undeclared status");
        body.push_str(&format!("{ni}return;\n"));
        body.push_str(&format!("{si}}}\n"));
        if ep.response.is_some() {
            let typed = ep
                .response_statuses
                .iter()
                .filter(|rs| rs.ty.is_some())
                .map(|rs| rs.status.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let typeless = ep
                .response_statuses
                .iter()
                .filter(|rs| rs.ty.is_none())
                .map(|rs| rs.status.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            emit_if_opener(
                body,
                si,
                ni,
                &[
                    &format!("[{typed}].includes(result.status)"),
                    "result.body === undefined",
                ],
            );
            (d.emit_500)(body, ni, "handler returned no body for a typed status");
            body.push_str(&format!("{ni}return;\n"));
            body.push_str(&format!("{si}}}\n"));
            if !typeless.is_empty() {
                emit_if_opener(
                    body,
                    si,
                    ni,
                    &[
                        &format!("[{typeless}].includes(result.status)"),
                        "result.body !== undefined",
                    ],
                );
                (d.emit_500)(body, ni, "handler returned a body for a bodyless status");
                body.push_str(&format!("{ni}return;\n"));
                body.push_str(&format!("{si}}}\n"));
                body.push_str(&format!("{si}if (result.body !== undefined) {{\n"));
                body.push_str(&format!(
                    "{ni}{recv}.status(result.status).{}(result.body);\n",
                    d.json_verb
                ));
                body.push_str(&format!("{si}}} else {{\n"));
                body.push_str(&format!(
                    "{ni}{recv}.status(result.status).{}();\n",
                    d.empty_verb
                ));
                body.push_str(&format!("{si}}}\n"));
            } else {
                // Every declared status is typed, and the guard above already
                // rejected an undefined body — so the body is always present here.
                body.push_str(&format!(
                    "{si}{recv}.status(result.status).{}(result.body);\n",
                    d.json_verb
                ));
            }
        } else {
            // All-typeless block: the envelope has no `body` field.
            body.push_str(&format!(
                "{si}{recv}.status(result.status).{}();\n",
                d.empty_verb
            ));
        }
    } else if !ep.response_headers.is_empty() {
        // Handler returns the envelope: set each response header from the typed
        // field (guard optional), then send the body.
        emit_call_stmt(body, si, &call, args, ";");
        let setter = format!("{recv}.{}", d.header_verb);
        for h in &ep.response_headers {
            // A `DateTime` header goes on the wire as RFC 3339 via `.toISOString()`;
            // `String(date)` would emit the locale form the peer can't parse back.
            // (`String(bool)` already yields lowercase `true`/`false`, matching the
            // other targets, so only `DateTime` needs special handling here.)
            let value = if matches!(unwrap_option_ts(&h.ty), Type::DateTime) {
                format!("result.{}.toISOString()", h.name)
            } else {
                format!("String(result.{})", h.name)
            };
            let set_args = vec![format!("\"{}\"", h.wire_name), value];
            if is_header_option(h) {
                body.push_str(&format!("{si}if (result.{} !== undefined) {{\n", h.name));
                emit_call_stmt(body, ni, &setter, &set_args, ";");
                body.push_str(&format!("{si}}}\n"));
            } else {
                emit_call_stmt(body, si, &setter, &set_args, ";");
            }
        }
        body.push_str(&format!("{si}{recv}.{}(result.body);\n", d.json_verb));
    } else if ep.response.is_some() {
        emit_call_stmt(body, si, &call, args, ";");
        body.push_str(&format!("{si}{recv}.{}(result);\n", d.json_verb));
    } else {
        emit_call_stmt(body, si, &format!("await handlers.{}", ep.name), args, ";");
        body.push_str(&format!("{si}{recv}.status(204).{}();\n", d.empty_verb));
    }
}

/// Emits the `} catch { … }` section of a route body into `body` — the
/// `ValidationError` → 400 guard (only when the body carries constraints) and
/// the declared-error → status map, terminating in a catch-all 500. Shared by
/// both frameworks via `d`. `ti`/`si`/`ni` are the try/statement/nested indents.
fn emit_route_error_catch(
    body: &mut String,
    ep: &EndpointInfo,
    d: Dialect,
    ti: &str,
    si: &str,
    ni: &str,
    param_enums: &BTreeSet<String>,
) {
    let recv = d.receiver;
    let verb = d.json_verb;
    let has_body_constraints = ep
        .body
        .as_ref()
        .is_some_and(|b| b.fields.iter().any(|f| f.constraint.is_some()));
    // An enum query/request-header param's `parse<Enum>` throws `ValidationError`
    // on an unknown variant, so the same 400 guard applies.
    let has_enum_param = ep
        .query_params
        .iter()
        .map(|q| &q.ty)
        .chain(ep.headers.iter().map(|h| &h.ty))
        .any(|ty| matches!(unwrap_option_ts(ty), Type::Named(n) if param_enums.contains(n)));
    let validates = has_body_constraints || has_enum_param;

    // The caught binding is only referenced when there is a ValidationError → 400
    // guard or declared errors; otherwise use an optional catch binding
    // (`catch {`) so no-unused-vars has nothing to flag.
    if validates || !ep.errors.is_empty() {
        body.push_str(&format!("{ti}}} catch (error: unknown) {{\n"));
    } else {
        body.push_str(&format!("{ti}}} catch {{\n"));
    }
    if validates {
        emit_guarded_block(
            body,
            si,
            "error instanceof ValidationError",
            &[
                format!("{recv}.status(400).{verb}({{ error: error.message }});"),
                "return;".to_string(),
            ],
        );
    }
    if ep.errors.is_empty() {
        body.push_str(&format!(
            "{si}{recv}.status(500).{verb}({{ error: \"Internal Server Error\" }});\n"
        ));
    } else {
        body.push_str(&format!("{si}if (error instanceof Error) {{\n"));
        for (name, code) in &ep.errors {
            emit_guarded_block(
                body,
                ni,
                &format!("error.message === \"{name}\""),
                &[
                    format!("{recv}.status({code}).{verb}({{ error: \"{name}\" }});"),
                    "return;".to_string(),
                ],
            );
        }
        body.push_str(&format!("{si}}}\n"));
        body.push_str(&format!(
            "{si}{recv}.status(500).{verb}({{ error: \"Internal Server Error\" }});\n"
        ));
    }
    body.push_str(&format!("{ti}}}\n"));
}

/// Generates an expression coercing a request header read server-side (from the
/// supplied `raw` accessor expression, typed `string | undefined`) to its typed
/// value, applying the default when the header is absent. Each framework passes
/// its own accessor: Express reads case-insensitive `req.header("Wire-Name")`,
/// while Fastify reads `rawHeaders["wire-name"]` off a `Record<string, string |
/// undefined>` cast of `request.headers`. The body is identical because both
/// accessors are typed `string | undefined`. Mirrors [`query_param_coercion_with`].
fn request_header_coercion_raw(
    h: &phoenix_sema::checker::HeaderParamInfo,
    raw: &str,
    param_enums: &BTreeSet<String>,
) -> String {
    let raw = raw.to_string();
    let is_option = is_header_option(h);

    let coerced = match &h.ty {
        Type::Int | Type::Float => format!("Number({raw})"),
        Type::Bool => format!("{raw} === \"true\""),
        Type::String => format!("{raw} as string"),
        // `new Date()` rejects `undefined`, and TS can't narrow a call-expression
        // `raw` across `!==`, so the `as string` cast is required (and not
        // unnecessary — it removes `undefined`).
        Type::DateTime => format!("new Date({raw} as string)"),
        Type::Uuid | Type::Decimal => {
            let parse = branded_scalar(&h.ty).expect("branded scalar").1;
            format!("{parse}({raw} as string)")
        }
        // A simple enum header: validate the wire string into the branded union
        // via `parse<Enum>` (the `as string` removes `undefined`, like the
        // branded scalars above).
        Type::Named(n) if param_enums.contains(n) => format!("parse{n}({raw} as string)"),
        Type::Generic(name, args) if name == "Option" && !args.is_empty() => match &args[0] {
            Type::Int | Type::Float => {
                format!("{raw} !== undefined ? Number({raw}) : undefined")
            }
            Type::Bool => format!("{raw} !== undefined ? {raw} === \"true\" : undefined"),
            Type::DateTime => {
                format!("{raw} !== undefined ? new Date({raw} as string) : undefined")
            }
            inner @ (Type::Uuid | Type::Decimal) => {
                let parse = branded_scalar(inner).expect("branded scalar").1;
                format!("{raw} !== undefined ? {parse}({raw} as string) : undefined")
            }
            Type::Named(n) if param_enums.contains(n) => {
                format!("{raw} !== undefined ? parse{n}({raw} as string) : undefined")
            }
            // `req.header(...)` is already typed `string | undefined`, so an
            // `as string | undefined` cast would be flagged by eslint's
            // no-unnecessary-type-assertion; use the raw read.
            _ => raw.clone(),
        },
        _ => format!("{raw} as string"),
    };

    if is_option {
        return coerced;
    }
    if let Some(ref default) = h.default_value {
        let default_ts = default_value_to_ts(default);
        format!("{raw} !== undefined ? {coerced} : {default_ts}")
    } else {
        coerced
    }
}

/// Generates an expression reading a response header on the client side from a
/// fetch `Response` (`response.headers.get("Wire-Name")`, typed
/// `string | null`) and coercing it to its typed value. Optional headers map a
/// missing value (`null`) to `undefined`; required headers assume the header is
/// present (the matching server always sets it).
fn response_header_coercion(h: &phoenix_sema::checker::HeaderParamInfo) -> String {
    let raw = format!("response.headers.get(\"{}\")", h.wire_name);
    let is_option = is_header_option(h);
    let inner = if is_option {
        match &h.ty {
            Type::Generic(_, args) if !args.is_empty() => &args[0],
            other => other,
        }
    } else {
        &h.ty
    };

    if is_option {
        match inner {
            Type::Int | Type::Float => {
                format!("{raw} !== null ? Number({raw}) : undefined")
            }
            Type::Bool => format!("{raw} !== null ? {raw} === \"true\" : undefined"),
            // `new Date()` rejects `null`; the `as string` cast removes it (and is
            // necessary, so not flagged by no-unnecessary-type-assertion).
            Type::DateTime => {
                format!("{raw} !== null ? new Date({raw} as string) : undefined")
            }
            inner @ (Type::Uuid | Type::Decimal) => {
                let parse = branded_scalar(inner).expect("branded scalar").1;
                format!("{raw} !== null ? {parse}({raw} as string) : undefined")
            }
            // A simple-enum response header (sema guarantees unit variants): the
            // server writes a valid variant, so the client narrows + casts the
            // `string | null` read down to the branded union.
            Type::Named(n) => format!("{raw} !== null ? ({raw} as {n}) : undefined"),
            // `response.headers.get(...)` is already typed `string | null`, so a
            // cast would be an unnecessary-type-assertion; `?? undefined` maps the
            // missing case (null) to undefined for the optional field.
            _ => format!("{raw} ?? undefined"),
        }
    } else {
        // Required: the header is contractually present (a generated server
        // always sets a non-`Option` response header — the envelope field is
        // type-required, so the handler must supply it). The coercions below
        // therefore don't special-case a `null` read. NOTE: if a *non-conforming*
        // (e.g. third-party) server omits it, the string branch yields a runtime
        // `null` typed as `string`, where Go/Python fall back to `""`. This
        // cross-language divergence is unreachable against a generated server and
        // is left as-is rather than fabricating an empty-string default that would
        // mask the contract violation.
        match inner {
            Type::Int | Type::Float => format!("Number({raw})"),
            Type::Bool => format!("{raw} === \"true\""),
            Type::DateTime => format!("new Date({raw} as string)"),
            inner @ (Type::Uuid | Type::Decimal) => {
                let parse = branded_scalar(inner).expect("branded scalar").1;
                format!("{parse}({raw} as string)")
            }
            // A simple-enum response header: cast the (contractually present)
            // `string | null` read down to the branded union.
            Type::Named(n) => format!("{raw} as {n}"),
            _ => format!("{raw} as string"),
        }
    }
}

/// Builds a URL template expression with path parameter substitution.
///
/// `/api/users/{id}` → `/api/users/${id}`
fn build_url_expr(path: &str, _params: &[String]) -> String {
    path.replace('{', "${")
}

use crate::{capitalize, to_screaming_snake};

#[cfg(test)]
#[path = "typescript_tests.rs"]
mod tests;
