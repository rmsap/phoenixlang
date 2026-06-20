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
    let env = std::env::var("PHOENIX_RUNTIME_LIB").ok();
    let exe = std::env::current_exe().ok();
    find_runtime_lib_resolved(env.as_deref(), exe.as_deref())
}

/// Resolve the runtime-lib directory from an explicit `$PHOENIX_RUNTIME_LIB`
/// value (`env`) and executable path (`exe`): the env value wins as-is
/// (trusted — step 1), otherwise fall back to the exe-relative walk
/// ([`find_runtime_lib_near`] — steps 2–4).
///
/// Taking both inputs as parameters rather than reading process globals lets
/// the env-precedence unit test exercise step 1 *without* mutating
/// `std::env`, which races other tests that read `$PHOENIX_RUNTIME_LIB` (the
/// `cc::windows_registry`-style injectable shape the wasm side already uses).
fn find_runtime_lib_resolved(env: Option<&str>, exe: Option<&Path>) -> Option<String> {
    if let Some(dir) = env {
        return Some(dir.to_string());
    }
    find_runtime_lib_near(exe?)
}

/// Search for the runtime static library relative to a specific executable
/// `exe`, using the same exe-dir → parent → parent/`lib` walk as
/// [`find_runtime_lib`] (steps 2–4), but **without** the
/// `$PHOENIX_RUNTIME_LIB` override.
///
/// Exposed so tests can probe discovery *from the perspective of the
/// `phoenix` binary they spawn* rather than their own. A test binary lives
/// in `target/<profile>/deps/`, so its `current_exe()` sees `deps/`
/// artifacts (e.g. a `libphoenix_runtime.a` cargo dropped there during a
/// `--workspace` build) that the shipped binary in `target/<profile>/` never
/// searches. Probing from the real binary's path keeps a test's skip
/// decision in lockstep with what `phoenix build` will actually find,
/// instead of skipping/running based on a lib only the test can see.
pub fn find_runtime_lib_near(exe: &Path) -> Option<String> {
    let exe_dir = exe.parent()?;

    // exe_dir itself (cargo build: target/debug/; install: bin/)
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

/// `cc`-driven Unix platforms, each paired with the system libraries
/// appended to the linker command line. (Windows links via MSVC `link.exe`
/// in [`run_linker`], not `cc`, so it is not listed here.)
///
/// This is the **single source of truth** for the `cc`-supported OS list:
/// [`run_linker`] consults it for flags, [`LinkError::UnsupportedPlatform`]'s
/// `Display` derives the "currently supported" list from it, and a future
/// FreeBSD entry only needs to be added here.
const SUPPORTED_PLATFORMS: &[(&str, &[&str])] = &[
    ("linux", &["-lpthread", "-ldl", "-lm"]),
    ("macos", &["-lpthread", "-lm"]),
];

/// Look up the system-library flags for the current target OS, or
/// produce an [`LinkError::UnsupportedPlatform`] naming it. Unix-only:
/// the Windows linker path in [`run_linker`] never consults this.
#[cfg(not(target_os = "windows"))]
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
    /// The linker (`cc` on Unix, `link.exe` on Windows) could not be
    /// spawned (PATH lookup or exec failure). The inner error is the
    /// OS-level cause.
    SpawnLinker(std::io::Error),
    /// The linker ran to completion but exited non-zero.
    LinkerFailed(std::process::ExitStatus),
    /// The host platform is not in [`SUPPORTED_PLATFORMS`] and is not
    /// Windows. The inner string is the unsupported target-os string
    /// from `std::env::consts::OS` (e.g. a BSD). Windows links via
    /// [`LinkError::MsvcToolchainNotFound`]'s path, not this one.
    UnsupportedPlatform(&'static str),
    /// Windows only: MSVC `link.exe` could not be located via
    /// `cc::windows_registry`. Phoenix requires VS Build Tools for native
    /// Windows linking, the same toolchain Rust uses.
    MsvcToolchainNotFound,
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
            Self::SpawnLinker(e) => {
                #[cfg(target_os = "windows")]
                {
                    write!(
                        f,
                        "could not invoke the MSVC linker (link.exe): {e} \
                         (install Visual Studio Build Tools)"
                    )
                }
                #[cfg(not(target_os = "windows"))]
                {
                    write!(f, "could not invoke `cc`: {e} (install gcc or clang)")
                }
            }
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
                // Windows is supported but lives outside `SUPPORTED_PLATFORMS`
                // (it links via MSVC `link.exe` in `run_linker`, not `cc`), so
                // it's appended here rather than derived from the list.
                write!(f, ", windows")
            }
            Self::MsvcToolchainNotFound => write!(
                f,
                "could not find the MSVC linker (link.exe); install Visual Studio \
                 Build Tools with the \"Desktop development with C++\" workload — \
                 Phoenix requires it for native Windows builds, the same toolchain \
                 Rust uses"
            ),
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
/// On Unix the system `cc` is driven directly; on Windows the MSVC
/// `link.exe` is located via [`cc::windows_registry`] and driven instead
/// (VS Build Tools is the prerequisite — see `docs/windows-native-link.md`).
/// On any other host (e.g. a BSD not yet wired into [`SUPPORTED_PLATFORMS`])
/// this returns [`LinkError::UnsupportedPlatform`] up front rather than
/// producing an opaque linker error.
pub fn link_executable(obj_path: &Path, exe_path: &Path) -> Result<(), LinkError> {
    link_executable_with_objects(obj_path, exe_path, &[])
}

/// Like [`link_executable`], but also links additional object files alongside the
/// Cranelift object and the runtime archive.
///
/// The motivating use is the native `extern js` binding: the compiler
/// emits a **weak** default definition of each `phx_extern_<module>__<name>`
/// symbol (it aborts), and a host shim object linked here provides **strong**
/// definitions that override them (the strong-beats-weak rule, independent of
/// link order). With no extra objects this is exactly [`link_executable`], so a
/// non-interop build is unaffected.
pub fn link_executable_with_objects(
    obj_path: &Path,
    exe_path: &Path,
    extra_objs: &[std::path::PathBuf],
) -> Result<(), LinkError> {
    let runtime_dir = find_runtime_lib().ok_or(LinkError::RuntimeLibNotFound)?;
    let runtime_archive = std::path::PathBuf::from(&runtime_dir).join(RUNTIME_LIB_NAME);
    run_linker(obj_path, exe_path, &runtime_archive, extra_objs)
}

/// Drive the system `cc` to combine the Cranelift object with the runtime
/// archive (and the platform's system libraries) into an executable.
///
/// Pass the static archive by full path. `-lphoenix_runtime` would pick the
/// companion cdylib (`.so` / `.dylib`) cargo now emits alongside the archive
/// (see `phoenix-runtime/Cargo.toml` for the `crate-type = ["cdylib"]`
/// rationale), which would then need an `LD_LIBRARY_PATH` nothing sets. Full
/// path works on Linux and macOS; `-l:libphoenix_runtime.a` would also work
/// but is GNU-ld-only.
#[cfg(not(target_os = "windows"))]
fn run_linker(
    obj_path: &Path,
    exe_path: &Path,
    runtime_archive: &Path,
    extra_objs: &[std::path::PathBuf],
) -> Result<(), LinkError> {
    let platform_args = platform_link_args()?;
    let mut cmd = std::process::Command::new("cc");
    cmd.arg("-o").arg(exe_path).arg(obj_path);
    // Host-shim objects before the runtime archive: their strong symbols
    // override the Cranelift object's weak `phx_extern_*` defaults.
    cmd.args(extra_objs);
    cmd.arg(runtime_archive).args(platform_args);
    let output = cmd.output().map_err(LinkError::SpawnLinker)?;
    report_linker_result(output)
}

/// Check a linker invocation's result, surfacing the linker's own
/// diagnostics. Capturing the output and re-emitting it via `eprintln!`
/// (rather than letting the child inherit the fd) means the messages —
/// e.g. MSVC `LNK2019` unresolved-external lines or `ld`'s messages —
/// show up in `phoenix build`'s stderr *and* in libtest's per-test
/// capture, instead of being written past the capture boundary.
///
/// Diagnostics are surfaced whether or not the link succeeded: a clean
/// link can still emit warnings (`ld: warning: …`, duplicate-symbol
/// notes) that the user should see — capturing the streams must not turn
/// into silently swallowing them on success.
///
/// Both captured streams are re-emitted on *our* stderr, so a linker that
/// wrote to its stdout (`cc`/`ld` rarely do; MSVC `link.exe` routinely
/// does) lands on stderr here. That's intentional: these are diagnostics,
/// stderr is where they belong, and it keeps them off any stdout a caller
/// might be parsing.
fn report_linker_result(output: std::process::Output) -> Result<(), LinkError> {
    for line in linker_diagnostics(&output) {
        eprintln!("{line}");
    }
    if output.status.success() {
        Ok(())
    } else {
        Err(LinkError::LinkerFailed(output.status))
    }
}

/// Collect the linker's diagnostic streams into the lines worth echoing to
/// the user, stderr first then stdout, skipping streams that are empty or
/// whitespace-only. Split out from [`report_linker_result`] so the
/// stream-selection logic is unit-testable without capturing process
/// stderr: the `eprintln!` side effect lives in the caller, the decision of
/// *what* to emit lives here.
///
/// stdout is included because MSVC's `link.exe` writes its diagnostics
/// there rather than to stderr.
fn linker_diagnostics(output: &std::process::Output) -> Vec<String> {
    let mut lines = Vec::new();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        lines.push(stderr.trim_end().to_string());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        lines.push(stdout.trim_end().to_string());
    }
    lines
}

