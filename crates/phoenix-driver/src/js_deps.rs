//! `[js-dependencies]` support (the BYO npm model): generate a
//! `package.json` for the developer's own `npm install`, and diagnose an
//! `extern js "pkg"` whose module isn't declared.
//!
//! Phoenix fetches and bundles nothing — it only records the intended packages
//! and hands the developer a `package.json` so `npm install` (in the build
//! output directory, beside the generated glue) resolves the glue's imports.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The distinct **named** host modules the entry package itself binds via
/// `extern js "pkg" { ... }` — every extern module the entry package
/// declares, except the ambient `js` (which is never an npm dependency).
///
/// `analysis.module.extern_functions` merges declarations from the whole
/// resolved module graph, including any dependency packages' own modules
/// (see `ModulePath::in_package` / `docs/design-decisions.md` §Phase 3.1 E).
/// A dependency's `extern js "pkg"` binding is *that package's* concern — its
/// own manifest, if it has one, is where `pkg` belongs — so it's filtered out
/// here rather than surfaced as an "undeclared" warning against the entry
/// package's `phoenix.toml`, which never referenced it.
pub fn used_js_modules(analysis: &phoenix_sema::Analysis) -> BTreeSet<String> {
    analysis
        .module
        .extern_functions
        .values()
        .filter(|info| !info.def_module.in_dependency_package())
        .filter_map(|info| info.extern_js.as_ref())
        .filter(|(module, _)| module != "js")
        .map(|(module, _)| module.clone())
        .collect()
}

/// The used npm modules not declared in `[js-dependencies]`, sorted — each one
/// warrants a warning (a likely typo, or a missing manifest entry that would
/// leave the generated `package.json` unable to resolve the glue's import).
pub fn undeclared_js_modules(
    used: &BTreeSet<String>,
    declared: &BTreeMap<String, String>,
) -> Vec<String> {
    used.iter()
        .filter(|m| !declared.contains_key(*m))
        .cloned()
        .collect()
}

/// The `package.json` text generated from `[js-dependencies]`. `"type":
/// "module"` so the ESM glue's `import` resolves; dependency specs are taken
/// verbatim (Phoenix does not interpret npm semver — the BYO model).
pub fn package_json_contents(deps: &BTreeMap<String, String>) -> String {
    let value = serde_json::json!({
        "type": "module",
        "dependencies": deps,
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&value).expect("package.json value serializes")
    )
}

/// Write `package.json` into `dir` from `deps`, **only if one is not already
/// present** — never clobber a developer-owned file (it may hold scripts /
/// devDependencies). Returns `Ok(true)` if written, `Ok(false)` if skipped
/// (already present, or `deps` empty).
pub fn write_package_json_if_absent(
    dir: &Path,
    deps: &BTreeMap<String, String>,
) -> std::io::Result<bool> {
    if deps.is_empty() {
        return Ok(false);
    }
    let path = dir.join("package.json");
    if path.exists() {
        return Ok(false);
    }
    std::fs::write(&path, package_json_contents(deps))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn package_json_has_type_module_and_sorted_deps() {
        let json = package_json_contents(&deps(&[("left-pad", "^1.3.0"), ("chalk", "^5")]));
        assert!(json.contains("\"type\": \"module\""), "{json}");
        assert!(json.contains("\"left-pad\": \"^1.3.0\""), "{json}");
        assert!(json.contains("\"chalk\": \"^5\""), "{json}");
        // BTreeMap ⇒ deterministic order (chalk before left-pad).
        assert!(json.find("chalk").unwrap() < json.find("left-pad").unwrap());
        assert!(json.ends_with("\n"));
        // Valid JSON.
        let _: serde_json::Value = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn write_is_skipped_when_present_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        // Empty deps → nothing written.
        assert!(!write_package_json_if_absent(dir.path(), &deps(&[])).unwrap());
        assert!(!dir.path().join("package.json").exists());
        // First write lands.
        assert!(write_package_json_if_absent(dir.path(), &deps(&[("left-pad", "^1")])).unwrap());
        let first = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        // A pre-existing package.json is never clobbered.
        assert!(!write_package_json_if_absent(dir.path(), &deps(&[("other", "^2")])).unwrap());
        let after = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        assert_eq!(first, after, "existing package.json must be left untouched");
    }

    #[test]
    fn undeclared_modules_are_those_used_but_not_declared() {
        let used: BTreeSet<String> = ["left-pad", "chalk"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let declared = deps(&[("left-pad", "^1")]);
        assert_eq!(
            undeclared_js_modules(&used, &declared),
            vec!["chalk".to_string()]
        );
    }
}
