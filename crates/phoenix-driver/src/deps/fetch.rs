//! The cache-backed [`ManifestProvider`]: clones git sources into the cache and
//! locates `path` sources in place.
//!
//! Git dependencies are fetched into `$PHOENIX_HOME/cache` (never the project
//! tree) using [`gix`] (pure-Rust git). A bare clone lives once per URL under
//! `git/db/<slug>`; the files of a resolved commit are materialized per commit
//! under `git/checkouts/<slug>/<sha>`. When a lockfile already pins a
//! dependency's commit and that commit is already checked out, the fetch is
//! fully offline — the basis for reproducible builds from a clean checkout.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crate::config::PhoenixConfig;
use crate::manifest::{Dependency, GitRef};

use super::cache::{git_checkout_dir, git_db_dir};
use super::graph::{
    FetchedPackage, ManifestProvider, PackageManifest, PackageSource, ResolveError,
};
use super::lock::LockedPackage;

/// Filename extension for the per-checkout completion marker. The marker lives
/// *beside* the checkout directory (`<sha>.ok`), never inside it, and is written
/// only *after* the worktree has been fully materialized. Its presence — not the
/// presence of `phoenix.toml`, which `gix` may write partway through a checkout —
/// is what proves a cached checkout is complete, so an interrupted checkout is
/// never mistaken for a usable one (mirrors Cargo's `.cargo-ok`). Keeping it
/// outside the checkout tree means it can never be shadowed by a file the
/// dependency repo itself ships (a repo carrying a same-named file would, if the
/// marker lived inside, let an interrupted checkout look complete), and it is
/// likewise invisible to the Phoenix module loader.
const CHECKOUT_OK_EXT: &str = "ok";

/// A [`ManifestProvider`] that fetches git dependencies into the on-disk cache
/// and resolves `path` dependencies relative to their declaring manifest.
pub struct CacheFetcher {
    /// The dependency cache root (`$PHOENIX_HOME/cache`).
    cache_root: PathBuf,
    /// Locked packages from an existing `phoenix.lock`, keyed by dependency
    /// name. A git dependency whose name is locked *and* whose URL still
    /// matches reuses the pinned commit — no ref resolution, and no network if
    /// the commit is already checked out.
    locked: BTreeMap<String, LockedPackage>,
    /// Git URLs whose bare clone has already been (re)fetched during *this*
    /// resolve. A git dependency reached via several paths — a diamond, or two
    /// parents depending on the same library — is fetched once per in-edge,
    /// because the resolver dedups only *after* fetching. Without this set each
    /// of those edges would wipe and re-clone the same repo over the network;
    /// recording refreshed URLs collapses them to a single clone per resolve.
    refreshed: HashSet<String>,
}

impl CacheFetcher {
    /// Create a fetcher writing into `cache_root`, honoring the `locked`
    /// revisions from an existing lockfile (pass an empty map for a fresh
    /// resolve).
    pub fn new(cache_root: PathBuf, locked: BTreeMap<String, LockedPackage>) -> Self {
        CacheFetcher {
            cache_root,
            locked,
            refreshed: HashSet::new(),
        }
    }

    /// Resolve a `path` dependency to its on-disk root.
    fn fetch_path(
        &self,
        name: &str,
        path: &str,
        parent_dir: &Path,
    ) -> Result<FetchedPackage, ResolveError> {
        let joined = parent_dir.join(path);
        let root = joined.canonicalize().map_err(|e| ResolveError::Fetch {
            name: name.to_string(),
            message: format!(
                "path dependency points at '{}', which could not be resolved: {e}",
                joined.display()
            ),
        })?;
        let manifest = read_package_manifest(name, &root)?;
        Ok(FetchedPackage {
            manifest,
            // The canonical root is both the package root and (for a path
            // source) the unification identity.
            source: PackageSource::Path { path: root.clone() },
            root,
        })
    }

