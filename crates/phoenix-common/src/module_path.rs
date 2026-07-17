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

/// Build the reserved leading segment that marks a module as belonging to
/// dependency package `alias`. Like [`BUILTIN_SEGMENT`], the angle brackets
/// (illegal in a Phoenix identifier) make the marker un-forgeable by any
/// user-declared module segment — so a dependency's module identity can never
/// silently collide with an entry-package module of the same path.
fn package_segment(alias: &str) -> String {
    format!("<pkg:{alias}>")
}

/// If `segment` is a package marker, its inner alias.
fn package_alias(segment: &str) -> Option<&str> {
    segment.strip_prefix("<pkg:")?.strip_suffix('>')
}

impl ModulePath {
    /// The entry module's path (empty).
    pub fn entry() -> Self {
        ModulePath(Vec::new())
    }

    /// The sentinel path for compiler-synthesized declarations.
    pub fn builtin() -> Self {
        ModulePath(vec![BUILTIN_SEGMENT.to_string()])
    }

    /// A module inside dependency package `alias`, at relative module path
    /// `rel` (an empty `rel` is the package's root module). The package is
    /// encoded as a reserved, un-forgeable leading segment, so the identity is
    /// distinct from any entry-package module — realizing the `(package, module
    /// path)` identity (see `docs/design-decisions.md` §Phase 3.1 E). The
    /// marker is invisible in the human display form ([`dotted`](Self::dotted) /
    /// `Display`) but preserved in the symbol-table key ([`module_qualify`]).
    pub fn in_package(alias: &str, rel: &[String]) -> Self {
        let mut segments = Vec::with_capacity(rel.len() + 1);
        segments.push(package_segment(alias));
        segments.extend(rel.iter().cloned());
        ModulePath(segments)
    }

    /// Split a package-qualified path into `(alias, relative segments)`, or
    /// `None` for an entry / local / builtin path.
    fn package_split(&self) -> Option<(&str, &[String])> {
        let alias = package_alias(self.0.first()?)?;
        Some((alias, &self.0[1..]))
    }

    /// True if this path is the entry module's path (empty).
    pub fn is_entry(&self) -> bool {
        self.0.is_empty()
    }

    /// True if this path is the builtin sentinel.
    pub fn is_builtin(&self) -> bool {
        self.0.len() == 1 && self.0[0] == BUILTIN_SEGMENT
    }

    /// True if this path names a module inside a dependency package (i.e. was
    /// built with [`in_package`](Self::in_package)), as opposed to a module of
    /// the entry package itself (entry or local). Lets a caller that merges
    /// declarations across the whole resolved module graph (entry package +
    /// every dependency) scope back down to "declared by *this* package" when
    /// that distinction matters — e.g. a dependency's own `extern js` bindings
    /// are that package's concern, not something the entry package's manifest
    /// should have to declare.
    pub fn in_dependency_package(&self) -> bool {
        self.package_split().is_some()
    }

    /// Join the segments with `.` for use in user-facing display
    /// (`["a", "b"]` → `"a.b"`).  The entry path returns `""` and the
    /// builtin sentinel returns the literal `"<builtin>"` segment. A
    /// package-qualified path renders with the dependency *alias* in place of
    /// its internal marker (`greet` / `greet.util`), so diagnostics never leak
    /// the `<pkg:…>` sentinel.
    ///
    /// This is the *display* form; for symbol-table keys use
    /// [`module_qualify`], which guarantees a bare-name result for entry
    /// and builtin paths, keeps the package marker so a dependency module's key
    /// stays distinct, and never collides with an identifier in non-trivial
    /// cases (the `::` separator is illegal in identifiers).
    pub fn dotted(&self) -> String {
        match self.package_split() {
            Some((alias, [])) => alias.to_string(),
            Some((alias, rest)) => format!("{alias}.{}", rest.join(".")),
            None => self.0.join("."),
        }
    }

    /// The raw segment join used as the symbol-table key prefix. Unlike
    /// [`dotted`](Self::dotted), this preserves the `<pkg:…>` marker verbatim,
    /// so a dependency module's mangled key can never coincide with an
    /// entry-package module's — the identity distinction the marker exists for.
    fn key_prefix(&self) -> String {
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
        format!("{}::{}", module.key_prefix(), name)
    }
}

/// Names reserved as compiler-*intrinsic* namespaces: importable as
/// `import json` but backed by no `.phx` source file. A single-segment
/// import path matching one of these binds an intrinsic namespace whose
/// members the compiler synthesizes (e.g. `import json` →
/// `json.encode(...)`), rather than resolving to a module on disk.
///
/// Reserving these names means a project cannot shadow them with a
/// top-level source module of the same name (the resolver skips file
/// resolution for them) — the same trade-off as a reserved keyword.
pub const INTRINSIC_NAMESPACES: &[&str] = &["json"];

