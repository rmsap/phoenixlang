//! Receiver-type classification for `impl` blocks.
//!
//! Pulled out of `check_register.rs` so that the registration pass stays
//! focused on the registration sequence itself. The helpers here are
//! consumed by [`Checker::register_impl`](crate::check_register) to
//! decide between coherence-success (`Local`), coherence-rejection
//! (`ForeignModule` / `ForeignAmbiguous`), and unknown-type rejection
//! (`Unknown`). The reserved-builtin guard is handled separately by
//! `Checker::is_builtin_name` in `register_impl` itself, since it
//! short-circuits before classification.

use crate::checker::{Checker, TraitInfo};
use phoenix_common::module_path::{ModulePath, bare_name, module_qualify};
use phoenix_parser::ast::ImplBlock;

/// Classification of an `impl` block's receiver type, produced by
/// [`Checker::classify_impl_target`] and consumed by
/// [`Checker::register_impl`](crate::check_register).
pub(crate) enum ImplTarget {
    /// Receiver type is declared in the current module — registration proceeds.
    Local,
    /// Receiver type is declared in exactly one non-builtin foreign
    /// module — coherence violation; diagnose and route methods
    /// through the orphan path.
    ForeignModule(ModulePath),
    /// Receiver type's bare name is declared in *more than one* foreign
    /// module. Still a coherence violation, but the diagnostic should
    /// list every candidate so the user can disambiguate. Modules are
    /// sorted by their dotted representation for determinism.
    ForeignAmbiguous(Vec<ModulePath>),
    /// Receiver type isn't declared anywhere reachable — diagnose
    /// "unknown type" and route methods through the orphan path.
    Unknown,
}

impl Checker {
    /// Classify what `impl Foo { ... }` targets, so
    /// [`Checker::register_impl`](crate::check_register::Checker::register_impl)
    /// can branch on coherence vs. unknown-type vs. proceed-normally.
    ///
    /// Scans the global struct/enum tables directly rather than going
    /// through the scope: a coherence violation (`impl Foo` where `Foo`
    /// is an *imported* type in scope) must still be diagnosed as a
    /// coherence violation, so we can't trust scope alone. The local
    /// fast-path checks for an own-module `Foo`; the foreign scan
    /// catches imports and unscoped foreign decls alike.
    ///
    /// The foreign-module scan is a linear walk over `structs` +
    /// `enums`. It only runs on the rejection path (same-module fast-
    /// path returns first), so it never fires for well-formed input.
    /// If it shows up in profiling, the cure is a `bare_name →
    /// ModulePath` index built once before `register_decls`.
    pub(crate) fn classify_impl_target(&self, imp: &ImplBlock) -> ImplTarget {
        let qualified_local = module_qualify(&self.current_module, &imp.type_name);
        if self.structs.contains_key(&qualified_local) || self.enums.contains_key(&qualified_local)
        {
            return ImplTarget::Local;
        }
        let foreign_modules = self.find_foreign_definition_modules(&imp.type_name);
        match foreign_modules.as_slice() {
            [] => ImplTarget::Unknown,
            [single] => ImplTarget::ForeignModule(single.clone()),
            multiple => ImplTarget::ForeignAmbiguous(multiple.to_vec()),
        }
    }

    /// Search the struct + enum tables for every definition whose bare
    /// name matches `type_name` and whose owning module is a non-builtin
    /// module other than the current one. The result is sorted by
    /// `dotted()` representation and deduped, so callers see a
    /// deterministic list. Used by [`Self::classify_impl_target`].
    fn find_foreign_definition_modules(&self, type_name: &str) -> Vec<ModulePath> {
        let is_foreign = |m: &ModulePath| !m.is_builtin() && m != &self.current_module;
        let matches = |qualified: &String, def_module: &ModulePath| {
            bare_name(qualified) == type_name && is_foreign(def_module)
        };
        let mut out: Vec<ModulePath> = self
            .structs
            .iter()
            .filter(|(q, i)| matches(q, &i.def_module))
            .map(|(_, i)| i.def_module.clone())
            .chain(
                self.enums
                    .iter()
                    .filter(|(q, i)| matches(q, &i.def_module))
                    .map(|(_, i)| i.def_module.clone()),
            )
            .collect();
        // Sort lexicographically on the underlying segment vector
        // rather than allocating a `dotted()` String per comparison.
        // Phoenix idents are alphanumeric + underscore (no `.`), so
        // segment-vector lex order matches `dotted()` lex order.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.dedup();
        out
    }

    /// Resolve an `impl Block`'s optional `trait_name` into the
    /// qualified-key form needed for `trait_impls` keying. Returns:
    ///
    /// - `Ok(None)` for an inherent impl (no `trait_name`).
    /// - `Ok(Some((local_name, qualified_key, info)))` when the trait
    ///   is registered in the current module.
    /// - `Err(())` after emitting an "unknown trait" diagnostic; the
    ///   caller is responsible for routing the impl's methods through
    ///   the orphan path.
    ///
    /// Today's resolution qualifies the trait name against
    /// `current_module` directly rather than going through the
    /// importer's scope. Supporting `impl ImportedTrait for LocalType`
    /// is a follow-up: the in-flight trait must already be registered
    /// (not just in scope) so `validate_trait_impl` has its
    /// `TraitInfo`, which in turn requires reordering the registration
    /// pass to handle traits across all modules before impls. Today's
    /// "unknown trait" diagnostic therefore fires for genuinely-
    /// undeclared traits *and* for imported trait names.
    pub(crate) fn resolve_impl_trait(
        &mut self,
        imp: &ImplBlock,
    ) -> Result<Option<(String, String, TraitInfo)>, ()> {
        let Some(trait_name) = &imp.trait_name else {
            return Ok(None);
        };
        let qualified_trait = module_qualify(&self.current_module, trait_name);
        match self.traits.get(&qualified_trait).cloned() {
            Some(trait_info) => Ok(Some((trait_name.clone(), qualified_trait, trait_info))),
            None => {
                self.error(format!("unknown trait `{}`", trait_name), imp.span);
                Err(())
            }
        }
    }
}