    /// Fetch a git dependency into the cache and return its checkout.
    fn fetch_git(
        &mut self,
        name: &str,
        url: &str,
        reference: &GitRef,
    ) -> Result<FetchedPackage, ResolveError> {
        // Reuse the locked commit only when the URL *and* the requested ref
        // still match. A changed URL or ref (e.g. a bumped tag) falls through to
        // fresh resolution, so it surfaces as lock drift instead of silently
        // pinning the stale commit.
        let locked_sha = self
            .locked
            .get(name)
            .filter(|lp| lp.git() == Some(url) && lp.matches_ref(reference))
            .map(|lp| lp.rev().to_string());

        // `cache_root` and `refreshed` are distinct fields, so the simultaneous
        // shared/mutable borrows below are disjoint and accepted.
        let (root, sha) = materialize_git(
            &self.cache_root,
            url,
            reference,
            locked_sha.as_deref(),
            &mut self.refreshed,
        )
        .map_err(|message| ResolveError::Fetch {
            name: name.to_string(),
            message,
        })?;
        let manifest = read_package_manifest(name, &root)?;
        Ok(FetchedPackage {
            manifest,
            root,
            source: PackageSource::Git {
                url: url.to_string(),
                git_ref: reference.clone(),
                rev: sha,
            },
        })
    }
}

impl ManifestProvider for CacheFetcher {
    fn fetch(
        &mut self,
        name: &str,
        dep: &Dependency,
        parent_dir: &Path,
    ) -> Result<FetchedPackage, ResolveError> {
        match dep {
            Dependency::Path { path } => self.fetch_path(name, path, parent_dir),
            Dependency::Git { url, reference } => self.fetch_git(name, url, reference),
        }
    }
}

/// Read and validate a dependency's `phoenix.toml`, building the
/// [`PackageManifest`] the resolver consumes.
fn read_package_manifest(name: &str, root: &Path) -> Result<PackageManifest, ResolveError> {
    let manifest_path = root.join("phoenix.toml");
    if !manifest_path.is_file() {
        return Err(ResolveError::Fetch {
            name: name.to_string(),
            message: format!(
                "dependency has no phoenix.toml at '{}'",
                manifest_path.display()
            ),
        });
    }
    // `load_file` parses *and* validates ([package] metadata + every dependency
    // source), so a malformed dependency manifest fails here with a precise
    // message rather than surfacing as a confusing downstream error.
    let config = PhoenixConfig::load_file(&manifest_path).map_err(|e| ResolveError::Fetch {
        name: name.to_string(),
        message: e.to_string(),
    })?;
    // Read the dependency table while `config` is still whole, then move
    // `package` out of it.
    let dependencies = config
        .dependencies()
        .map_err(|error| ResolveError::Manifest {
            name: name.to_string(),
            error,
        })?;
    let package = config.package.ok_or_else(|| ResolveError::Fetch {
        name: name.to_string(),
        message: format!(
            "dependency's phoenix.toml has no [package] section (at '{}'); \
             a depend-able package must declare [package] name and version",
            manifest_path.display()
        ),
    })?;
    // The dependency key is the name this package is imported under and the key
    // it is locked by; if the fetched manifest declares a *different* package
    // name, the manifest is almost certainly misconfigured (there is no rename
    // syntax). Reject it loudly rather than silently resolving the foreign
    // package under the requested name.
    if package.name != name {
        return Err(ResolveError::Fetch {
            name: name.to_string(),
            message: format!(
                "dependency name mismatch: declared as `{name}` but the package at \
                 '{}' is named `{}`; rename the dependency to `{}` (Phoenix has no \
                 dependency-rename syntax)",
                manifest_path.display(),
                package.name,
                package.name
            ),
        });
    }
    // `version` already passed `validate()` inside `load_file`, so this reparse
    // is effectively infallible; it exists only to recover the typed value the
    // resolver compares with. The error arm is kept defensively (unreachable).
    let version = semver::Version::parse(&package.version).map_err(|e| ResolveError::Manifest {
        name: name.to_string(),
        error: crate::manifest::ManifestError::InvalidPackageVersion(
            package.version.clone(),
            e.to_string(),
        ),
    })?;
    Ok(PackageManifest {
        name: package.name,
        version,
        dependencies,
    })
}