/// True iff `path` is a single segment naming a compiler-intrinsic
/// namespace (see [`INTRINSIC_NAMESPACES`]). The module resolver uses
/// this to skip file resolution, and sema uses it to bind the intrinsic
/// namespace instead of looking the path up as a source module.
pub fn is_intrinsic_namespace(path: &[String]) -> bool {
    matches!(path, [seg] if INTRINSIC_NAMESPACES.contains(&seg.as_str()))
}

/// Inverse of [`module_qualify`]: extract the bare name from a qualified
/// key. For an entry/builtin key like `"Option"` (no `::` separator) the
/// input is returned unchanged; for a non-entry key like `"a.b::foo"`
/// the part after the last `::` is returned.
///
/// Uses `rsplit_once` so that if a future syntax change ever lets a
/// qualified key contain more than one `::` separator, the *last* one
/// (which separates the module path from the identifier) is the
/// boundary — that matches `module_qualify`'s `format!("{}::{}", …)`.
pub fn bare_name(qualified: &str) -> &str {
    qualified
        .rsplit_once("::")
        .map(|(_, n)| n)
        .unwrap_or(qualified)
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
    fn bare_name_strips_module_segment() {
        assert_eq!(bare_name("a.b::foo"), "foo");
        assert_eq!(bare_name("foo"), "foo");
    }

    #[test]
    fn bare_name_round_trips_module_qualify() {
        let mp = ModulePath(vec!["a".into(), "b".into()]);
        assert_eq!(bare_name(&module_qualify(&mp, "foo")), "foo");
        assert_eq!(
            bare_name(&module_qualify(&ModulePath::entry(), "foo")),
            "foo"
        );
        assert_eq!(
            bare_name(&module_qualify(&ModulePath::builtin(), "Option")),
            "Option"
        );
    }

    #[test]
    fn builtin_displays_as_builtin_marker() {
        assert_eq!(ModulePath::builtin().to_string(), "<builtin>");
    }

    #[test]
    fn package_module_is_distinct_from_entry_module_of_same_path() {
        // The core guarantee: a dependency `util`'s root and an entry-package
        // top-level `util` must be different identities (different keys), even
        // though they display the same.
        let dep_root = ModulePath::in_package("util", &[]);
        let entry_mod = ModulePath(vec!["util".into()]);
        assert_ne!(dep_root, entry_mod);
        assert_ne!(
            module_qualify(&dep_root, "f"),
            module_qualify(&entry_mod, "f"),
            "package and entry modules must mangle to distinct keys"
        );
        // Two different packages with the same relative path are also distinct.
        assert_ne!(
            ModulePath::in_package("a", &["m".into()]),
            ModulePath::in_package("b", &["m".into()])
        );
    }

    #[test]
    fn package_module_displays_with_alias_not_marker() {
        // Display/dotted never leak the internal `<pkg:…>` marker.
        assert_eq!(ModulePath::in_package("greet", &[]).dotted(), "greet");
        assert_eq!(
            ModulePath::in_package("greet", &["util".into()]).dotted(),
            "greet.util"
        );
        assert_eq!(
            ModulePath::in_package("greet", &["a".into(), "b".into()]).to_string(),
            "greet.a.b"
        );
    }

    #[test]
    fn package_module_key_preserves_marker_and_round_trips_bare_name() {
        // The mangled key keeps the package distinction (so it can't collide),
        // and `bare_name` still recovers the plain identifier from it.
        let key = module_qualify(&ModulePath::in_package("greet", &["util".into()]), "foo");
        assert!(key.ends_with("::foo"));
        assert!(
            key.contains("<pkg:greet>"),
            "key must retain the marker: {key}"
        );
        assert_eq!(bare_name(&key), "foo");
    }

    #[test]
    fn package_module_is_not_entry_or_builtin() {
        let p = ModulePath::in_package("greet", &[]);
        assert!(!p.is_entry());
        assert!(!p.is_builtin());
    }

    #[test]
    fn in_dependency_package_distinguishes_package_from_local_modules() {
        assert!(ModulePath::in_package("greet", &[]).in_dependency_package());
        assert!(ModulePath::in_package("greet", &["util".into()]).in_dependency_package());
        assert!(!ModulePath::entry().in_dependency_package());
        assert!(!ModulePath::builtin().in_dependency_package());
        assert!(!ModulePath(vec!["util".into()]).in_dependency_package());
    }
}
