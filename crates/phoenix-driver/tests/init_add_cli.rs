//! End-to-end CLI tests for `phoenix init` and `phoenix add`.
//!
//! Drives the compiled `phoenix` binary in throwaway tempdirs. Git-source tests
//! build a *local* git repo with the `git` CLI (always present in dev/CI), so
//! no network is touched. `PHOENIX_HOME` is pointed at a throwaway dir so the
//! real `~/.phoenix` cache is never touched.

use std::path::Path;
use std::process::Command;

/// A `phoenix` command rooted at `cwd`, with a throwaway `PHOENIX_HOME`.
fn phoenix(cwd: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_phoenix"));
    cmd.current_dir(cwd);
    cmd.env("PHOENIX_HOME", home);
    cmd
}

/// Run a `git` subcommand in `dir`, asserting success (identity via env so no
/// global git config is needed).
fn git(dir: &Path, args: &[&str]) {
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
}

/// Build a one-commit local git repo for a depend-able package `name`, tagged
/// `tag`, and return its path (kept alive by the returned `TempDir`).
fn make_git_package(name: &str, tag: &str) -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["-c", "init.defaultBranch=main", "init"]);
    std::fs::write(
        repo.path().join("phoenix.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"1.0.0\"\n"),
    )
    .unwrap();
    std::fs::write(
        repo.path().join("mod.phx"),
        "public function value() -> String { \"dep\" }\n",
    )
    .unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-m", "v1"]);
    git(repo.path(), &["tag", tag]);
    repo
}

#[test]
fn init_scaffolds_runnable_project() {
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let out = phoenix(proj.path(), home.path())
        .args(["init", "--name", "myapp"])
        .output()
        .expect("run phoenix init");
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let manifest = std::fs::read_to_string(proj.path().join("phoenix.toml")).unwrap();
    assert!(manifest.contains("name = \"myapp\""), "{manifest}");
    assert!(manifest.contains("version = \"0.1.0\""), "{manifest}");
    assert!(proj.path().join("main.phx").is_file());

    // The scaffolded entry actually runs.
    let run = phoenix(proj.path(), home.path())
        .args(["run", "main.phx"])
        .output()
        .expect("run phoenix run");
    assert!(
        run.status.success(),
        "run of scaffold failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("Hello, myapp!"),
        "stdout: {}",
        String::from_utf8_lossy(&run.stdout)
    );
}

#[test]
fn init_defaults_name_to_directory() {
    // No --name: the package name comes from the directory (sanitized).
    let parent = tempfile::tempdir().unwrap();
    let proj = parent.path().join("my proj");
    std::fs::create_dir(&proj).unwrap();
    let home = tempfile::tempdir().unwrap();
    let out = phoenix(&proj, home.path())
        .arg("init")
        .output()
        .expect("run phoenix init");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manifest = std::fs::read_to_string(proj.join("phoenix.toml")).unwrap();
    // "my proj" → sanitized to "my-proj".
    assert!(manifest.contains("name = \"my-proj\""), "{manifest}");
}

#[test]
fn init_refuses_to_overwrite_existing_manifest() {
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    phoenix(proj.path(), home.path())
        .args(["init", "--name", "a"])
        .output()
        .unwrap();
    let second = phoenix(proj.path(), home.path())
        .args(["init", "--name", "b"])
        .output()
        .unwrap();
    assert!(!second.status.success(), "second init must fail");
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already exists"),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
}