/// Ensure a worktree for `url` at the requested revision exists in the cache,
/// returning `(checkout_dir, resolved_sha)`.
///
/// When `locked_sha` is supplied and already checked out, this is fully offline.
/// Otherwise a `git/db/<slug>` clone is created/updated, the ref is resolved to
/// a commit, and a `git/checkouts/<slug>/<sha>` worktree is materialized.
///
/// `refreshed` tracks URLs already fetched this resolve so a repo reached via
/// several edges is cloned only once (see [`CacheFetcher::refreshed`]).
fn materialize_git(
    cache_root: &Path,
    url: &str,
    reference: &GitRef,
    locked_sha: Option<&str>,
    refreshed: &mut HashSet<String>,
) -> Result<(PathBuf, String), String> {
    // `locked_sha` is the one externally-controlled component joined into a
    // cache path (it comes straight from `phoenix.lock`). Reject anything that
    // isn't a bare hex object id *before* it is used as a directory name, so a
    // hand-edited or hostile lockfile can't escape the cache root via `..`.
    // (A resolved ref, by contrast, is always gix-produced hex.)
    if let Some(sha) = locked_sha
        && !is_hex_oid(sha)
    {
        return Err(format!(
            "phoenix.lock records an invalid commit id '{sha}' for '{url}' \
             (expected a hexadecimal git object id)"
        ));
    }

    // Fast path: a pinned commit already fully checked out needs no network.
    if let Some(sha) = locked_sha {
        let checkout = git_checkout_dir(cache_root, url, sha);
        if checkout_is_complete(&checkout) {
            return Ok((checkout, sha.to_string()));
        }
    }

    let db = git_db_dir(cache_root, url);
    // Re-clone at most once per URL per resolve; a later edge to the same repo
    // reopens the existing clone instead of wiping and re-fetching it.
    let repo = if refreshed.contains(url) && db.exists() {
        open_db(&db, url)?
    } else {
        let repo = ensure_db(&db, url).map_err(|e| format!("git fetch of '{url}' failed: {e}"))?;
        refreshed.insert(url.to_string());
        repo
    };

    let sha = match locked_sha {
        Some(s) => s.to_string(),
        None => resolve_ref(&repo, reference).map_err(|e| {
            format!(
                "could not resolve {} in '{url}': {e}",
                describe_ref(reference)
            )
        })?,
    };

    let checkout = git_checkout_dir(cache_root, url, &sha);
    if !checkout_is_complete(&checkout) {
        materialize_checkout(&repo, &checkout, &sha)
            .map_err(|e| format!("could not check out {sha} of '{url}': {e}"))?;
    }
    Ok((checkout, sha))
}

/// The completion-marker path beside a checkout dir (`<checkout>.ok`). The
/// checkout's final component is a full hex SHA (no `.`), so `with_extension`
/// appends rather than replaces, yielding a sibling that can't shadow another
/// commit's checkout dir.
fn checkout_ok_marker(checkout: &Path) -> PathBuf {
    checkout.with_extension(CHECKOUT_OK_EXT)
}

/// Whether `checkout` holds a fully materialized worktree (its completion marker
/// is present). A partial checkout left by an interrupted run lacks the marker
/// and is treated as absent, so it gets cleared and rebuilt rather than handed
/// back incomplete.
fn checkout_is_complete(checkout: &Path) -> bool {
    checkout_ok_marker(checkout).is_file()
}

/// Open an already-fetched bare clone for `url` without touching the network.
///
/// Used when an earlier edge in the same resolve already refreshed this URL's
/// clone, so a later edge can reuse it rather than wiping and re-fetching.
fn open_db(db: &Path, url: &str) -> Result<gix::Repository, String> {
    gix::open(db).map_err(|e| format!("could not open cached clone of '{url}': {e}"))
}

