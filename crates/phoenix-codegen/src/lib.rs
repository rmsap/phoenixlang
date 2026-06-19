//! Code generation backends for Phoenix Gen.
//!
//! This crate generates typed code for target languages from a parsed and
//! type-checked Phoenix program containing `endpoint` declarations.
//!
//! Supported targets:
//! - **TypeScript** ([`typescript`]) — generates interfaces, derived types,
//!   a fetch-based client SDK, server handler interfaces, and Express router.
//! - **Python** ([`python`]) — generates Pydantic models, a typed httpx client,
//!   a handler Protocol class, and a FastAPI router.
//! - **Go** ([`go`]) — generates structs with JSON tags, an HTTP client,
//!   a Handlers interface, and a `net/http` router.
//! - **OpenAPI** ([`openapi`]) — generates an OpenAPI 3.1 JSON specification.
#![warn(missing_docs)]

/// Go code generation backend (structs, net/http client/server).
pub mod go;
/// The active ISO-4217 currency codes (shared `Money` currency validation).
mod iso4217;
/// OpenAPI 3.1 JSON specification generation.
pub mod openapi;
/// Python code generation backend (Pydantic, FastAPI, httpx).
pub mod python;
/// TypeScript code generation backend (interfaces, fetch client, Express router).
pub mod typescript;

pub use go::{GoFiles, GoServerFramework, generate_go, generate_go_with};
pub use openapi::generate_openapi;
pub use python::{PythonFiles, generate_python};
pub use typescript::{
    GeneratedFiles, TsServerFramework, generate_typescript, generate_typescript_with,
};

/// Controls which subset of files to generate.
///
/// When generating code for a target language, the output consists of shared
/// type definitions, a client SDK, and server-side handlers/router. This enum
/// lets callers request only the client or server subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GenMode {
    /// Generate all files (types + client + server). Default.
    #[default]
    Both,
    /// Generate only shared types and the client SDK.
    ClientOnly,
    /// Generate only shared types, handler interfaces, and the server router.
    ServerOnly,
}

// The capitalization rule for generated type names lives in `phoenix-common`
// so sema's envelope-collision check uses the exact same function — see
// `phoenix_common::idents::capitalize`. `to_screaming_snake` is shared by the
// Python and TypeScript backends so the enum-value/const casing cannot drift.
pub(crate) use phoenix_common::{capitalize, to_screaming_snake};

use phoenix_parser::ast::{Declaration, Program};
use phoenix_sema::Analysis;
use phoenix_sema::types::Type;
use std::collections::BTreeSet;

/// Peels a single `Option<…>` or `List<…>` layer off a query/header param type to
/// reach the scalar/enum element, returning the type unchanged otherwise. A
/// `List<Enum>`/`List<Uuid>` element is validated exactly like the scalar form, so
/// the per-element checks (enum-name collection here, Go regex-var registration)
/// share this one-layer unwrap. `Option<List<…>>` cannot occur (sema rejects it),
/// so a single layer suffices.
pub(crate) fn unwrap_option_or_list(ty: &Type) -> &Type {
    match ty {
        Type::Generic(name, args) if (name == "Option" || name == "List") && args.len() == 1 => {
            &args[0]
        }
        other => other,
    }
}

/// The set of simple (unit-variant) enum names used in any query param or request
/// header (a single `Option<…>` layer unwrapped). Shared by the Go and TypeScript
/// backends, which both gate inbound enum validation on it: Go emits a `Valid()`
/// method, TS a `parse<Enum>` validator + `ValidationError → 400` mapping.
/// Response headers are excluded: the server *writes* them, so there is no inbound
/// value to validate — the client casts on read.
pub(crate) fn param_enum_names(program: &Program, check_result: &Analysis) -> BTreeSet<String> {
    let simple_enums: BTreeSet<&str> = program
        .declarations
        .iter()
        .filter_map(|d| match d {
            Declaration::Enum(e) if e.variants.iter().all(|v| v.fields.is_empty()) => {
                Some(e.name.as_str())
            }
            _ => None,
        })
        .collect();
    let mut used = BTreeSet::new();
    for ep in &check_result.endpoints {
        let types = ep
            .query_params
            .iter()
            .map(|q| &q.ty)
            .chain(ep.headers.iter().map(|h| &h.ty));
        for ty in types {
            // Unwrap a single `Option<…>` or `List<…>` to reach the (possibly enum)
            // element: a `List<Enum>` param validates each element the same way a
            // scalar enum param does.
            let inner = unwrap_option_or_list(ty);
            if let Type::Named(n) = inner
                && simple_enums.contains(n.as_str())
            {
                used.insert(n.clone());
            }
        }
    }
    used
}