#[test]
fn init_keeps_existing_entry_file() {
    // A `main.phx` already present (but no manifest) must be left byte-for-byte
    // untouched while `phoenix.toml` is created.
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let existing = "function main() {\n  print(\"do not clobber me\")\n}\n";
    std::fs::write(proj.path().join("main.phx"), existing).unwrap();

    let out = phoenix(proj.path(), home.path())
        .args(["init", "--name", "app"])
        .output()
        .expect("run phoenix init");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(proj.path().join("phoenix.toml").is_file());
    // The entry file is unchanged, and the command says so.
    assert_eq!(
        std::fs::read_to_string(proj.path().join("main.phx")).unwrap(),
        existing
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Kept existing main.phx"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn add_path_dependency_edits_manifest_without_lockfile() {
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    phoenix(proj.path(), home.path())
        .args(["init", "--name", "app"])
        .output()
        .unwrap();
    // A sibling path-dependency package.
    let dep = proj.path().join("util");
    std::fs::create_dir(&dep).unwrap();
    std::fs::write(
        dep.join("phoenix.toml"),
        "[package]\nname = \"util\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    let out = phoenix(proj.path(), home.path())
        .args(["add", "util", "--path", "util"])
        .output()
        .expect("run phoenix add");
    assert!(
        out.status.success(),
        "add --path failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manifest = std::fs::read_to_string(proj.path().join("phoenix.toml")).unwrap();
    assert!(
        manifest.contains(r#"util = { path = "util" }"#),
        "{manifest}"
    );
    // Path deps are never locked, so no phoenix.lock should appear.
    assert!(!proj.path().join("phoenix.lock").exists());
}

#[test]
fn add_git_dependency_edits_manifest_and_writes_lock() {
    let repo = make_git_package("greet", "v1.0.0");
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    phoenix(proj.path(), home.path())
        .args(["init", "--name", "app"])
        .output()
        .unwrap();

    let out = phoenix(proj.path(), home.path())
        .args([
            "add",
            "greet",
            "--git",
            repo.path().to_str().unwrap(),
            "--tag",
            "v1.0.0",
        ])
        .output()
        .expect("run phoenix add --git");
    assert!(
        out.status.success(),
        "add --git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let manifest = std::fs::read_to_string(proj.path().join("phoenix.toml")).unwrap();
    assert!(manifest.contains("greet = { git ="), "{manifest}");
    assert!(manifest.contains(r#"tag = "v1.0.0""#), "{manifest}");

    // A git dep is locked: phoenix.lock exists and pins the commit.
    let lock = std::fs::read_to_string(proj.path().join("phoenix.lock"))
        .expect("phoenix.lock should be written for a git dependency");
    assert!(lock.contains("[packages.greet]"), "{lock}");
    assert!(lock.contains("rev = "), "{lock}");
}

#[test]
fn add_upserts_existing_dependency_and_refreshes_lock() {
    // Re-adding a dependency through the CLI overwrites its entry in place
    // rather than appending a duplicate, and the lockfile stays consistent.
    let repo = make_git_package("greet", "v1.0.0");
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    phoenix(proj.path(), home.path())
        .args(["init", "--name", "app"])
        .output()
        .unwrap();

    let url = repo.path().to_str().unwrap();
    // First add: pin the tag.
    let first = phoenix(proj.path(), home.path())
        .args(["add", "greet", "--git", url, "--tag", "v1.0.0"])
        .output()
        .expect("run first phoenix add");
    assert!(
        first.status.success(),
        "first add failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    // Second add of the same name: switch to tracking the branch instead.
    let second = phoenix(proj.path(), home.path())
        .args(["add", "greet", "--git", url, "--branch", "main"])
        .output()
        .expect("run second phoenix add");
    assert!(
        second.status.success(),
        "second add failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    let manifest = std::fs::read_to_string(proj.path().join("phoenix.toml")).unwrap();
    // Exactly one `greet` entry, now tracking the branch (the tag key is gone).
    assert_eq!(
        manifest.matches("greet = {").count(),
        1,
        "expected a single upserted entry: {manifest}"
    );
    assert!(manifest.contains(r#"branch = "main""#), "{manifest}");
    assert!(
        !manifest.contains("tag ="),
        "old tag ref must be gone: {manifest}"
    );

    // The lockfile still pins the git dependency after the upsert.
    let lock = std::fs::read_to_string(proj.path().join("phoenix.lock"))
        .expect("phoenix.lock should still exist after upsert");
    assert!(lock.contains("[packages.greet]"), "{lock}");
}

#[test]
fn add_second_git_dependency_updates_existing_lock() {
    // Adding a git dep to a project that already has a lockfile refreshes the
    // lock to pin *both* packages rather than clobbering the first.
    let repo_a = make_git_package("greet", "v1.0.0");
    let repo_b = make_git_package("farewell", "v1.0.0");
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    phoenix(proj.path(), home.path())
        .args(["init", "--name", "app"])
        .output()
        .unwrap();

    let add = |name: &str, url: &str| {
        let out = phoenix(proj.path(), home.path())
            .args(["add", name, "--git", url, "--tag", "v1.0.0"])
            .output()
            .expect("run phoenix add");
        assert!(
            out.status.success(),
            "add {name} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    add("greet", repo_a.path().to_str().unwrap());
    add("farewell", repo_b.path().to_str().unwrap());

    // Both entries are in the manifest.
    let manifest = std::fs::read_to_string(proj.path().join("phoenix.toml")).unwrap();
    assert!(manifest.contains("greet = { git ="), "{manifest}");
    assert!(manifest.contains("farewell = { git ="), "{manifest}");

    // The refreshed lock pins both packages, not just the second.
    let lock = std::fs::read_to_string(proj.path().join("phoenix.lock"))
        .expect("phoenix.lock should exist after two git adds");
    assert!(lock.contains("[packages.greet]"), "{lock}");
    assert!(lock.contains("[packages.farewell]"), "{lock}");
}

#[test]
fn add_bad_git_ref_rolls_back_manifest() {
    let repo = make_git_package("greet", "v1.0.0");
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    phoenix(proj.path(), home.path())
        .args(["init", "--name", "app"])
        .output()
        .unwrap();
    let before = std::fs::read_to_string(proj.path().join("phoenix.toml")).unwrap();

    let out = phoenix(proj.path(), home.path())
        .args([
            "add",
            "greet",
            "--git",
            repo.path().to_str().unwrap(),
            "--tag",
            "v9.9.9-does-not-exist",
        ])
        .output()
        .expect("run phoenix add --git");
    assert!(!out.status.success(), "add with a bad ref must fail");

    // Atomic: the manifest is restored byte-for-byte, and no lockfile is left.
    let after = std::fs::read_to_string(proj.path().join("phoenix.toml")).unwrap();
    assert_eq!(
        before, after,
        "manifest must be rolled back on resolve failure"
    );
    assert!(!proj.path().join("phoenix.lock").exists());
}

#[test]
fn transitive_git_via_path_dep_fetches_into_cache_not_project_tree() {
    // A git source reached *only* transitively — through a `path` dependency's
    // own `[dependencies]` — must still fetch into `$PHOENIX_HOME/cache`, never
    // into the project tree. Regression guard: deciding "needs a cache?" from
    // the project's *direct* deps alone (all `path` here) once mislocated the
    // cache to the project directory, cloning the transitive git dep under
    // `<project>/git/…` and violating the "never inside the project tree" rule.
    let leaf = make_git_package("leaf", "v1.0.0");

    // `mid`: a local path package that itself declares the git dep on `leaf`.
    let mid = tempfile::tempdir().unwrap();
    std::fs::write(
        mid.path().join("phoenix.toml"),
        // Single-quoted (literal) TOML string so a Windows path's backslashes
        // aren't interpreted as escapes.
        format!(
            "[package]\nname = \"mid\"\nversion = \"1.0.0\"\n\n\
             [dependencies]\nleaf = {{ git = '{}', tag = \"v1.0.0\" }}\n",
            leaf.path().display()
        ),
    )
    .unwrap();
    std::fs::write(
        mid.path().join("mod.phx"),
        "public function midValue() -> String { \"mid\" }\n",
    )
    .unwrap();

    // `app`: depends on `mid` only by local path (no direct git dep).
    let app = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        app.path().join("phoenix.toml"),
        format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\nmid = {{ path = '{}' }}\n",
            mid.path().display()
        ),
    )
    .unwrap();
    std::fs::write(
        app.path().join("main.phx"),
        "import mid { midValue }\nfunction main() { print(midValue()) }\n",
    )
    .unwrap();

    let out = phoenix(app.path(), home.path())
        .args(["check", "main.phx"])
        .output()
        .expect("run phoenix check");
    assert!(
        out.status.success(),
        "check with a transitive git dep should succeed; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The transitive git clone landed in the cache, not the project tree.
    assert!(
        home.path().join("cache").join("git").is_dir(),
        "transitive git dep should have been fetched into $PHOENIX_HOME/cache"
    );
    assert!(
        !app.path().join("git").exists(),
        "no git cache may be written inside the project tree"
    );
    assert!(
        !mid.path().join("git").exists(),
        "no git cache may be written inside a path dependency's tree either"
    );
}

#[test]
fn add_without_manifest_errors() {
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let out = phoenix(proj.path(), home.path())
        .args(["add", "util", "--path", "../util"])
        .output()
        .expect("run phoenix add");
    assert!(!out.status.success(), "add without a manifest must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("phoenix init"),
        "stderr should suggest `phoenix init`: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn add_conflicting_sources_errors() {
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    phoenix(proj.path(), home.path())
        .args(["init", "--name", "app"])
        .output()
        .unwrap();
    let out = phoenix(proj.path(), home.path())
        .args(["add", "x", "--git", "u", "--path", "p"])
        .output()
        .expect("run phoenix add");
    assert!(!out.status.success(), "git+path must conflict");
    // The manifest must be untouched (validation happens before any edit).
    let manifest = std::fs::read_to_string(proj.path().join("phoenix.toml")).unwrap();
    assert!(!manifest.contains("[dependencies]"), "{manifest}");
}
