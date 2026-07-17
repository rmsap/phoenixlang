//! Configuration file support for Phoenix projects.
//!
//! Loads `phoenix.toml` from the current directory or any ancestor,
//! providing default values for the `gen` subcommand so that users
//! don't need to pass `--target`, `--out`, etc. every time.

use crate::manifest::{Dependency, ManifestError, PackageConfig, parse_dependency};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Top-level `phoenix.toml` configuration.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhoenixConfig {
    /// Package metadata (`[package]` section). Present for any project that
    /// declares dependencies or is itself a depend-able package; absent for a
    /// bare `[gen]`-only schema project.
    pub package: Option<PackageConfig>,
    /// Declared dependencies (`[dependencies]` section), as raw TOML values so
    /// the source kind (git / path / reserved registry string) can be validated
    /// with precise diagnostics by [`PhoenixConfig::dependencies`] rather than
    /// the generic serde "did not match any variant" message. Read through the
    /// validating [`PhoenixConfig::dependencies`] accessor rather than this raw
    /// field, which is `pub(crate)` only so deserialization and tests can reach
    /// it.
    #[serde(default, rename = "dependencies")]
    pub(crate) raw_dependencies: BTreeMap<String, toml::Value>,
    /// Declared npm/JavaScript dependencies (`[js-dependencies]` section):
    /// package name → version spec, verbatim as written into a generated
    /// `package.json` (the BYO model). Read through the validating
    /// [`PhoenixConfig::js_dependencies`] accessor.
    #[serde(default, rename = "js-dependencies")]
    pub(crate) raw_js_dependencies: BTreeMap<String, String>,
    /// Code generation configuration (`[gen]` section).
    #[serde(default, rename = "gen")]
    pub codegen: GenConfig,
}

/// The `[gen]` section of `phoenix.toml`.
///
/// Supports two modes of configuration:
///
/// **Single target** — set `target` and `out_dir` directly:
///
/// ```toml
/// [gen]
/// schema = "api/schema.phx"
/// target = "typescript"
/// out_dir = "./generated"
/// mode = "both"
/// ```
///
/// **Multiple targets** — use `[gen.targets.<name>]` sub-tables to generate
/// code for several languages in one `phoenix gen` invocation:
///
/// ```toml
/// [gen]
/// schema = "api/schema.phx"
///
/// [gen.targets.typescript]
/// out_dir = "frontend/src/generated"
/// mode = "client"
///
/// [gen.targets.python]
/// out_dir = "backend/generated"
/// mode = "server"
///
/// [gen.targets.openapi]
/// out_dir = "docs"
/// ```
///
/// When `targets` is present, running `phoenix gen` (with no `--target` flag)
/// generates code for every configured target. The `--target` flag on the CLI
/// selects a single target from the map and ignores the rest.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenConfig {
    /// Path to the `.phx` schema file.
    pub schema: Option<String>,
    /// Default target language (single-target mode).
    pub target: Option<String>,
    /// Default output directory (single-target mode).
    pub out_dir: Option<String>,
    /// Default generation mode: `"client"`, `"server"`, or `"both"`.
    pub mode: Option<String>,
    /// Default server framework, interpreted per target: TypeScript
    /// `"express"` (default) or `"fastify"`, Go `"net/http"` (default) or
    /// `"chi"`. Ignored by the python/openapi targets. Named to match the
    /// `--framework` CLI flag (TOML key: `framework`).
    pub framework: Option<String>,
    /// Per-target configuration for multi-target generation.
    pub targets: Option<HashMap<String, TargetConfig>>,
}

/// Per-target configuration within `[gen.targets.<name>]`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    /// Output directory for this target.
    pub out_dir: Option<String>,
    /// Generation mode for this target: `"client"`, `"server"`, or `"both"`.
    pub mode: Option<String>,
    /// Server framework for this target, interpreted per target: TypeScript
    /// `"express"` (default) or `"fastify"`, Go `"net/http"` (default) or
    /// `"chi"`. Ignored by the python/openapi targets (TOML key: `framework`).
    pub framework: Option<String>,
}