/// Windows: locate MSVC `link.exe` via [`cc::windows_registry`] — the same
/// vswhere-based discovery rustc uses — apply the toolchain's `LIB`/`PATH`
/// environment so the CRT and Windows SDK import libraries resolve without a
/// Developer Command Prompt, then link the object + `phoenix_runtime.lib`
/// into a console `.exe`.
///
/// The object and runtime staticlib don't pull in std's system dependencies
/// on their own, so they're appended explicitly from [`WINDOWS_SYSTEM_LIBS`]
/// (which also selects the CRT supplying `mainCRTStartup`, the entry point
/// that calls the emitted `main`). VS Build Tools is a prerequisite
/// (`docs/windows-native-link.md`).
#[cfg(target_os = "windows")]
fn run_linker(
    obj_path: &Path,
    exe_path: &Path,
    runtime_archive: &Path,
    extra_objs: &[std::path::PathBuf],
) -> Result<(), LinkError> {
    let target = host_msvc_target();
    let tool = cc::windows_registry::find_tool(&target, "link.exe")
        .ok_or(LinkError::MsvcToolchainNotFound)?;
    let mut command = tool.to_command();
    // Apply the toolchain environment (LIB / PATH / INCLUDE) so link.exe
    // finds the CRT and SDK import libs. Re-applying values to_command may
    // already carry is harmless.
    for (key, value) in tool.env() {
        command.env(key, value);
    }
    let output = command
        .arg("/NOLOGO")
        .arg("/SUBSYSTEM:CONSOLE")
        .arg(format!("/OUT:{}", exe_path.display()))
        .arg(obj_path)
        .args(extra_objs)
        .arg(runtime_archive)
        .args(WINDOWS_SYSTEM_LIBS)
        .output()
        .map_err(LinkError::SpawnLinker)?;
    report_linker_result(output)
}

