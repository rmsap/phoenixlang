//! The Phoenix package manager.
//!
//! Resolves a project's `[dependencies]` into a concrete, deduplicated set of
//! packages on disk, then feeds their roots to the module resolver so `import`
//! can reach across package boundaries.
//!
//! Layering:
//!
//! - [`graph`] — source-agnostic transitive resolution + semver conflict
//!   detection over a [`graph::ManifestProvider`]. Unit-tested with in-memory
//!   manifests; no filesystem or network access.
//! - (later PRs) a cache-backed provider that clones git sources and locates
//!   `path` sources, plus `phoenix.lock` read/write.

pub mod graph;

pub use graph::{
    FetchedPackage, ManifestProvider, PackageManifest, ResolveError, ResolvedGraph,
    ResolvedPackage, resolve_graph,
};
