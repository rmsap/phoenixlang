//! Runtime library discovery and executable linking for compiled Phoenix
//! binaries.
//!
//! Both the `phoenix build` CLI command and the Cranelift integration tests
//! need to locate `libphoenix_runtime.a` (or `phoenix_runtime.lib` on
//! Windows) and invoke the system linker. This module provides shared,
//! platform-aware helpers so the logic is not duplicated across the
//! driver, the benches, and the integration tests.

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

/// Platforms on which [`link_executable`] knows how to invoke `cc`,
/// each paired with the system libraries that get appended to the
/// linker command line.
///
/// This is the **single source of truth** for the supported-OS list:
/// [`link_executable`] consults it to dispatch flags, [`LinkError::UnsupportedPlatform`]'s
/// `Display` derives the "currently supported" list from it, and a
/// future Windows / FreeBSD entry only needs to be added here.
const SUPPORTED_PLATFORMS: &[(&str, &[&str])] = &[
    ("linux", &["-lpthread", "-ldl", "-lm"]),
    ("macos", &["-lpthread", "-lm"]),
];

/// Look up the system-library flags for the current target OS, or
/// produce an [`LinkError::UnsupportedPlatform`] naming it.
fn platform_link_args() -> Result<&'static [&'static str], LinkError> {
    let os = std::env::consts::OS;
    SUPPORTED_PLATFORMS
        .iter()
        .find(|(name, _)| *name == os)
        .map(|(_, args)| *args)
        .ok_or(LinkError::UnsupportedPlatform(os))
}

/// Failure modes for [`link_executable`]. Most variants represent
/// environmental problems (missing toolchain, missing runtime library,
/// unsupported host); [`LinkError::LinkerFailed`] additionally covers
/// the case where `cc` ran cleanly but rejected the input (e.g. a
/// malformed object). Callers typically translate the error into a
/// panic or `process::exit` with the `Display` message — a Phoenix
/// program can't compile around any of these.
#[derive(Debug)]
#[non_exhaustive]
pub enum LinkError {
    /// `find_runtime_lib` returned `None`. The runtime static library
    /// is missing from every search path; the user must build it or
    /// set `$PHOENIX_RUNTIME_LIB`.
    RuntimeLibNotFound,
    /// `cc` could not be spawned (PATH lookup or exec failure). The
    /// inner error is the OS-level cause.
    SpawnLinker(std::io::Error),
    /// `cc` ran to completion but exited non-zero.
    LinkerFailed(std::process::ExitStatus),
    /// The host platform is not in [`SUPPORTED_PLATFORMS`]. The
    /// inner string is the unsupported target-os string from
    /// `std::env::consts::OS`. Today this includes Windows and
    /// every BSD; see [`link_executable`] for context.
    UnsupportedPlatform(&'static str),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RuntimeLibNotFound => write!(
                f,
                "could not find {RUNTIME_LIB_NAME}; \
                 set $PHOENIX_RUNTIME_LIB to the directory containing it, \
                 reinstall Phoenix with the install script, or — for in-tree \
                 development — run `cargo build -p phoenix-runtime` first"
            ),
            Self::SpawnLinker(e) => write!(f, "could not invoke `cc`: {e} (install gcc or clang)"),
            Self::LinkerFailed(s) => write!(f, "linker exited with {s}"),
            Self::UnsupportedPlatform(os) => {
                write!(
                    f,
                    "linking compiled Phoenix binaries is not yet supported on {os}; \
                     currently supported: "
                )?;
                let mut first = true;
                for (name, _) in SUPPORTED_PLATFORMS {
                    if !first {
                        write!(f, ", ")?;
                    }
                    write!(f, "{name}")?;
                    first = false;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for LinkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SpawnLinker(e) => Some(e),
            _ => None,
        }
    }
}