/// System libraries a consumer must link alongside the Rust-built
/// `phoenix_runtime.lib` on windows-msvc — the Windows analog of the Unix
/// `-lpthread -ldl -lm` in [`SUPPORTED_PLATFORMS`]. The Cranelift-emitted
/// object carries no `/DEFAULTLIB` directives and the Rust staticlib does not
/// embed these, so std's references (Nt* syscalls, sockets, user profile,
/// backtrace symbolization) would otherwise be unresolved (LNK2019/LNK1120).
/// `/defaultlib:msvcrt` selects the dynamic CRT (supplying `mainCRTStartup`),
/// matching how `phoenix_runtime.lib` is built.
///
/// Sourced from `rustc --print native-static-libs` for
/// `x86_64-pc-windows-msvc`; regenerate with that command if a future
/// toolchain or runtime change adds std dependencies.
#[cfg(target_os = "windows")]
const WINDOWS_SYSTEM_LIBS: &[&str] = &[
    "kernel32.lib",
    "ntdll.lib",
    "userenv.lib",
    "ws2_32.lib",
    "dbghelp.lib",
    "/defaultlib:msvcrt",
];

/// The MSVC target triple for the current Windows host, e.g.
/// `x86_64-pc-windows-msvc`. Drives [`cc::windows_registry`] tool lookup so
/// the right architecture's linker is selected (future arm64-windows too).
#[cfg(target_os = "windows")]
fn host_msvc_target() -> String {
    let arch = match std::env::consts::ARCH {
        // `std::env::consts::ARCH` reports "x86"; the triple spells it "i686".
        "x86" => "i686",
        // "x86_64", "aarch64", … already match the triple's arch field.
        other => other,
    };
    format!("{arch}-pc-windows-msvc")
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
    /// Unix-only: the `cc`-based precheck below gates the `#[cfg(unix)]`
    /// link tests; Windows tests use the MSVC toolchain probe instead.
    #[cfg(unix)]
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
    #[cfg(unix)]
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

    /// Windows counterpart of `link_executable_pulls_in_runtime_library`:
    /// compile a trivial `main` that calls a real runtime symbol with MSVC
    /// `cl.exe`, then link it via `link_executable` (which drives `link.exe`)
    /// and run it. Proves the Windows `run_linker` path, the `LIB` env
    /// application, and the `phoenix_runtime.lib` resolution end-to-end.
    ///
    /// Skipped (with a visible warning) when the MSVC toolchain or the
    /// runtime lib isn't present; `PHOENIX_REQUIRE_RUNTIME_LIB=1` turns the
    /// skip into a hard failure, mirroring the Unix gate.
    #[cfg(windows)]
    #[test]
    fn link_executable_links_and_runs_with_msvc() {
        let target = super::host_msvc_target();
        let cl = match (
            cc::windows_registry::find_tool(&target, "cl.exe"),
            cc::windows_registry::find_tool(&target, "link.exe"),
            find_runtime_lib(),
        ) {
            (Some(cl), Some(_link), Some(_lib)) => cl,
            _ => {
                eprintln!(
                    "warning: skipping link_executable_links_and_runs_with_msvc — MSVC \
                     toolchain or runtime lib unavailable (set PHOENIX_REQUIRE_RUNTIME_LIB=1 \
                     to fail instead; install VS Build Tools and run `cargo build -p \
                     phoenix-runtime` to fix)"
                );
                if env_flag_enabled("PHOENIX_REQUIRE_RUNTIME_LIB") {
                    panic!(
                        "PHOENIX_REQUIRE_RUNTIME_LIB=1 set but the MSVC toolchain or runtime \
                         lib is unavailable"
                    );
                }
                return;
            }
        };

        let dir = std::env::temp_dir().join(format!("phoenix_link_win_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = TempDirGuard(dir.clone());
        let src_path = dir.join("calls_runtime.c");
        let obj_path = dir.join("calls_runtime.obj");
        let exe_path = dir.join("calls_runtime.exe");
        std::fs::write(
            &src_path,
            b"extern void phx_gc_shutdown(void);\n\
              int main(void) { phx_gc_shutdown(); return 0; }\n",
        )
        .unwrap();

        // Compile to .obj with cl.exe (/c = compile only, /Fo = output path),
        // applying the toolchain env so the C headers resolve. `/MD` selects
        // the dynamic CRT to match the Rust-built `phoenix_runtime.lib`
        // (windows-msvc defaults to the dynamic CRT), avoiding a CRT-flavor
        // mismatch at link time.
        let mut cl_cmd = cl.to_command();
        for (key, value) in cl.env() {
            cl_cmd.env(key, value);
        }
        let cl_status = cl_cmd
            .arg("/nologo")
            .arg("/MD")
            .arg("/c")
            .arg(&src_path)
            .arg(format!("/Fo{}", obj_path.display()))
            .status()
            .expect("cl.exe spawn failed");
        assert!(
            cl_status.success(),
            "cl /c calls_runtime.c failed: {cl_status}"
        );

        let result = link_executable(&obj_path, &exe_path);
        assert!(
            result.is_ok(),
            "link_executable failed on Windows for an object that references the runtime: {result:?}"
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
        // The SpawnLinker hint names the host's linker/toolchain.
        #[cfg(not(target_os = "windows"))]
        {
            assert!(
                spawn.contains("cc"),
                "SpawnLinker should name the linker: {spawn}"
            );
            assert!(
                spawn.contains("gcc") || spawn.contains("clang"),
                "SpawnLinker should hint at an installable compiler: {spawn}"
            );
        }
        #[cfg(target_os = "windows")]
        {
            assert!(
                spawn.contains("link.exe"),
                "SpawnLinker should name the MSVC linker: {spawn}"
            );
            assert!(
                spawn.contains("Build Tools"),
                "SpawnLinker should hint at installing VS Build Tools: {spawn}"
            );
        }

        // `MsvcToolchainNotFound` exists (and its `Display` compiles) on every
        // platform; assert it points users at the right Windows toolchain.
        let msvc = LinkError::MsvcToolchainNotFound.to_string();
        assert!(
            msvc.contains("link.exe"),
            "MsvcToolchainNotFound should name the MSVC linker: {msvc}"
        );
        assert!(
            msvc.contains("Build Tools"),
            "MsvcToolchainNotFound should mention VS Build Tools: {msvc}"
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

    /// An explicit `$PHOENIX_RUNTIME_LIB` value is honored as-is and wins
    /// over the exe-relative walk (passing `exe: None` proves the walk is
    /// not consulted). Exercised through the pure resolver so the test does
    /// not mutate the process-global env var — doing so previously raced
    /// other tests that read it (`find_runtime_lib_succeeds_in_cargo_test`,
    /// the `link_executable_*` tests), a flake that surfaced on narrower
    /// parallel runs like the Windows CI job's `--lib link::` filter.
    #[test]
    fn find_runtime_lib_respects_env_var() {
        let result = find_runtime_lib_resolved(Some("/custom/runtime/dir"), None);
        assert_eq!(result.as_deref(), Some("/custom/runtime/dir"));
    }

    /// Construct a synthetic `ExitStatus` for a given exit `code` so the
    /// diagnostic-routing tests can build `std::process::Output` values
    /// without shelling out. Construction is platform-specific: Unix
    /// `from_raw` takes a wait-status (the exit code lives in the high
    /// byte), Windows `from_raw` takes the raw exit code directly.
    #[cfg(unix)]
    fn synthetic_exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }
    #[cfg(windows)]
    fn synthetic_exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code as u32)
    }

    fn output_with(stdout: &str, stderr: &str, code: i32) -> std::process::Output {
        std::process::Output {
            status: synthetic_exit_status(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// `linker_diagnostics` decides *which* of the linker's streams reach
    /// the user. Covers each shape independently of the `eprintln!` side
    /// effect: stderr-only (the `ld`/`cc` case), stdout-only (MSVC
    /// `link.exe` writes diagnostics there), both (stderr first), and
    /// whitespace-only streams (suppressed so a clean link prints nothing).
    #[test]
    fn linker_diagnostics_surfaces_nonempty_streams() {
        // stderr only — the `ld` / `cc` case.
        let out = output_with("", "ld: warning: something fishy\n", 0);
        assert_eq!(
            linker_diagnostics(&out),
            vec!["ld: warning: something fishy".to_string()]
        );

        // stdout only — MSVC `link.exe` writes diagnostics to stdout.
        let out = output_with("trivial.obj : error LNK2019: unresolved external\n", "", 1);
        assert_eq!(
            linker_diagnostics(&out),
            vec!["trivial.obj : error LNK2019: unresolved external".to_string()]
        );

        // Both streams populated — stderr is emitted before stdout.
        let out = output_with("on stdout\n", "on stderr\n", 1);
        assert_eq!(
            linker_diagnostics(&out),
            vec!["on stderr".to_string(), "on stdout".to_string()]
        );

        // Whitespace-only streams are suppressed entirely.
        let out = output_with("   \n", "\n\t\n", 0);
        assert!(
            linker_diagnostics(&out).is_empty(),
            "whitespace-only streams should produce no diagnostic lines"
        );
    }

    /// `report_linker_result` maps exit status to `Ok`/`LinkerFailed`
    /// independently of whether diagnostics were present. The key
    /// regression guard is the success-with-output case: capturing the
    /// linker's streams must surface warnings, not swallow them, while
    /// still returning `Ok` for a successful link.
    #[test]
    fn report_linker_result_maps_status_with_diagnostics_present() {
        // Clean link that nonetheless emitted a warning still succeeds
        // (and the warning is echoed via `eprintln!`, visible in capture).
        let out = output_with("", "ld: warning: harmless noise\n", 0);
        assert!(
            report_linker_result(out).is_ok(),
            "a successful link with warnings must return Ok, not swallow or fail"
        );

        // Non-zero exit maps to LinkerFailed even with diagnostics present.
        let out = output_with("error LNK1120: 1 unresolved externals\n", "", 1);
        match report_linker_result(out) {
            Err(LinkError::LinkerFailed(_)) => {}
            other => panic!("expected LinkerFailed, got {other:?}"),
        }
    }
}
