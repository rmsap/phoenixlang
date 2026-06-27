//! Transitive dependency-graph resolution with semver conflict detection.
//!
//! This is the source-agnostic core of the package manager: it walks a root
//! manifest's `[dependencies]` transitively, fetching each package's manifest
//! through a [`ManifestProvider`], and produces a flat, deduplicated
//! [`ResolvedGraph`] — exactly one resolved package per dependency *name* (the
//! key used in `[dependencies]`, which is also the name an `import`'s first
//! path segment matches against).
//!
//! Fetching (git clone / path lookup) is deliberately abstracted behind
//! [`ManifestProvider`] so this layer is exercised over in-memory injected
//! manifests in unit tests and over the real cache-backed fetcher at runtime.
//!
//! ## Identity and unification
//!
//! A package's identity in the graph is its **dependency key** — the name on
//! the left of `=` in `[dependencies]`. (See `docs/design-decisions.md`
//! §Phase 3.1.) The same key may be required by several packages (a
//! "diamond"); all such requirements unify to a single resolved package
//! because the flat cross-package namespace admits exactly one root per name.
//! Unification rules (see [`ResolveError`]):
//!
//! - All requirements for a key must share the same upstream **source identity**
//!   (the provider-supplied `source_id`: the git URL, or the canonical path of a
//!   `path` dependency). Two different upstreams for one name cannot be
//!   reconciled — that is a [`ResolveError::SourceConflict`].
//! - Among same-upstream requirements that pin different refs (and thus possibly
//!   different `[package].version`s), the versions must be semver-compatible
//!   under caret (`^`) semantics; the **highest** is chosen. Incompatible majors
//!   are a [`ResolveError::VersionConflict`]. This is where semver does real
//!   work: two parents pinning `v1.2.0` and `v1.5.0` of the same library
//!   coexist (resolves to `1.5.0`), but `v1.x` and `v2.x` do not.
//!
//! ## Pruning superseded versions
//!
//! The transitive walk records every version reached from anywhere, including a
//! version pulled in only to lose unification to a higher one. After unifying,
//! resolution keeps only the packages reachable from the root through each
//! name's *selected* (highest) version, so a discarded version's *exclusive*
//! subtree neither appears in the [`ResolvedGraph`] nor raises a conflict the
//! graph wouldn't actually contain.
//!
//! Residuals remain for names a superseded version *shares* with the surviving
//! one: such a name is still reachable, so unification sees *all* its recorded
//! candidates, including the one the superseded version pulled in. A fully
//! general fix needs backtracking (deferred); the observable consequences are:
//!
//! - When two versions of a package each require a common dependency at
//!   incompatible majors, that dependency's conflict is still reported even
//!   though only the higher version — and its requirement — survives.
//! - When the superseded version required a *compatible but higher* version of
//!   a shared dependency, that higher version is silently selected even though
//!   no surviving requirement asked for it. The result stays semver-compatible
//!   with the surviving requirement (caret), so it is sound, but it is higher
//!   than minimal-version selection would pick.
//! - A cycle reachable *only* through a superseded version is still reported,
//!   because cycle detection runs eagerly during the walk, before pruning.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::manifest::{Dependency, GitRef, ManifestError};

/// A package's manifest as seen by the resolver: its name, version, and its own
/// declared dependencies. Built from a fetched/located `phoenix.toml`.
#[derive(Debug, Clone)]
pub struct PackageManifest {
    /// The `[package].name` declared by the package itself. Informational for
    /// diagnostics; cross-package identity uses the *dependency key*, not this.
    pub name: String,
    /// The `[package].version` (semver).
    pub version: semver::Version,
    /// The package's own `[dependencies]`, keyed by dependency name.
    pub dependencies: BTreeMap<String, Dependency>,
}

/// A package located on disk together with the data the resolver needs to
/// unify and (later) lock it.
#[derive(Debug, Clone)]
pub struct FetchedPackage {
    /// The parsed manifest.
    pub manifest: PackageManifest,
    /// The directory containing the package's `phoenix.toml` — the package
    /// root that modules resolve under.
    pub root: PathBuf,
    /// A stable identifier for the package's *upstream* used for unification:
    /// the git URL for a git source, or the canonical root path for a `path`
    /// source. Distinct refs of the same git repo share a `source_id`.
    pub source_id: String,
    /// For a git source, the resolved commit SHA (what gets pinned in the
    /// lockfile); `None` for a `path` source, which is never SHA-pinned.
    pub rev: Option<String>,
    /// For a git source, the ref the manifest *requested* (tag/branch/rev/
    /// default). Recorded in the lockfile so a manifest ref change is detected
    /// as drift; `None` for a `path` source.
    pub git_ref: Option<GitRef>,
}

/// Locates a dependency and parses its manifest.
///
/// Implemented in tests by an in-memory map and at runtime by the cache-backed
/// git/path fetcher. Implementations may record side data (e.g. lockfile
/// entries) as they fetch.
pub trait ManifestProvider {
    /// Fetch (or locate) the package named `name` from source `dep`, relative
    /// to `parent_dir` (the directory of the manifest that declared it — needed
    /// to resolve relative `path` dependencies). Returns the located package or
    /// a [`ResolveError`].
    fn fetch(
        &mut self,
        name: &str,
        dep: &Dependency,
        parent_dir: &std::path::Path,
    ) -> Result<FetchedPackage, ResolveError>;
}