/// A resolved target ready for code generation.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    /// Target language name (e.g. `"typescript"`, `"python"`).
    pub target: String,
    /// Output directory.
    pub out_dir: String,
    /// Generation mode string (e.g. `"client"`, `"server"`, `"both"`), or `None` for default.
    pub mode: Option<String>,
    /// Server framework, interpreted per target (TypeScript
    /// `"express"`/`"fastify"`, Go `"net/http"`/`"chi"`); `None` for default.
    /// Ignored by the python/openapi targets.
    pub framework: Option<String>,
    /// Whether `framework` was set *specifically for this target* (a per-target
    /// `[gen.targets.<name>] framework`) rather than inherited from the top-level
    /// `[gen] framework`. The driver uses this for provenance-aware validation: a
    /// per-target value is a typo if it's unknown for the target (error), whereas
    /// a broadcast top-level default unknown for one target is tolerated (that
    /// target falls back to its own default). See `run_gen` in `lib.rs`.
    pub framework_explicit: bool,
}

impl GenConfig {
    /// Resolves the configured targets into a list of [`ResolvedTarget`]s.
    ///
    /// - If `targets` is present, returns one entry per configured target.
    /// - Otherwise, returns a single entry from the top-level `target`/`out_dir`/`mode` fields.
    /// - Returns `None` if neither `targets` nor `target` is set (the caller
    ///   should fall back to CLI defaults).
    pub fn resolve_targets(&self) -> Option<Vec<ResolvedTarget>> {
        if let Some(targets) = &self.targets {
            if targets.is_empty() {
                return None;
            }
            let default_out_dir = self.out_dir.as_deref().unwrap_or("./generated");
            let mut result: Vec<ResolvedTarget> = targets
                .iter()
                .map(|(name, cfg)| ResolvedTarget {
                    target: name.clone(),
                    out_dir: cfg
                        .out_dir
                        .clone()
                        .unwrap_or_else(|| format!("{}/{}", default_out_dir, name)),
                    mode: cfg.mode.clone().or_else(|| self.mode.clone()),
                    framework: cfg.framework.clone().or_else(|| self.framework.clone()),
                    // Explicit only when set under this target; a value inherited
                    // from the top-level `[gen] framework` is a broadcast default.
                    framework_explicit: cfg.framework.is_some(),
                })
                .collect();
            // Sort for deterministic output order
            result.sort_by(|a, b| a.target.cmp(&b.target));
            Some(result)
        } else if self.target.is_some() {
            Some(vec![ResolvedTarget {
                target: self.target.clone().unwrap(),
                out_dir: self
                    .out_dir
                    .clone()
                    .unwrap_or_else(|| "./generated".to_string()),
                mode: self.mode.clone(),
                framework: self.framework.clone(),
                // Single-target config: the framework (if any) is unambiguously
                // bound to this one target, so it's explicit — a typo must error
                // rather than silently fall back to the target's default, even
                // with no CLI `--framework` (see `resolve_fw` in `run_gen`, which
                // only derives strictness from `single` when a CLI `--framework`
                // is present; absent that it reads this flag).
                framework_explicit: true,
            }])
        } else {
            None
        }
    }
}

/// Errors that can occur when loading `phoenix.toml`.
#[derive(Debug)]
pub enum ConfigError {
    /// The config file could not be read from disk.
    ReadError(PathBuf, std::io::Error),
    /// The config file contains invalid TOML or unexpected fields.
    /// `toml::de::Error` is boxed to keep `ConfigError` (and every
    /// `Result<_, ConfigError>` it appears in) small — it exceeds the
    /// `clippy::result_large_err` threshold unboxed on some targets
    /// (notably windows-msvc).
    ParseError(PathBuf, Box<toml::de::Error>),
    /// The TOML parsed, but the manifest failed semantic validation
    /// (invalid `[package]` metadata or a malformed dependency source).
    /// `ManifestError` is boxed for the same reason as `ParseError` above:
    /// keep `ConfigError` (and the `Result`s carrying it) under the
    /// `clippy::result_large_err` threshold.
    ManifestError(PathBuf, Box<ManifestError>),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::ReadError(path, e) => {
                write!(f, "could not read '{}': {}", path.display(), e)
            }
            ConfigError::ParseError(path, e) => {
                write!(f, "invalid config in '{}': {}", path.display(), e)
            }
            ConfigError::ManifestError(path, e) => {
                write!(f, "invalid manifest in '{}': {}", path.display(), e)
            }
        }
    }
}

