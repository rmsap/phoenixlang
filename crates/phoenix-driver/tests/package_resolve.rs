//! Integration tests for the package manager's fetch + lockfile layer.
//!
//! These build throwaway *local* git repositories with the `git` CLI and
//! resolve against them through [`phoenix_driver::deps::resolve_project`], so
//! the cache-backed git fetcher, the `phoenix.lock` machinery, and `--locked`
//! drift detection are all exercised end-to-end with no network access. (The
//! production fetcher itself uses `gix`, not the CLI; `git` is only the fixture
//! builder here, and is always present in this repo's dev/CI environment.)

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use phoenix_driver::deps::resolve_project;
use phoenix_driver::manifest::{Dependency, GitRef};

/// Run a `git` subcommand in `dir`, asserting success. Author/committer identity
/// is supplied via env so commits work without relying on global git config.
fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Commit every `(name, contents)` file into a git repo at `dir`, returning the
/// commit SHA. Initializes the repo on first use; commits on top of HEAD after.
fn commit_files(dir: &Path, message: &str, files: &[(&str, &str)]) -> String {
    if !dir.join(".git").exists() {
        git(dir, &["-c", "init.defaultBranch=main", "init"]);
    }
    for (name, contents) in files {
        std::fs::write(dir.join(name), contents).expect("write file");
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-m", message]);
    let out = git(dir, &["rev-parse", "HEAD"]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Tag `sha` as `tag` in the repo at `dir`.
fn tag_commit(dir: &Path, tag: &str, sha: &str) {
    git(dir, &["tag", "-f", tag, sha]);
}

/// A minimal depend-able package source: `[package]` + a public function.
///
/// The source file is always named `greet.phx`, independent of `name` — it's a
/// fixed fixture detail, so a test using this for a package called `core` still
/// finds its source at `core_root/greet.phx`.
fn package_files(name: &str, version: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "phoenix.toml",
            format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n"),
        ),
        (
            "greet.phx",
            "public function greet() -> String { \"hi\" }\n".to_string(),
        ),
    ]
}

fn write_files(dir: &Path, files: &[(&'static str, String)]) {
    for (name, contents) in files {
        std::fs::write(dir.join(name), contents).expect("write");
    }
}

fn git_dep_ref(url: &Path, reference: GitRef) -> BTreeMap<String, Dependency> {
    let mut deps = BTreeMap::new();
    deps.insert(
        "greet".to_string(),
        Dependency::Git {
            url: url.to_string_lossy().into_owned(),
            reference,
        },
    );
    deps
}

fn git_dep(url: &Path, tag: &str) -> BTreeMap<String, Dependency> {
    git_dep_ref(url, GitRef::Tag(tag.to_string()))
}

/// The current branch name of the repo at `dir` (the default branch `init`
/// created — pinned to `main` by [`commit_files`]).
fn current_branch(dir: &Path) -> String {
    let out = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn git_dependency_fetches_and_writes_lockfile() {
    let repo_dir = tempfile::tempdir().unwrap();
    let files = package_files("greet", "1.0.0");
    let sha = commit_files(
        repo_dir.path(),
        "v1",
        &files
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );
    tag_commit(repo_dir.path(), "v1.0.0", &sha);

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let deps = git_dep(repo_dir.path(), "v1.0.0");

    let res = resolve_project(proj.path(), &deps, Some(cache.path()), false).expect("resolve");

    // The package resolved to the tagged commit, with its files on disk.
    let greet = &res.graph.packages["greet"];
    assert_eq!(greet.version.to_string(), "1.0.0");
    assert_eq!(greet.rev(), Some(sha.as_str()));
    assert!(greet.root.join("greet.phx").is_file());

    // A lockfile was written beside the manifest, pinning the commit.
    assert!(res.lock_changed);
    let lock_path = proj.path().join("phoenix.lock");
    assert!(lock_path.is_file());
    let lock_text = std::fs::read_to_string(&lock_path).unwrap();
    assert!(lock_text.contains("[packages.greet]"), "{lock_text}");
    assert!(
        lock_text.contains(&format!("rev = \"{sha}\"")),
        "{lock_text}"
    );
}

#[test]
fn git_dependency_on_branch_resolves_to_branch_head() {
    let repo_dir = tempfile::tempdir().unwrap();
    let files = package_files("greet", "1.0.0");
    let sha = commit_files(
        repo_dir.path(),
        "v1",
        &files
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let deps = git_dep_ref(
        repo_dir.path(),
        GitRef::Branch(current_branch(repo_dir.path())),
    );

    let res =
        resolve_project(proj.path(), &deps, Some(cache.path()), false).expect("resolve branch");
    let greet = &res.graph.packages["greet"];
    assert_eq!(greet.rev(), Some(sha.as_str()));
    assert!(greet.root.join("greet.phx").is_file());

    // A branch dep records the requested branch as the lock's ref key.
    let lock_text = std::fs::read_to_string(proj.path().join("phoenix.lock")).unwrap();
    assert!(lock_text.contains("branch = "), "{lock_text}");
    assert!(
        lock_text.contains(&format!("rev = \"{sha}\"")),
        "{lock_text}"
    );
}

#[test]
fn git_dependency_default_branch_resolves_to_head() {
    let repo_dir = tempfile::tempdir().unwrap();
    let files = package_files("greet", "1.0.0");
    let sha = commit_files(
        repo_dir.path(),
        "v1",
        &files
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let deps = git_dep_ref(repo_dir.path(), GitRef::DefaultBranch);

    let res = resolve_project(proj.path(), &deps, Some(cache.path()), false)
        .expect("resolve default branch");
    let greet = &res.graph.packages["greet"];
    assert_eq!(greet.rev(), Some(sha.as_str()));
    assert!(greet.root.join("greet.phx").is_file());

    // A default-branch dep records no tag/branch ref key — the commit pins it.
    let lock_text = std::fs::read_to_string(proj.path().join("phoenix.lock")).unwrap();
    assert!(!lock_text.contains("tag = "), "{lock_text}");
    assert!(!lock_text.contains("branch = "), "{lock_text}");
    assert!(
        lock_text.contains(&format!("rev = \"{sha}\"")),
        "{lock_text}"
    );
}

#[test]
fn nonexistent_ref_errors_with_resolution_diagnostic() {
    // A manifest pinning a ref the upstream doesn't have (here a tag that was
    // never created) fails the `resolve_ref` step with a clear "could not
    // resolve" diagnostic naming the requested ref — not a panic and not a
    // blander downstream error. (The locked-but-missing-*commit* case fails
    // later, at checkout; this is the unlocked resolve-failure path.)
    let repo_dir = tempfile::tempdir().unwrap();
    let files = package_files("greet", "1.0.0");
    commit_files(
        repo_dir.path(),
        "v1",
        &files
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );
    // Deliberately no `tag_commit` — the requested tag does not exist.

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let deps = git_dep(repo_dir.path(), "v9.9.9");

    let err = resolve_project(proj.path(), &deps, Some(cache.path()), false)
        .expect_err("a nonexistent ref must error");
    let msg = err.to_string();
    assert!(msg.contains("could not resolve"), "got: {msg}");
    assert!(msg.contains("v9.9.9"), "error should name the ref: {msg}");
    // The failure is reported before any lockfile is written.
    assert!(!proj.path().join("phoenix.lock").exists());
}

#[test]
fn second_resolve_is_in_sync_and_does_not_rewrite() {
    let repo_dir = tempfile::tempdir().unwrap();
    let files = package_files("greet", "1.0.0");
    let sha = commit_files(
        repo_dir.path(),
        "v1",
        &files
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );
    tag_commit(repo_dir.path(), "v1.0.0", &sha);

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let deps = git_dep(repo_dir.path(), "v1.0.0");

    resolve_project(proj.path(), &deps, Some(cache.path()), false).expect("first resolve");
    // A second resolve (even with --locked) must see no drift and not rewrite.
    let res2 =
        resolve_project(proj.path(), &deps, Some(cache.path()), true).expect("locked resolve");
    assert!(
        !res2.lock_changed,
        "in-sync --locked resolve must not write"
    );
}

#[test]
fn reproducible_from_clean_cache_under_locked() {
    // The headline guarantee: with a committed lockfile, a wiped cache rebuilds
    // the exact pinned commit offline-from-clone, under --locked.
    let repo_dir = tempfile::tempdir().unwrap();
    let files = package_files("greet", "1.0.0");
    let sha = commit_files(
        repo_dir.path(),
        "v1",
        &files
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );
    tag_commit(repo_dir.path(), "v1.0.0", &sha);

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let deps = git_dep(repo_dir.path(), "v1.0.0");

    // Initial resolve writes the lock.
    resolve_project(proj.path(), &deps, Some(cache.path()), false).expect("first resolve");

    // Wipe the cache entirely — simulating a clean checkout on another machine.
    std::fs::remove_dir_all(cache.path()).unwrap();
    std::fs::create_dir_all(cache.path()).unwrap();

    // Under --locked, resolution rematerializes the pinned commit and succeeds.
    let res =
        resolve_project(proj.path(), &deps, Some(cache.path()), true).expect("locked rebuild");
    let greet = &res.graph.packages["greet"];
    assert_eq!(greet.rev(), Some(sha.as_str()));
    assert!(greet.root.join("greet.phx").is_file());
    assert!(
        !res.lock_changed,
        "--locked must not rewrite an in-sync lock"
    );
}

#[test]
fn locked_rebuild_is_offline_when_checkout_survives() {
    // The headline offline guarantee: with a committed lockfile and the per-commit
    // checkout still on disk, a `--locked` resolve reuses it without touching the
    // network — even if the bare `git/db` clone is gone. Deleting only `git/db`
    // (keeping `git/checkouts`) means re-cloning would fail to find the commit, so
    // a successful resolve proves the fast path never went to the clone.
    let repo_dir = tempfile::tempdir().unwrap();
    let files = package_files("greet", "1.0.0");
    let sha = commit_files(
        repo_dir.path(),
        "v1",
        &files
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );
    tag_commit(repo_dir.path(), "v1.0.0", &sha);

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let deps = git_dep(repo_dir.path(), "v1.0.0");

    // Initial resolve clones, checks out, and writes the lock.
    resolve_project(proj.path(), &deps, Some(cache.path()), false).expect("first resolve");

    // Drop the bare clone but keep the materialized checkout, then make the clone
    // unreachable by deleting the upstream repo entirely.
    std::fs::remove_dir_all(cache.path().join("git").join("db")).unwrap();
    drop(repo_dir);

    // Under --locked the pinned commit's surviving checkout is reused offline.
    let res =
        resolve_project(proj.path(), &deps, Some(cache.path()), true).expect("offline rebuild");
    let greet = &res.graph.packages["greet"];
    assert_eq!(greet.rev(), Some(sha.as_str()));
    assert!(greet.root.join("greet.phx").is_file());
    assert!(
        !res.lock_changed,
        "--locked must not rewrite an in-sync lock"
    );
}

#[test]
fn git_dependency_pinned_by_rev_resolves_and_records_rev_req() {
    // A dependency pinned to an explicit commit resolves to exactly that commit
    // and records `rev` (not tag/branch) as the lock's requested-ref key.
    let repo_dir = tempfile::tempdir().unwrap();
    let files = package_files("greet", "1.0.0");
    let sha = commit_files(
        repo_dir.path(),
        "v1",
        &files
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let deps = git_dep_ref(repo_dir.path(), GitRef::Rev(sha.clone()));

    let res = resolve_project(proj.path(), &deps, Some(cache.path()), false).expect("resolve rev");
    let greet = &res.graph.packages["greet"];
    assert_eq!(greet.rev(), Some(sha.as_str()));

    let lock_text = std::fs::read_to_string(proj.path().join("phoenix.lock")).unwrap();
    assert!(!lock_text.contains("tag = "), "{lock_text}");
    assert!(!lock_text.contains("branch = "), "{lock_text}");
    assert!(
        lock_text.contains(&format!("rev_req = \"{sha}\"")),
        "{lock_text}"
    );
    assert!(
        lock_text.contains(&format!("rev = \"{sha}\"")),
        "{lock_text}"
    );
}

#[test]
fn locked_rebuild_with_abbreviated_rev_reuses_checkout_offline() {
    // A dependency pinned by an *abbreviated* explicit rev, once locked, rebuilds
    // from the surviving per-commit checkout without touching the clone — the
    // case-insensitive-prefix `matches_ref` path exercised end-to-end rather than
    // only in a unit test. Deleting just `git/db` (keeping `git/checkouts`) means
    // re-cloning would fail to find the commit, so a successful `--locked` resolve
    // proves the abbreviated rev still matched the lock and the offline fast path
    // was taken.
    let repo_dir = tempfile::tempdir().unwrap();
    let files = package_files("greet", "1.0.0");
    let sha = commit_files(
        repo_dir.path(),
        "v1",
        &files
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    // Pin by a 10-char abbreviated prefix of the full 40-char commit SHA.
    let abbrev = sha[..10].to_string();
    let deps = git_dep_ref(repo_dir.path(), GitRef::Rev(abbrev));

    // Initial resolve clones, checks out the abbreviated rev's commit, writes the
    // lock (pinning the full resolved SHA).
    let first =
        resolve_project(proj.path(), &deps, Some(cache.path()), false).expect("first resolve");
    assert_eq!(first.graph.packages["greet"].rev(), Some(sha.as_str()));

    // Drop the bare clone but keep the materialized checkout, then make the clone
    // unreachable by deleting the upstream repo entirely.
    std::fs::remove_dir_all(cache.path().join("git").join("db")).unwrap();
    drop(repo_dir);

    // Under --locked the abbreviated rev still matches the pinned commit, so its
    // surviving checkout is reused offline (no clone needed).
    let res =
        resolve_project(proj.path(), &deps, Some(cache.path()), true).expect("offline rebuild");
    let greet = &res.graph.packages["greet"];
    assert_eq!(greet.rev(), Some(sha.as_str()));
    assert!(greet.root.join("greet.phx").is_file());
    assert!(
        !res.lock_changed,
        "--locked must not rewrite an in-sync lock"
    );
}

#[test]
fn locked_rev_pointing_at_missing_commit_errors_cleanly() {
    // A `phoenix.lock` hand-edited to a syntactically valid (bare hex) but
    // nonexistent commit must surface a clean fetch error when its checkout has
    // to be rebuilt — not a panic. The path-traversal guard only rejects *non*-hex
    // revs; a well-formed-but-absent SHA gets past it and fails at checkout.
    let repo_dir = tempfile::tempdir().unwrap();
    let files = package_files("greet", "1.0.0");
    let sha = commit_files(
        repo_dir.path(),
        "v1",
        &files
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );
    tag_commit(repo_dir.path(), "v1.0.0", &sha);

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let deps = git_dep(repo_dir.path(), "v1.0.0");

    // Initial resolve writes the lock pinning the real commit.
    resolve_project(proj.path(), &deps, Some(cache.path()), false).expect("first resolve");

    // Repoint the lock's `rev` at a bare-hex SHA that doesn't exist, and wipe the
    // cache so the offline fast path can't reuse the real checkout.
    let lock_path = proj.path().join("phoenix.lock");
    let missing = "d".repeat(40);
    let edited = std::fs::read_to_string(&lock_path)
        .unwrap()
        .replace(&sha, &missing);
    std::fs::write(&lock_path, edited).unwrap();
    std::fs::remove_dir_all(cache.path()).unwrap();
    std::fs::create_dir_all(cache.path()).unwrap();

    // Rebuilding the checkout for the missing commit fails with a clear message.
    let err = resolve_project(proj.path(), &deps, Some(cache.path()), false)
        .expect_err("missing commit must error");
    assert!(
        err.to_string().contains("could not check out"),
        "expected a checkout error, got: {err}"
    );
}

#[test]
fn changed_url_re_resolves_instead_of_reusing_locked_commit() {
    // A locked commit is reused only when the URL still matches. Re-pointing a
    // dependency at a different repo (even at the same version) must fall through
    // to fresh resolution against the new source, not silently reuse the stale
    // pinned commit.
    let repo_a = tempfile::tempdir().unwrap();
    let files_a = package_files("greet", "1.0.0");
    let sha_a = commit_files(
        repo_a.path(),
        "a",
        &files_a
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );
    tag_commit(repo_a.path(), "v1.0.0", &sha_a);

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();

    // Lock against repo A.
    let deps_a = git_dep(repo_a.path(), "v1.0.0");
    resolve_project(proj.path(), &deps_a, Some(cache.path()), false).expect("lock against A");

    // A distinct repo B at the same name/version but a different commit.
    let repo_b = tempfile::tempdir().unwrap();
    let mut files_b = package_files("greet", "1.0.0");
    files_b.push((
        "extra.phx",
        "public function extra() -> Int { 1 }\n".to_string(),
    ));
    let sha_b = commit_files(
        repo_b.path(),
        "b",
        &files_b
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );
    tag_commit(repo_b.path(), "v1.0.0", &sha_b);
    assert_ne!(sha_a, sha_b, "fixtures must differ");

    // Re-point at repo B and re-resolve (unlocked): the URL no longer matches the
    // lock, so the pinned commit from A is not reused.
    let deps_b = git_dep(repo_b.path(), "v1.0.0");
    let res =
        resolve_project(proj.path(), &deps_b, Some(cache.path()), false).expect("re-resolve B");
    let greet = &res.graph.packages["greet"];
    assert_eq!(greet.rev(), Some(sha_b.as_str()), "must resolve B's commit");
    assert!(
        res.lock_changed,
        "the lock must be rewritten for the new source"
    );
    let lock_text = std::fs::read_to_string(proj.path().join("phoenix.lock")).unwrap();
    assert!(
        lock_text.contains(&format!("rev = \"{sha_b}\"")),
        "{lock_text}"
    );
}

#[test]
fn locked_detects_drift_when_manifest_changes() {
    let repo_dir = tempfile::tempdir().unwrap();
    // First version, tagged v1.0.0.
    let v1 = package_files("greet", "1.0.0");
    let sha1 = commit_files(
        repo_dir.path(),
        "v1",
        &v1.iter().map(|(n, c)| (*n, c.as_str())).collect::<Vec<_>>(),
    );
    tag_commit(repo_dir.path(), "v1.0.0", &sha1);

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();

    // Lock against v1.0.0.
    let deps_v1 = git_dep(repo_dir.path(), "v1.0.0");
    resolve_project(proj.path(), &deps_v1, Some(cache.path()), false).expect("lock v1");

    // A new compatible release, tagged v1.1.0.
    let v11 = package_files("greet", "1.1.0");
    let sha2 = commit_files(
        repo_dir.path(),
        "v1.1",
        &v11.iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );
    tag_commit(repo_dir.path(), "v1.1.0", &sha2);

    // Manifest now wants v1.1.0 but the lock still pins v1.0.0 → drift error.
    let deps_v11 = git_dep(repo_dir.path(), "v1.1.0");
    let err = resolve_project(proj.path(), &deps_v11, Some(cache.path()), true)
        .expect_err("expected --locked drift");
    assert!(
        err.to_string().contains("out of date"),
        "expected drift message, got: {err}"
    );
}

#[test]
fn locked_detects_drift_on_ref_key_change_same_commit() {
    // Changing only the *kind* of requested ref — tag → branch — while it still
    // resolves to the same commit is drift: the lockfile recorded `tag`, the
    // manifest now asks for a `branch`, so a `--locked` build must refuse rather
    // than silently treat the differently-pinned source as in sync.
    let repo_dir = tempfile::tempdir().unwrap();
    let files = package_files("greet", "1.0.0");
    let sha = commit_files(
        repo_dir.path(),
        "v1",
        &files
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );
    // The tag and the default branch HEAD point at the same commit.
    tag_commit(repo_dir.path(), "v1.0.0", &sha);
    let branch = current_branch(repo_dir.path());

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();

    // Lock against the tag.
    let deps_tag = git_dep(repo_dir.path(), "v1.0.0");
    resolve_project(proj.path(), &deps_tag, Some(cache.path()), false).expect("lock against tag");

    // Re-point at the branch (same commit) under --locked → ref-key drift.
    let deps_branch = git_dep_ref(repo_dir.path(), GitRef::Branch(branch));
    let err = resolve_project(proj.path(), &deps_branch, Some(cache.path()), true)
        .expect_err("expected --locked ref-change drift");
    assert!(
        err.to_string().contains("out of date"),
        "expected drift message, got: {err}"
    );
}

#[test]
fn locked_branch_dep_reuses_pinned_commit_until_ref_unchanged() {
    // A moving ref (a branch) is pinned by the lockfile: once locked, an
    // unlocked re-resolve against the *same* branch reuses the pinned commit
    // and does not advance to the new branch HEAD or rewrite the lock. Only a
    // change of the requested ref re-resolves (covered elsewhere). This locks in
    // the "lockfile is authoritative for a moving ref" semantics.
    let repo_dir = tempfile::tempdir().unwrap();
    let files = package_files("greet", "1.0.0");
    let sha1 = commit_files(
        repo_dir.path(),
        "v1",
        &files
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );
    let branch = current_branch(repo_dir.path());

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let deps = git_dep_ref(repo_dir.path(), GitRef::Branch(branch));

    // Lock against the branch HEAD (sha1).
    let first =
        resolve_project(proj.path(), &deps, Some(cache.path()), false).expect("lock branch");
    assert_eq!(first.graph.packages["greet"].rev(), Some(sha1.as_str()));

    // Advance the branch HEAD to a new commit.
    let sha2 = commit_files(
        repo_dir.path(),
        "v2",
        &[(
            "greet.phx",
            "public function greet() -> String { \"yo\" }\n",
        )],
    );
    assert_ne!(sha1, sha2, "branch must have moved");

    // An unlocked re-resolve with the same branch ref reuses the pinned commit.
    let again =
        resolve_project(proj.path(), &deps, Some(cache.path()), false).expect("reuse pinned");
    assert_eq!(
        again.graph.packages["greet"].rev(),
        Some(sha1.as_str()),
        "must reuse the pinned commit, not advance to the new branch HEAD"
    );
    assert!(!again.lock_changed, "no ref change → no lock rewrite");
}

#[test]
fn transitive_git_diamond_resolves_shared_dep_once() {
    // app → a (git) → core (git)
    // app → b (git) → core (git)
    //
    // The shared `core` is reached via two edges in one resolve. This exercises
    // the cache fetcher's transitive path (reading a fetched package's own
    // `[dependencies]` from its checkout) and its multi-edge reuse path (the
    // second edge reopens the existing clone instead of re-cloning), and the
    // diamond must unify to a single resolved `core`.
    let core_repo = tempfile::tempdir().unwrap();
    let core_sha = commit_files(
        core_repo.path(),
        "core",
        &package_files("core", "1.0.0")
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );
    tag_commit(core_repo.path(), "v1.0.0", &core_sha);
    // Single-quoted TOML literal string: no escaping needed for OS paths
    // (including Windows backslashes).
    let core_url = core_repo.path().to_string_lossy().into_owned();

    // Two intermediate packages, each a git dep that itself depends on `core`.
    let a_repo = tempfile::tempdir().unwrap();
    let a_manifest = format!(
        "[package]\nname = \"a\"\nversion = \"1.0.0\"\n\n\
         [dependencies]\ncore = {{ git = '{core_url}', tag = \"v1.0.0\" }}\n"
    );
    let a_sha = commit_files(
        a_repo.path(),
        "a",
        &[
            ("phoenix.toml", a_manifest.as_str()),
            ("a.phx", "public function a() -> Int { 1 }\n"),
        ],
    );
    tag_commit(a_repo.path(), "v1.0.0", &a_sha);

    let b_repo = tempfile::tempdir().unwrap();
    let b_manifest = format!(
        "[package]\nname = \"b\"\nversion = \"1.0.0\"\n\n\
         [dependencies]\ncore = {{ git = '{core_url}', tag = \"v1.0.0\" }}\n"
    );
    let b_sha = commit_files(
        b_repo.path(),
        "b",
        &[
            ("phoenix.toml", b_manifest.as_str()),
            ("b.phx", "public function b() -> Int { 1 }\n"),
        ],
    );
    tag_commit(b_repo.path(), "v1.0.0", &b_sha);

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let mut deps = BTreeMap::new();
    deps.insert(
        "a".to_string(),
        Dependency::Git {
            url: a_repo.path().to_string_lossy().into_owned(),
            reference: GitRef::Tag("v1.0.0".into()),
        },
    );
    deps.insert(
        "b".to_string(),
        Dependency::Git {
            url: b_repo.path().to_string_lossy().into_owned(),
            reference: GitRef::Tag("v1.0.0".into()),
        },
    );

    let res =
        resolve_project(proj.path(), &deps, Some(cache.path()), false).expect("resolve diamond");

    // All three git packages resolved; `core` unified to one entry at the
    // tagged commit with its files materialized.
    let core = &res.graph.packages["core"];
    assert_eq!(core.version.to_string(), "1.0.0");
    assert_eq!(core.rev(), Some(core_sha.as_str()));
    assert!(core.root.join("greet.phx").is_file());
    assert!(res.graph.packages.contains_key("a"));
    assert!(res.graph.packages.contains_key("b"));

    // One bare clone per distinct git URL (a, b, core) — both edges to `core`
    // share its single slugged db directory rather than each cloning their own.
    let db = cache.path().join("git").join("db");
    let clones = std::fs::read_dir(&db).unwrap().count();
    assert_eq!(
        clones, 3,
        "expected one db clone per distinct git URL (a, b, core)"
    );

    // The lockfile pins all three git packages.
    let lock_text = std::fs::read_to_string(proj.path().join("phoenix.lock")).unwrap();
    for name in ["a", "b", "core"] {
        assert!(
            lock_text.contains(&format!("[packages.{name}]")),
            "{lock_text}"
        );
    }
}

#[test]
fn path_dependency_resolves_without_lockfile() {
    // A path dependency resolves in place and is never recorded in the lock.
    let dep_dir = tempfile::tempdir().unwrap();
    write_files(dep_dir.path(), &package_files("util", "0.2.0"));

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let mut deps = BTreeMap::new();
    deps.insert(
        "util".to_string(),
        Dependency::Path {
            path: dep_dir.path().to_string_lossy().into_owned(),
        },
    );

    let res =
        resolve_project(proj.path(), &deps, Some(cache.path()), false).expect("resolve path dep");
    let util = &res.graph.packages["util"];
    assert_eq!(util.version.to_string(), "0.2.0");
    assert!(util.rev().is_none());
    // No git deps → no lockfile.
    assert!(!res.lock_changed);
    assert!(!proj.path().join("phoenix.lock").exists());
}

#[test]
fn path_dep_with_transitive_git_dep_fetches_and_locks_only_the_git_dep() {
    // app → mid (path) → core (git)
    //
    // A local `path` dependency that itself declares a *git* dependency. This is
    // the mixed-source transitive shape the diamond (git→git) and the top-level
    // path test don't cover: `mid` resolves in place, the resolver reads its
    // `[dependencies]`, and `core` — reachable only transitively — is fetched
    // from git and pinned. The lockfile records the git `core` but never the
    // path `mid`.
    let core_repo = tempfile::tempdir().unwrap();
    let core_sha = commit_files(
        core_repo.path(),
        "core",
        &package_files("core", "1.0.0")
            .iter()
            .map(|(n, c)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
    );
    tag_commit(core_repo.path(), "v1.0.0", &core_sha);
    // Single-quoted TOML literal string: no escaping for OS paths (incl. Windows
    // backslashes).
    let core_url = core_repo.path().to_string_lossy().into_owned();

    // `mid` is a plain on-disk directory (a path dep, not a git repo) whose
    // manifest pulls in the git `core`.
    let mid_dir = tempfile::tempdir().unwrap();
    let mid_manifest = format!(
        "[package]\nname = \"mid\"\nversion = \"1.0.0\"\n\n\
         [dependencies]\ncore = {{ git = '{core_url}', tag = \"v1.0.0\" }}\n"
    );
    std::fs::write(mid_dir.path().join("phoenix.toml"), mid_manifest).unwrap();
    std::fs::write(
        mid_dir.path().join("mid.phx"),
        "public function mid() -> Int { 1 }\n",
    )
    .unwrap();

    let proj = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let mut deps = BTreeMap::new();
    deps.insert(
        "mid".to_string(),
        Dependency::Path {
            path: mid_dir.path().to_string_lossy().into_owned(),
        },
    );

    let res =
        resolve_project(proj.path(), &deps, Some(cache.path()), false).expect("resolve mixed");

    // `mid` resolved in place (no rev); `core` was fetched from git and pinned.
    let mid = &res.graph.packages["mid"];
    assert!(mid.rev().is_none(), "path dep is never SHA-pinned");
    let core = &res.graph.packages["core"];
    assert_eq!(core.rev(), Some(core_sha.as_str()));
    assert!(core.root.join("greet.phx").is_file());

    // The lockfile pins the transitively-reached git `core` but not the path
    // `mid`.
    assert!(res.lock_changed);
    let lock_text = std::fs::read_to_string(proj.path().join("phoenix.lock")).unwrap();
    assert!(lock_text.contains("[packages.core]"), "{lock_text}");
    assert!(!lock_text.contains("[packages.mid]"), "{lock_text}");
    assert!(
        lock_text.contains(&format!("rev = \"{core_sha}\"")),
        "{lock_text}"
    );
}
