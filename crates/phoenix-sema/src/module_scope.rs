//! Per-module visibility scopes — Phase A construction and lookups.
//!
//! Each [`ModuleScope`] maps the *local* names visible inside a module
//! (own declarations, built-ins, and items brought in via `import`) to
//! their *qualified* keys in the global symbol tables. Lookup helpers on
//! [`Checker`](crate::checker::Checker) consult the current module's
//! scope to translate a user-written name into the table key.
//!
//! Two-phase build, both running *before* registration so that
//! `resolve_type_expr` (invoked from each `register_*`) sees every name
//! that is visible at use-sites — including names brought in via
//! `import`. This is what lets a function signature in module `a` write
//! `function process(u: User) -> User` after `import b { User }` without
//! requiring a local alias.
//!
//! - **Phase A** ([`Checker::build_module_scopes_phase_a`], in this file)
//!   — register every module's own declarations plus the builtins (no
//!   imports).
//! - **Phase B** (in [`crate::import_resolve`]) — resolve each module's
//!   `import` declarations against every target module's *AST*. Reads
//!   visibility and `name_span` from the parser-level decls (no
//!   dependency on registration tables), which is what lets Phase B run
//!   before registration.
//!
//! The split also lets modules with mutual imports (`a` imports from
//! `b`; `b` imports from `a`) resolve cleanly — Phase A puts every
//! module's own symbols in scope before any Phase B import tries to
//! land. (Re-exports / transitive imports are not a feature today, so
//! Phase B does not need a fixed-point iteration.)

use std::collections::HashMap;

use phoenix_common::diagnostics::Diagnostic;
use phoenix_common::module_path::{ModulePath, module_qualify};
use phoenix_common::span::Span;
use phoenix_parser::ast::{Declaration, Program};

use crate::checker::{Checker, EnumInfo, FunctionInfo, StructInfo, TraitInfo, TypeAliasInfo};

/// Trait exposing each registered decl's owning module path. Used by
/// [`Checker::snapshot_builtin_names`] so a single generic helper can
/// scan every typed table (`functions`, `structs`, …) for builtin
/// entries without a macro.
pub(crate) trait HasDefModule {
    fn def_module(&self) -> &ModulePath;
}

macro_rules! impl_has_def_module {
    ($($t:ty),+ $(,)?) => {
        $(
            impl HasDefModule for $t {
                fn def_module(&self) -> &ModulePath { &self.def_module }
            }
        )+
    };
}

impl_has_def_module!(FunctionInfo, StructInfo, EnumInfo, TraitInfo, TypeAliasInfo);

/// The set of names visible inside a module, mapped to qualified table keys.
///
/// `local_name` is what the user types in source: the original name for
/// own declarations and unaliased imports, or the alias for
/// `Foo as Bar`. `qualified_key` is what the symbol tables
/// (`Checker.functions`, `.structs`, `.enums`, `.traits`,
/// `.type_aliases`) are keyed by.
#[derive(Debug, Default)]
pub(crate) struct ModuleScope {
    /// `local_name → qualified_key`. Lookups that miss are diagnosed as
    /// "name not in scope" by callers (typically via the bodied
    /// `lookup_*` helpers).
    pub visible_symbols: HashMap<String, String>,
}

/// One enum visible in the current module's scope that owns the
/// queried variant. Carries the owning enum's `definition_span` so the
/// ambiguity diagnostic can attach a "declared here" note per
/// candidate. Internal to [`Checker::lookup_visible_enum_variant`] and
/// its helpers.
#[derive(Debug)]
struct EnumVariantMatch {
    local_name: String,
    parent_type_params: Vec<String>,
    variant_types: Vec<crate::types::Type>,
    enum_definition_span: Span,
}

impl ModuleScope {
    /// Insert `local_name → qualified_key`. Silently overwrites prior
    /// entries — the registration order is own-module symbols first,
    /// then builtins (which only fill empty slots via `.entry().or_insert`),
    /// then imports (which can override builtins by alias).
    pub(crate) fn insert(&mut self, local_name: String, qualified_key: String) {
        self.visible_symbols.insert(local_name, qualified_key);
    }
}

