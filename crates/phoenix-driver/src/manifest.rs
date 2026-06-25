//! Package-manager manifest model for `phoenix.toml`.
//!
//! Models the `[package]` and `[dependencies]` sections — distinct from the
//! `[gen]` code-generation config in [`crate::config`], which they share only a
//! file with. Parsing here turns raw TOML values into typed [`PackageConfig`]
//! and [`Dependency`] values, rejecting malformed input with precise
//! [`ManifestError`] diagnostics rather than serde's generic messages.

use serde::Deserialize;

/// The `[package]` section of `phoenix.toml`.
///
/// ```toml
/// [package]
/// name = "my-app"
/// version = "0.1.0"
/// description = "An example Phoenix package"   # optional
/// authors = ["Ada <ada@example.com>"]          # optional
/// license = "MIT"                              # optional
/// ```
///
/// `name` and `version` are required whenever a `[package]` table is present;
/// a missing one is a parse error (serde "missing field"). `version` must be a
/// valid semver string, checked by [`PackageConfig::validate`].
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageConfig {
    /// The package name. Cross-package imports match an import's first path
    /// segment against the declared dependency names, not against this field;
    /// this names the package for diagnostics and (eventually) publishing.
    pub name: String,
    /// The package version (semver). Validated by [`PackageConfig::validate`].
    pub version: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Optional author list.
    pub authors: Option<Vec<String>>,
    /// Optional SPDX license expression.
    pub license: Option<String>,
}

impl PackageConfig {
    /// Validates the package metadata: `name` non-empty and `version` a
    /// well-formed semver string. Returns a [`ManifestError`] describing the
    /// first problem found.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.name.trim().is_empty() {
            return Err(ManifestError::EmptyPackageName);
        }
        semver::Version::parse(&self.version).map_err(|e| {
            ManifestError::InvalidPackageVersion(self.version.clone(), e.to_string())
        })?;
        Ok(())
    }
}

/// A single resolved dependency declaration from `[dependencies]`.
///
/// Supports two source kinds. A bare semver string
/// (`dep = "^1.2"`) is reserved for a future central registry and, until then,
/// is rejected with a clear "no registry configured" error rather than being
/// silently accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dependency {
    /// A git source: a clone URL plus the ref to check out.
    Git {
        /// The clone URL.
        url: String,
        /// Which commit/tag/branch to check out.
        reference: GitRef,
    },
    /// A local filesystem path source (relative to the manifest directory).
    /// Invaluable for local dev, monorepos, and testing the resolver itself.
    Path {
        /// The path as written in the manifest (resolved relative to the
        /// manifest's directory by the resolver).
        path: String,
    },
}

/// Which revision of a git dependency to check out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRef {
    /// A specific tag (`tag = "v1.2.3"`).
    Tag(String),
    /// A specific commit SHA (`rev = "abc123"`).
    Rev(String),
    /// A branch (`branch = "main"`); the resolved commit is pinned in the lockfile.
    Branch(String),
    /// No ref specified — the remote's default branch HEAD.
    DefaultBranch,
}