/// One node of the resolved dependency graph.
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    /// The dependency name (key) this package is known by across the project.
    pub name: String,
    /// The resolved semver version.
    pub version: semver::Version,
    /// The package root directory (contains its `phoenix.toml`).
    pub root: PathBuf,
    /// The package's own declared dependencies, keyed by name. Used to build
    /// the per-package dependency map that cross-package import resolution
    /// threads through.
    pub dependencies: BTreeMap<String, Dependency>,
    /// The upstream source identity (see [`FetchedPackage::source_id`]).
    pub source_id: String,
    /// The resolved commit SHA for a git source; `None` for a `path` source.
    /// Drives lockfile generation ([`super::lock::Lockfile::from_graph`]).
    pub rev: Option<String>,
    /// The manifest-requested ref for a git source; `None` for a `path` source.
    /// Recorded in the lockfile so a ref change surfaces as drift.
    pub git_ref: Option<GitRef>,
}

/// The fully resolved dependency graph: one [`ResolvedPackage`] per name.
#[derive(Debug, Clone, Default)]
pub struct ResolvedGraph {
    /// Resolved packages keyed by dependency name.
    pub packages: BTreeMap<String, ResolvedPackage>,
}

impl ResolvedGraph {
    /// The package root for a dependency name, if resolved.
    pub fn root_of(&self, name: &str) -> Option<&std::path::Path> {
        self.packages.get(name).map(|p| p.root.as_path())
    }
}

