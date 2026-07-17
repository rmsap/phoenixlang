//! Bridge from the resolved dependency graph to the module resolver.
//!
//! Discovers the project manifest for an entry file, resolves + fetches its
//! declared dependencies, and projects the resulting [`ResolvedGraph`] into the
//! [`PackageResolution`] that `phoenix_modules` threads through cross-package
//! `import` resolution. Kept out of the driver's `lib.rs` because it is purely
//! dependency-graph projection — the compile glue that consumes it lives there.

use std::collections::HashMap;
use std::path::Path;

use phoenix_modules::PackageResolution;

use crate::config::PhoenixConfig;

use super::ResolvedGraph;

/// Discover the project manifest for the entry file and, if it declares
/// dependencies, resolve + fetch them into a [`PackageResolution`] the module
/// resolver threads through. Returns the default (empty) resolution when there
/// is no manifest or no dependencies — which reproduces single-package behavior.
///
/// `locked` forbids updating `phoenix.lock`. The returned `bool` is whether this
/// resolution changed the on-disk lockfile; the caller owns the user-facing
/// notice so all stderr reporting stays at the driver layer. The returned
/// [`PhoenixConfig`] (`None` if no manifest exists) is the one this function
/// already loaded and validated to discover `[dependencies]` — callers that
/// need other manifest sections (e.g. `[js-dependencies]`) read them off this
/// value instead of re-discovering and re-parsing `phoenix.toml` themselves.
pub(crate) fn build_package_resolution(
    entry: &Path,
    locked: bool,
) -> Result<(PackageResolution, bool, Option<PhoenixConfig>), String> {
    // `Path::parent()` yields `Some("")` for a bare filename (`main.phx`), not
    // `None`, and `find_with_path("")` would then probe only `./phoenix.toml`
    // and refuse to walk up (an empty path can't `pop()`). Treat that empty
    // parent as the current directory so manifest discovery starts from the
    // cwd in every form of invocation.
    let entry_dir = match entry.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let Some((config, manifest_path)) =
        PhoenixConfig::find_with_path(entry_dir).map_err(|e| e.to_string())?
    else {
        return Ok((PackageResolution::default(), false, None));
    };
    let dependencies = config.dependencies().map_err(|e| e.to_string())?;
    if dependencies.is_empty() {
        return Ok((PackageResolution::default(), false, Some(config)));
    }
    let manifest_dir = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    // Resolve the cache root lazily (`None`): it is consulted only to fetch git
    // sources, so a project that reaches no git source — even one whose git
    // source hides behind a `path` dependency's own dependencies — never
    // requires a resolvable `$PHOENIX_HOME`, and a git source is always fetched
    // into the real cache rather than the project tree.
    let resolution = super::resolve_project(&manifest_dir, &dependencies, None, locked)
        .map_err(|e| e.to_string())?;
    let entry_deps = dependencies.keys().cloned().collect();
    Ok((
        package_resolution_from_graph(entry_deps, &resolution.graph),
        resolution.lock_changed,
        Some(config),
    ))
}

/// Project a resolved dependency graph into the [`PackageResolution`] the module
/// resolver consumes: every package's root, and the dependency names each
/// package itself declares (so transitive cross-package imports dispatch).
fn package_resolution_from_graph(
    entry_deps: Vec<String>,
    graph: &ResolvedGraph,
) -> PackageResolution {
    let mut roots = HashMap::new();
    let mut deps_of = HashMap::new();
    for (name, pkg) in &graph.packages {
        roots.insert(name.clone(), pkg.root.clone());
        deps_of.insert(name.clone(), pkg.dependencies.keys().cloned().collect());
    }
    PackageResolution {
        entry_deps,
        roots,
        deps: deps_of,
    }
}
