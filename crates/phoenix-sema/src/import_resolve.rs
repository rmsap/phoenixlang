//! Phase B of per-module scope construction: resolve `import`
//! declarations against target modules' ASTs.
//!
//! Reads visibility and `name_span` directly from parser-level decls
//! (`func.visibility`, `s.name_span`, …) rather than the registered
//! `*Info` tables, so Phase B can run *before* `register_decls` —
//! making imported names available during signature-time
//! `resolve_type_expr` calls. See the file-level doc on
//! `module_scope.rs` for the two-phase rationale.

use std::collections::HashMap;

use phoenix_common::diagnostics::Diagnostic;
use phoenix_common::module_path::{ModulePath, module_qualify};
use phoenix_common::span::Span;
use phoenix_parser::ast::{Declaration, ImportDecl, ImportItems, Program, Visibility};

use crate::checker::Checker;

/// One AST decl matched by [`Checker::collect_named_import_matches`],
/// used to drive both single-match resolution and the cross-namespace
/// ambiguity diagnostic.
pub(super) struct ImportMatch {
    /// Human-readable namespace label (`"function"`, `"struct"`, …)
    /// used in the cross-namespace ambiguity diagnostic.
    pub kind: &'static str,
    /// Declared visibility of the matched decl. Read by `resolve_import`
    /// to decide between `Ok(())` and `Err(Private)` for the first match.
    pub visibility: Visibility,
    /// `name_span` of the matched decl — drives the `with_note` shape on
    /// both private-import and cross-namespace ambiguity diagnostics.
    pub definition_span: Span,
}

/// Internal classification used by [`Checker::resolve_named_import_from_ast`].
pub(super) enum ImportResolveError {
    /// The qualified key was not present in any symbol table.
    NotFound,
    /// The item exists but is private; carries the definition span
    /// so the diagnostic can render a `with_note`.
    Private(Span),
}

impl Checker {
    /// Phase B of `build_module_scopes`: walk every module's `import`
    /// declarations and resolve each against every target module's
    /// *AST*. Inserts resolved items into the importing module's scope,
    /// or emits a diagnostic for missing / private / nonexistent items.
    pub(crate) fn build_module_scopes_phase_b(
        &mut self,
        modules: &[phoenix_modules::ResolvedSourceModule],
    ) {
        // Index modules by path so each `import target { ... }` can
        // look up the target's AST in O(1).
        let by_path: HashMap<ModulePath, &Program> = modules
            .iter()
            .map(|m| (m.module_path.clone(), &m.program))
            .collect();

        for module in modules {
            let importer_path = module.module_path.clone();
            for decl in &module.program.declarations {
                if let Declaration::Import(imp) = decl {
                    let target_path = ModulePath(imp.path.clone());
                    let Some(target_program) = by_path.get(&target_path) else {
                        // Resolver should have caught this — defense in depth.
                        self.diagnostics.push(Diagnostic::error(
                            format!("cannot find module `{}`", target_path),
                            imp.span,
                        ));
                        continue;
                    };
                    self.resolve_import(&importer_path, &target_path, target_program, imp);
                }
            }
        }
    }

    /// Resolve a single `import` declaration. For each named item, or
    /// for the wildcard form, look up the target item in the target
    /// module's AST, validate visibility, and (on success) insert
    /// `local_name → qualified_target_key` into the importer's scope.
    fn resolve_import(
        &mut self,
        importer_path: &ModulePath,
        target_path: &ModulePath,
        target_program: &Program,
        imp: &ImportDecl,
    ) {
        match &imp.items {
            ImportItems::Named(items) => {
                for item in items {
                    let qualified_target = module_qualify(target_path, &item.name);
                    let matches = Self::collect_named_import_matches(target_program, &item.name);
                    if matches.len() > 1 {
                        self.emit_cross_namespace_ambiguity_diagnostic(
                            &item.name,
                            target_path,
                            item.span,
                            &matches,
                        );
                    }
                    let resolution = Self::resolve_named_import_from_ast(&matches);
                    let local_name = item.alias.as_deref().unwrap_or(&item.name).to_string();
                    match resolution {
                        Ok(()) => {
                            self.insert_into_scope(importer_path, local_name, qualified_target);
                        }
                        Err(ImportResolveError::NotFound) => {
                            self.diagnostics.push(Diagnostic::error(
                                format!(
                                    "`{}` is not declared in module `{}`",
                                    item.name, target_path
                                ),
                                item.span,
                            ));
                        }
                        Err(ImportResolveError::Private(definition_span)) => {
                            self.emit_private_access_diagnostic(
                                format!("`{}` is private to module `{}`", item.name, target_path),
                                item.span,
                                definition_span,
                                format!(
                                    "mark `{}` as `public` in `{}` to export it",
                                    item.name, target_path
                                ),
                            );
                        }
                    }
                }
            }
            ImportItems::Wildcard => {
                // Enumerate every public named decl in the target
                // module's AST and insert each under its bare name.
                let exported = Self::collect_public_items_from_ast(target_path, target_program);
                for (bare, qualified) in exported {
                    self.insert_into_scope(importer_path, bare, qualified);
                }
            }
        }
    }

