//! Format-preserving edits to `phoenix.toml` for `phoenix add`.
//!
//! Uses `toml_edit` so a developer's comments, key ordering, and whitespace
//! survive an edit — unlike a parse-into-`PhoenixConfig`-then-reserialize round
//! trip, which would discard all of that.

use toml_edit::{DocumentMut, InlineTable, Item, Table, Value};

use crate::manifest::{Dependency, GitRef};

/// Errors from editing a manifest document.
#[derive(Debug)]
pub enum EditError {
    /// The manifest text was not valid TOML.
    Parse(Box<toml_edit::TomlError>),
    /// A `[dependencies]` entry exists but is not a table (e.g. someone wrote
    /// `dependencies = 3`), so a dependency can't be inserted into it.
    DependenciesNotATable,
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::Parse(e) => write!(f, "could not parse phoenix.toml: {e}"),
            EditError::DependenciesNotATable => {
                write!(f, "`[dependencies]` in phoenix.toml is not a table")
            }
        }
    }
}

impl std::error::Error for EditError {}

/// Insert or replace `name` in the `[dependencies]` table of `manifest_text`,
/// returning the edited document text with all surrounding formatting intact.
///
/// The dependency is written as an inline table — `dep = { git = "…", tag = "…" }`
/// or `dep = { path = "…" }` — matching `phoenix.toml.example`. A `DefaultBranch`
/// git ref writes only `git`, with no `tag`/`rev`/`branch` key. Re-adding an
/// existing name overwrites its entry (upsert).
pub fn upsert_dependency(
    manifest_text: &str,
    name: &str,
    dep: &Dependency,
) -> Result<String, EditError> {
    let mut doc = manifest_text
        .parse::<DocumentMut>()
        .map_err(|e| EditError::Parse(Box::new(e)))?;

    // Ensure a `[dependencies]` table exists, creating it if absent. An existing
    // non-table value is a malformed manifest we refuse to clobber blindly.
    let deps_item = doc
        .as_table_mut()
        .entry("dependencies")
        .or_insert_with(|| Item::Table(Table::new()));
    let deps = deps_item
        .as_table_mut()
        .ok_or(EditError::DependenciesNotATable)?;

    deps.insert(
        name,
        Item::Value(Value::InlineTable(dependency_inline(dep))),
    );
    Ok(doc.to_string())
}

/// Render a [`Dependency`] as the inline table written into `[dependencies]`.
fn dependency_inline(dep: &Dependency) -> InlineTable {
    let mut table = InlineTable::new();
    match dep {
        Dependency::Git { url, reference } => {
            table.insert("git", url.as_str().into());
            match reference {
                GitRef::Tag(t) => {
                    table.insert("tag", t.as_str().into());
                }
                GitRef::Branch(b) => {
                    table.insert("branch", b.as_str().into());
                }
                GitRef::Rev(r) => {
                    table.insert("rev", r.as_str().into());
                }
                // No ref key — the remote's default branch.
                GitRef::DefaultBranch => {}
            }
        }
        Dependency::Path { path } => {
            table.insert("path", path.as_str().into());
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(url: &str, reference: GitRef) -> Dependency {
        Dependency::Git {
            url: url.to_string(),
            reference,
        }
    }

    #[test]
    fn adds_dependencies_section_when_absent() {
        let text = "[package]\nname = \"app\"\nversion = \"0.1.0\"\n";
        let out = upsert_dependency(
            text,
            "http",
            &git("https://example.com/http.git", GitRef::Tag("v1.2.0".into())),
        )
        .unwrap();
        assert!(out.contains("[dependencies]"), "{out}");
        assert!(
            out.contains(r#"http = { git = "https://example.com/http.git", tag = "v1.2.0" }"#),
            "{out}"
        );
        // The result must reparse and round-trip through the manifest schema.
        let cfg: crate::config::PhoenixConfig = toml::from_str(&out).unwrap();
        assert!(cfg.dependencies().unwrap().contains_key("http"));
    }

    #[test]
    fn preserves_comments_and_existing_entries() {
        let text = "\
# my project
[package]
name = \"app\"      # the app
version = \"0.1.0\"

[dependencies]
# pinned for now
util = { path = \"../util\" }
";
        let out =
            upsert_dependency(text, "io", &git("u/io.git", GitRef::Branch("main".into()))).unwrap();
        // Comments and the prior dependency survive verbatim.
        assert!(out.contains("# my project"), "{out}");
        assert!(out.contains("# the app"), "{out}");
        assert!(out.contains("# pinned for now"), "{out}");
        assert!(out.contains(r#"util = { path = "../util" }"#), "{out}");
        // And the new dep is appended.
        assert!(
            out.contains(r#"io = { git = "u/io.git", branch = "main" }"#),
            "{out}"
        );
    }

    #[test]
    fn upsert_overwrites_existing_dependency() {
        let text = "[dependencies]\nhttp = { git = \"u/http.git\", tag = \"v1.0.0\" }\n";
        let out = upsert_dependency(
            text,
            "http",
            &git("u/http.git", GitRef::Tag("v2.0.0".into())),
        )
        .unwrap();
        assert!(out.contains(r#"tag = "v2.0.0""#), "{out}");
        assert!(!out.contains("v1.0.0"), "old entry must be replaced: {out}");
        // Exactly one `http` entry remains.
        assert_eq!(out.matches("http =").count(), 1, "{out}");
    }

    #[test]
    fn path_dependency_has_no_ref_keys() {
        let out = upsert_dependency(
            "[package]\nname=\"a\"\nversion=\"0.1.0\"\n",
            "util",
            &Dependency::Path {
                path: "../util".into(),
            },
        )
        .unwrap();
        assert!(out.contains(r#"util = { path = "../util" }"#), "{out}");
    }

    #[test]
    fn default_branch_git_writes_no_ref_key() {
        let out = upsert_dependency(
            "[package]\nname=\"a\"\nversion=\"0.1.0\"\n",
            "io",
            &git("u/io.git", GitRef::DefaultBranch),
        )
        .unwrap();
        assert!(out.contains(r#"io = { git = "u/io.git" }"#), "{out}");
        assert!(!out.contains("tag ="), "{out}");
        assert!(!out.contains("branch ="), "{out}");
    }

    #[test]
    fn rejects_non_table_dependencies() {
        let err = upsert_dependency(
            "dependencies = 3\n",
            "x",
            &Dependency::Path { path: "p".into() },
        )
        .unwrap_err();
        assert!(matches!(err, EditError::DependenciesNotATable));
    }
}
