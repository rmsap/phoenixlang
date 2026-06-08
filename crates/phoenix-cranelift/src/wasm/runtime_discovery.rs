//! Discovery of the compiled `phoenix_runtime.wasm` module.
//!
//! The wasm32-linear backend's [embed-and-merge step](runtime_merge)
//! needs the runtime crate compiled for `wasm32-wasip1` and packaged as
//! a complete WebAssembly module (cdylib output). This module locates
//! the resulting `phoenix_runtime.wasm` file, mirroring the native
//! [`crate::link::find_runtime_lib`] discovery pattern so the two
//! backends look the same to users.
//!
//! # Search order
//!
//! 1. `$PHOENIX_RUNTIME_WASM` environment variable — trusted as-is
//!    (caller's responsibility to point at a valid .wasm file).
//! 2. Install layout: `<exe_dir>/../lib/phoenix_runtime.wasm` — the
//!    `bin/` + `../lib/` shape shipped by the release tarball and placed
//!    by `install.sh`, mirroring [`crate::link::find_runtime_lib`]'s
//!    native `bin/../lib/` arm.
//! 3. Cargo target directories relative to the current executable:
//!    `target/wasm32-wasip1/{release,debug}/phoenix_runtime.wasm` walked
//!    upward from the executable directory until found or root is hit.
//!
//! Returns `None` if the runtime can't be found, letting callers
//! produce an actionable diagnostic that names the expected paths
//! and the `cargo build` command needed to populate them.

use std::path::Path;

/// Filename emitted by `cargo build -p phoenix-runtime --target wasm32-wasip1`
/// when `phoenix-runtime`'s `[lib]` declares `crate-type = ["cdylib"]`.
///
/// Hyphen-to-underscore conversion follows cargo's standard rule for
/// library artifact names (matches how `libphoenix_runtime.a` is named
/// on native).
pub const RUNTIME_WASM_FILENAME: &str = "phoenix_runtime.wasm";

fn start_dir() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Testable form of the discovery walk: takes the env-var value and
/// the walk-start directory as parameters rather than reading process
/// state. Lets unit tests cover both the env-var-takes-precedence and
/// the fall-through-to-walk arms deterministically, without racing on
/// `std::env::set_var` from parallel tests.
fn find_runtime_wasm_with(env_value: Option<&str>, start_dir: Option<&Path>) -> Option<String> {
    // 1. Environment variable — trust the user.
    if let Some(path) = env_value {
        // Validate the env var points at a real file before returning
        // it so a stale value produces an actionable diagnostic rather
        // than a wasmparser "unexpected end of file" later in the
        // pipeline.
        if Path::new(path).is_file() {
            return Some(path.to_string());
        }
    }

    let start = start_dir?;

    // 2. Install layout: `<exe_dir>/../lib/phoenix_runtime.wasm`. This is
    //    the `bin/` + `../lib/` shape the release tarball + `install.sh`
    //    produce, mirroring `link::find_runtime_lib`'s native arm so a
    //    downloaded Phoenix finds its shipped wasm runtime the same way it
    //    finds `libphoenix_runtime.a`. Checked before the cargo walk so an
    //    install never accidentally resolves a stale dev-tree artifact.
    if let Some(parent) = start.parent() {
        let candidate = parent.join("lib").join(RUNTIME_WASM_FILENAME);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }

    // 3. Walk upward from the start directory looking for the
    //    canonical cargo-build layout. We look for both `release` and
    //    `debug` profiles — `release` is preferred for the runtime
    //    (smaller, faster), but a developer with only `debug` built
    //    should still work.
    let mut dir: &Path = start;
    loop {
        for profile in ["release", "debug"] {
            let candidate = dir
                .join("target")
                .join("wasm32-wasip1")
                .join(profile)
                .join(RUNTIME_WASM_FILENAME);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    None
}

/// Error returned by [`find_runtime_wasm_or_diagnostic`] when the
/// runtime can't be located. Carries an actionable message that names
/// the env var, the expected cargo command, and the canonical search
/// paths so the user's next step is unambiguous.
#[derive(Debug)]
pub struct RuntimeWasmNotFound {
    /// The path the env var pointed at, if any — surfaced in the
    /// diagnostic so a typo'd `PHOENIX_RUNTIME_WASM` is debuggable.
    pub env_var_value: Option<String>,
}

impl std::fmt::Display for RuntimeWasmNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "could not find {RUNTIME_WASM_FILENAME}; it ships in Phoenix's `lib/` \
             directory — reinstall with the install script, set $PHOENIX_RUNTIME_WASM \
             to a built copy, or — for in-tree development — build it with \
             `cargo build -p phoenix-runtime --target wasm32-wasip1 --release` \
             (or `--debug` for unoptimized builds), then re-run. \
             Searched: $PHOENIX_RUNTIME_WASM env var{}, then `<exe_dir>/../lib/{RUNTIME_WASM_FILENAME}` \
             (install layout), then upward from the current executable for \
             `target/wasm32-wasip1/{{release,debug}}/{RUNTIME_WASM_FILENAME}`. \
             For in-tree builds, make sure `rustup target add wasm32-wasip1` has been run.",
            match &self.env_var_value {
                Some(v) => format!(" (currently `{v}`)"),
                None => " (unset)".to_string(),
            }
        )
    }
}