/// Errors produced while validating the manifest beyond raw TOML parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// A `[dependencies]` entry had an empty or whitespace-only name.
    EmptyDependencyName,
    /// `[package] name` is empty or whitespace-only.
    EmptyPackageName,
    /// `[package] version` is not a valid semver string.
    InvalidPackageVersion(String, String),
    /// A dependency value was a bare string (reserved for the future registry).
    RegistryUnsupported {
        /// The dependency name.
        name: String,
        /// The version string the user wrote.
        version: String,
    },
    /// A dependency declared neither `git` nor `path`.
    MissingSource {
        /// The dependency name.
        name: String,
    },
    /// A dependency declared both `git` and `path` (mutually exclusive).
    ConflictingSource {
        /// The dependency name.
        name: String,
    },
    /// A git dependency declared more than one of `tag` / `rev` / `branch`.
    MultipleGitRefs {
        /// The dependency name.
        name: String,
    },
    /// A `tag` / `rev` / `branch` key was used on a non-git (path) dependency.
    GitRefOnPathDep {
        /// The dependency name.
        name: String,
    },
    /// A dependency table contained an unrecognized key.
    UnknownDependencyKey {
        /// The dependency name.
        name: String,
        /// The offending key.
        key: String,
    },
    /// A recognized key (`git` / `path` / `tag` / `rev` / `branch`) was present
    /// but its value was not a string (e.g. `tag = 123`). Caught so a mistyped
    /// value is loud rather than silently coerced to "absent".
    NonStringValue {
        /// The dependency name.
        name: String,
        /// The offending key.
        key: String,
    },
    /// A dependency value was neither a string nor a table.
    InvalidDependencyValue {
        /// The dependency name.
        name: String,
    },
    /// A source key (`git` / `path`) was present but its value was empty.
    EmptySource {
        /// The dependency name.
        name: String,
        /// The offending key (`git` or `path`).
        key: String,
    },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::EmptyDependencyName => {
                write!(f, "`[dependencies]` contains an entry with an empty name")
            }
            ManifestError::EmptyPackageName => {
                write!(f, "`[package] name` must not be empty")
            }
            ManifestError::InvalidPackageVersion(v, e) => {
                write!(f, "`[package] version` '{v}' is not valid semver: {e}")
            }
            ManifestError::RegistryUnsupported { name, version } => write!(
                f,
                "dependency `{name} = \"{version}\"` uses a bare version string, which requires a \
                 package registry — none is configured yet. Use a git source \
                 (`{name} = {{ git = \"...\", tag = \"...\" }}`) or a local path \
                 (`{name} = {{ path = \"../{name}\" }}`) instead."
            ),
            ManifestError::MissingSource { name } => write!(
                f,
                "dependency `{name}` must specify a source: either `git = \"...\"` or `path = \"...\"`"
            ),
            ManifestError::ConflictingSource { name } => write!(
                f,
                "dependency `{name}` specifies both `git` and `path`; choose exactly one source"
            ),
            ManifestError::MultipleGitRefs { name } => write!(
                f,
                "dependency `{name}` specifies more than one of `tag` / `rev` / `branch`; choose one"
            ),
            ManifestError::GitRefOnPathDep { name } => write!(
                f,
                "dependency `{name}` is a path dependency, so `tag` / `rev` / `branch` do not apply"
            ),
            ManifestError::UnknownDependencyKey { name, key } => write!(
                f,
                "dependency `{name}` has an unknown key `{key}` \
                 (expected one of: git, tag, rev, branch, path)"
            ),
            ManifestError::NonStringValue { name, key } => {
                write!(f, "dependency `{name}` key `{key}` must be a string")
            }
            ManifestError::InvalidDependencyValue { name } => write!(
                f,
                "dependency `{name}` must be a table with a `git` or `path` source"
            ),
            ManifestError::EmptySource { name, key } => write!(
                f,
                "dependency `{name}` has an empty `{key}` value; provide a non-empty source"
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

/// Parses one raw `[dependencies]` value into a typed [`Dependency`],
/// rejecting bare registry strings and malformed source tables with precise
/// diagnostics. Pure so it is unit-testable.
pub fn parse_dependency(name: &str, value: &toml::Value) -> Result<Dependency, ManifestError> {
    if name.trim().is_empty() {
        return Err(ManifestError::EmptyDependencyName);
    }
    let table = match value {
        toml::Value::String(version) => {
            return Err(ManifestError::RegistryUnsupported {
                name: name.to_string(),
                version: version.clone(),
            });
        }
        toml::Value::Table(t) => t,
        _ => {
            return Err(ManifestError::InvalidDependencyValue {
                name: name.to_string(),
            });
        }
    };

    // Reject unknown keys and non-string values up front so a typo in a key
    // (`tagg`) or a mistyped value (`tag = 123`) is loud rather than silently
    // dropped by the `as_str()` reads below — which would coerce a non-string
    // value to `None`, indistinguishable from the key being absent.
    for (key, val) in table {
        if !matches!(key.as_str(), "git" | "tag" | "rev" | "branch" | "path") {
            return Err(ManifestError::UnknownDependencyKey {
                name: name.to_string(),
                key: key.clone(),
            });
        }
        if !val.is_str() {
            return Err(ManifestError::NonStringValue {
                name: name.to_string(),
                key: key.clone(),
            });
        }
    }

    // Every present key is now known to be a string, so `as_str()` yields
    // `Some` exactly when the key is present.
    let git = table.get("git").and_then(|v| v.as_str());
    let path = table.get("path").and_then(|v| v.as_str());
    let tag = table.get("tag").and_then(|v| v.as_str());
    let rev = table.get("rev").and_then(|v| v.as_str());
    let branch = table.get("branch").and_then(|v| v.as_str());

    match (git, path) {
        (Some(_), Some(_)) => Err(ManifestError::ConflictingSource {
            name: name.to_string(),
        }),
        (None, None) => Err(ManifestError::MissingSource {
            name: name.to_string(),
        }),
        (None, Some(p)) => {
            // Check the source value itself before secondary concerns, matching
            // the git branch's "is the source valid?"-first ordering.
            if p.trim().is_empty() {
                return Err(ManifestError::EmptySource {
                    name: name.to_string(),
                    key: "path".to_string(),
                });
            }
            if tag.is_some() || rev.is_some() || branch.is_some() {
                return Err(ManifestError::GitRefOnPathDep {
                    name: name.to_string(),
                });
            }
            // Store trimmed: surrounding whitespace already failed the
            // empty-check above, so keeping it would only produce a confusing
            // path-resolution error downstream.
            Ok(Dependency::Path {
                path: p.trim().to_string(),
            })
        }
        (Some(url), None) => {
            if url.trim().is_empty() {
                return Err(ManifestError::EmptySource {
                    name: name.to_string(),
                    key: "git".to_string(),
                });
            }
            let reference = match (tag, rev, branch) {
                (Some(t), None, None) => GitRef::Tag(t.to_string()),
                (None, Some(r), None) => GitRef::Rev(r.to_string()),
                (None, None, Some(b)) => GitRef::Branch(b.to_string()),
                (None, None, None) => GitRef::DefaultBranch,
                _ => {
                    return Err(ManifestError::MultipleGitRefs {
                        name: name.to_string(),
                    });
                }
            };
            // Trimmed for the same reason as the path source above: a clone
            // URL with surrounding whitespace is never intended.
            Ok(Dependency::Git {
                url: url.trim().to_string(),
                reference,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a single `[dependencies]` entry's value from a TOML fragment and
    /// runs it through [`parse_dependency`], so each test exercises the parser
    /// directly rather than threading through the whole [`PhoenixConfig`].
    fn parse(name: &str, toml_value: &str) -> Result<Dependency, ManifestError> {
        let value: toml::Value = toml::from_str(&format!("v = {toml_value}")).unwrap();
        let inner = value.as_table().unwrap().get("v").unwrap().clone();
        parse_dependency(name, &inner)
    }

    fn pkg(name: &str, version: &str) -> PackageConfig {
        PackageConfig {
            name: name.to_string(),
            version: version.to_string(),
            ..Default::default()
        }
    }

    // ── PackageConfig::validate ────────────────────────────────────────

    #[test]
    fn package_validate_accepts_valid() {
        pkg("my-app", "0.1.0").validate().expect("valid package");
    }

    #[test]
    fn package_validate_rejects_invalid_version() {
        assert!(matches!(
            pkg("x", "not-a-version").validate().unwrap_err(),
            ManifestError::InvalidPackageVersion(..)
        ));
    }

    #[test]
    fn package_validate_rejects_empty_name() {
        assert_eq!(
            pkg("  ", "1.0.0").validate().unwrap_err(),
            ManifestError::EmptyPackageName
        );
    }

    // ── parse_dependency: git sources ──────────────────────────────────

    #[test]
    fn git_with_tag() {
        assert_eq!(
            parse(
                "http",
                r#"{ git = "https://example.com/http.git", tag = "v1.2.3" }"#
            )
            .unwrap(),
            Dependency::Git {
                url: "https://example.com/http.git".to_string(),
                reference: GitRef::Tag("v1.2.3".to_string()),
            }
        );
    }

    #[test]
    fn git_with_rev() {
        assert_eq!(
            parse("http", r#"{ git = "u", rev = "abc123" }"#).unwrap(),
            Dependency::Git {
                url: "u".to_string(),
                reference: GitRef::Rev("abc123".to_string()),
            }
        );
    }

    #[test]
    fn git_with_branch() {
        assert_eq!(
            parse("http", r#"{ git = "u", branch = "main" }"#).unwrap(),
            Dependency::Git {
                url: "u".to_string(),
                reference: GitRef::Branch("main".to_string()),
            }
        );
    }

    #[test]
    fn git_default_branch() {
        assert_eq!(
            parse("http", r#"{ git = "https://example.com/http.git" }"#).unwrap(),
            Dependency::Git {
                url: "https://example.com/http.git".to_string(),
                reference: GitRef::DefaultBranch,
            }
        );
    }

    // ── parse_dependency: path sources ─────────────────────────────────

    #[test]
    fn path_source() {
        assert_eq!(
            parse("util", r#"{ path = "../util" }"#).unwrap(),
            Dependency::Path {
                path: "../util".to_string()
            }
        );
    }

    // ── parse_dependency: rejections ───────────────────────────────────

    #[test]
    fn bare_string_is_registry_error() {
        let err = parse("http", r#""^1.2""#).unwrap_err();
        match &err {
            ManifestError::RegistryUnsupported { name, version } => {
                assert_eq!(name, "http");
                assert_eq!(version, "^1.2");
            }
            other => panic!("expected RegistryUnsupported, got {other:?}"),
        }
        // The message must clearly say no registry is configured.
        assert!(err.to_string().contains("registry"));
    }

    #[test]
    fn non_table_non_string_value_rejected() {
        // A dependency value that is neither a string nor a table (here an
        // integer) is an outright InvalidDependencyValue, not silently ignored.
        assert!(matches!(
            parse("x", "123").unwrap_err(),
            ManifestError::InvalidDependencyValue { .. }
        ));
    }

    #[test]
    fn conflicting_source_rejected() {
        assert!(matches!(
            parse("x", r#"{ git = "u", path = "p" }"#).unwrap_err(),
            ManifestError::ConflictingSource { .. }
        ));
    }

    #[test]
    fn multiple_git_refs_rejected() {
        assert!(matches!(
            parse("x", r#"{ git = "u", tag = "t", branch = "b" }"#).unwrap_err(),
            ManifestError::MultipleGitRefs { .. }
        ));
    }

    #[test]
    fn git_ref_on_path_dep_rejected() {
        assert!(matches!(
            parse("x", r#"{ path = "p", tag = "t" }"#).unwrap_err(),
            ManifestError::GitRefOnPathDep { .. }
        ));
    }

    #[test]
    fn missing_source_rejected() {
        // `tag` alone is a recognized key, so this is specifically MissingSource
        // rather than an unknown-key error.
        assert!(matches!(
            parse("x", r#"{ tag = "t" }"#).unwrap_err(),
            ManifestError::MissingSource { .. }
        ));
    }

    #[test]
    fn unknown_key_rejected() {
        assert!(matches!(
            parse("x", r#"{ git = "u", tagg = "t" }"#).unwrap_err(),
            ManifestError::UnknownDependencyKey { .. }
        ));
    }

    #[test]
    fn non_string_ref_value_rejected() {
        // Regression guard: a recognized key with a non-string value (here
        // `tag = 123`) must error loudly, not be coerced to "absent" and
        // silently resolve to the default branch.
        assert!(matches!(
            parse("x", r#"{ git = "u", tag = 123 }"#).unwrap_err(),
            ManifestError::NonStringValue { key, .. } if key == "tag"
        ));
    }

    #[test]
    fn non_string_source_value_rejected() {
        assert!(matches!(
            parse("x", "{ git = 123 }").unwrap_err(),
            ManifestError::NonStringValue { key, .. } if key == "git"
        ));
    }

    #[test]
    fn empty_git_source_rejected() {
        assert!(matches!(
            parse("x", r#"{ git = "" }"#).unwrap_err(),
            ManifestError::EmptySource { key, .. } if key == "git"
        ));
    }

    #[test]
    fn empty_path_source_rejected() {
        assert!(matches!(
            parse("x", r#"{ path = "   " }"#).unwrap_err(),
            ManifestError::EmptySource { key, .. } if key == "path"
        ));
    }

    #[test]
    fn empty_dependency_name_rejected() {
        // A whitespace-only `[dependencies]` key (TOML permits `"" = ...`) can
        // never match an import segment; reject it for parity with the
        // non-empty `[package] name` rule rather than storing an unusable dep.
        assert_eq!(
            parse("  ", r#"{ path = "../util" }"#).unwrap_err(),
            ManifestError::EmptyDependencyName
        );
    }

    #[test]
    fn source_values_are_trimmed() {
        // Surrounding whitespace passes the non-empty check but is never
        // intended, so the stored source is trimmed (a leading-space clone URL
        // or path would otherwise fail confusingly downstream).
        assert_eq!(
            parse("http", r#"{ git = "  https://example.com/http.git  " }"#).unwrap(),
            Dependency::Git {
                url: "https://example.com/http.git".to_string(),
                reference: GitRef::DefaultBranch,
            }
        );
        assert_eq!(
            parse("util", r#"{ path = "  ../util  " }"#).unwrap(),
            Dependency::Path {
                path: "../util".to_string()
            }
        );
    }
}
