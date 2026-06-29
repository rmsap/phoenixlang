//! End-to-end CLI tests for cross-package imports.
//!
//! Drives the compiled `phoenix` binary against the `multi_package` fixture: an
//! `app` project that depends (via a local `path` source) on a `greet` package.
//! `greet` has its own internal `util` module whose name collides with the
//! app's local `util` — a successful check/run proves package-qualified
//! identity keeps them distinct and that `build`/`run`/`check` resolve
//! dependencies before compiling.

use std::path::PathBuf;
use std::process::Command;

mod common;
use common::skip_if_no_runtime_lib;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// A `phoenix` command rooted at the workspace, with `PHOENIX_HOME` pointed at a
/// throwaway dir so the test never touches the real `~/.phoenix` (the fixture
/// uses only path dependencies, so nothing is actually fetched).
fn phoenix_bin(home: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_phoenix"));
    cmd.current_dir(workspace_root());
    cmd.env("PHOENIX_HOME", home);
    cmd
}

const APP_MAIN: &str = "tests/fixtures/multi_package/app/main.phx";

#[test]
fn check_succeeds_across_package_boundary() {
    let home = tempfile::tempdir().unwrap();
    let output = phoenix_bin(home.path())
        .args(["check", APP_MAIN])
        .output()
        .expect("failed to run phoenix");
    assert!(
        output.status.success(),
        "expected cross-package `check` to succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn check_succeeds_with_bare_filename_from_project_dir() {
    // Invoking `phoenix check main.phx` from *inside* the project directory
    // exercises the bare-filename path: `Path::parent()` yields `Some("")`, which
    // `build_package_resolution` must treat as the cwd so manifest discovery still
    // finds `app/phoenix.toml` and resolves the `greet` path dependency. A failure
    // here would mean the empty-parent branch regressed and the dependency went
    // undiscovered (the local import would then fail to find `greet`).
    let home = tempfile::tempdir().unwrap();
    let app_dir = workspace_root().join("tests/fixtures/multi_package/app");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_phoenix"));
    cmd.current_dir(&app_dir);
    cmd.env("PHOENIX_HOME", home.path());
    let output = cmd
        .args(["check", "main.phx"])
        .output()
        .expect("failed to run phoenix");
    assert!(
        output.status.success(),
        "expected bare-filename `check` from the project dir to succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn check_discovers_manifest_from_entry_subdirectory() {
    // The entry file sits in a subdirectory *below* its `phoenix.toml`
    // (`app/src/main.phx` under `app/phoenix.toml`). This exercises the
    // walk-*up* in manifest discovery — more than the immediate parent — while a
    // dependency is declared, and the deliberate dual-root behavior: manifest /
    // dependency resolution is rooted at `app/` (so `../dep` resolves and the
    // lockfile would land there), while the entry package's own local imports
    // resolve relative to `app/src/` (the entry file's directory). A local
    // `import helper` must find `app/src/helper.phx`, and `import dep` must
    // dispatch cross-package to the declared path dependency.
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    let root = proj.path();

    // A local path dependency the app declares.
    std::fs::create_dir_all(root.join("dep")).unwrap();
    std::fs::write(
        root.join("dep/phoenix.toml"),
        "[package]\nname = \"dep\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("dep/mod.phx"),
        "public function value() -> String { \"x\" }\n",
    )
    .unwrap();

    // The app: manifest at the project root, entry one directory deeper, plus a
    // sibling local module the entry imports.
    std::fs::create_dir_all(root.join("app/src")).unwrap();
    std::fs::write(
        root.join("app/phoenix.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\ndep = { path = \"../dep\" }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/src/helper.phx"),
        "public function local() -> String { \"local\" }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/src/main.phx"),
        "import dep { value }\nimport helper { local }\n\
         function main() {\n  print(value())\n  print(local())\n}\n",
    )
    .unwrap();

    let main = root.join("app/src/main.phx");
    let output = phoenix_bin(home.path())
        .args(["check", main.to_str().unwrap()])
        .output()
        .expect("failed to run phoenix");
    assert!(
        output.status.success(),
        "expected manifest discovery to walk up from the entry subdirectory and \
         resolve both the path dependency and the local sibling module; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn run_executes_cross_package_program() {
    let home = tempfile::tempdir().unwrap();
    let output = phoenix_bin(home.path())
        .args(["run", APP_MAIN])
        .output()
        .expect("failed to run phoenix");
    assert!(
        output.status.success(),
        "expected cross-package `run` to succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // `greeting()` resolves through greet.util.libValue → "42"; the app's own
    // util.localValue → "1". Both modules are named `util` but never collide.
    // Assert on whole trimmed lines rather than substrings of the raw output so
    // the two values can't satisfy the check by accident (a substring search for
    // "1" would also match inside "42").
    let lines: Vec<&str> = stdout.lines().map(str::trim).collect();
    assert!(
        lines.contains(&"42") && lines.contains(&"1"),
        "expected a line from each package (`42` and `1`); got stdout:\n{stdout}"
    );
}

#[test]
fn build_compiles_cross_package_program() {
    // `build` takes the same `parse_resolve_check` path as `check`/`run`, so a
    // successful native compile proves the cross-package plumbing reaches the
    // build command too: an unresolved `import greet` would fail before codegen.
    // Gated on the runtime lib (the link step needs `libphoenix_runtime.a`),
    // matching `build_cli.rs`.
    if skip_if_no_runtime_lib("build_compiles_cross_package_program") {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let out_dir = tempfile::tempdir().unwrap();
    let bin = out_dir
        .path()
        .join(format!("app{}", std::env::consts::EXE_SUFFIX));
    let build = phoenix_bin(home.path())
        .args(["build", APP_MAIN, "--target", "native", "-o"])
        .arg(&bin)
        .output()
        .expect("failed to run phoenix");
    assert!(
        build.status.success(),
        "expected cross-package `build` to succeed; stderr:\n{}",
        String::from_utf8_lossy(&build.stderr),
    );
    assert!(
        bin.exists(),
        "expected a compiled binary at {}",
        bin.display()
    );

    // Running the artifact exercises both packages' `util` modules end to end.
    let run = Command::new(&bin).output().expect("run compiled binary");
    assert!(run.status.success(), "compiled binary exited non-zero");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let lines: Vec<&str> = stdout.lines().map(str::trim).collect();
    assert!(
        lines.contains(&"42") && lines.contains(&"1"),
        "expected a line from each package (`42` and `1`); got stdout:\n{stdout}"
    );
}

#[test]
fn locked_without_lockfile_is_fine_for_path_only_deps() {
    // A path-only project pins nothing, so `--locked` is trivially satisfied
    // even with no phoenix.lock present.
    let home = tempfile::tempdir().unwrap();
    let output = phoenix_bin(home.path())
        .args(["check", APP_MAIN, "--locked"])
        .output()
        .expect("failed to run phoenix");
    assert!(
        output.status.success(),
        "expected `--locked` check to succeed for a path-only project; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    // A path-only project pins nothing, so the run must not materialize a
    // lockfile in the fixture (which would be uncommitted churn under `--locked`).
    let lock = workspace_root().join("tests/fixtures/multi_package/app/phoenix.lock");
    assert!(
        !lock.exists(),
        "path-only `--locked` check must not write a phoenix.lock; found {}",
        lock.display(),
    );
}

#[test]
fn locked_with_stale_lockfile_drifts_and_errors() {
    // A path-only project resolves to an *empty* lockfile (path deps pin
    // nothing), so a committed phoenix.lock that still names a git package is
    // drift. Under `--locked` the driver must surface that and exit non-zero
    // rather than silently rewriting the lock. Built in a throwaway project so
    // the drift error path is exercised end-to-end without any git fetching.
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    let root = proj.path();

    // A local path dependency the app declares.
    std::fs::create_dir_all(root.join("dep")).unwrap();
    std::fs::write(
        root.join("dep/phoenix.toml"),
        "[package]\nname = \"dep\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("dep/mod.phx"),
        "public function value() -> String { \"x\" }\n",
    )
    .unwrap();

    // The app: declares the path dep and ships a stale lockfile naming a git
    // package the resolved graph no longer contains.
    std::fs::create_dir_all(root.join("app")).unwrap();
    std::fs::write(
        root.join("app/phoenix.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\ndep = { path = \"../dep\" }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/main.phx"),
        "import dep { value }\nfunction main() { print(value()) }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/phoenix.lock"),
        "version = 1\n\n[packages.fakegit]\nversion = \"1.0.0\"\n\
         git = \"https://example.com/fakegit.git\"\n\
         rev = \"0123456789abcdef0123456789abcdef01234567\"\n",
    )
    .unwrap();

    let main = root.join("app/main.phx");
    let output = phoenix_bin(home.path())
        .args(["check", main.to_str().unwrap(), "--locked"])
        .output()
        .expect("failed to run phoenix");
    assert!(
        !output.status.success(),
        "expected `--locked` check to fail on a stale lockfile; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("phoenix.lock is out of date"),
        "expected a lock-drift diagnostic; got stderr:\n{stderr}"
    );
}
