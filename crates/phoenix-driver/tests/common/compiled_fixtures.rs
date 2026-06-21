//! Shared helpers for integration tests that build a Phoenix fixture
//! into a temporary native binary and exec it. Cross-platform; the
//! tests themselves may be `#[cfg(target_os = "linux")]` for further
//! restrictions (e.g. valgrind / `RLIMIT_AS`), but the build pipeline
//! and temp-file plumbing live here.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

pub fn phoenix_bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_phoenix"));
    cmd.current_dir(workspace_root());
    cmd
}

/// Owns a freshly-built fixture binary; deletes it on drop so a panic
/// mid-test doesn't litter `/tmp`.
pub struct TempBin(pub PathBuf);

impl Drop for TempBin {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A temp *directory* that removes itself on drop, so a failing assertion
/// unwinds without leaking it. `Deref<Target = Path>` lets callers keep using
/// `dir.join(...)` as if it were a `PathBuf`. The name is disambiguated by the
/// current PID (across concurrent `cargo test` processes) and the caller's
/// `name` (within a single run's parallel threads) so neither collides.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(name: &str) -> TempDir {
        let path =
            std::env::temp_dir().join(format!("phoenix_test_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}

impl std::ops::Deref for TempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Build `tests/fixtures/{fixture}` into a temp binary whose name is
/// disambiguated by `bin_prefix` and the current PID so parallel test
/// runs don't collide. Panics with the build's stderr on failure.
pub fn build_fixture(fixture: &str, bin_prefix: &str) -> TempBin {
    let path = format!("tests/fixtures/{fixture}");
    let bin_name = format!(
        "{bin_prefix}_{}_{}",
        std::process::id(),
        fixture.trim_end_matches(".phx")
    );
    let bin = TempBin(std::env::temp_dir().join(&bin_name));
    let _ = std::fs::remove_file(&bin.0);

    let build = phoenix_bin()
        .args(["build", &path, "-o"])
        .arg(&bin.0)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `phoenix build {path}`: {e}"));
    if !build.status.success() {
        panic!(
            "`phoenix build {path}` exited non-zero\n  stdout: {}\n  stderr: {}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );
    }
    bin
}