/// (Re)create the cached bare clone for `url`, fetching all of its refs.
///
/// The clone is rebuilt rather than incrementally fetched, so a newly published
/// ref (e.g. a freshly pushed tag) is visible on an unlocked re-resolve. This is
/// only reached off the offline fast path: a locked build whose commit is
/// already checked out never calls here, so reproducible builds stay
/// network-free. (A leftover partial clone is removed first so a fresh clone
/// isn't blocked by a non-empty directory.)
fn ensure_db(db: &Path, url: &str) -> Result<gix::Repository, String> {
    if db.exists() {
        std::fs::remove_dir_all(db).map_err(|e| {
            format!(
                "could not clear stale cache clone at '{}': {e}",
                db.display()
            )
        })?;
    }
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "could not create cache directory '{}': {e}",
                parent.display()
            )
        })?;
    }
    // Intentionally inert today: gix wants a cancellation flag, but Phoenix
    // doesn't yet wire one to a signal handler, so this never flips. A clone
    // interrupted by Ctrl-C is made safe by the cache layout instead — the bare
    // `db` is rebuilt wholesale on the next fetch, and a half-materialized
    // checkout is rejected by its missing completion marker (see
    // `materialize_checkout`). Wiring real interruption pairs with the cache
    // locking work tracked in known-issues.
    let interrupt = AtomicBool::new(false);
    let (repo, _outcome) = gix::prepare_clone_bare(url, db)
        .map_err(|e| e.to_string())?
        .fetch_only(gix::progress::Discard, &interrupt)
        .map_err(|e| e.to_string())?;
    Ok(repo)
}

/// Resolve a [`GitRef`] to a concrete commit SHA (hex) in the cached clone.
///
/// Each candidate revspec is peeled with `^{commit}` so an annotated tag
/// resolves to its commit. A branch is tried under both `refs/heads/*` and
/// `refs/remotes/origin/*` because where a clone records branches varies.
fn resolve_ref(repo: &gix::Repository, reference: &GitRef) -> Result<String, String> {
    let candidates: Vec<String> = match reference {
        GitRef::Tag(t) => vec![format!("refs/tags/{t}^{{commit}}")],
        GitRef::Branch(b) => vec![
            format!("refs/heads/{b}^{{commit}}"),
            format!("refs/remotes/origin/{b}^{{commit}}"),
        ],
        GitRef::Rev(r) => vec![format!("{r}^{{commit}}")],
        GitRef::DefaultBranch => vec!["HEAD^{commit}".to_string()],
    };
    // Accumulate every attempt's error rather than keeping only the last: a
    // branch tries both `refs/heads/*` and `refs/remotes/origin/*`, so reporting
    // all of them keeps the more informative failure from being masked by a
    // blander "not found" on the second candidate.
    let mut errors: Vec<String> = Vec::new();
    for spec in &candidates {
        match repo.rev_parse_single(gix::bstr::BStr::new(spec)) {
            Ok(id) => return Ok(id.detach().to_string()),
            Err(e) => errors.push(format!("`{spec}`: {e}")),
        }
    }
    // `candidates` is never empty (each arm yields ≥1 spec), so `errors` is
    // populated here; the fallback is defensive.
    Err(if errors.is_empty() {
        "no matching ref".to_string()
    } else {
        errors.join("; ")
    })
}

