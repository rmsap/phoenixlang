//! Project-level dependency resolution: ties the manifest, the lockfile, and
//! the cache-backed fetcher together into the operation `phoenix build` / `run`
//! / `check` run before compiling.
//!
//! Flow:
//! 1. Read an existing `phoenix.lock` (if any); its pinned commits make the
//!    fetch reproducible and offline where possible.
//! 2. Resolve the dependency graph through the [`CacheFetcher`].
//! 3. Derive a fresh lockfile from the resolved graph and reconcile it with
//!    what's committed: with `--locked`, any drift is an error; otherwise the
//!    lockfile is rewritten when it changed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::manifest::Dependency;

use super::fetch::CacheFetcher;
use super::graph::{ResolveError, ResolvedGraph, resolve_graph};
use super::lock::{LockError, Lockfile};

/// The outcome of resolving a project's dependencies.
#[derive(Debug)]
pub struct ProjectResolution {
    /// The resolved dependency graph (one package per name).
    pub graph: ResolvedGraph,
    /// The lockfile derived from `graph`.
    pub lockfile: Lockfile,
    /// Whether this resolve changed the on-disk `phoenix.lock` — created it,
    /// rewrote it, or removed it (the last git dependency being dropped removes
    /// the file rather than rewriting an empty one). `false` means the on-disk
    /// state was already in sync and left untouched.
    pub lock_changed: bool,
}

/// Errors from [`resolve_project`].
#[derive(Debug)]
pub enum ProjectResolveError {
    /// Graph resolution / fetching failed.
    Resolve(ResolveError),
    /// Reading, writing, or reconciling the lockfile failed (includes
    /// `--locked` drift).
    Lock(LockError),
}

