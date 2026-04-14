//! Runtime library discovery for linking compiled Phoenix binaries.
//!
//! Both the `phoenix build` CLI command and the Cranelift integration tests
//! need to locate `libphoenix_runtime.a` (or `phoenix_runtime.lib` on
//! Windows) for linking.  This module provides a shared, platform-aware
//! search function so the logic is not duplicated.

use std::path::Path;

/// Name of the Phoenix runtime static library on the current platform.
#[cfg(target_os = "windows")]
pub const RUNTIME_LIB_NAME: &str = "phoenix_runtime.lib";

/// Name of the Phoenix runtime static library on the current platform.
#[cfg(not(target_os = "windows"))]
pub const RUNTIME_LIB_NAME: &str = "libphoenix_runtime.a";

/// Find the directory containing the Phoenix runtime static library.
///
/// Searches in order:
///
/// 1. `$PHOENIX_RUNTIME_LIB` environment variable (trusted as-is)
/// 2. The directory containing the current executable (`target/debug/` in
///    cargo builds)
/// 3. The parent of that directory (`target/debug/deps/../` for cargo tests)
/// 4. `../lib` relative to the executable directory (standard install layout:
///    `bin/` + `../lib/`)
///
/// Returns `None` if the library cannot be found, allowing the caller to
/// produce an actionable error message.
pub fn find_runtime_lib() -> Option<String> {
    // 1. Environment variable — trust the user.
    if let Ok(dir) = std::env::var("PHOENIX_RUNTIME_LIB") {
        return Some(dir);
    }

    // 2–4. Search relative to the current executable.
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;

    // exe_dir itself (cargo build: target/debug/)
    if exe_dir.join(RUNTIME_LIB_NAME).exists() {
        return Some(dir_to_string(exe_dir));
    }

    // exe_dir/.. (cargo test: target/debug/deps/../ = target/debug/)
    if let Some(parent) = exe_dir.parent() {
        if parent.join(RUNTIME_LIB_NAME).exists() {
            return Some(dir_to_string(parent));
        }

        // parent/lib (standard install: bin/../lib/)
        let lib_dir = parent.join("lib");
        if lib_dir.join(RUNTIME_LIB_NAME).exists() {
            return Some(dir_to_string(&lib_dir));
        }
    }

    None
}

fn dir_to_string(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The library name must match the platform
    /// convention.  On Unix the static lib is `libphoenix_runtime.a`; on
    /// Windows it is `phoenix_runtime.lib`.
    #[test]
    fn runtime_lib_name_matches_platform() {
        #[cfg(target_os = "windows")]
        assert_eq!(RUNTIME_LIB_NAME, "phoenix_runtime.lib");

        #[cfg(not(target_os = "windows"))]
        assert_eq!(RUNTIME_LIB_NAME, "libphoenix_runtime.a");
    }

    /// `find_runtime_lib` should succeed in the cargo test environment
    /// because `cargo build -p phoenix-runtime` produces the `.a` in
    /// `target/debug/`.
    #[test]
    fn find_runtime_lib_succeeds_in_cargo_test() {
        let dir = find_runtime_lib();
        assert!(
            dir.is_some(),
            "should find runtime lib in cargo test environment"
        );
        let dir = dir.unwrap();
        let path = std::path::Path::new(&dir).join(RUNTIME_LIB_NAME);
        assert!(
            path.exists(),
            "runtime lib should exist at {}",
            path.display()
        );
    }

    /// `find_runtime_lib` should respect the `$PHOENIX_RUNTIME_LIB`
    /// environment variable as the highest-priority search path.
    #[test]
    fn find_runtime_lib_respects_env_var() {
        let key = "PHOENIX_RUNTIME_LIB";
        // Temporarily set the env var to a custom value.
        // SAFETY: this test is the only writer to this specific env var
        // and cargo test runs each test in its own thread but we accept
        // the race here since the key is test-specific.
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, "/custom/runtime/dir") };
        let result = find_runtime_lib();
        // Restore.
        match prev {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
        assert_eq!(result.as_deref(), Some("/custom/runtime/dir"));
    }
}