impl std::error::Error for RuntimeWasmNotFound {}

/// Convert a [`RuntimeWasmNotFound`] into a [`CompileError`] tagged
/// with [`CompileErrorKind::RuntimeWasmNotFound`]. Centralizing the
/// kind-tag in a `From` impl lets a unit test assert the discriminator
/// is right — without this, a regression at the `compile_wasm_linear`
/// call site that swapped `with_kind(...)` for `new(...)` would only
/// surface on CI hosts that happen to lack the runtime artifact (where
/// the integration test's skip path catches it).
impl From<RuntimeWasmNotFound> for crate::error::CompileError {
    fn from(e: RuntimeWasmNotFound) -> Self {
        Self::with_kind(
            e.to_string(),
            crate::error::CompileErrorKind::RuntimeWasmNotFound,
        )
    }
}

/// Convenience wrapper: locate the runtime or return a
/// [`RuntimeWasmNotFound`] carrying an actionable diagnostic. Used by
/// the embed-and-merge step's entry point so callers don't have to
/// reconstruct the "how do I fix this?" message themselves.
pub fn find_runtime_wasm_or_diagnostic() -> Result<String, RuntimeWasmNotFound> {
    // Read the env var once so we can both consult it for the search
    // and embed its current value in the not-found diagnostic without
    // racing a second `std::env::var` call.
    let env_value = std::env::var("PHOENIX_RUNTIME_WASM").ok();
    if let Some(path) = find_runtime_wasm_with(env_value.as_deref(), start_dir().as_deref()) {
        return Ok(path);
    }
    Err(RuntimeWasmNotFound {
        env_var_value: env_value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_names_env_var_and_cargo_command() {
        // Force the not-found path by unsetting the env var and using
        // a non-existent executable directory. The diagnostic must
        // surface every "next step" piece a user would need.
        let err = RuntimeWasmNotFound {
            env_var_value: None,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("PHOENIX_RUNTIME_WASM"),
            "diagnostic should name the env var: {msg}"
        );
        assert!(
            msg.contains("cargo build -p phoenix-runtime"),
            "diagnostic should name the cargo command: {msg}"
        );
        assert!(
            msg.contains("wasm32-wasip1"),
            "diagnostic should name the target triple: {msg}"
        );
        assert!(
            msg.contains("rustup target add"),
            "diagnostic should remind about installing the rustup target: {msg}"
        );
    }

    #[test]
    fn diagnostic_includes_stale_env_var_value() {
        let err = RuntimeWasmNotFound {
            env_var_value: Some("/nonexistent/path.wasm".to_string()),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("/nonexistent/path.wasm"),
            "diagnostic should surface a stale env-var value: {msg}"
        );
    }

    #[test]
    fn env_var_pointing_at_real_file_takes_precedence() {
        // Set up a real .wasm-ish file in a tempdir, point the env-var
        // at it, and verify it's returned without walking. start_dir
        // is `None` to prove the walk isn't consulted.
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("fake_runtime.wasm");
        std::fs::write(&file_path, b"\0asm\x01\0\0\0").expect("write fake wasm");
        let env = file_path.to_string_lossy().into_owned();
        let found = find_runtime_wasm_with(Some(&env), None).expect("env var should be honored");
        assert_eq!(found, env);
    }

    #[test]
    fn install_layout_lib_dir_is_discovered() {
        // Replicate the shipped layout: `<prefix>/bin/phoenix` alongside
        // `<prefix>/lib/phoenix_runtime.wasm`. With `start_dir` = the
        // `bin/` directory and no env var, discovery must resolve the
        // runtime via the `../lib` arm — the path a downloaded Phoenix
        // relies on. No `target/` tree exists in the tempdir, so a match
        // here proves the install arm, not the cargo walk.
        let prefix = tempfile::tempdir().expect("tempdir");
        let bin_dir = prefix.path().join("bin");
        let lib_dir = prefix.path().join("lib");
        std::fs::create_dir_all(&bin_dir).expect("create bin/");
        std::fs::create_dir_all(&lib_dir).expect("create lib/");
        let wasm_path = lib_dir.join(RUNTIME_WASM_FILENAME);
        std::fs::write(&wasm_path, b"\0asm\x01\0\0\0").expect("write fake wasm");

        let found = find_runtime_wasm_with(None, Some(&bin_dir))
            .expect("install-layout `../lib` arm should resolve the runtime");
        assert_eq!(found, wasm_path.to_string_lossy());
    }

    #[test]
    fn diagnostic_names_install_layout() {
        // The downloaded-binary story: the diagnostic must point users at
        // the shipped `lib/` directory and the reinstall path, not only
        // the in-tree cargo command.
        let msg = RuntimeWasmNotFound {
            env_var_value: None,
        }
        .to_string();
        assert!(
            msg.contains("lib/"),
            "diagnostic should mention the install `lib/` directory: {msg}"
        );
        assert!(
            msg.contains("install script"),
            "diagnostic should mention reinstalling via the install script: {msg}"
        );
    }

    #[test]
    fn env_var_pointing_at_missing_file_falls_through_to_walk() {
        // Bad env var + no start dir → must return None (not panic, not
        // resolve from somewhere else). Documents the "fall through to
        // walk on stale env var" behavior so any future change to that
        // policy has to update this test.
        let result = find_runtime_wasm_with(Some("/definitely/does/not/exist.wasm"), None);
        assert!(result.is_none(), "expected None, got {result:?}");
    }

    #[test]
    fn no_env_var_and_no_start_dir_returns_none() {
        assert!(find_runtime_wasm_with(None, None).is_none());
    }

    #[test]
    fn into_compile_error_tags_kind() {
        // Regression guard: a refactor that converts via
        // `CompileError::new(e.to_string())` would drop the discriminator
        // and break the integration test's skip path. Asserting on the
        // `From` impl rather than on `compile_wasm_linear` end-to-end
        // keeps this test deterministic (no env vars, no filesystem)
        // while still pinning the contract that the call site relies on.
        use crate::error::{CompileError, CompileErrorKind};
        let runtime_err = RuntimeWasmNotFound {
            env_var_value: Some("/nonexistent.wasm".to_string()),
        };
        let stringified = runtime_err.to_string();
        let compile_err: CompileError = runtime_err.into();
        assert_eq!(compile_err.kind, CompileErrorKind::RuntimeWasmNotFound);
        assert_eq!(compile_err.message, stringified);
    }
}
