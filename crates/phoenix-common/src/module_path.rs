//! Module-path identity shared across sema, IR, the resolver, and backends.
//!
//! A `ModulePath` is a dotted module identifier — `["models", "user"]` for
//! `models.user` — used to namespace declarations across files in a Phoenix
//! project. The empty path is reserved for the *entry module* (the file
//! passed to `phoenix run` / `phoenix build`).
//!
//! See [docs/design-decisions.md, "Module system"](../../docs/design-decisions.md)
//! for the discovery and resolution rules.

/// A dotted module path.
///
/// `ModulePath::entry()` (the empty path) represents the entry module —
/// declarations there register under their bare name (preserving
/// single-file behavior). All other user modules carry a non-empty path
/// that is folded into the registered name as a `module.path::name`
/// mangled key. See `module_qualify`.
///
/// `ModulePath::builtin()` is a distinct sentinel for compiler-synthesized
/// declarations (`Option`, `Result`, etc.). It is *not* the entry module
/// — `is_entry()` returns `false` for it — but `module_qualify` still
/// produces a bare name so builtins are callable from any module by their
/// short name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModulePath(pub Vec<String>);

/// Sentinel segment used for the builtin module. The angle brackets
/// guarantee the string can never collide with a user-declared module
/// segment (Phoenix identifiers are alphanumeric + `_`).
const BUILTIN_SEGMENT: &str = "<builtin>";

impl ModulePath {
    /// The entry module's path (empty).
    pub fn entry() -> Self {
        ModulePath(Vec::new())
    }

    /// The sentinel path for compiler-synthesized declarations.
    pub fn builtin() -> Self {
        ModulePath(vec![BUILTIN_SEGMENT.to_string()])
    }

    /// True if this path is the entry module's path (empty).
    pub fn is_entry(&self) -> bool {
        self.0.is_empty()
    }

    /// True if this path is the builtin sentinel.
    pub fn is_builtin(&self) -> bool {
        self.0.len() == 1 && self.0[0] == BUILTIN_SEGMENT
    }

    /// Join the segments with `.` for use in user-facing display
    /// (`["a", "b"]` → `"a.b"`).  The entry path returns `""` and the
    /// builtin sentinel returns the literal `"<builtin>"` segment.
    ///
    /// This is the *display* form; for symbol-table keys use
    /// [`module_qualify`], which guarantees a bare-name result for entry
    /// and builtin paths and never collides with an identifier in non-
    /// trivial cases (the `::` separator is illegal in identifiers).
    pub fn dotted(&self) -> String {
        self.0.join(".")
    }
}

impl std::fmt::Display for ModulePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_entry() {
            f.write_str("<entry>")
        } else if self.is_builtin() {
            f.write_str("<builtin>")
        } else {
            f.write_str(&self.dotted())
        }
    }
}

/// Qualify a bare name with a module path for use as a flat-table key.
///
/// `module_qualify(&ModulePath::entry(), "foo")` returns `"foo"` (single-file
/// programs unchanged). `module_qualify(&ModulePath::builtin(), "Option")`
/// also returns `"Option"` so builtins remain accessible by their short
/// name from every module. `module_qualify(&ModulePath(["a", "b"]), "foo")`
/// returns `"a.b::foo"`. The `"::"` separator is chosen so the module
/// segment cannot collide with an identifier (Phoenix identifiers use
/// alphanumerics + `_`, never `:`).
///
/// This is the single source of truth for the mangling rule. Sema's
/// registration pass, the IR lowering pass, and the default-wrapper
/// synthesis pass all route through here.
pub fn module_qualify(module: &ModulePath, name: &str) -> String {
    if module.is_entry() || module.is_builtin() {
        name.to_string()
    } else {
        format!("{}::{}", module.dotted(), name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_dotted_is_empty() {
        assert_eq!(ModulePath::entry().dotted(), "");
    }

    #[test]
    fn entry_displays_as_entry_marker() {
        assert_eq!(ModulePath::entry().to_string(), "<entry>");
    }

    #[test]
    fn nested_path_dotted() {
        assert_eq!(ModulePath(vec!["a".into(), "b".into()]).dotted(), "a.b");
    }

    #[test]
    fn module_qualify_entry_is_bare_name() {
        assert_eq!(module_qualify(&ModulePath::entry(), "foo"), "foo");
    }

    #[test]
    fn module_qualify_nested_uses_double_colon() {
        let mp = ModulePath(vec!["a".into(), "b".into()]);
        assert_eq!(module_qualify(&mp, "foo"), "a.b::foo");
    }

    #[test]
    fn ordering_is_lexicographic() {
        let a = ModulePath(vec!["a".into()]);
        let b = ModulePath(vec!["b".into()]);
        assert!(a < b);
        let entry = ModulePath::entry();
        assert!(entry < a);
    }

    #[test]
    fn builtin_is_distinct_from_entry() {
        let builtin = ModulePath::builtin();
        let entry = ModulePath::entry();
        assert_ne!(builtin, entry);
        assert!(!builtin.is_entry());
        assert!(builtin.is_builtin());
        assert!(!entry.is_builtin());
    }

    #[test]
    fn module_qualify_builtin_is_bare_name() {
        assert_eq!(module_qualify(&ModulePath::builtin(), "Option"), "Option");
    }

    #[test]
    fn builtin_displays_as_builtin_marker() {
        assert_eq!(ModulePath::builtin().to_string(), "<builtin>");
    }
}