impl PhoenixConfig {
    /// Searches for `phoenix.toml` starting from `start_dir` and walking
    /// up to the filesystem root.
    ///
    /// Returns `Ok(None)` if no config file is found (the normal case for
    /// projects that don't use a config file).
    pub fn find_and_load(start_dir: &Path) -> Result<Option<Self>, ConfigError> {
        Ok(Self::find_with_path(start_dir)?.map(|(config, _path)| config))
    }

    /// Like [`find_and_load`](Self::find_and_load) but also returns the path of
    /// the `phoenix.toml` that was loaded, so callers can resolve relative
    /// `path` dependencies and write back the lockfile beside it.
    pub fn find_with_path(start_dir: &Path) -> Result<Option<(Self, PathBuf)>, ConfigError> {
        let mut dir = start_dir.to_path_buf();
        loop {
            let candidate = dir.join("phoenix.toml");
            if candidate.is_file() {
                let config = Self::load_file(&candidate)?;
                return Ok(Some((config, candidate)));
            }
            if !dir.pop() {
                break;
            }
        }
        Ok(None)
    }

    /// Loads, parses, and validates a specific `phoenix.toml` file.
    ///
    /// Beyond raw TOML parsing this runs [`PackageConfig::validate`] and
    /// [`PhoenixConfig::dependencies`] so a malformed manifest (bad semver,
    /// conflicting dependency source, etc.) is rejected here rather than
    /// staying latent until a downstream consumer happens to look.
    pub fn load_file(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::ReadError(path.to_path_buf(), e))?;
        let config: PhoenixConfig = toml::from_str(&contents)
            .map_err(|e| ConfigError::ParseError(path.to_path_buf(), Box::new(e)))?;
        config.validate_manifest(path)?;
        Ok(config)
    }

    /// Runs semantic validation over the parsed manifest: `[package]`
    /// metadata and every `[dependencies]` source. Maps the first
    /// [`ManifestError`] to a [`ConfigError`] carrying `path` for context.
    fn validate_manifest(&self, path: &Path) -> Result<(), ConfigError> {
        let to_config_err = |e| ConfigError::ManifestError(path.to_path_buf(), Box::new(e));
        if let Some(pkg) = &self.package {
            pkg.validate().map_err(to_config_err)?;
        }
        self.dependencies().map_err(to_config_err)?;
        self.js_dependencies().map_err(to_config_err)?;
        Ok(())
    }

    /// Validates and returns the declared dependencies as typed [`Dependency`]
    /// values, keyed by dependency name. Returns the first [`ManifestError`]
    /// encountered (bare registry string, missing/conflicting source, etc.).
    pub fn dependencies(&self) -> Result<BTreeMap<String, Dependency>, ManifestError> {
        let mut out = BTreeMap::new();
        for (name, value) in &self.raw_dependencies {
            out.insert(name.clone(), parse_dependency(name, value)?);
        }
        Ok(out)
    }

    /// Validates and returns the declared npm/JavaScript dependencies
    /// (`[js-dependencies]`): package name → version spec. Each name and spec
    /// must be non-empty; the spec is otherwise taken verbatim (it flows into a
    /// generated `package.json` for the developer's own `npm install` — the BYO
    /// model, so Phoenix does not interpret npm semver here).
    pub fn js_dependencies(&self) -> Result<BTreeMap<String, String>, ManifestError> {
        for (name, version) in &self.raw_js_dependencies {
            if name.trim().is_empty() {
                return Err(ManifestError::EmptyJsDependencyName);
            }
            if version.trim().is_empty() {
                return Err(ManifestError::EmptyJsDependencyVersion { name: name.clone() });
            }
        }
        Ok(self.raw_js_dependencies.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_config() {
        let config: PhoenixConfig = toml::from_str("").unwrap();
        assert!(config.codegen.target.is_none());
        assert!(config.codegen.schema.is_none());
    }

    #[test]
    fn parse_js_dependencies_section() {
        let config: PhoenixConfig =
            toml::from_str("[js-dependencies]\nleft-pad = \"^1.3.0\"\nchalk = \"5\"\n").unwrap();
        let js = config.js_dependencies().unwrap();
        assert_eq!(js.get("left-pad").map(String::as_str), Some("^1.3.0"));
        assert_eq!(js.get("chalk").map(String::as_str), Some("5"));
    }

    #[test]
    fn js_dependencies_coexist_with_package_and_gen() {
        let config: PhoenixConfig = toml::from_str(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
             [js-dependencies]\nleft-pad = \"^1\"\n\n[gen]\ntarget = \"go\"\n",
        )
        .unwrap();
        assert_eq!(config.package.as_ref().unwrap().name, "app");
        assert_eq!(config.codegen.target.as_deref(), Some("go"));
        assert!(config.js_dependencies().unwrap().contains_key("left-pad"));
    }

    #[test]
    fn js_dependencies_empty_version_rejected() {
        let config: PhoenixConfig = toml::from_str("[js-dependencies]\nleft-pad = \"\"\n").unwrap();
        assert!(matches!(
            config.js_dependencies().unwrap_err(),
            crate::manifest::ManifestError::EmptyJsDependencyVersion { .. }
        ));
    }

    #[test]
    fn js_dependencies_non_string_version_rejected() {
        // A non-string value (e.g. a table) is a serde parse error.
        let result: Result<PhoenixConfig, _> =
            toml::from_str("[js-dependencies]\nleft-pad = { version = \"1\" }\n");
        assert!(result.is_err());
    }

    #[test]
    fn parse_gen_section_only() {
        let config: PhoenixConfig = toml::from_str("[gen]\n").unwrap();
        assert!(config.codegen.target.is_none());
    }

    #[test]
    fn parse_full_single_target_config() {
        let config: PhoenixConfig = toml::from_str(
            r#"
[gen]
schema = "api/schema.phx"
target = "python"
out_dir = "./out"
mode = "server"
"#,
        )
        .unwrap();
        assert_eq!(config.codegen.schema.as_deref(), Some("api/schema.phx"));
        assert_eq!(config.codegen.target.as_deref(), Some("python"));
        assert_eq!(config.codegen.out_dir.as_deref(), Some("./out"));
        assert_eq!(config.codegen.mode.as_deref(), Some("server"));
    }

    #[test]
    fn parse_multi_target_config() {
        let config: PhoenixConfig = toml::from_str(
            r#"
[gen]
schema = "api/schema.phx"

[gen.targets.typescript]
out_dir = "frontend/src/generated"
mode = "client"

[gen.targets.python]
out_dir = "backend/generated"
mode = "server"

[gen.targets.openapi]
out_dir = "docs"
"#,
        )
        .unwrap();
        let targets = config.codegen.targets.as_ref().unwrap();
        assert_eq!(targets.len(), 3);

        let ts = &targets["typescript"];
        assert_eq!(ts.out_dir.as_deref(), Some("frontend/src/generated"));
        assert_eq!(ts.mode.as_deref(), Some("client"));

        let py = &targets["python"];
        assert_eq!(py.out_dir.as_deref(), Some("backend/generated"));
        assert_eq!(py.mode.as_deref(), Some("server"));

        let oa = &targets["openapi"];
        assert_eq!(oa.out_dir.as_deref(), Some("docs"));
        assert!(oa.mode.is_none());
    }

    #[test]
    fn resolve_single_target() {
        let config: PhoenixConfig = toml::from_str(
            r#"
[gen]
target = "go"
out_dir = "./out"
mode = "client"
"#,
        )
        .unwrap();
        let resolved = config.codegen.resolve_targets().unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].target, "go");
        assert_eq!(resolved[0].out_dir, "./out");
        assert_eq!(resolved[0].mode.as_deref(), Some("client"));
    }

    #[test]
    fn resolve_multi_targets() {
        let config: PhoenixConfig = toml::from_str(
            r#"
[gen]
schema = "api.phx"

[gen.targets.typescript]
out_dir = "ts_out"

[gen.targets.python]
out_dir = "py_out"
mode = "server"
"#,
        )
        .unwrap();
        let resolved = config.codegen.resolve_targets().unwrap();
        assert_eq!(resolved.len(), 2);
        // Sorted alphabetically
        assert_eq!(resolved[0].target, "python");
        assert_eq!(resolved[0].out_dir, "py_out");
        assert_eq!(resolved[0].mode.as_deref(), Some("server"));
        assert_eq!(resolved[1].target, "typescript");
        assert_eq!(resolved[1].out_dir, "ts_out");
    }

    #[test]
    fn resolve_multi_targets_inherits_top_level_mode() {
        let config: PhoenixConfig = toml::from_str(
            r#"
[gen]
mode = "client"

[gen.targets.typescript]
out_dir = "ts"

[gen.targets.go]
out_dir = "go"
mode = "server"
"#,
        )
        .unwrap();
        let resolved = config.codegen.resolve_targets().unwrap();
        // go has explicit mode override
        assert_eq!(resolved[0].target, "go");
        assert_eq!(resolved[0].mode.as_deref(), Some("server"));
        // typescript inherits top-level mode
        assert_eq!(resolved[1].target, "typescript");
        assert_eq!(resolved[1].mode.as_deref(), Some("client"));
    }

    #[test]
    fn resolve_multi_targets_framework_override_and_inherit() {
        let config: PhoenixConfig = toml::from_str(
            r#"
[gen]
framework = "express"

[gen.targets.typescript]
out_dir = "ts"

[gen.targets.go]
out_dir = "go"
framework = "chi"
"#,
        )
        .unwrap();
        let resolved = config.codegen.resolve_targets().unwrap();
        // go carries its explicit per-target framework override; `framework_explicit`
        // marks it as bound to this target, so the driver validates it strictly.
        assert_eq!(resolved[0].target, "go");
        assert_eq!(resolved[0].framework.as_deref(), Some("chi"));
        assert!(resolved[0].framework_explicit);
        // typescript has no per-target value, so it inherits the top-level default —
        // a broadcast value (`framework_explicit == false`), which the driver
        // therefore tolerates rather than rejecting if a target can't use it.
        assert_eq!(resolved[1].target, "typescript");
        assert_eq!(resolved[1].framework.as_deref(), Some("express"));
        assert!(!resolved[1].framework_explicit);
    }

    #[test]
    fn resolve_single_target_inherits_top_level_framework() {
        let config: PhoenixConfig = toml::from_str(
            r#"
[gen]
schema = "api.phx"
target = "typescript"
framework = "fastify"
"#,
        )
        .unwrap();
        let resolved = config.codegen.resolve_targets().unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].framework.as_deref(), Some("fastify"));
        // A single-target config's framework is bound to that one target, so it's
        // explicit — the driver validates it strictly even with no CLI override.
        assert!(resolved[0].framework_explicit);
    }

    #[test]
    fn resolve_multi_targets_default_out_dir() {
        let config: PhoenixConfig = toml::from_str(
            r#"
[gen]
out_dir = "./build"

[gen.targets.typescript]

[gen.targets.python]
out_dir = "custom"
"#,
        )
        .unwrap();
        let resolved = config.codegen.resolve_targets().unwrap();
        // python has explicit out_dir
        assert_eq!(resolved[0].out_dir, "custom");
        // typescript gets default: {out_dir}/{target}
        assert_eq!(resolved[1].out_dir, "./build/typescript");
    }

    #[test]
    fn resolve_no_targets_no_target_returns_none() {
        let config: PhoenixConfig = toml::from_str("[gen]\nschema = \"a.phx\"\n").unwrap();
        assert!(config.codegen.resolve_targets().is_none());
    }

    #[test]
    fn unknown_field_rejected() {
        let result: Result<PhoenixConfig, _> = toml::from_str(
            r#"
[gen]
targt = "python"
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn unknown_top_level_field_rejected() {
        let result: Result<PhoenixConfig, _> = toml::from_str("foo = 1\n");
        assert!(result.is_err());
    }

    #[test]
    fn unknown_target_config_field_rejected() {
        let result: Result<PhoenixConfig, _> = toml::from_str(
            r#"
[gen.targets.typescript]
outdir = "wrong"
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn partial_config_ok() {
        let config: PhoenixConfig = toml::from_str(
            r#"
[gen]
target = "go"
"#,
        )
        .unwrap();
        assert_eq!(config.codegen.target.as_deref(), Some("go"));
        assert!(config.codegen.schema.is_none());
        assert!(config.codegen.out_dir.is_none());
        assert!(config.codegen.mode.is_none());
    }

    #[test]
    fn find_and_load_returns_none_for_empty_dir() {
        let dir = std::env::temp_dir().join("phoenix_config_test_empty");
        let _ = std::fs::create_dir_all(&dir);
        let result = PhoenixConfig::find_and_load(&dir).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn find_and_load_reads_file() {
        let dir = std::env::temp_dir().join("phoenix_config_test_load");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("phoenix.toml"), "[gen]\ntarget = \"python\"\n").unwrap();
        let config = PhoenixConfig::find_and_load(&dir).unwrap().unwrap();
        assert_eq!(config.codegen.target.as_deref(), Some("python"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── [package] + [dependencies] schema ──────────────────

    #[test]
    fn parse_package_section() {
        let config: PhoenixConfig = toml::from_str(
            r#"
[package]
name = "my-app"
version = "0.1.0"
description = "An example"
authors = ["Ada <ada@example.com>"]
license = "MIT"
"#,
        )
        .unwrap();
        let pkg = config.package.as_ref().expect("package present");
        assert_eq!(pkg.name, "my-app");
        assert_eq!(pkg.version, "0.1.0");
        assert_eq!(pkg.description.as_deref(), Some("An example"));
        assert_eq!(
            pkg.authors.as_deref(),
            Some(&["Ada <ada@example.com>".to_string()][..])
        );
        assert_eq!(pkg.license.as_deref(), Some("MIT"));
        pkg.validate().expect("valid package");
    }

    #[test]
    fn package_and_gen_coexist() {
        // The [gen] section must keep parsing alongside [package]/[dependencies].
        let config: PhoenixConfig = toml::from_str(
            r#"
[package]
name = "svc"
version = "1.0.0"

[dependencies]
util = { path = "../util" }

[gen]
schema = "api.phx"
target = "go"
"#,
        )
        .unwrap();
        assert_eq!(config.package.as_ref().unwrap().name, "svc");
        assert_eq!(config.codegen.target.as_deref(), Some("go"));
        let deps = config.dependencies().unwrap();
        assert!(deps.contains_key("util"));
    }

    #[test]
    fn package_missing_name_is_parse_error() {
        let result: Result<PhoenixConfig, _> = toml::from_str("[package]\nversion = \"1.0.0\"\n");
        assert!(result.is_err());
    }

    #[test]
    fn package_unknown_field_rejected() {
        let result: Result<PhoenixConfig, _> =
            toml::from_str("[package]\nname = \"x\"\nversion = \"1.0.0\"\nfoo = 1\n");
        assert!(result.is_err());
    }

    // Exhaustive `parse_dependency` and `PackageConfig::validate` cases live in
    // `manifest.rs`; these tests cover only the wiring from a parsed
    // `PhoenixConfig` through `dependencies()` and `load_file` validation.

    #[test]
    fn dependencies_method_maps_raw_values_to_typed() {
        let config: PhoenixConfig = toml::from_str(
            r#"
[dependencies]
http = { git = "https://example.com/http.git", tag = "v1.2.3" }
util = { path = "../util" }
"#,
        )
        .unwrap();
        let deps = config.dependencies().unwrap();
        assert_eq!(
            deps["http"],
            Dependency::Git {
                url: "https://example.com/http.git".to_string(),
                reference: crate::manifest::GitRef::Tag("v1.2.3".to_string()),
            }
        );
        assert_eq!(
            deps["util"],
            Dependency::Path {
                path: "../util".to_string()
            }
        );
    }

    #[test]
    fn dependencies_method_surfaces_first_error() {
        let config: PhoenixConfig =
            toml::from_str("[dependencies]\nx = { git = \"u\", path = \"p\" }\n").unwrap();
        assert!(matches!(
            config.dependencies().unwrap_err(),
            ManifestError::ConflictingSource { .. }
        ));
    }

    #[test]
    fn load_file_rejects_invalid_package_version() {
        // Validation must fire at load time, not stay latent until a consumer
        // happens to call `validate()`.
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("phoenix.toml");
        std::fs::write(&path, "[package]\nname = \"x\"\nversion = \"nope\"\n").unwrap();
        let err = PhoenixConfig::load_file(&path).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::ManifestError(_, inner)
                if matches!(*inner, ManifestError::InvalidPackageVersion(..))
        ));
    }

    #[test]
    fn load_file_rejects_malformed_dependency() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("phoenix.toml");
        std::fs::write(&path, "[dependencies]\nx = { git = \"u\", path = \"p\" }\n").unwrap();
        let err = PhoenixConfig::load_file(&path).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::ManifestError(_, inner)
                if matches!(*inner, ManifestError::ConflictingSource { .. })
        ));
    }

    #[test]
    fn load_file_accepts_gen_only_manifest() {
        // The common existing case: a schema-only project with no [package]
        // and no [dependencies]. Manifest validation must accept it (nothing
        // to validate) rather than a future `validate_manifest` change
        // accidentally rejecting a package-less config.
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("phoenix.toml");
        std::fs::write(&path, "[gen]\nschema = \"api.phx\"\ntarget = \"go\"\n").unwrap();
        let config = PhoenixConfig::load_file(&path).expect("gen-only manifest loads");
        assert!(config.package.is_none());
        assert!(config.dependencies().unwrap().is_empty());
        assert_eq!(config.codegen.target.as_deref(), Some("go"));
    }

    #[test]
    fn find_with_path_walks_up_and_returns_manifest_path() {
        // The config lives at an ancestor of the start dir; `find_with_path`
        // must walk up to it and return that file's path (so callers can
        // resolve relative `path` deps and write the lockfile beside it).
        let root = tempfile::tempdir().expect("create tempdir");
        let manifest = root.path().join("phoenix.toml");
        std::fs::write(&manifest, "[package]\nname = \"x\"\nversion = \"1.0.0\"\n").unwrap();
        let nested = root.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();

        let (config, found) = PhoenixConfig::find_with_path(&nested)
            .expect("load succeeds")
            .expect("manifest found");
        assert_eq!(config.package.as_ref().unwrap().name, "x");
        // Canonicalize both sides: tempdirs on macOS resolve through /private.
        assert_eq!(
            std::fs::canonicalize(&found).unwrap(),
            std::fs::canonicalize(&manifest).unwrap()
        );
    }

    #[test]
    fn find_with_path_returns_none_when_absent() {
        // No `phoenix.toml` anywhere up the walk → `Ok(None)`, the normal case
        // for projects that don't use a config file.
        let root = tempfile::tempdir().expect("create tempdir");
        let nested = root.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(
            PhoenixConfig::find_with_path(&nested)
                .expect("walk succeeds")
                .is_none()
        );
    }
}