impl std::fmt::Display for ProjectResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectResolveError::Resolve(e) => write!(f, "{e}"),
            ProjectResolveError::Lock(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProjectResolveError {}

impl From<ResolveError> for ProjectResolveError {
    fn from(e: ResolveError) -> Self {
        ProjectResolveError::Resolve(e)
    }
}

impl From<LockError> for ProjectResolveError {
    fn from(e: LockError) -> Self {
        ProjectResolveError::Lock(e)
    }
}

/// Resolve a project's dependencies, fetching into `cache_root` and maintaining
/// `phoenix.lock` beside the manifest.
///
/// - `manifest_dir` is the directory containing the project's `phoenix.toml`
///   (relative `path` deps resolve against it, and the lockfile lives there).
/// - `deps` is the project's validated `[dependencies]`.
/// - `locked`: when `true`, a present lockfile is authoritative and any drift
///   between it and the freshly-resolved graph is an error (the lockfile is
///   never rewritten). When `false`, the lockfile is (re)written if it changed.
pub fn resolve_project(
    manifest_dir: &Path,
    deps: &BTreeMap<String, Dependency>,
    cache_root: &Path,
    locked: bool,
) -> Result<ProjectResolution, ProjectResolveError> {
    let lock_path = manifest_dir.join("phoenix.lock");
    let existing = if lock_path.is_file() {
        Some(Lockfile::read(&lock_path)?)
    } else {
        None
    };

    let locked_packages = existing
        .as_ref()
        .map(|l| l.packages.clone())
        .unwrap_or_default();
    let mut fetcher = CacheFetcher::new(cache_root.to_path_buf(), locked_packages);

    let graph = resolve_graph(deps, manifest_dir, &mut fetcher)?;
    let fresh = Lockfile::from_graph(&graph);

    let lock_changed = reconcile_lock(&lock_path, &fresh, existing.as_ref(), locked)?;

    Ok(ProjectResolution {
        graph,
        lockfile: fresh,
        lock_changed,
    })
}

/// Reconcile the freshly-resolved lockfile with what's committed. Returns
/// whether the on-disk file changed — written, rewritten, or removed (`true`),
/// versus already in sync and left untouched (`false`).
fn reconcile_lock(
    lock_path: &Path,
    fresh: &Lockfile,
    existing: Option<&Lockfile>,
    locked: bool,
) -> Result<bool, LockError> {
    match existing {
        Some(old) => {
            let diffs = fresh.diff(old);
            if diffs.is_empty() {
                return Ok(false);
            }
            if locked {
                return Err(LockError::Drift(diffs));
            }
            // A project that dropped its last git dependency leaves nothing to
            // pin: remove the stale lockfile rather than rewriting a contentless
            // `version = 1` file (which would otherwise linger in the repo).
            if fresh.packages.is_empty() {
                std::fs::remove_file(lock_path).map_err(LockError::Write)?;
                return Ok(true);
            }
            fresh.write(lock_path)?;
            Ok(true)
        }
        None => {
            // No lockfile yet. Under `--locked` a build must have a committed
            // lock to be reproducible; refuse rather than silently resolving.
            if locked {
                if fresh.packages.is_empty() {
                    // Nothing to lock (no git deps) — `--locked` is trivially
                    // satisfied without a file.
                    return Ok(false);
                }
                return Err(LockError::Drift(vec![
                    "no phoenix.lock present, but git dependencies require one under --locked"
                        .to_string(),
                ]));
            }
            // Only write a lockfile when there is something to pin (git deps);
            // a path-only / dependency-free project gets no lockfile.
            if fresh.packages.is_empty() {
                return Ok(false);
            }
            fresh.write(lock_path)?;
            Ok(true)
        }
    }
}

/// Convenience for callers (`build`/`run`/`check`) that want the cache at its
/// default location: resolves [`super::cache::default_cache_dir`], erroring if
/// no `$PHOENIX_HOME` / home directory can be determined.
pub fn default_cache_root() -> Result<PathBuf, String> {
    cache_root_or_error(super::cache::default_cache_dir())
}

/// Pure core of [`default_cache_root`]: map a resolved cache dir to `Ok`, or an
/// actionable error when none could be determined. Split out so the mapping is
/// unit-testable without depending on the live `$PHOENIX_HOME` / home directory.
fn cache_root_or_error(dir: Option<PathBuf>) -> Result<PathBuf, String> {
    dir.ok_or_else(|| {
        "could not determine the dependency cache location: set $PHOENIX_HOME \
         (or ensure a home directory is available)"
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::graph::{ResolvedGraph, ResolvedPackage};
    use crate::deps::lock::Lockfile;

    fn git_graph(name: &str, version: &str, url: &str, rev: &str) -> ResolvedGraph {
        let pkg = ResolvedPackage {
            name: name.to_string(),
            version: semver::Version::parse(version).unwrap(),
            root: PathBuf::from("/cache/x"),
            dependencies: BTreeMap::new(),
            source_id: url.to_string(),
            rev: Some(rev.to_string()),
            git_ref: Some(crate::manifest::GitRef::Tag(format!("v{version}"))),
        };
        ResolvedGraph {
            packages: [(name.to_string(), pkg)].into_iter().collect(),
        }
    }

    #[test]
    fn reconcile_writes_when_no_existing_lock_and_git_deps() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("phoenix.lock");
        let fresh = Lockfile::from_graph(&git_graph("http", "1.0.0", "u/http.git", "abc12345"));
        let wrote = reconcile_lock(&lock_path, &fresh, None, false).unwrap();
        assert!(wrote);
        assert!(lock_path.is_file());
    }

    #[test]
    fn reconcile_no_write_when_in_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("phoenix.lock");
        let fresh = Lockfile::from_graph(&git_graph("http", "1.0.0", "u/http.git", "abc12345"));
        let wrote = reconcile_lock(&lock_path, &fresh, Some(&fresh.clone()), false).unwrap();
        assert!(!wrote);
        assert!(!lock_path.exists(), "in-sync resolve must not write");
    }

    #[test]
    fn reconcile_locked_drift_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("phoenix.lock");
        let old = Lockfile::from_graph(&git_graph("http", "1.0.0", "u/http.git", "aaaa1111"));
        let fresh = Lockfile::from_graph(&git_graph("http", "1.1.0", "u/http.git", "bbbb2222"));
        let err = reconcile_lock(&lock_path, &fresh, Some(&old), true).unwrap_err();
        assert!(matches!(err, LockError::Drift(_)));
        assert!(!lock_path.exists(), "--locked must never rewrite the lock");
    }

    #[test]
    fn reconcile_locked_missing_lock_with_git_deps_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("phoenix.lock");
        let fresh = Lockfile::from_graph(&git_graph("http", "1.0.0", "u/http.git", "abc12345"));
        let err = reconcile_lock(&lock_path, &fresh, None, true).unwrap_err();
        assert!(matches!(err, LockError::Drift(_)));
    }

    #[test]
    fn reconcile_removes_stale_lock_when_all_git_deps_dropped() {
        // A project that previously pinned a git dep, then dropped it, must end
        // up with *no* lockfile rather than a contentless `version = 1` file.
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("phoenix.lock");
        let old = Lockfile::from_graph(&git_graph("http", "1.0.0", "u/http.git", "abc12345"));
        old.write(&lock_path).unwrap();
        assert!(lock_path.is_file());

        let empty = Lockfile {
            version: crate::deps::lock::LOCKFILE_VERSION,
            packages: BTreeMap::new(),
        };
        let wrote = reconcile_lock(&lock_path, &empty, Some(&old), false).unwrap();
        assert!(wrote, "dropping the last git dep is a change");
        assert!(
            !lock_path.exists(),
            "the stale lockfile must be removed, not rewritten empty"
        );
    }

    #[test]
    fn cache_root_or_error_maps_some_and_none() {
        // Some(dir) passes through; None becomes an actionable error mentioning
        // the env var the user can set to fix it.
        assert_eq!(
            cache_root_or_error(Some(PathBuf::from("/x/cache"))).unwrap(),
            PathBuf::from("/x/cache")
        );
        let err = cache_root_or_error(None).unwrap_err();
        assert!(err.contains("$PHOENIX_HOME"), "got: {err}");
    }

    #[test]
    fn reconcile_no_lock_for_dependency_free_project() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("phoenix.lock");
        let empty = Lockfile {
            version: crate::deps::lock::LOCKFILE_VERSION,
            packages: BTreeMap::new(),
        };
        // Neither --locked nor a normal resolve writes a lock when there's
        // nothing to pin.
        assert!(!reconcile_lock(&lock_path, &empty, None, false).unwrap());
        assert!(!reconcile_lock(&lock_path, &empty, None, true).unwrap());
        assert!(!lock_path.exists());
    }
}