impl Checker {
    /// Snapshot the bare names of every builtin currently registered in
    /// the symbol tables. Called once after [`Self::register_builtins`]
    /// and cached on the [`Checker`] so per-module scope construction
    /// can extend each module's `visible_symbols` cheaply instead of
    /// rescanning all five typed tables per module.
    ///
    /// A builtin name may appear in more than one table (e.g. a builtin
    /// type whose constructor is also a function); the `HashSet`
    /// representation in `builtin_local_names` deduplicates.
    ///
    /// Builtin methods are reachable via their receiver type, which
    /// must be registered in one of the typed tables for scope-aware
    /// lookup to find it. The `assert!` below pins that invariant —
    /// if a future builtin registers methods on a receiver that is
    /// *not* present in any typed table, this snapshot would miss the
    /// receiver name and `lookup_methods` would silently return None
    /// at every use-site. Failing fast here is the right alternative.
    pub(crate) fn snapshot_builtin_names(&mut self) {
        let mut names: Vec<String> = Vec::new();
        extend_builtin_names(&self.functions, &mut names);
        extend_builtin_names(&self.structs, &mut names);
        extend_builtin_names(&self.enums, &mut names);
        extend_builtin_names(&self.traits, &mut names);
        extend_builtin_names(&self.type_aliases, &mut names);
        for type_name in self.methods.keys() {
            assert!(
                self.structs.contains_key(type_name)
                    || self.enums.contains_key(type_name)
                    || self.traits.contains_key(type_name)
                    || self.type_aliases.contains_key(type_name)
                    || self.functions.contains_key(type_name),
                "method-table receiver `{type_name}` has no entry in any typed table — \
                 scope-aware method lookup would silently miss it. If a new builtin needs \
                 methods on a primitive-typed receiver, extend `snapshot_builtin_names` \
                 to surface the name explicitly.",
            );
        }
        self.builtin_local_names = names.into_iter().collect();
    }

    /// True iff `name` is the bare name of a registered builtin
    /// (`Option`, `Result`, …). Builtin names are reserved across
    /// every module — user code cannot redeclare them as a function,
    /// struct, enum, trait, or type alias in any module. O(1) via
    /// `builtin_local_names`'s `HashSet` representation.
    pub(crate) fn is_builtin_name(&self, name: &str) -> bool {
        self.builtin_local_names.contains(name)
    }

    /// Build a single module's Phase-A scope: own declarations plus
    /// builtins. Used by both the multi-module path
    /// ([`Self::build_module_scopes_phase_a`]) and the single-file path
    /// ([`Checker::check_program`]).
    pub(crate) fn build_one_module_scope_phase_a(
        &mut self,
        module_path: &ModulePath,
        program: &Program,
    ) {
        let mut scope = ModuleScope::default();

        // Own-module declarations: insert each user-source name keyed
        // to its module-qualified table entry. Skip names that shadow
        // a builtin — registration will reject those decls, leaving no
        // table entry under the user's qualified key, so a scope
        // mapping to that key would silently break later references
        // (the bare-name builtin loop below would also see the slot
        // already filled and skip its own insert).
        for decl in &program.declarations {
            let name = match decl {
                Declaration::Function(f) => Some(&f.name),
                Declaration::Struct(s) => Some(&s.name),
                Declaration::Enum(e) => Some(&e.name),
                Declaration::Trait(t) => Some(&t.name),
                Declaration::TypeAlias(ta) => Some(&ta.name),
                _ => None,
            };
            if let Some(n) = name {
                if self.is_builtin_name(n) {
                    continue;
                }
                let qualified = module_qualify(module_path, n);
                scope.insert(n.clone(), qualified);
            }
        }

        // Built-ins: visible under their bare name in every module.
        // The qualified key for a builtin is the bare name itself.
        for bn in &self.builtin_local_names {
            scope
                .visible_symbols
                .entry(bn.clone())
                .or_insert_with(|| bn.clone());
        }

        let prior = self.module_scopes.insert(module_path.clone(), scope);
        assert!(
            prior.is_none(),
            "Phase A scope construction ran twice for module `{module_path}` — \
             callers must build each module's scope exactly once before Phase B",
        );
    }

    /// Phase A of `build_module_scopes`: register every module's own
    /// declarations plus the builtins into its scope. No imports yet —
    /// those are resolved in Phase B once `register_decls` has populated
    /// every item's visibility.
    pub(crate) fn build_module_scopes_phase_a(
        &mut self,
        modules: &[phoenix_modules::ResolvedSourceModule],
    ) {
        for module in modules {
            self.build_one_module_scope_phase_a(&module.module_path, &module.program);
        }
    }

