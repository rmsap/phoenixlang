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
/// OpenAPI 3.1 JSON specification generation.
pub mod openapi;
/// Python code generation backend (Pydantic, FastAPI, httpx).
pub mod python;
/// TypeScript code generation backend (interfaces, fetch client, Express router).
pub mod typescript;

pub use go::{GoFiles, generate_go};
pub use openapi::generate_openapi;
pub use python::{PythonFiles, generate_python};
pub use typescript::{GeneratedFiles, generate_typescript};

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
// `phoenix_common::idents::capitalize`.
pub(crate) use phoenix_common::capitalize;

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