/// Link a previously-emitted object file at `obj_path` into a native
/// executable at `exe_path`, pulling in the Phoenix runtime static
/// library plus the platform's standard system libraries.
///
/// The object file must already exist on disk; this function does not
/// write it. Callers manage their own scratch directories so they can
/// choose between "clean up on success" (the driver) and "keep
/// artifacts for debugging" (the benches).
///
/// Supported targets live in [`SUPPORTED_PLATFORMS`]. Calling on any
/// other host returns [`LinkError::UnsupportedPlatform`] up front
/// rather than producing an opaque linker error: a real Windows path
/// (link.exe + ucrt + the right runtime libs) and a real FreeBSD
/// path (`-lpthread` / `-lm`, no `-ldl`) are each their own follow-up
/// and belong in [`SUPPORTED_PLATFORMS`] rather than at each call site.
pub fn link_executable(obj_path: &Path, exe_path: &Path) -> Result<(), LinkError> {
    let platform_args = platform_link_args()?;
    let runtime_dir = find_runtime_lib().ok_or(LinkError::RuntimeLibNotFound)?;

    let status = std::process::Command::new("cc")
        .arg("-o")
        .arg(exe_path)
        .arg(obj_path)
        .arg(format!("-L{runtime_dir}"))
        .arg("-lphoenix_runtime")
        .args(platform_args)
        .status()
        .map_err(LinkError::SpawnLinker)?;
    if !status.success() {
        return Err(LinkError::LinkerFailed(status));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Treat an env var as set only when its value is literally `"1"`.
    /// Avoids the common footgun where `PHOENIX_REQUIRE_CC=0` (or `=""`)
    /// would trip a `.is_ok()` check and quietly enter strict mode.
    fn env_flag_enabled(key: &str) -> bool {
        std::env::var(key).as_deref() == Ok("1")
    }

    /// RAII cleanup for a test-owned scratch directory: removes it
    /// unconditionally on drop, including when assertions panic. Lets
    /// the test body assert first and clean up on the way out instead
    /// of capturing every value into a local just to be able to clean
    /// up before the asserts.
    struct TempDirGuard(std::path::PathBuf);
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Outcome of the cc + runtime-lib precondition check shared by
    /// every test that actually drives `link_executable` end-to-end.
    enum SkipReason {
        NoCc,
        NoRuntimeLib,
    }

    /// Check for `cc` on PATH and the runtime static library, applying
    /// the strict-mode env-var gate. Returns `Ok(())` when both are
    /// present, `Err(reason)` after emitting a visible warning (or
    /// panicking under the matching `PHOENIX_REQUIRE_*=1` flag). The
    /// `label` argument is the test name, embedded in the skip/panic
    /// message so a CI log unambiguously identifies which test
    /// short-circuited.
    fn precheck_link_environment(label: &str) -> Result<(), SkipReason> {
        if std::process::Command::new("cc")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .is_err()
        {
            eprintln!(
                "warning: skipping {label} — `cc` not available on PATH \
                 (this skip should be visible in CI output; set \
                 PHOENIX_REQUIRE_CC=1 to fail instead)"
            );
            if env_flag_enabled("PHOENIX_REQUIRE_CC") {
                panic!(
                    "PHOENIX_REQUIRE_CC=1 set but `cc` is not available on PATH — \
                     install gcc or clang"
                );
            }
            return Err(SkipReason::NoCc);
        }
        if find_runtime_lib().is_none() {
            eprintln!(
                "warning: skipping {label} — runtime lib not built \
                 (set PHOENIX_REQUIRE_RUNTIME_LIB=1 to fail instead; \
                 `cargo build -p phoenix-runtime` to fix)"
            );
            if env_flag_enabled("PHOENIX_REQUIRE_RUNTIME_LIB") {
                panic!(
                    "PHOENIX_REQUIRE_RUNTIME_LIB=1 set but the runtime static library \
                     is not on any search path — run `cargo build -p phoenix-runtime` \
                     or set $PHOENIX_RUNTIME_LIB"
                );
            }
            return Err(SkipReason::NoRuntimeLib);
        }
        Ok(())
    }

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

    /// Positive-path coverage for `link_executable`: a trivial valid
    /// object file links cleanly and the resulting binary exits zero
    /// when run. The end-to-end success path is also exercised by
    /// `phoenix-cranelift/tests/compile_basic.rs` via `link_binary`,
    /// but that path runs the full IR pipeline; this colocated test
    /// fails fast on a link regression without dragging in lower /
    /// parser / sema / Cranelift compile.
    ///
    /// Skipped (with a visible warning) when either `cc` is not on
    /// PATH or the runtime static library hasn't been built yet. The
    /// skip is fail-loud via `PHOENIX_REQUIRE_CC=1` or
    /// `PHOENIX_REQUIRE_RUNTIME_LIB=1` so a CI config that has those
    /// prerequisites can refuse to silently disable the test. Same
    /// gating pattern as the existing `LinkerFailed` test below.
    #[cfg(unix)]
    #[test]
    fn link_executable_succeeds_on_trivial_object() {
        if precheck_link_environment("link_executable_succeeds_on_trivial_object").is_err() {
            return;
        }

        let dir = std::env::temp_dir().join(format!("phoenix_link_pos_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = TempDirGuard(dir.clone());
        let src_path = dir.join("trivial.c");
        let obj_path = dir.join("trivial.o");
        let exe_path = dir.join("trivial_exe");
        std::fs::write(&src_path, b"int main(void) { return 0; }\n").unwrap();

        let cc_status = std::process::Command::new("cc")
            .arg("-c")
            .arg(&src_path)
            .arg("-o")
            .arg(&obj_path)
            .status()
            .expect("cc spawn failed");
        assert!(cc_status.success(), "cc -c trivial.c failed: {cc_status}");

        let result = link_executable(&obj_path, &exe_path);
        assert!(result.is_ok(), "link_executable failed: {result:?}");
        assert!(
            exe_path.exists(),
            "link_executable returned Ok but produced no exe"
        );
        let run_status = std::process::Command::new(&exe_path)
            .status()
            .expect("spawning linked binary failed");
        assert!(
            run_status.success(),
            "linked trivial binary exited non-zero: {run_status}"
        );
    }

    /// Companion to `link_executable_succeeds_on_trivial_object` that
    /// *actually* exercises the runtime-library link: a trivial main
    /// references no runtime symbols, so a regression that dropped
    /// `-lphoenix_runtime` from the linker command would still pass
    /// the trivial test. This test references a real exported runtime
    /// symbol (`phx_gc_shutdown` — picked because it is no-arg, idempotent,
    /// and safe to call before any allocation), so the link succeeds only
    /// when `-lphoenix_runtime` is actually on the command line and
    /// resolved against the static library.
    ///
    /// Same skip gates as the trivial test (no `cc` / no runtime lib).
    #[cfg(unix)]
    #[test]
    fn link_executable_pulls_in_runtime_library() {
        if precheck_link_environment("link_executable_pulls_in_runtime_library").is_err() {
            return;
        }

        let dir = std::env::temp_dir().join(format!("phoenix_link_rt_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = TempDirGuard(dir.clone());
        let src_path = dir.join("calls_runtime.c");
        let obj_path = dir.join("calls_runtime.o");
        let exe_path = dir.join("calls_runtime_exe");
        // Force the linker to resolve a real runtime symbol. Without
        // `-lphoenix_runtime`, this object has an undefined reference
        // and the link fails — which is precisely the regression we
        // want this test to catch.
        std::fs::write(
            &src_path,
            b"extern void phx_gc_shutdown(void);\n\
              int main(void) { phx_gc_shutdown(); return 0; }\n",
        )
        .unwrap();

        let cc_status = std::process::Command::new("cc")
            .arg("-c")
            .arg(&src_path)
            .arg("-o")
            .arg(&obj_path)
            .status()
            .expect("cc spawn failed");
        assert!(
            cc_status.success(),
            "cc -c calls_runtime.c failed: {cc_status}"
        );

        let result = link_executable(&obj_path, &exe_path);
        assert!(
            result.is_ok(),
            "link_executable failed for an object that references the runtime — \
             likely a regression that dropped -lphoenix_runtime: {result:?}"
        );
        let run_status = std::process::Command::new(&exe_path)
            .status()
            .expect("spawning linked binary failed");
        assert!(
            run_status.success(),
            "linked runtime-using binary exited non-zero: {run_status}"
        );
    }

    /// `link_executable` should surface a `LinkerFailed` error when `cc`
    /// runs to completion but exits non-zero. We drive that path by
    /// handing it bytes that aren't a valid object file — `cc` reports
    /// "file format not recognized" (or similar) and exits non-zero.
    /// Locks in the variant mapping so a future refactor can't silently
    /// reroute non-zero exits to `SpawnLinker` or to a panic.
    ///
    /// Skipped on systems without `cc` on PATH: that case exercises
    /// `SpawnLinker`, not `LinkerFailed`, so a misclassification panic
    /// here would look like a real regression.
    #[test]
    fn link_executable_reports_linker_failed_on_invalid_object() {
        // The skip gate models exactly the `SpawnLinker` precondition:
        // we only want to skip when `cc` cannot be spawned at all.
        // `Command::output()` succeeds whenever the spawn does — the
        // child's exit code is irrelevant here (some `cc` variants may
        // return non-zero for `--version` on stderr-noisy days).
        if std::process::Command::new("cc")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .is_err()
        {
            eprintln!(
                "warning: skipping link_executable_reports_linker_failed_on_invalid_object \
                 — `cc` not available on PATH (this skip should be visible in CI output; \
                 set PHOENIX_REQUIRE_CC=1 to fail instead)"
            );
            if env_flag_enabled("PHOENIX_REQUIRE_CC") {
                panic!(
                    "PHOENIX_REQUIRE_CC=1 set but `cc` is not available on PATH — \
                     install gcc or clang"
                );
            }
            return;
        }

        let dir = std::env::temp_dir().join(format!("phoenix_link_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = TempDirGuard(dir.clone());
        let obj_path = dir.join("not_an_object.o");
        let exe_path = dir.join("not_an_object_exe");
        std::fs::write(&obj_path, b"this is not an object file").unwrap();

        match link_executable(&obj_path, &exe_path) {
            Err(LinkError::LinkerFailed(_)) => {}
            other => panic!("expected LinkerFailed, got {other:?}"),
        }
    }

    /// Lock in the user-facing `Display` text of each [`LinkError`]
    /// variant. The messages drive `phoenix build`'s error output and
    /// the bench harness panic, so a refactor that strips actionable
    /// hints should surface here.
    #[test]
    fn link_error_display_covers_all_variants() {
        let not_found = LinkError::RuntimeLibNotFound.to_string();
        assert!(
            not_found.contains(RUNTIME_LIB_NAME),
            "RuntimeLibNotFound should name the missing lib: {not_found}"
        );
        assert!(
            not_found.contains("PHOENIX_RUNTIME_LIB"),
            "RuntimeLibNotFound should mention the env var: {not_found}"
        );

        let spawn = LinkError::SpawnLinker(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no such file",
        ))
        .to_string();
        assert!(
            spawn.contains("cc"),
            "SpawnLinker should name the linker: {spawn}"
        );
        assert!(
            spawn.contains("gcc") || spawn.contains("clang"),
            "SpawnLinker should hint at an installable compiler: {spawn}"
        );

        let unsupported = LinkError::UnsupportedPlatform("freebsd").to_string();
        assert!(
            unsupported.contains("freebsd"),
            "UnsupportedPlatform should name the unsupported OS: {unsupported}"
        );
        assert!(
            unsupported.contains("linux") && unsupported.contains("macos"),
            "UnsupportedPlatform should name the supported OSes: {unsupported}"
        );

        // `LinkerFailed` carries a real `ExitStatus`. Construct one
        // synthetically on Unix (where `from_raw` is available) so
        // this assertion runs everywhere the rest of the test suite
        // runs without shelling out.
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            let status = std::process::ExitStatus::from_raw(1 << 8);
            let failed = LinkError::LinkerFailed(status).to_string();
            assert!(
                failed.contains("linker"),
                "LinkerFailed should name the linker: {failed}"
            );
            assert!(
                failed.contains("exited"),
                "LinkerFailed should describe the failure mode: {failed}"
            );
        }
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