    /// Resolve a *bare* user-source name in the current module's
    /// scope to its qualified key in the global symbol tables.
    /// Returns `None` if the name is not in scope.
    ///
    /// Callers whose input may already be qualified (e.g. names
    /// extracted from `Type::Named` post-Phase 2.6) should go
    /// through [`Self::canonicalize_type_name`] instead — that
    /// passes qualified names through without re-translating, which
    /// `resolve_visible` would fail on (a key like `"lib::User"` is
    /// not a local-name entry in any scope).
    pub(crate) fn resolve_visible(&self, local_name: &str) -> Option<&str> {
        let scope = self.module_scopes.get(&self.current_module)?;
        scope.visible_symbols.get(local_name).map(|s| s.as_str())
    }

    /// Canonicalize a possibly-bare-or-qualified name to the key the
    /// global symbol tables are indexed by. Used by `lookup_*` helpers
    /// (and via [`Self::qualify_in_current`] by `Type::*` payload
    /// construction) so call sites can pass either a bare AST name or
    /// a qualified key extracted from `Type::Named` and get the same
    /// answer.
    ///
    /// Bare names are translated through the current module's scope
    /// (cross-module references resolve to the qualified key); names
    /// classified as already qualified by [`QualifiedKind::classify`]
    /// are returned verbatim — `resolve_visible` would fail on them
    /// (a key like `"lib::User"` is not a local-name entry).
    pub(crate) fn canonicalize_type_name<'a>(&'a self, name: &'a str) -> &'a str {
        match QualifiedKind::classify(name) {
            QualifiedKind::Qualified => name,
            QualifiedKind::Bare => self.resolve_visible(name).unwrap_or(name),
        }
    }

    /// Translate a bare user-source name into the canonical
    /// `Type::Named` / `Type::Generic` / `Type::Dyn` payload string —
    /// the owned-`String` sibling of [`Self::canonicalize_type_name`].
    ///
    /// Consults the current module's scope first so cross-module
    /// references (`User` imported from `models`) produce the
    /// `models::User` form that downstream comparisons (sema's own
    /// type-equality checks, IR's `lower_type`, the Cranelift
    /// backend's layout lookups) all expect. Falls back to the
    /// bare name when the name is not in scope (e.g. type variables,
    /// `Self`, built-ins not pre-loaded into the scope) — those
    /// produce the same string they did before module-system
    /// support so the single-file behavior is unchanged (the entry
    /// module's own decls qualify-to-bare via `module_qualify`).
    ///
    /// Phase 2.6 invariant: every `Type::Named(name)` flowing through
    /// sema, IR, and codegen carries a key that matches what
    /// `register_function`/`register_struct`/etc. used as the
    /// symbol-table key, so a single global table lookup hits.
    pub(crate) fn qualify_in_current(&self, local_name: &str) -> String {
        let canonical = self.canonicalize_type_name(local_name).to_string();
        debug_assert!(
            QualifiedKind::is_well_formed(&canonical),
            "qualify_in_current produced a malformed Type::* payload `{}` \
             — `QualifiedKind::classify` assumes payloads are either bare \
             or `seg::seg[::seg…]`",
            canonical,
        );
        canonical
    }
}

/// Discriminator between bare and qualified names in sema string keys.
///
/// The module-path separator `::` is the only legal use of `::` in a
/// sema name string — Phoenix identifiers do not contain `::`, so a
/// substring probe is a sound predicate. If the lexer ever admits
/// `::` inside identifiers, swap [`Self::classify`] for a typed marker
/// (e.g. wrap qualified keys in a newtype); localizing both the
/// classification and the well-formedness contract here makes that
/// future change a one-site edit.
pub(crate) enum QualifiedKind {
    /// No `::` separator — a bare user-source name.
    Bare,
    /// Contains a `::` separator — a `seg::seg[::seg…]` key.
    Qualified,
}

impl QualifiedKind {
    /// Classify `name` as bare or qualified based on the presence of
    /// the `::` separator.
    pub(crate) fn classify(name: &str) -> Self {
        if name.contains("::") {
            QualifiedKind::Qualified
        } else {
            QualifiedKind::Bare
        }
    }