/// Errors produced while resolving the dependency graph.
#[derive(Debug)]
pub enum ResolveError {
    /// A manifest failed schema validation (bad version, malformed dep, …).
    Manifest {
        /// The dependency name whose manifest was invalid.
        name: String,
        /// The underlying validation error.
        error: ManifestError,
    },
    /// Two requirements for the same name resolve to different upstream sources.
    SourceConflict {
        /// The dependency name.
        name: String,
        /// The first source identity seen.
        first: String,
        /// The conflicting source identity.
        second: String,
    },
    /// Two requirements for the same name resolve to semver-incompatible
    /// versions (differing majors under caret semantics).
    VersionConflict {
        /// The dependency name.
        name: String,
        /// One resolved version.
        a: semver::Version,
        /// The other resolved version.
        b: semver::Version,
    },
    /// The dependency graph contains a cycle (a package depends on itself,
    /// transitively).
    Cyclic {
        /// The names forming the cycle, in order, ending at the repeated name.
        cycle: Vec<String>,
    },
    /// Fetching/locating a dependency failed (clone error, missing path, …).
    Fetch {
        /// The dependency name.
        name: String,
        /// A human-readable description of the failure.
        message: String,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Manifest { name, error } => {
                write!(f, "in dependency `{name}`: {error}")
            }
            ResolveError::SourceConflict {
                name,
                first,
                second,
            } => write!(
                f,
                "dependency `{name}` is required from two different sources \
                 ({first} and {second}); a single name must come from one source"
            ),
            ResolveError::VersionConflict { name, a, b } => {
                // Print the lower version first for a stable, readable message.
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                write!(
                    f,
                    "dependency `{name}` has conflicting versions {lo} and {hi}: \
                     they are not semver-compatible (differing major versions), \
                     so they cannot be unified to one package"
                )
            }
            ResolveError::Cyclic { cycle } => {
                write!(f, "dependency cycle: {}", cycle.join(" → "))
            }
            ResolveError::Fetch { name, message } => {
                write!(f, "could not fetch dependency `{name}`: {message}")
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Whether two versions are compatible under cargo-style caret (`^`) semantics
/// (same major for `>= 1.0.0`; same major.minor for `0.x`). Symmetric.
fn caret_compatible(a: &semver::Version, b: &semver::Version) -> bool {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    // `^lo` is exactly the compatibility class cargo uses for a bare version
    // requirement; ask whether the higher version falls in it.
    match semver::VersionReq::parse(&format!("^{lo}")) {
        Ok(req) => req.matches(hi),
        Err(_) => lo == hi,
    }
}

/// The candidate with the highest version. Both reachability and unification
/// must follow the *same* selected version — reachability to prune the
/// superseded version's exclusive subtree, unification to build the resolved
/// package — so they share this one selection rule rather than reimplementing
/// it and risking drift. The slice is never empty at either call site (every
/// candidate list is created via `or_default().push`).
///
/// Same-version candidates are already collapsed upstream by the `visited`
/// dedup (keyed by name + source + version), so this never has to break a
/// same-version tie — which is what keeps the chosen `rev`/`git_ref`
/// deterministic (first-fetched wins, per the dedup note in `resolve_graph`).
/// If that dedup key ever changes, revisit the tie-break here.
fn select_highest(candidates: &[FetchedPackage]) -> &FetchedPackage {
    candidates
        .iter()
        .max_by(|a, b| a.manifest.version.cmp(&b.manifest.version))
        .expect("candidate list is never empty")
}

/// Resolve a root manifest's dependencies transitively into a [`ResolvedGraph`].
///
/// `root_deps` is the entry project's `[dependencies]`; `root_dir` is the
/// directory of the entry `phoenix.toml` (so relative `path` deps resolve).
/// `provider` locates each package and parses its manifest.
pub fn resolve_graph(
    root_deps: &BTreeMap<String, Dependency>,
    root_dir: &std::path::Path,
    provider: &mut impl ManifestProvider,
) -> Result<ResolvedGraph, ResolveError> {
    // Every distinct (name, source, version) requirement seen during the walk,
    // grouped by name so we can unify after traversal.
    let mut candidates: BTreeMap<String, Vec<FetchedPackage>> = BTreeMap::new();
    // Dedup identical (name, source_id, version): a diamond must neither
    // reprocess the same package's subtree nor record it as a candidate twice.
    // For git sources `source_id` is the URL *without* the ref, so the same name
    // reached via the same URL at two refs that resolve to the *same* version
    // (e.g. a branch that moved) collapses here: `select_highest` then keeps the
    // first-fetched commit and the lockfile pins it, rather than erroring on the
    // same-version/different-commit ambiguity. This is intentional (first-seen
    // wins); a ref that resolves to a *different* version is still unified or
    // conflict-checked normally.
    let mut visited: std::collections::HashSet<(String, String, semver::Version)> =
        std::collections::HashSet::new();

    // Explicit DFS stack carrying the parent directory for relative path deps
    // and the chain of names for cycle detection.
    struct Frame {
        name: String,
        dep: Dependency,
        parent_dir: PathBuf,
        chain: Vec<String>,
    }
    let mut stack: Vec<Frame> = root_deps
        .iter()
        .rev()
        .map(|(name, dep)| Frame {
            name: name.clone(),
            dep: dep.clone(),
            parent_dir: root_dir.to_path_buf(),
            chain: Vec::new(),
        })
        .collect();

    while let Some(frame) = stack.pop() {
        // Cycle: this name already appears in its own ancestry.
        if frame.chain.contains(&frame.name) {
            let mut cycle = frame.chain.clone();
            cycle.push(frame.name.clone());
            // Trim to start at the first occurrence for a tight report.
            if let Some(start) = cycle.iter().position(|n| n == &frame.name) {
                cycle = cycle[start..].to_vec();
            }
            return Err(ResolveError::Cyclic { cycle });
        }

        let fetched = provider.fetch(&frame.name, &frame.dep, &frame.parent_dir)?;
        let key = (
            frame.name.clone(),
            fetched.source_id.clone(),
            fetched.manifest.version.clone(),
        );
        // Same name+source+version already walked: its candidate is recorded
        // and its subtree explored. Don't duplicate either.
        if !visited.insert(key) {
            continue;
        }

        candidates
            .entry(frame.name.clone())
            .or_default()
            .push(fetched.clone());

        let mut child_chain = frame.chain.clone();
        child_chain.push(frame.name.clone());
        for (child_name, child_dep) in fetched.manifest.dependencies.iter().rev() {
            stack.push(Frame {
                name: child_name.clone(),
                dep: child_dep.clone(),
                parent_dir: fetched.root.clone(),
                chain: child_chain.clone(),
            });
        }
    }

    // The walk records every version reached from anywhere, including versions
    // pulled in only to be superseded at unification. Before unifying, compute
    // which names are actually reachable from the root through each package's
    // *selected* (highest) version, so a discarded version's exclusive subtree
    // neither lands in the graph nor raises a conflict the graph wouldn't
    // actually contain.
    let mut reachable: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut worklist: Vec<String> = root_deps.keys().cloned().collect();
    while let Some(name) = worklist.pop() {
        if !reachable.insert(name.clone()) {
            continue;
        }
        if let Some(cands) = candidates.get(&name) {
            // The selected version is the highest; follow its dependencies.
            let chosen = select_highest(cands);
            for child in chosen.manifest.dependencies.keys() {
                if !reachable.contains(child) {
                    worklist.push(child.clone());
                }
            }
        }
    }

    // Unify each reachable name's candidates into a single resolved package.
    let mut packages = BTreeMap::new();
    for (name, cands) in candidates {
        if !reachable.contains(&name) {
            // Pulled in only by a version that lost unification; not part of
            // the resolved graph.
            continue;
        }
        // Source identity must agree across all requirements. `cands[0]` is the
        // first candidate discovered in the DFS walk, so the reported
        // `first`/`second` ordering is stable across runs.
        let first_source = cands[0].source_id.clone();
        if let Some(diff) = cands.iter().find(|c| c.source_id != first_source) {
            return Err(ResolveError::SourceConflict {
                name,
                first: first_source,
                second: diff.source_id.clone(),
            });
        }
        // Versions must be pairwise caret-compatible.
        for i in 0..cands.len() {
            for j in (i + 1)..cands.len() {
                if !caret_compatible(&cands[i].manifest.version, &cands[j].manifest.version) {
                    return Err(ResolveError::VersionConflict {
                        name,
                        a: cands[i].manifest.version.clone(),
                        b: cands[j].manifest.version.clone(),
                    });
                }
            }
        }
        // Choose the highest version — the same selection reachability followed.
        let chosen = select_highest(&cands).clone();
        packages.insert(
            name.clone(),
            ResolvedPackage {
                name,
                version: chosen.manifest.version,
                root: chosen.root,
                dependencies: chosen.manifest.dependencies,
                source_id: chosen.source_id,
                rev: chosen.rev,
                git_ref: chosen.git_ref,
            },
        );
    }

    Ok(ResolvedGraph { packages })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::GitRef;

    /// An in-memory provider: maps a `source_id` (git URL or path string) to a
    /// manifest, so tests inject a dependency graph without touching the
    /// filesystem.
    struct MapProvider {
        manifests: BTreeMap<String, (semver::Version, BTreeMap<String, Dependency>)>,
    }

    impl MapProvider {
        fn new() -> Self {
            MapProvider {
                manifests: BTreeMap::new(),
            }
        }

        fn add(
            &mut self,
            source_id: &str,
            version: &str,
            deps: &[(&str, Dependency)],
        ) -> &mut Self {
            let deps = deps
                .iter()
                .map(|(n, d)| (n.to_string(), d.clone()))
                .collect();
            self.manifests.insert(
                source_id.to_string(),
                (semver::Version::parse(version).unwrap(), deps),
            );
            self
        }
    }

    fn source_id_of(dep: &Dependency) -> String {
        match dep {
            Dependency::Git { url, .. } => url.clone(),
            Dependency::Path { path } => path.clone(),
        }
    }

    impl ManifestProvider for MapProvider {
        fn fetch(
            &mut self,
            name: &str,
            dep: &Dependency,
            _parent_dir: &std::path::Path,
        ) -> Result<FetchedPackage, ResolveError> {
            let sid = source_id_of(dep);
            let (version, deps) =
                self.manifests
                    .get(&sid)
                    .cloned()
                    .ok_or_else(|| ResolveError::Fetch {
                        name: name.to_string(),
                        message: format!("no injected manifest for source `{sid}`"),
                    })?;
            Ok(FetchedPackage {
                manifest: PackageManifest {
                    name: name.to_string(),
                    version,
                    dependencies: deps,
                },
                root: PathBuf::from(format!("/virtual/{sid}")),
                source_id: sid,
                rev: None,
                git_ref: None,
            })
        }
    }

    fn git(url: &str, tag: &str) -> Dependency {
        Dependency::Git {
            url: url.to_string(),
            reference: GitRef::Tag(tag.to_string()),
        }
    }

    fn path(p: &str) -> Dependency {
        Dependency::Path {
            path: p.to_string(),
        }
    }

    fn root_deps(deps: &[(&str, Dependency)]) -> BTreeMap<String, Dependency> {
        deps.iter()
            .map(|(n, d)| (n.to_string(), d.clone()))
            .collect()
    }

    #[test]
    fn resolves_single_dependency() {
        let mut p = MapProvider::new();
        p.add("u/http", "1.0.0", &[]);
        let graph = resolve_graph(
            &root_deps(&[("http", git("u/http", "v1"))]),
            std::path::Path::new("/proj"),
            &mut p,
        )
        .unwrap();
        assert_eq!(graph.packages.len(), 1);
        assert_eq!(graph.packages["http"].version.to_string(), "1.0.0");
    }

    #[test]
    fn resolves_empty_root_deps() {
        // A project with no dependencies resolves to an empty graph rather than
        // erroring.
        let mut p = MapProvider::new();
        let graph = resolve_graph(&root_deps(&[]), std::path::Path::new("/proj"), &mut p).unwrap();
        assert!(graph.packages.is_empty());
    }

    #[test]
    fn resolves_path_dependency_and_threads_parent_dir() {
        // A path dependency resolves like any other source, and its own
        // transitive dependency is fetched with `parent_dir` set to the parent
        // package's root — the directory relative `path` sources resolve
        // against. A custom provider asserts that threading directly.
        struct PathProvider;
        impl ManifestProvider for PathProvider {
            fn fetch(
                &mut self,
                name: &str,
                dep: &Dependency,
                parent_dir: &std::path::Path,
            ) -> Result<FetchedPackage, ResolveError> {
                assert!(matches!(dep, Dependency::Path { .. }));
                match name {
                    "util" => {
                        // Declared by the root, so resolved relative to root_dir.
                        assert_eq!(parent_dir, std::path::Path::new("/proj"));
                        Ok(FetchedPackage {
                            manifest: PackageManifest {
                                name: "util".to_string(),
                                version: semver::Version::parse("1.0.0").unwrap(),
                                dependencies: [("core".to_string(), path("../core"))]
                                    .into_iter()
                                    .collect(),
                            },
                            root: PathBuf::from("/proj/util"),
                            source_id: "/proj/util".to_string(),
                            rev: None,
                            git_ref: None,
                        })
                    }
                    "core" => {
                        // Declared by `util`, so resolved relative to util's root.
                        assert_eq!(parent_dir, std::path::Path::new("/proj/util"));
                        Ok(FetchedPackage {
                            manifest: PackageManifest {
                                name: "core".to_string(),
                                version: semver::Version::parse("1.0.0").unwrap(),
                                dependencies: BTreeMap::new(),
                            },
                            root: PathBuf::from("/proj/core"),
                            source_id: "/proj/core".to_string(),
                            rev: None,
                            git_ref: None,
                        })
                    }
                    other => panic!("unexpected fetch for `{other}`"),
                }
            }
        }
        let graph = resolve_graph(
            &root_deps(&[("util", path("./util"))]),
            std::path::Path::new("/proj"),
            &mut PathProvider,
        )
        .unwrap();
        assert_eq!(graph.packages.len(), 2);
        assert_eq!(graph.packages["util"].root, PathBuf::from("/proj/util"));
        assert_eq!(graph.packages["core"].root, PathBuf::from("/proj/core"));
    }

    #[test]
    fn resolves_transitive_chain() {
        // app → http → io
        let mut p = MapProvider::new();
        p.add("u/http", "1.0.0", &[("io", git("u/io", "v1"))]);
        p.add("u/io", "2.3.0", &[]);
        let graph = resolve_graph(
            &root_deps(&[("http", git("u/http", "v1"))]),
            std::path::Path::new("/proj"),
            &mut p,
        )
        .unwrap();
        assert_eq!(graph.packages.len(), 2);
        assert!(graph.packages.contains_key("http"));
        assert_eq!(graph.packages["io"].version.to_string(), "2.3.0");
    }

    #[test]
    fn diamond_same_source_dedups() {
        // app → a → core, app → b → core (same source+version).
        let mut p = MapProvider::new();
        p.add("u/a", "1.0.0", &[("core", git("u/core", "v1"))]);
        p.add("u/b", "1.0.0", &[("core", git("u/core", "v1"))]);
        p.add("u/core", "1.0.0", &[]);
        let graph = resolve_graph(
            &root_deps(&[("a", git("u/a", "v1")), ("b", git("u/b", "v1"))]),
            std::path::Path::new("/proj"),
            &mut p,
        )
        .unwrap();
        assert_eq!(graph.packages.len(), 3);
        assert_eq!(graph.packages["core"].version.to_string(), "1.0.0");
    }

    #[test]
    fn diamond_compatible_versions_picks_highest() {
        // Two parents pin `core` to compatible versions of the same repo. A
        // custom provider returns a version keyed off the git tag so both
        // requirements share a source_id but differ in version.
        struct TaggedVersions;
        impl ManifestProvider for TaggedVersions {
            fn fetch(
                &mut self,
                name: &str,
                dep: &Dependency,
                _p: &std::path::Path,
            ) -> Result<FetchedPackage, ResolveError> {
                if name == "core" {
                    let version = match dep {
                        Dependency::Git {
                            reference: GitRef::Tag(t),
                            ..
                        } if t == "v1.5" => "1.5.0",
                        _ => "1.2.0",
                    };
                    return Ok(FetchedPackage {
                        manifest: PackageManifest {
                            name: name.to_string(),
                            version: semver::Version::parse(version).unwrap(),
                            dependencies: BTreeMap::new(),
                        },
                        root: PathBuf::from("/virtual/core"),
                        source_id: "u/core".to_string(),
                        rev: None,
                        git_ref: None,
                    });
                }
                // `a` wants core v1.2; `b` wants core v1.5.
                let core_tag = if name == "a" { "v1.2" } else { "v1.5" };
                Ok(FetchedPackage {
                    manifest: PackageManifest {
                        name: name.to_string(),
                        version: semver::Version::parse("1.0.0").unwrap(),
                        dependencies: [("core".to_string(), git("u/core", core_tag))]
                            .into_iter()
                            .collect(),
                    },
                    root: PathBuf::from(format!("/virtual/{name}")),
                    source_id: format!("u/{name}"),
                    rev: None,
                    git_ref: None,
                })
            }
        }
        let graph = resolve_graph(
            &root_deps(&[("a", git("u/a", "v1")), ("b", git("u/b", "v1"))]),
            std::path::Path::new("/proj"),
            &mut TaggedVersions,
        )
        .unwrap();
        assert_eq!(graph.packages["core"].version.to_string(), "1.5.0");
    }

    #[test]
    fn source_conflict_reported() {
        // app → core (repo X), app → a → core (repo Y) — different upstreams.
        let mut p = MapProvider::new();
        p.add("X/core", "1.0.0", &[]);
        p.add("Y/core", "1.0.0", &[]);
        p.add("u/a", "1.0.0", &[("core", git("Y/core", "v1"))]);
        let err = resolve_graph(
            &root_deps(&[("core", git("X/core", "v1")), ("a", git("u/a", "v1"))]),
            std::path::Path::new("/proj"),
            &mut p,
        )
        .unwrap_err();
        match err {
            ResolveError::SourceConflict { name, .. } => assert_eq!(name, "core"),
            other => panic!("expected SourceConflict, got {other}"),
        }
    }

    #[test]
    fn version_conflict_reported() {
        // Same source identity, two incompatible majors → VersionConflict.
        struct TwoVersionProvider;
        impl ManifestProvider for TwoVersionProvider {
            fn fetch(
                &mut self,
                name: &str,
                dep: &Dependency,
                _p: &std::path::Path,
            ) -> Result<FetchedPackage, ResolveError> {
                if name == "core" {
                    let version = match dep {
                        Dependency::Git {
                            reference: GitRef::Tag(t),
                            ..
                        } if t == "v2" => "2.0.0",
                        _ => "1.0.0",
                    };
                    return Ok(FetchedPackage {
                        manifest: PackageManifest {
                            name: name.to_string(),
                            version: semver::Version::parse(version).unwrap(),
                            dependencies: BTreeMap::new(),
                        },
                        root: PathBuf::from("/virtual/core"),
                        source_id: "u/core".to_string(),
                        rev: None,
                        git_ref: None,
                    });
                }
                // `a` depends on core v2.
                Ok(FetchedPackage {
                    manifest: PackageManifest {
                        name: name.to_string(),
                        version: semver::Version::parse("1.0.0").unwrap(),
                        dependencies: [("core".to_string(), git("u/core", "v2"))]
                            .into_iter()
                            .collect(),
                    },
                    root: PathBuf::from("/virtual/a"),
                    source_id: "u/a".to_string(),
                    rev: None,
                    git_ref: None,
                })
            }
        }
        let err = resolve_graph(
            &root_deps(&[("core", git("u/core", "v1")), ("a", git("u/a", "v1"))]),
            std::path::Path::new("/proj"),
            &mut TwoVersionProvider,
        )
        .unwrap_err();
        match err {
            ResolveError::VersionConflict { name, .. } => assert_eq!(name, "core"),
            other => panic!("expected VersionConflict, got {other}"),
        }
        assert!(
            ResolveError::VersionConflict {
                name: "core".into(),
                a: semver::Version::parse("1.0.0").unwrap(),
                b: semver::Version::parse("2.0.0").unwrap(),
            }
            .to_string()
            .contains("not semver-compatible")
        );
    }

    #[test]
    fn cycle_reported() {
        // a → b → a
        let mut p = MapProvider::new();
        p.add("u/a", "1.0.0", &[("b", git("u/b", "v1"))]);
        p.add("u/b", "1.0.0", &[("a", git("u/a", "v1"))]);
        let err = resolve_graph(
            &root_deps(&[("a", git("u/a", "v1"))]),
            std::path::Path::new("/proj"),
            &mut p,
        )
        .unwrap_err();
        match err {
            ResolveError::Cyclic { cycle } => {
                assert!(cycle.contains(&"a".to_string()) && cycle.contains(&"b".to_string()));
            }
            other => panic!("expected Cyclic, got {other}"),
        }
    }

    #[test]
    fn direct_self_cycle_reported() {
        // a → a: the degenerate cycle. A package that depends directly on
        // itself must still be caught by the chain check.
        let mut p = MapProvider::new();
        p.add("u/a", "1.0.0", &[("a", git("u/a", "v1"))]);
        let err = resolve_graph(
            &root_deps(&[("a", git("u/a", "v1"))]),
            std::path::Path::new("/proj"),
            &mut p,
        )
        .unwrap_err();
        match err {
            ResolveError::Cyclic { cycle } => {
                assert_eq!(cycle, vec!["a".to_string(), "a".to_string()]);
            }
            other => panic!("expected Cyclic, got {other}"),
        }
    }

    #[test]
    fn discarded_version_deps_are_pruned() {
        // app → a → core v1.2 (which needs `legacy`), app → b → core v1.5 (no
        // `legacy`). v1.5 wins unification; `legacy` was pulled in only by the
        // superseded v1.2 and must not survive into the resolved graph.
        struct P;
        impl ManifestProvider for P {
            fn fetch(
                &mut self,
                name: &str,
                dep: &Dependency,
                _p: &std::path::Path,
            ) -> Result<FetchedPackage, ResolveError> {
                let (version, deps): (&str, BTreeMap<String, Dependency>) = match name {
                    "core" => {
                        let tag = match dep {
                            Dependency::Git {
                                reference: GitRef::Tag(t),
                                ..
                            } => t.as_str(),
                            _ => "",
                        };
                        if tag == "v1.5" {
                            ("1.5.0", BTreeMap::new())
                        } else {
                            (
                                "1.2.0",
                                [("legacy".to_string(), git("u/legacy", "v1"))]
                                    .into_iter()
                                    .collect(),
                            )
                        }
                    }
                    "a" => (
                        "1.0.0",
                        [("core".to_string(), git("u/core", "v1.2"))]
                            .into_iter()
                            .collect(),
                    ),
                    "b" => (
                        "1.0.0",
                        [("core".to_string(), git("u/core", "v1.5"))]
                            .into_iter()
                            .collect(),
                    ),
                    // `legacy` and anything else: a leaf at 1.0.0.
                    _ => ("1.0.0", BTreeMap::new()),
                };
                let source_id = if name == "core" {
                    "u/core".to_string()
                } else {
                    format!("u/{name}")
                };
                Ok(FetchedPackage {
                    manifest: PackageManifest {
                        name: name.to_string(),
                        version: semver::Version::parse(version).unwrap(),
                        dependencies: deps,
                    },
                    root: PathBuf::from(format!("/virtual/{name}")),
                    source_id,
                    rev: None,
                    git_ref: None,
                })
            }
        }
        let graph = resolve_graph(
            &root_deps(&[("a", git("u/a", "v1")), ("b", git("u/b", "v1"))]),
            std::path::Path::new("/proj"),
            &mut P,
        )
        .unwrap();
        assert_eq!(graph.packages["core"].version.to_string(), "1.5.0");
        assert!(
            !graph.packages.contains_key("legacy"),
            "dep of the superseded core v1.2 leaked into the graph: {:?}",
            graph.packages.keys().collect::<Vec<_>>()
        );
        // Only a, b, and core survive; legacy is pruned.
        assert_eq!(graph.packages.len(), 3);
    }

    #[test]
    fn fetch_error_propagates() {
        // A provider that fails to locate a dependency surfaces its error
        // unchanged rather than swallowing it.
        struct Failing;
        impl ManifestProvider for Failing {
            fn fetch(
                &mut self,
                name: &str,
                _d: &Dependency,
                _p: &std::path::Path,
            ) -> Result<FetchedPackage, ResolveError> {
                Err(ResolveError::Fetch {
                    name: name.to_string(),
                    message: "boom".to_string(),
                })
            }
        }
        let err = resolve_graph(
            &root_deps(&[("http", git("u/http", "v1"))]),
            std::path::Path::new("/proj"),
            &mut Failing,
        )
        .unwrap_err();
        match err {
            ResolveError::Fetch { name, message } => {
                assert_eq!(name, "http");
                assert_eq!(message, "boom");
            }
            other => panic!("expected Fetch, got {other}"),
        }
    }

    #[test]
    fn manifest_error_propagates() {
        // A provider rejecting a package's manifest surfaces
        // ResolveError::Manifest, the variant the runtime fetcher reports for
        // schema-invalid `phoenix.toml` files.
        struct BadManifest;
        impl ManifestProvider for BadManifest {
            fn fetch(
                &mut self,
                name: &str,
                _d: &Dependency,
                _p: &std::path::Path,
            ) -> Result<FetchedPackage, ResolveError> {
                Err(ResolveError::Manifest {
                    name: name.to_string(),
                    error: ManifestError::EmptyPackageName,
                })
            }
        }
        let err = resolve_graph(
            &root_deps(&[("http", git("u/http", "v1"))]),
            std::path::Path::new("/proj"),
            &mut BadManifest,
        )
        .unwrap_err();
        match err {
            ResolveError::Manifest { name, .. } => assert_eq!(name, "http"),
            other => panic!("expected Manifest, got {other}"),
        }
    }

    #[test]
    fn caret_compatibility_rules() {
        let v = |s: &str| semver::Version::parse(s).unwrap();
        assert!(caret_compatible(&v("1.2.0"), &v("1.5.0")));
        assert!(!caret_compatible(&v("1.0.0"), &v("2.0.0")));
        // 0.x: differing minors are incompatible (cargo 0.x rule).
        assert!(!caret_compatible(&v("0.1.0"), &v("0.2.0")));
        assert!(caret_compatible(&v("0.1.0"), &v("0.1.5")));
    }

    #[test]
    fn root_of_returns_resolved_root_or_none() {
        let mut p = MapProvider::new();
        p.add("u/http", "1.0.0", &[]);
        let graph = resolve_graph(
            &root_deps(&[("http", git("u/http", "v1"))]),
            std::path::Path::new("/proj"),
            &mut p,
        )
        .unwrap();
        assert_eq!(
            graph.root_of("http"),
            Some(std::path::Path::new("/virtual/u/http"))
        );
        assert_eq!(graph.root_of("absent"), None);
    }

    #[test]
    fn cycle_report_trims_non_cyclic_prefix() {
        // root → x → a → b → a. `x` leads into but is not part of the cycle, so
        // the reported cycle must start at the first repeated name (`a`), not at
        // `x`.
        let mut p = MapProvider::new();
        p.add("u/x", "1.0.0", &[("a", git("u/a", "v1"))]);
        p.add("u/a", "1.0.0", &[("b", git("u/b", "v1"))]);
        p.add("u/b", "1.0.0", &[("a", git("u/a", "v1"))]);
        let err = resolve_graph(
            &root_deps(&[("x", git("u/x", "v1"))]),
            std::path::Path::new("/proj"),
            &mut p,
        )
        .unwrap_err();
        match err {
            ResolveError::Cyclic { cycle } => {
                assert_eq!(
                    cycle,
                    vec!["a".to_string(), "b".to_string(), "a".to_string()]
                );
            }
            other => panic!("expected Cyclic, got {other}"),
        }
    }

    #[test]
    fn shared_transitive_dep_across_versions_is_a_known_residual() {
        // app → a → core v1.2 (→ shared v1), app → b → core v1.5 (→ shared v2).
        // core v1.5 wins unification, so `shared` should resolve to v2 alone.
        // The current resolver records `shared` v1 from the *superseded* core
        // v1.2 — whose subtree the reachability prune does not reach, because
        // `shared` is still reachable through the surviving core v1.5 — and so
        // reports a spurious VersionConflict. A fully general fix needs
        // backtracking (see module docs); this test pins the current behavior so
        // that fix is observable when it lands.
        struct P;
        impl ManifestProvider for P {
            fn fetch(
                &mut self,
                name: &str,
                dep: &Dependency,
                _p: &std::path::Path,
            ) -> Result<FetchedPackage, ResolveError> {
                let (version, deps): (&str, BTreeMap<String, Dependency>) = match name {
                    "core" => {
                        let tag = match dep {
                            Dependency::Git {
                                reference: GitRef::Tag(t),
                                ..
                            } => t.as_str(),
                            _ => "",
                        };
                        if tag == "v1.5" {
                            (
                                "1.5.0",
                                [("shared".to_string(), git("u/shared", "v2"))]
                                    .into_iter()
                                    .collect(),
                            )
                        } else {
                            (
                                "1.2.0",
                                [("shared".to_string(), git("u/shared", "v1"))]
                                    .into_iter()
                                    .collect(),
                            )
                        }
                    }
                    "shared" => {
                        let tag = match dep {
                            Dependency::Git {
                                reference: GitRef::Tag(t),
                                ..
                            } => t.as_str(),
                            _ => "",
                        };
                        if tag == "v2" {
                            ("2.0.0", BTreeMap::new())
                        } else {
                            ("1.0.0", BTreeMap::new())
                        }
                    }
                    "a" => (
                        "1.0.0",
                        [("core".to_string(), git("u/core", "v1.2"))]
                            .into_iter()
                            .collect(),
                    ),
                    "b" => (
                        "1.0.0",
                        [("core".to_string(), git("u/core", "v1.5"))]
                            .into_iter()
                            .collect(),
                    ),
                    _ => ("1.0.0", BTreeMap::new()),
                };
                let source_id = match name {
                    "core" => "u/core".to_string(),
                    "shared" => "u/shared".to_string(),
                    _ => format!("u/{name}"),
                };
                Ok(FetchedPackage {
                    manifest: PackageManifest {
                        name: name.to_string(),
                        version: semver::Version::parse(version).unwrap(),
                        dependencies: deps,
                    },
                    root: PathBuf::from(format!("/virtual/{name}")),
                    source_id,
                    rev: None,
                    git_ref: None,
                })
            }
        }
        let err = resolve_graph(
            &root_deps(&[("a", git("u/a", "v1")), ("b", git("u/b", "v1"))]),
            std::path::Path::new("/proj"),
            &mut P,
        )
        .unwrap_err();
        match err {
            ResolveError::VersionConflict { name, .. } => assert_eq!(name, "shared"),
            other => panic!("expected the residual VersionConflict on `shared`, got {other}"),
        }
    }

    #[test]
    fn compatible_superseded_dep_version_is_a_known_residual() {
        // app → a → core v1.2 (→ shared v1.3), app → b → core v1.5 (→ shared
        // v1.1). core v1.5 wins unification, so the only *surviving* requirement
        // on `shared` is core v1.5's v1.1. But the superseded core v1.2 also
        // recorded shared v1.3, and because the two are caret-compatible the
        // resolver silently selects the higher v1.3 — a version no surviving
        // requirement asked for. The result is still semver-safe (v1.3 satisfies
        // ^1.1), so this is sound but violates minimal-version selection. This is
        // the silent sibling of the conflict residual above: a fully general fix
        // needs backtracking (see module docs). This test pins the current
        // behavior so the intended result (v1.1) is observable when that lands.
        struct P;
        impl ManifestProvider for P {
            fn fetch(
                &mut self,
                name: &str,
                dep: &Dependency,
                _p: &std::path::Path,
            ) -> Result<FetchedPackage, ResolveError> {
                let (version, deps): (&str, BTreeMap<String, Dependency>) = match name {
                    "core" => {
                        let tag = match dep {
                            Dependency::Git {
                                reference: GitRef::Tag(t),
                                ..
                            } => t.as_str(),
                            _ => "",
                        };
                        if tag == "v1.5" {
                            (
                                "1.5.0",
                                [("shared".to_string(), git("u/shared", "v1.1"))]
                                    .into_iter()
                                    .collect(),
                            )
                        } else {
                            (
                                "1.2.0",
                                [("shared".to_string(), git("u/shared", "v1.3"))]
                                    .into_iter()
                                    .collect(),
                            )
                        }
                    }
                    "shared" => {
                        let tag = match dep {
                            Dependency::Git {
                                reference: GitRef::Tag(t),
                                ..
                            } => t.as_str(),
                            _ => "",
                        };
                        if tag == "v1.3" {
                            ("1.3.0", BTreeMap::new())
                        } else {
                            ("1.1.0", BTreeMap::new())
                        }
                    }
                    "a" => (
                        "1.0.0",
                        [("core".to_string(), git("u/core", "v1.2"))]
                            .into_iter()
                            .collect(),
                    ),
                    "b" => (
                        "1.0.0",
                        [("core".to_string(), git("u/core", "v1.5"))]
                            .into_iter()
                            .collect(),
                    ),
                    _ => ("1.0.0", BTreeMap::new()),
                };
                let source_id = match name {
                    "core" => "u/core".to_string(),
                    "shared" => "u/shared".to_string(),
                    _ => format!("u/{name}"),
                };
                Ok(FetchedPackage {
                    manifest: PackageManifest {
                        name: name.to_string(),
                        version: semver::Version::parse(version).unwrap(),
                        dependencies: deps,
                    },
                    root: PathBuf::from(format!("/virtual/{name}")),
                    source_id,
                    rev: None,
                    git_ref: None,
                })
            }
        }
        let graph = resolve_graph(
            &root_deps(&[("a", git("u/a", "v1")), ("b", git("u/b", "v1"))]),
            std::path::Path::new("/proj"),
            &mut P,
        )
        .unwrap();
        assert_eq!(graph.packages["core"].version.to_string(), "1.5.0");
        // Current (residual) behavior: the superseded core v1.2's shared v1.3
        // wins over the surviving requirement's v1.1. The intended post-fix
        // result is "1.1.0".
        assert_eq!(graph.packages["shared"].version.to_string(), "1.3.0");
    }

    #[test]
    fn same_version_diamond_keeps_first_fetched_rev() {
        // app → a (git) → core (git u/core)
        // app → b (git) → core (git u/core)
        //
        // Both edges reach `core` at the *same* source and version but resolve to
        // *different* commits (a moving ref fetched twice). The `visited` dedup
        // collapses them on (name, source_id, version), so the first-fetched
        // candidate wins and its `rev`/`git_ref` is what the lockfile pins. This
        // locks in the deterministic tie-break documented on `select_highest`:
        // the DFS visits `a` before `b`, so `core`'s commit is `a`'s "aaaa…",
        // never `b`'s "bbbb…".
        #[derive(Default)]
        struct P {
            core_fetches: u32,
        }
        impl ManifestProvider for P {
            fn fetch(
                &mut self,
                name: &str,
                _dep: &Dependency,
                _parent_dir: &std::path::Path,
            ) -> Result<FetchedPackage, ResolveError> {
                let (source_id, deps): (String, BTreeMap<String, Dependency>) = match name {
                    "a" => (
                        "u/a".to_string(),
                        [("core".to_string(), git("u/core", "main"))]
                            .into_iter()
                            .collect(),
                    ),
                    "b" => (
                        "u/b".to_string(),
                        [("core".to_string(), git("u/core", "main"))]
                            .into_iter()
                            .collect(),
                    ),
                    "core" => ("u/core".to_string(), BTreeMap::new()),
                    other => panic!("unexpected fetch for `{other}`"),
                };
                // `core` is fetched once per in-edge (the resolver dedups only
                // after fetching); hand back a different commit each time so the
                // tie-break has something to choose between.
                let (rev, git_ref) = if name == "core" {
                    self.core_fetches += 1;
                    if self.core_fetches == 1 {
                        (Some("a".repeat(40)), GitRef::Branch("main".into()))
                    } else {
                        (Some("b".repeat(40)), GitRef::Branch("main".into()))
                    }
                } else {
                    (Some(format!("{name}rev")), GitRef::Tag("v1".into()))
                };
                Ok(FetchedPackage {
                    manifest: PackageManifest {
                        name: name.to_string(),
                        version: semver::Version::parse("1.0.0").unwrap(),
                        dependencies: deps,
                    },
                    root: PathBuf::from(format!("/virtual/{name}")),
                    source_id,
                    rev,
                    git_ref: Some(git_ref),
                })
            }
        }

        let mut p = P::default();
        let graph = resolve_graph(
            &root_deps(&[("a", git("u/a", "v1")), ("b", git("u/b", "v1"))]),
            std::path::Path::new("/proj"),
            &mut p,
        )
        .unwrap();
        // Both edges were fetched (dedup happens *after* fetch), but the resolved
        // `core` keeps the first-fetched commit, not the second.
        assert_eq!(p.core_fetches, 2, "both in-edges fetch core before dedup");
        assert_eq!(
            graph.packages["core"].rev.as_deref(),
            Some("a".repeat(40).as_str()),
            "the first-fetched commit must win the same-version tie-break"
        );
    }
}