/// Whether `s` is a syntactically valid *full* git object id: exactly a
/// SHA-1 (40) or SHA-256 (64) run of hex digits. This both guarantees the value
/// is safe to use as a path component (no `/`, no `..`) and rejects an
/// obviously-corrupt lockfile `rev` (truncated, empty, or a stray short string)
/// before it is joined into a cache path. The only value gated here is a
/// lockfile's resolved `rev`, which Phoenix always writes as a full oid — an
/// abbreviated manifest `rev` is matched against the lock's full SHA separately
/// (see [`super::lock::LockedPackage::matches_ref`]) and never reaches this.
fn is_hex_oid(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A human-readable description of a ref for error messages.
fn describe_ref(reference: &GitRef) -> String {
    match reference {
        GitRef::Rev(r) => format!("rev '{r}'"),
        GitRef::Tag(t) => format!("tag '{t}'"),
        GitRef::Branch(b) => format!("branch '{b}'"),
        GitRef::DefaultBranch => "the default branch".to_string(),
    }
}

/// Materialize the files of commit `sha` from `repo` into `checkout`.
///
/// Only the package's source files are written — no `.git` — because the
/// resolver reads `phoenix.toml` and sources straight from `root`. The commit's
/// tree is turned into an index and checked out into the (empty) directory. The
/// completion marker ([`checkout_ok_marker`]) is written last, once the worktree
/// is complete, so an interrupted run leaves no checkout that
/// [`checkout_is_complete`] accepts.
///
/// The checkout holds untrusted third-party content, so a dependency repo could
/// contain a symlink pointing outside its own tree. That stays safe because the
/// module loader canonicalizes every resolved module path (resolving symlinks)
/// and rejects any that escapes the package root via `ensure_under_root` — the
/// same `EscapesRoot` guard, applied per-package, that protects a local project
/// (see `phoenix-modules`, test `escapes_root_via_symlink`). `gix` itself
/// rejects `..`/absolute tree entries during checkout, so nothing is written
/// outside `checkout` either.
fn materialize_checkout(repo: &gix::Repository, checkout: &Path, sha: &str) -> Result<(), String> {
    if let Some(parent) = checkout.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "could not create checkout directory '{}': {e}",
                parent.display()
            )
        })?;
    }
    // A leftover partial checkout (e.g. an interrupted previous run) must not
    // shadow a clean one — and if it can't be cleared, fail loudly rather than
    // checking out over foreign files (which `destination_is_initially_empty`
    // would then choke on with a more confusing error). Clear any stale marker
    // first so a crash before the worktree is rebuilt can't leave the marker
    // standing over an empty/partial checkout.
    let marker = checkout_ok_marker(checkout);
    if marker.exists() {
        std::fs::remove_file(&marker).map_err(|e| {
            format!(
                "could not clear stale checkout marker at '{}': {e}",
                marker.display()
            )
        })?;
    }
    if checkout.exists() {
        std::fs::remove_dir_all(checkout).map_err(|e| {
            format!(
                "could not clear stale checkout at '{}': {e}",
                checkout.display()
            )
        })?;
    }
    std::fs::create_dir_all(checkout).map_err(|e| e.to_string())?;

    // Each step names what it was doing so a corrupt/missing object is
    // diagnosable; the caller further prefixes "could not check out {sha} …".
    let oid =
        gix::ObjectId::from_hex(sha.as_bytes()).map_err(|e| format!("malformed commit id: {e}"))?;
    let tree_id = repo
        .find_object(oid)
        .map_err(|e| format!("commit object not found in clone: {e}"))?
        .try_into_commit()
        .map_err(|e| format!("object is not a commit: {e}"))?
        .tree_id()
        .map_err(|e| format!("could not read the commit's tree: {e}"))?
        .detach();
    let mut index = repo
        .index_from_tree(&tree_id)
        .map_err(|e| format!("could not build an index from the tree: {e}"))?;

    // Inert cancellation flag (see `ensure_db`): an interrupted checkout is made
    // safe by the completion marker written last, not by flipping this.
    let interrupt = AtomicBool::new(false);
    let opts = gix::worktree::state::checkout::Options {
        destination_is_initially_empty: true,
        ..Default::default()
    };
    gix::worktree::state::checkout(
        &mut index,
        checkout,
        repo.objects.clone(),
        &gix::progress::Discard,
        &gix::progress::Discard,
        &interrupt,
        opts,
    )
    .map_err(|e| format!("could not write worktree files: {e}"))?;

    // Marks the checkout complete; checked by `checkout_is_complete` before any
    // future resolve reuses it. Written last (and beside the checkout dir, not
    // inside it) so a crash mid-checkout can't leave a marked-but-partial
    // worktree.
    std::fs::write(&marker, []).map_err(|e| {
        format!(
            "could not finalize checkout at '{}': {e}",
            checkout.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_hex_oid_accepts_full_oids_and_rejects_everything_else() {
        assert!(is_hex_oid(&"a".repeat(40))); // full SHA-1 length
        assert!(is_hex_oid(&"0123456789abcdefABCDEF".repeat(3)[..40])); // 40 mixed-case hex
        assert!(is_hex_oid(&"a".repeat(64))); // full SHA-256 length
        assert!(!is_hex_oid("")); // empty
        assert!(!is_hex_oid("abc123")); // valid hex but too short to be a full oid
        assert!(!is_hex_oid(&"a".repeat(39))); // truncated SHA-1
        assert!(!is_hex_oid(&"z".repeat(40))); // right length, non-hex letters
        assert!(!is_hex_oid("abc/def")); // path separator
        assert!(!is_hex_oid("../../etc")); // traversal
        assert!(!is_hex_oid(&"a".repeat(65))); // longer than any oid
    }

    #[test]
    fn materialize_git_rejects_path_traversal_in_locked_sha() {
        // A hand-edited or hostile lockfile `rev` that isn't a bare hex object id
        // must be rejected *before* it is joined into a cache path, so it cannot
        // escape the cache root via `..`. The guard fires before any clone, so
        // this never touches the network.
        let tmp = tempfile::tempdir().unwrap();
        let mut refreshed = HashSet::new();
        let err = materialize_git(
            tmp.path(),
            "https://example.com/x.git",
            &GitRef::DefaultBranch,
            Some("../../../../etc/passwd"),
            &mut refreshed,
        )
        .unwrap_err();
        assert!(err.contains("invalid commit id"), "got: {err}");
        // Nothing was cloned: the guard rejected before touching the cache.
        assert!(!tmp.path().join("git").exists());
    }

    #[test]
    fn describe_ref_messages() {
        assert!(describe_ref(&GitRef::Tag("v1".into())).contains("tag 'v1'"));
        assert!(describe_ref(&GitRef::DefaultBranch).contains("default branch"));
    }

    #[test]
    fn path_dependency_missing_manifest_errors() {
        // A path pointing at a directory without a phoenix.toml fails with a
        // clear message rather than a panic. (The git path needs a real repo
        // and is covered by the integration test.)
        let tmp = tempfile::tempdir().unwrap();
        let dep_dir = tmp.path().join("notapkg");
        std::fs::create_dir_all(&dep_dir).unwrap();
        let fetcher = CacheFetcher::new(tmp.path().join("cache"), BTreeMap::new());
        let err = fetcher
            .fetch_path("notapkg", "notapkg", tmp.path())
            .unwrap_err();
        match err {
            ResolveError::Fetch { name, message } => {
                assert_eq!(name, "notapkg");
                assert!(message.contains("phoenix.toml"), "got: {message}");
            }
            other => panic!("expected Fetch error, got {other}"),
        }
    }

    #[test]
    fn path_dependency_name_mismatch_errors() {
        // A path dependency whose phoenix.toml declares a name different from
        // the dependency key is rejected, not silently resolved under the
        // requested name (there is no rename syntax).
        let tmp = tempfile::tempdir().unwrap();
        let dep_dir = tmp.path().join("util");
        std::fs::create_dir_all(&dep_dir).unwrap();
        std::fs::write(
            dep_dir.join("phoenix.toml"),
            "[package]\nname = \"actuallycore\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let fetcher = CacheFetcher::new(tmp.path().join("cache"), BTreeMap::new());
        let err = fetcher.fetch_path("util", "util", tmp.path()).unwrap_err();
        match err {
            ResolveError::Fetch { name, message } => {
                assert_eq!(name, "util");
                assert!(message.contains("name mismatch"), "got: {message}");
                assert!(message.contains("actuallycore"), "got: {message}");
            }
            other => panic!("expected Fetch error, got {other}"),
        }
    }

    #[test]
    fn path_dependency_resolves_in_place() {
        // A well-formed path dependency resolves to its canonical root with no
        // rev (path sources are never SHA-pinned).
        let tmp = tempfile::tempdir().unwrap();
        let dep_dir = tmp.path().join("util");
        std::fs::create_dir_all(&dep_dir).unwrap();
        std::fs::write(
            dep_dir.join("phoenix.toml"),
            "[package]\nname = \"util\"\nversion = \"0.3.0\"\n",
        )
        .unwrap();
        let fetcher = CacheFetcher::new(tmp.path().join("cache"), BTreeMap::new());
        let fetched = fetcher.fetch_path("util", "util", tmp.path()).unwrap();
        assert_eq!(fetched.manifest.version.to_string(), "0.3.0");
        assert_eq!(fetched.manifest.name, "util");
        assert!(
            matches!(fetched.source, PackageSource::Path { .. }),
            "a path dependency must carry an explicit Path source"
        );
        assert_eq!(fetched.root, dep_dir.canonicalize().unwrap());
    }
}