    /// Walk the target module's AST and collect every declaration that
    /// matches `name`, in declaration order. Phoenix keeps separate
    /// registration namespaces for functions / structs / enums / traits
    /// / type aliases, so a target module that declares e.g. both
    /// `function Foo` and `struct Foo` will produce two matches here —
    /// the caller emits an ambiguity diagnostic and falls back to the
    /// first match's resolution. The `kind` label drives the diagnostic
    /// wording.
    fn collect_named_import_matches(target_program: &Program, name: &str) -> Vec<ImportMatch> {
        let mut matches = Vec::new();
        for decl in &target_program.declarations {
            let matched = match decl {
                Declaration::Function(f) if f.name == name => {
                    Some(("function", f.visibility, f.name_span))
                }
                Declaration::Struct(s) if s.name == name => {
                    Some(("struct", s.visibility, s.name_span))
                }
                Declaration::Enum(e) if e.name == name => Some(("enum", e.visibility, e.name_span)),
                Declaration::Trait(t) if t.name == name => {
                    Some(("trait", t.visibility, t.name_span))
                }
                Declaration::TypeAlias(ta) if ta.name == name => {
                    Some(("type alias", ta.visibility, ta.name_span))
                }
                _ => None,
            };
            if let Some((kind, visibility, definition_span)) = matched {
                matches.push(ImportMatch {
                    kind,
                    visibility,
                    definition_span,
                });
            }
        }
        matches
    }

    /// Classify the collected matches into the import-resolution outcome
    /// for the *first* matching decl. The cross-namespace ambiguity
    /// diagnostic — if any — is emitted by the caller before this is
    /// invoked; this helper just decides whether to insert the import,
    /// emit a private-access diagnostic, or report the name as missing.
    fn resolve_named_import_from_ast(matches: &[ImportMatch]) -> Result<(), ImportResolveError> {
        match matches.first() {
            None => Err(ImportResolveError::NotFound),
            Some(m) if m.visibility == Visibility::Public => Ok(()),
            Some(m) => Err(ImportResolveError::Private(m.definition_span)),
        }
    }

    /// Emit the cross-namespace ambiguity diagnostic when an import
    /// targets a name that resolves in more than one namespace (e.g.
    /// `function Foo` and `struct Foo`). The diagnostic carries one
    /// note per candidate so the user can disambiguate, and the
    /// caller falls back to the first match for the actual scope
    /// insert (deterministic tie-break: declaration order).
    fn emit_cross_namespace_ambiguity_diagnostic(
        &mut self,
        name: &str,
        target_path: &ModulePath,
        use_span: Span,
        matches: &[ImportMatch],
    ) {
        let kinds = matches
            .iter()
            .map(|m| m.kind)
            .collect::<Vec<_>>()
            .join(", ");
        let mut diag = Diagnostic::error(
            format!(
                "`{name}` is declared in multiple namespaces in module `{target_path}` ({kinds}); \
                 the {} declaration will be imported — rename one of the declarations to disambiguate",
                matches[0].kind,
            ),
            use_span,
        );
        for m in matches {
            diag = diag.with_note(
                m.definition_span,
                format!("`{name}` declared as a {} here", m.kind),
            );
        }
        self.diagnostics.push(diag);
    }

    /// Walk the target module's AST and collect `(bare_name,
    /// qualified_key)` pairs for every named decl with `Public`
    /// visibility. Used by wildcard-import resolution.
    fn collect_public_items_from_ast(
        target_path: &ModulePath,
        target_program: &Program,
    ) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for decl in &target_program.declarations {
            let entry = match decl {
                Declaration::Function(f) if f.visibility == Visibility::Public => Some(&f.name),
                Declaration::Struct(s) if s.visibility == Visibility::Public => Some(&s.name),
                Declaration::Enum(e) if e.visibility == Visibility::Public => Some(&e.name),
                Declaration::Trait(t) if t.visibility == Visibility::Public => Some(&t.name),
                Declaration::TypeAlias(ta) if ta.visibility == Visibility::Public => Some(&ta.name),
                _ => None,
            };
            if let Some(name) = entry {
                let qualified = module_qualify(target_path, name);
                out.push((name.clone(), qualified));
            }
        }
        out
    }

    /// Insert `local_name → qualified_key` into the named module's
    /// scope. Phase A always runs before any caller of this method, so
    /// the scope must already exist; an absent scope is a real
    /// invariant violation and we surface it via `expect` rather than
    /// silently creating a fresh one (which would mask the bug).
    fn insert_into_scope(
        &mut self,
        importer_path: &ModulePath,
        local_name: String,
        qualified_key: String,
    ) {
        self.module_scopes
            .get_mut(importer_path)
            .expect("module scope must be built in Phase A before Phase B inserts imports")
            .insert(local_name, qualified_key);
    }
}