/// Whether the schema references `Money` anywhere a generated artifact would name
/// the `Money` type: a struct/body field, a response, or a pagination item type.
/// Gates the one-time `Money` type/component emission (Go struct, OpenAPI
/// component, etc.). Shared by the Go and OpenAPI backends; the TypeScript backend
/// uses its generic `schema_uses_scalar` and Python a shallow per-file scan.
///
/// Recursion into types uses [`Type::mentions_money`] — the same predicate sema's
/// `check_endpoint` uses for the query/header restriction, so both agree on what
/// counts as a `Money` use.
///
/// This deliberately omits query-param/header positions: sema rejects a `Money`
/// there (it isn't URL/header encodable — see `phoenix-sema`'s `check_endpoint`),
/// so a `Money` can only reach codegen through the positions scanned here. Were
/// that restriction lifted, this gate would have to grow those positions too, or
/// the gated targets (Go/Python/OpenAPI) would emit a dangling `Money` reference.
pub(crate) fn schema_uses_money(program: &Program, check_result: &Analysis) -> bool {
    let in_struct = program.declarations.iter().any(|d| {
        matches!(d, Declaration::Struct(s)
            if check_result
                .module
                .struct_info_by_name(&s.name)
                .is_some_and(|si| si.fields.iter().any(|f| f.ty.mentions_money())))
    });
    let in_ep = check_result.endpoints.iter().any(|ep| {
        ep.response.as_ref().is_some_and(Type::mentions_money)
            || ep
                .body
                .as_ref()
                .is_some_and(|b| b.fields.iter().any(|f| f.ty.mentions_money()))
            || ep
                .pagination
                .as_ref()
                .is_some_and(|p| p.item_type.mentions_money())
    });
    in_struct || in_ep
}

/// `true` if the schema mentions [`Type::JsValue`] anywhere a Gen backend would
/// emit it: a struct field, an enum-variant payload, an endpoint
/// query/header/body/response/pagination type, or a type alias.
///
/// `JsValue` is an executable-language host-FFI handle (Phase 2.5) with no wire
/// representation, so it has no place in a Gen schema. The driver's `emit_target`
/// uses this to reject such a schema uniformly across all four targets — without
/// it, each backend's type-mapper would silently guess a *different* fallback
/// (`unknown` / `interface{}` / `object`). Recursion uses
/// [`Type::mentions_jsvalue`]. `extern js` signatures (which legitimately carry
/// `JsValue`) are not scanned here — they live in `extern_functions`, not these
/// emittable tables, and are rejected separately by `emit_target`.
///
/// Unlike the sibling [`schema_uses_money`], this takes only the [`Analysis`]
/// (no `program`): it scans the *resolved* `module.structs`/`enums` rather than
/// walking `program.declarations`, so it also catches `JsValue` in imported
/// structs/enums. The two gates should eventually converge on this form.
pub fn schema_mentions_jsvalue(check_result: &Analysis) -> bool {
    let m = &check_result.module;
    let in_struct = m
        .structs
        .iter()
        .any(|s| s.fields.iter().any(|f| f.ty.mentions_jsvalue()));
    let in_enum = m.enums.iter().any(|e| {
        e.variants
            .iter()
            .any(|(_, tys)| tys.iter().any(Type::mentions_jsvalue))
    });
    let in_alias = check_result
        .type_aliases
        .values()
        .any(|a| a.target.mentions_jsvalue());
    let in_ep = check_result.endpoints.iter().any(|ep| {
        ep.query_params.iter().any(|q| q.ty.mentions_jsvalue())
            || ep.headers.iter().any(|h| h.ty.mentions_jsvalue())
            || ep.response_headers.iter().any(|h| h.ty.mentions_jsvalue())
            || ep.response.as_ref().is_some_and(Type::mentions_jsvalue)
            || ep
                .body
                .as_ref()
                .is_some_and(|b| b.fields.iter().any(|f| f.ty.mentions_jsvalue()))
            || ep
                .response_statuses
                .iter()
                .any(|rs| rs.ty.as_ref().is_some_and(Type::mentions_jsvalue))
            || ep
                .pagination
                .as_ref()
                .is_some_and(|p| p.item_type.mentions_jsvalue())
    });
    in_struct || in_enum || in_alias || in_ep
}