    /// True when `name` is either bare (no `::`) or a well-formed
    /// `seg::seg[::seg…]` qualified key — every `::`-separated segment
    /// is non-empty. Pins the input contract for [`Self::classify`]
    /// so a construction site that produces a malformed payload trips
    /// a `debug_assert!` rather than silently breaking the bare-vs-
    /// qualified split downstream.
    pub(crate) fn is_well_formed(name: &str) -> bool {
        !name.is_empty() && name.split("::").all(|seg| !seg.is_empty())
    }
}

impl Checker {
    /// Look up a function by user-source name in the current module's scope.
    /// Returns `None` when the name is not in scope.
    pub(crate) fn lookup_function(
        &self,
        local_name: &str,
    ) -> Option<&crate::checker::FunctionInfo> {
        let qualified = self.resolve_visible(local_name)?;
        self.functions.get(qualified)
    }

    /// Look up a struct by name. Accepts either a bare user-source
    /// name (resolved through the current module's scope) or an
    /// already-qualified key (e.g. extracted from `Type::Named`),
    /// via `canonicalize_type_name`.
    pub(crate) fn lookup_struct(&self, name: &str) -> Option<&crate::checker::StructInfo> {
        self.structs.get(self.canonicalize_type_name(name))
    }

    /// Look up an enum by name. Accepts bare or qualified names —
    /// see [`Self::lookup_struct`] for the canonicalization rules.
    pub(crate) fn lookup_enum(&self, name: &str) -> Option<&crate::checker::EnumInfo> {
        self.enums.get(self.canonicalize_type_name(name))
    }

    /// Look up a trait by name. Accepts bare or qualified names —
    /// see [`Self::lookup_struct`] for the canonicalization rules.
    pub(crate) fn lookup_trait(&self, name: &str) -> Option<&crate::checker::TraitInfo> {
        self.traits.get(self.canonicalize_type_name(name))
    }

    /// Look up a type alias by name. Accepts bare or qualified names.
    pub(crate) fn lookup_type_alias(&self, name: &str) -> Option<&crate::checker::TypeAliasInfo> {
        self.type_aliases.get(self.canonicalize_type_name(name))
    }

    /// Look up the methods table for a receiver type name. Accepts a
    /// bare user-source name (resolved through the current module's
    /// scope) or an already-qualified key — methods are registered
    /// under the receiver's qualified key (e.g. `lib::User`) and use
    /// sites that wrote the receiver as `User` resolve through scope.
    ///
    /// Builtins like `Option` / `Result` are reachable here because
    /// `snapshot_builtin_names` walks the typed tables (`enums` for
    /// today's builtins) and adds those names to every module's scope.
    pub(crate) fn lookup_methods(
        &self,
        type_name: &str,
    ) -> Option<&HashMap<String, crate::checker::MethodInfo>> {
        self.methods.get(self.canonicalize_type_name(type_name))
    }

    /// Returns true if the given receiver type implements the given
    /// trait. Both inputs accept bare or qualified names; bare names
    /// resolve through the current module's scope before probing
    /// `self.trait_impls`. An unresolved type or trait cannot satisfy
    /// a bound, so a missing scope entry returns `false` directly.
    pub(crate) fn has_trait_impl(&self, type_name: &str, trait_name: &str) -> bool {
        let q_type = self.canonicalize_type_name(type_name);
        let q_trait = self.canonicalize_type_name(trait_name);
        self.trait_impls
            .contains(&(q_type.to_string(), q_trait.to_string()))
    }

    /// Find the enum visible in the current module's scope that owns
    /// `variant_name` and return its bare user-source name (i.e. the
    /// alias the user can write at this site), parent type parameters,
    /// and the variant's field types.
    ///
    /// Iterates the current scope rather than the global enums table so
    /// variant lookup respects visibility — a variant whose owning enum
    /// was never imported (or is private) won't resolve from here.
    /// Built-in `Option`/`Result` are visible without import and so
    /// remain reachable.
    ///
    /// If two or more visible enums share `variant_name`, an ambiguity
    /// diagnostic is emitted at `use_span` listing every candidate, and
    /// the resolution falls back to the alphabetically-first local
    /// name so downstream type-checking still has a concrete shape to
    /// work against.
    pub(crate) fn lookup_visible_enum_variant(
        &mut self,
        variant_name: &str,
        use_span: Span,
    ) -> Option<(String, Vec<String>, Vec<crate::types::Type>)> {
        let mut matches = self.collect_visible_enum_variant_matches(variant_name);
        if matches.is_empty() {
            return None;
        }
        // Deterministic tie-break: pick alphabetically-first local name.
        matches.sort_by(|a, b| a.local_name.cmp(&b.local_name));
        if matches.len() > 1 {
            self.emit_ambiguous_variant_diagnostic(variant_name, use_span, &matches);
        }
        matches
            .into_iter()
            .next()
            .map(|m| (m.local_name, m.parent_type_params, m.variant_types))
    }

