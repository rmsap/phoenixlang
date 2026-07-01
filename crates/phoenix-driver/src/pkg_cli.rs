//! `phoenix init` and `phoenix add` — the package-manager scaffolding and
//! manifest-editing commands.

use std::path::Path;
use std::process;

use crate::config::PhoenixConfig;
use crate::deps;
use crate::manifest::{self, Dependency};

/// Scaffold a new project in the current directory: a `phoenix.toml` with a
/// `[package]` section and a runnable `main.phx` entry file.
///
/// `name` defaults to the current directory's name. Refuses to overwrite an
/// existing `phoenix.toml`; an existing `main.phx` is left untouched.
pub fn cmd_init(name: Option<&str>) {
    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("error: could not determine the current directory: {e}");
        process::exit(1);
    });

    // An explicit `--name` that sanitizes to empty (e.g. `--name "!!!"`) falls
    // back to the directory name, not straight to "app" — hence the filter
    // before the `or_else`, and again after it for an empty directory name.
    let pkg_name = name
        .map(sanitize_package_name)
        .filter(|n| !n.is_empty())
        .or_else(|| {
            cwd.file_name()
                .and_then(|n| n.to_str())
                .map(sanitize_package_name)
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "app".to_string());

    let manifest_path = cwd.join("phoenix.toml");
    if manifest_path.exists() {
        eprintln!(
            "error: phoenix.toml already exists in {} — refusing to overwrite",
            cwd.display()
        );
        process::exit(1);
    }

    let manifest = format!("[package]\nname = \"{pkg_name}\"\nversion = \"0.1.0\"\n");
    write_file_or_exit(&manifest_path, &manifest);

    // The entry file lives beside the manifest: its directory is the entry
    // package's module root, which coincides with the project root here.
    let entry_path = cwd.join("main.phx");
    let created_entry = if entry_path.exists() {
        false
    } else {
        let entry = format!("function main() {{\n  print(\"Hello, {pkg_name}!\")\n}}\n");
        write_file_or_exit(&entry_path, &entry);
        true
    };

    println!("Created phoenix.toml (package `{pkg_name}`)");
    if created_entry {
        println!("Created main.phx");
    } else {
        println!("Kept existing main.phx");
    }
}

/// Add a dependency to `phoenix.toml` and refresh `phoenix.lock`.
///
/// The dependency is validated with the same rules as a hand-written manifest
/// entry, then written (format-preserving) into `[dependencies]`. Resolution
/// runs to refresh the lockfile; on failure (bad URL/ref, version conflict,
/// missing path, …) the manifest edit is **rolled back** to its original bytes
/// and any existing `phoenix.lock` is left untouched, so a failed `add` leaves
/// the project unchanged. On success the manifest gains the entry and
/// `phoenix.lock` may be (re)written to pin the new resolution.
pub fn cmd_add(
    name: &str,
    git: Option<String>,
    tag: Option<String>,
    rev: Option<String>,
    branch: Option<String>,
    path: Option<String>,
) {
    // Validate the requested source through the manifest schema, so the CLI and
    // a hand-written `[dependencies]` entry accept exactly the same things.
    let dep = build_dependency(name, git, tag, rev, branch, path).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        process::exit(1);
    });

    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("error: could not determine the current directory: {e}");
        process::exit(1);
    });
    let (_, manifest_path) = match PhoenixConfig::find_with_path(&cwd) {
        Ok(Some(found)) => found,
        Ok(None) => {
            eprintln!(
                "error: no phoenix.toml found in {} or any parent — run `phoenix init` first",
                cwd.display()
            );
            process::exit(1);
        }
        Err(e) => {
            eprintln!("error: {e}");
            process::exit(1);
        }
    };

    let original = std::fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
        eprintln!("error: could not read {}: {e}", manifest_path.display());
        process::exit(1);
    });

    let edited = deps::edit::upsert_dependency(&original, name, &dep).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        process::exit(1);
    });
    write_file_or_exit(&manifest_path, &edited);

    // Refresh the lockfile. On any resolution failure, restore the original
    // manifest so the add is atomic.
    let manifest_dir = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    match refresh_lock(&manifest_dir, &manifest_path) {
        Ok(lock_changed) => {
            println!("Added `{name}` to phoenix.toml");
            if lock_changed {
                println!("Updated phoenix.lock");
            }
        }
        Err(message) => {
            // Roll back the manifest edit.
            if let Err(restore_err) = std::fs::write(&manifest_path, &original) {
                eprintln!(
                    "error: {message}\n\
                     additionally, restoring {} failed: {restore_err} — \
                     the `{name}` entry may need to be removed by hand",
                    manifest_path.display()
                );
            } else {
                eprintln!("error: {message}\nphoenix.toml was left unchanged");
            }
            process::exit(1);
        }
    }
}