/// A sort key ordering endpoint paths most-specific (most-static) first.
///
/// The generated Express (TypeScript) and FastAPI (Python) routers both match
/// routes first-registered-wins, so a parametric route (`/api/posts/{id}`)
/// registered before a static sibling (`/api/posts/paged`) would shadow it — the
/// static path gets captured as `id = "paged"`. Sorting by this key registers a
/// literal route before a parametric sibling so literal segments win, matching
/// the most-specific-wins semantics Go's `net/http.ServeMux` (1.22+) already
/// provides (Go needs no sort; OpenAPI has no routing).
///
/// The key is a per-segment vector where a static segment sorts before a
/// `{param}` segment (`0` < `1`) at the first position they differ; a shorter
/// path sorts before a longer one with the same prefix. Lexicographic `Vec`
/// comparison then yields the desired ordering, and a *stable* sort preserves
/// source order among equally-specific paths (snapshot-stable). Example:
/// `/api/posts/paged` → `[0,0,0]` sorts before `/api/posts/{id}` → `[0,0,1]`.
///
/// This is a **heuristic** that covers the common case (a static segment vs. a
/// `{param}` sibling at the same position), not a full most-specific resolver.
/// It does not detect cross-segment ambiguity: `/a/{x}/c` → `[0,1,0]` and
/// `/a/b/{y}` → `[0,0,1]` both match `/a/b/c`, and the key simply orders
/// `/a/b/{y}` first (param wins at the last segment despite `/a/{x}/c`'s static
/// tail) rather than flagging the pair as a conflict the way Go's ServeMux
/// (1.22+) would. Such overlaps are rare in practice; if they arise, declare the
/// more-static route first so source order (preserved by the stable sort among
/// equal keys) resolves it.
pub(crate) fn route_specificity_key(path: &str) -> Vec<u8> {
    path.split('/')
        .filter(|seg| !seg.is_empty())
        .map(|seg| u8::from(seg.starts_with('{')))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_key_static_segment_sorts_before_param() {
        // A static segment (`paged`) must order before a `{param}` sibling at the
        // same position so the literal route is registered first.
        assert!(
            route_specificity_key("/api/posts/paged") < route_specificity_key("/api/posts/{id}")
        );
    }

    #[test]
    fn route_key_orders_a_full_route_set_most_static_first() {
        // Mirrors the gen_api.phx fixture: the parametric `/api/posts/{id}` is
        // declared in source BEFORE the static `/api/posts/paged` and
        // `/api/posts/feed`, which without this sort would shadow them. Sorting by
        // the key (a stable sort) must register every static sibling first while
        // keeping the two static siblings in their original source order.
        let mut paths = vec!["/api/posts/{id}", "/api/posts/paged", "/api/posts/feed"];
        paths.sort_by_key(|p| route_specificity_key(p));
        assert_eq!(
            paths,
            vec!["/api/posts/paged", "/api/posts/feed", "/api/posts/{id}"],
            "static siblings must precede the parametric route, preserving their source order"
        );
    }

    #[test]
    fn route_key_shorter_path_sorts_before_longer_prefix() {
        // `/users` is a prefix of `/users/{id}`; the shorter path sorts first.
        assert!(route_specificity_key("/users") < route_specificity_key("/users/{id}"));
    }

    #[test]
    fn route_key_is_stable_for_equal_specificity() {
        // Two equally-specific (all-static) sibling paths compare equal, so a
        // stable sort leaves them in source order — the snapshot-stability the
        // generators rely on.
        assert_eq!(
            route_specificity_key("/api/posts"),
            route_specificity_key("/api/search"),
        );
    }

    #[test]
    fn route_key_ignores_empty_segments() {
        // Leading/trailing slashes produce empty segments that must be dropped so
        // `/users/` and `/users` key identically.
        assert_eq!(
            route_specificity_key("/users/"),
            route_specificity_key("/users"),
        );
    }
}