    /// Read-only sibling of [`Self::lookup_visible_enum_variant`]:
    /// gather every visible enum that owns `variant_name`. Split out so
    /// the caller can emit the ambiguity diagnostic without holding an
    /// immutable borrow of `self.module_scopes` across `&mut self.error`.
    ///
    /// Multiple local names can resolve to the same enum (e.g. the
    /// same enum imported once unaliased and once aliased); the result
    /// dedupes by qualified key so a single underlying enum yields a
    /// single match. The kept local name is the alphabetically-first
    /// one, which matches the caller's tie-break rule.
    ///
    /// Each match carries the owning enum's `definition_span` so the
    /// ambiguity diagnostic can attach a "declared here" note per
    /// candidate.
    fn collect_visible_enum_variant_matches(&self, variant_name: &str) -> Vec<EnumVariantMatch> {
        let Some(scope) = self.module_scopes.get(&self.current_module) else {
            return Vec::new();
        };
        let mut by_qualified: HashMap<&str, EnumVariantMatch> = HashMap::new();
        for (local_name, qualified_key) in &scope.visible_symbols {
            let Some(enum_info) = self.enums.get(qualified_key) else {
                continue;
            };
            let Some((_, types)) = enum_info.variants.iter().find(|(n, _)| n == variant_name)
            else {
                continue;
            };
            let candidate = EnumVariantMatch {
                local_name: local_name.clone(),
                parent_type_params: enum_info.type_params.clone(),
                variant_types: types.clone(),
                enum_definition_span: enum_info.definition_span,
            };
            match by_qualified.entry(qualified_key.as_str()) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(candidate);
                }
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    if candidate.local_name < slot.get().local_name {
                        slot.insert(candidate);
                    }
                }
            }
        }
        by_qualified.into_values().collect()
    }

    fn emit_ambiguous_variant_diagnostic(
        &mut self,
        variant_name: &str,
        use_span: Span,
        matches: &[EnumVariantMatch],
    ) {
        let alternatives = matches
            .iter()
            .map(|m| format!("`{}::{variant_name}`", m.local_name))
            .collect::<Vec<_>>()
            .join(", ");
        let mut diag = Diagnostic::error(
            format!("variant `{variant_name}` is ambiguous: could resolve to {alternatives}"),
            use_span,
        );
        for m in matches {
            diag = diag.with_note(
                m.enum_definition_span,
                format!(
                    "`{variant_name}` declared as a variant of `{}` here",
                    m.local_name
                ),
            );
        }
        self.diagnostics.push(diag);
    }

    /// Emit a private-access diagnostic with a "declared here" note and
    /// a "mark as public" suggestion. Used by both private-import
    /// rejection ([`crate::import_resolve`]) and private-field access
    /// at use-sites ([`crate::field_privacy`]) so the rich shape stays
    /// consistent.
    pub(crate) fn emit_private_access_diagnostic(
        &mut self,
        message: String,
        use_span: Span,
        definition_span: Span,
        suggestion: String,
    ) {
        self.diagnostics.push(
            Diagnostic::error(message, use_span)
                .with_note(definition_span, "declared here")
                .with_suggestion(suggestion),
        );
    }
}

/// Append the bare name of every builtin entry in `table` to `names`.
/// Used by [`Checker::snapshot_builtin_names`] to walk every typed
/// table once at startup without a macro. `T` is any registered-decl
/// info type — see the [`HasDefModule`] impls above.
fn extend_builtin_names<T: HasDefModule>(table: &HashMap<String, T>, names: &mut Vec<String>) {
    for (name, info) in table {
        if info.def_module().is_builtin() {
            names.push(name.clone());
        }
    }
}
