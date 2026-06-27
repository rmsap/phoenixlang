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
//! - [`cache`] — where git sources are fetched (`$PHOENIX_HOME/cache`).
//! - [`fetch`] — the cache-backed provider that clones git sources and locates
//!   `path` sources.
//! - [`lock`] — `phoenix.lock` generation, reading, and drift detection.
//! - [`resolve`] — the project-level entry point that ties manifest, lockfile,
//!   and fetcher together (to be wired into `build` / `run` / `check` in a
//!   later PR; currently driven by the integration tests).

pub mod cache;
pub mod fetch;
pub mod graph;
pub mod lock;
pub mod resolve;

pub use graph::{
    FetchedPackage, ManifestProvider, PackageManifest, ResolveError, ResolvedGraph,
    ResolvedPackage, resolve_graph,
};
pub use lock::{LockError, LockedPackage, Lockfile};
pub use resolve::{ProjectResolution, ProjectResolveError, resolve_project};
