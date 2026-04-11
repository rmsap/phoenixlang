//! Configuration file support for Phoenix projects.
//!
//! Loads `phoenix.toml` from the current directory or any ancestor,
//! providing default values for the `gen` subcommand so that users
//! don't need to pass `--target`, `--out`, etc. every time.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Top-level `phoenix.toml` configuration.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhoenixConfig {
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
    ParseError(PathBuf, toml::de::Error),
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
        let mut dir = start_dir.to_path_buf();
        loop {
            let candidate = dir.join("phoenix.toml");
            if candidate.is_file() {
                let contents = std::fs::read_to_string(&candidate)
                    .map_err(|e| ConfigError::ReadError(candidate.clone(), e))?;
                let config: PhoenixConfig =
                    toml::from_str(&contents).map_err(|e| ConfigError::ParseError(candidate, e))?;
                return Ok(Some(config));
            }
            if !dir.pop() {
                break;
            }
        }
        Ok(None)
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
}