/// Re-read the edited manifest's dependencies and resolve them, refreshing the
/// lockfile. Returns whether the lockfile changed, or an error message.
fn refresh_lock(manifest_dir: &Path, manifest_path: &Path) -> Result<bool, String> {
    let config = PhoenixConfig::load_file(manifest_path).map_err(|e| e.to_string())?;
    let dependencies = config.dependencies().map_err(|e| e.to_string())?;
    let cache_root = deps::resolve::default_cache_root()?;
    let resolution = deps::resolve_project(manifest_dir, &dependencies, &cache_root, false)
        .map_err(|e| e.to_string())?;
    Ok(resolution.lock_changed)
}

/// Build a validated [`Dependency`] from the `add` flags by routing them
/// through the manifest schema's [`manifest::parse_dependency`], so a malformed
/// combination (both `git` and `path`, two refs, no source, …) produces the
/// same diagnostic a hand-written manifest would.
fn build_dependency(
    name: &str,
    git: Option<String>,
    tag: Option<String>,
    rev: Option<String>,
    branch: Option<String>,
    path: Option<String>,
) -> Result<Dependency, manifest::ManifestError> {
    let mut table = toml::map::Map::new();
    let mut put = |key: &str, value: Option<String>| {
        if let Some(v) = value {
            table.insert(key.to_string(), toml::Value::String(v));
        }
    };
    put("git", git);
    put("tag", tag);
    put("rev", rev);
    put("branch", branch);
    put("path", path);
    manifest::parse_dependency(name, &toml::Value::Table(table))
}

/// Sanitize an arbitrary string (a `--name` value or a directory name) into a
/// package name: keep alphanumerics, `-`, and `_`; map any other run to a
/// single `-`; trim leading/trailing `-`.
fn sanitize_package_name(raw: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Write `contents` to `path`, exiting the process with a diagnostic on error.
fn write_file_or_exit(path: &Path, contents: &str) {
    if let Err(e) = std::fs::write(path, contents) {
        eprintln!("error: could not write {}: {e}", path.display());
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::GitRef;

    #[test]
    fn build_dependency_git_with_tag() {
        let dep = build_dependency(
            "http",
            Some("u/http.git".into()),
            Some("v1".into()),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            dep,
            Dependency::Git {
                url: "u/http.git".into(),
                reference: GitRef::Tag("v1".into()),
            }
        );
    }

    #[test]
    fn build_dependency_path() {
        let dep = build_dependency("util", None, None, None, None, Some("../util".into())).unwrap();
        assert_eq!(
            dep,
            Dependency::Path {
                path: "../util".into()
            }
        );
    }

    #[test]
    fn build_dependency_rejects_conflicting_and_missing_sources() {
        // git + path → conflict (reusing the manifest validation).
        assert!(
            build_dependency("x", Some("u".into()), None, None, None, Some("p".into())).is_err()
        );
        // no source at all → missing source.
        assert!(build_dependency("x", None, None, None, None, None).is_err());
        // two refs → multiple refs.
        assert!(
            build_dependency(
                "x",
                Some("u".into()),
                Some("t".into()),
                Some("r".into()),
                None,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn sanitize_package_name_cases() {
        assert_eq!(sanitize_package_name("my-app"), "my-app");
        assert_eq!(sanitize_package_name("My App"), "My-App");
        assert_eq!(sanitize_package_name("weird!!name"), "weird-name");
        assert_eq!(sanitize_package_name("--leading--"), "leading");
        assert_eq!(sanitize_package_name("under_score"), "under_score");
        assert_eq!(sanitize_package_name("!!!"), "");
    }
}
