//! Multi-module orchestration for the AST interpreter.
//!
//! Holds the bits that distinguish the multi-file [`run_modules`] path
//! from the single-file [`super::run`] path: per-module scope-driven
//! name qualification ([`Interpreter::qualify`]), per-decl registration
//! under module-qualified keys ([`Interpreter::register_decl_in_module`]),
//! and the [`run_modules`] entry point itself. The call/method dispatch
//! core (`call_function_in_module`, `call_method`) stays in `mod.rs`
//! because it's shared between paths.

use super::{EnumDef, FunctionEntry, Interpreter, Result, RuntimeError, StructDef, error};
use phoenix_parser::ast::Declaration;
use std::collections::HashMap;
use std::rc::Rc;

impl Interpreter {
    /// Translate a bare user-source name into the registry key under
    /// which the corresponding function / struct / enum / type alias
    /// is stored, using the current module's scope.
    ///
    /// Returns the bare name when no module is on the stack (the
    /// single-file `run` path) — the registry was populated with bare
    /// keys, so the lookup hits directly.
    ///
    /// Returns `module_qualify(current_module, name)` when a scope is
    /// active but the name is not present in it. Two legitimate
    /// reasons to land here:
    ///   1. The caller is probing a name that isn't in the
    ///      function/struct/enum namespace — most commonly an enum
    ///      variant name (variants aren't scope entries; their host
    ///      enum is). `eval_struct_or_variant` qualifies-then-probes
    ///      the struct table first, falls through on miss, and looks
    ///      up the variant in `variant_to_enum` by bare name.
    ///   2. Sema accepted a use of a name that won't be found at
    ///      runtime — the fallback yields a key that will miss every
    ///      table, surfacing as the caller's "undefined …" error
    ///      rather than a panic. (sema should have caught this; the
    ///      fallback exists so the interpreter degrades gracefully.)
    ///
    /// # Why `String`, not `Cow<'_, str>`
    ///
    /// The IR-side sibling (`LoweringContext::qualify` in
    /// `phoenix-ir/src/lower.rs`) returns `Cow<'_, str>` and has a
    /// `Cow::Borrowed` fast path for entry/builtin modules so the
    /// per-call-site allocation is zero. This function deliberately
    /// returns an owned `String` instead — the AST interpreter's hot
    /// path is dominated by env lookups, AST cloning, and `Value`
    /// construction (which already allocate per call), so the
    /// per-call `to_string()` here is in the noise. Adding the `Cow`
    /// machinery would buy roughly nothing measurable while
    /// complicating every caller's signature with a borrow-vs-own
    /// branch. If a future profile points at name qualification, the
    /// IR-side `Cow` shape is the recipe to copy.
    pub(crate) fn qualify(&self, name: &str) -> String {
        let Some(current_module) = self.module_stack.last() else {
            return name.to_string();
        };
        let current_module: &phoenix_common::module_path::ModulePath = current_module.as_ref();
        if let Some(scope) = self.module_scopes.get(current_module)
            && let Some(qualified) = scope.get(name)
        {
            return qualified.clone();
        }
        phoenix_common::module_path::module_qualify(current_module, name)
    }

    /// Multi-module orchestration for [`run_modules`]. Registers every
    /// module's declarations under module-qualified keys (matching
    /// sema's `module_qualify`), then invokes `main` in the entry
    /// module.
    pub(crate) fn run_modules_inner(
        &mut self,
        modules: &[phoenix_modules::ResolvedSourceModule],
    ) -> Result<()> {
        self.register_builtin_enums();

        for module in modules {
            // Wrap each module's path once so every decl that records
            // it (in `FunctionEntry` / `MethodDef`) shares the same
            // allocation, and stack pushes are Rc bumps.
            let module_path = Rc::new(module.module_path.clone());
            for decl in &module.program.declarations {
                self.register_decl_in_module(decl, &module_path);
            }
        }

        // Invoke `main` in the entry module. `call_function_in_module`
        // owns the module-stack push/pop for the callee's frame
        // (`def_module = Some(entry)`); we don't pre-push here because
        // it would double-push and obscure the lifecycle.
        //
        // The bare `"main"` lookup key relies on two upstream invariants:
        //   1. `module_qualify(&ModulePath::entry(), "main") == "main"`
        //      — entry/builtin modules qualify to bare in the single
        //      source of truth (`phoenix_common::module_path::module_qualify`),
        //      so `register_decl_in_module` writes the entry's `main`
        //      into `self.functions` under the key `"main"`.
        //   2. Sema rejects `main` in any non-entry module
        //      (`negative_main_in_non_entry_module` in
        //      `crates/phoenix-driver/tests/multi_module_negative.rs`
        //      pins this), so a `lib::main` registration cannot reach
        //      this code in well-formed input.
        // If invariant (1) ever changes — e.g. entry gains a non-empty
        // qualifier — replace this with
        // `module_qualify(&ModulePath::entry(), "main")` so the lookup
        // key stays in lockstep with the registration key.
        let main_entry = self.functions.get("main").cloned();
        match main_entry {
            Some(entry) => self
                .call_function_in_module(
                    &entry.decl,
                    vec![],
                    vec![],
                    Some(Rc::new(phoenix_common::module_path::ModulePath::entry())),
                )
                .map(|_| ()),
            None => error("no `main` function found"),
        }
    }

    /// Register a single AST declaration under module-qualified keys.
    /// Used by the multi-module path; mirrors the per-decl logic in
    /// [`Interpreter::run_program`] but qualifies every key it inserts
    /// and records each function's owning module in its
    /// [`FunctionEntry`] (and each method's in its
    /// [`super::MethodDef`]) so [`Interpreter::call_function_in_module`]
    /// / [`Interpreter::call_method`] can push the right scope before
    /// evaluating the body.
    ///
    /// **Safety contract for variant-name "later wins":** this method
    /// keys `variant_to_enum` by bare variant name globally, so two
    /// enums sharing a variant name overwrite each other. That's safe
    /// because (a) sema rejects ambiguous variant references in scope
    /// at the use site (`module_scope::lookup_visible_enum_variant`),
    /// and (b) the resolver only loads modules transitively reachable
    /// from the entry, so an unscoped enum from an unimported module
    /// never reaches registration.
    ///
    /// TODO: collapse this with [`Interpreter::run_program`]'s per-decl
    /// block once the AST interpreter is fully multi-module and
    /// `run_program` is retired — the two paths only differ in (a)
    /// qualifying keys and (b) recording owning-module entries, both
    /// of which can be gated on an `Option<&Rc<ModulePath>>` argument
    /// the same way `register_methods` already is.
    pub(crate) fn register_decl_in_module(
        &mut self,
        decl: &Declaration,
        module_path: &Rc<phoenix_common::module_path::ModulePath>,
    ) {
        match decl {
            Declaration::Function(func) => {
                let qualified =
                    phoenix_common::module_path::module_qualify(module_path, &func.name);
                self.functions.insert(
                    qualified,
                    FunctionEntry {
                        decl: func.clone(),
                        module: Some(Rc::clone(module_path)),
                    },
                );
            }
            Declaration::Struct(s) => {
                let qualified = phoenix_common::module_path::module_qualify(module_path, &s.name);
                let field_names: Vec<String> = s.fields.iter().map(|f| f.name.clone()).collect();
                self.structs
                    .insert(qualified.clone(), StructDef { field_names });
                self.register_methods(&qualified, &s.methods, Some(module_path));
                for ti in &s.trait_impls {
                    self.register_methods(&qualified, &ti.methods, Some(module_path));
                }
            }
            Declaration::Enum(e) => {
                let qualified = phoenix_common::module_path::module_qualify(module_path, &e.name);
                let mut variants = HashMap::new();
                for v in &e.variants {
                    variants.insert(v.name.clone(), v.fields.len());
                    // `variant_to_enum` is keyed by bare variant name globally,
                    // so a user enum that shares a variant name with a builtin
                    // (`enum Foo { Some }`) or with another visible enum
                    // overwrites the prior mapping. This matches the pre-
                    // multi-module single-file behaviour ("later wins") and
                    // is benign in practice because:
                    //   * sema rejects user-shadowed *enum names* like
                    //     `enum Some {}` upstream, and
                    //   * cross-module variant collisions surface as an
                    //     "ambiguous variant" diagnostic at the use site
                    //     (`module_scope::lookup_visible_enum_variant`),
                    //     which exits the driver before we run.
                    self.variant_to_enum
                        .insert(v.name.clone(), qualified.clone());
                }
                self.enums.insert(qualified.clone(), EnumDef { variants });
                self.register_methods(&qualified, &e.methods, Some(module_path));
                for ti in &e.trait_impls {
                    self.register_methods(&qualified, &ti.methods, Some(module_path));
                }
            }
            Declaration::Impl(imp) => {
                let qualified_type =
                    phoenix_common::module_path::module_qualify(module_path, &imp.type_name);
                self.register_methods(&qualified_type, &imp.methods, Some(module_path));
            }
            Declaration::Trait(_)
            | Declaration::TypeAlias(_)
            | Declaration::Endpoint(_)
            | Declaration::Schema(_)
            | Declaration::Import(_) => {}
        }
    }
}

/// Multi-module entry point: aggregates declarations from every
/// [`phoenix_modules::ResolvedSourceModule`] under qualified names,
/// drains sema's `module_scopes` so name lookups translate
/// imported / aliased names correctly, and invokes `main` in the
/// entry module.
///
/// Takes `&mut Analysis` so the interpreter can `mem::take` the
/// `lambda_captures` and `module_scopes` maps rather than clone them
/// — sema's product is consumed once per program run, so taking
/// ownership of the per-module maps avoids a hash-map clone whose
/// size scales with declaration count.
///
/// Single-element inputs (entry-only) reduce to the same behavior as
/// [`super::run`] — the entry module qualifies to bare names, every
/// name resolves identically.
pub fn run_modules(
    modules: &[phoenix_modules::ResolvedSourceModule],
    analysis: &mut phoenix_sema::Analysis,
) -> std::result::Result<(), RuntimeError> {
    let mut interpreter = Interpreter::new();
    interpreter.lambda_captures = std::mem::take(&mut analysis.module.lambda_captures);
    interpreter.module_scopes = std::mem::take(&mut analysis.module.module_scopes);
    interpreter.run_modules_inner(modules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_common::module_path::ModulePath;

    /// Pin the [`Interpreter::qualify`] fallback path documented in
    /// this file: when the current module's scope exists but does not
    /// contain the requested name (case 2 in `qualify`'s doc — sema
    /// accepted a reference that won't be found at runtime), `qualify`
    /// returns `module_qualify(current_module, name)` rather than
    /// panicking or returning the bare name. Downstream registry
    /// probes miss every table and surface a "undefined …" error to
    /// the user — graceful degradation, not a panic.
    ///
    /// Without this test the fallback branch is exercised only
    /// indirectly (variant lookups fall through to the `variant_to_enum`
    /// bare-key probe, so the qualified key it returns is never
    /// observed). A regression that swapped the fallback for an
    /// `unwrap_or(name)` would silently corrupt cross-module dispatch
    /// rather than producing a clean miss.
    #[test]
    fn qualify_falls_back_to_module_qualify_for_unknown_name_in_non_entry_scope() {
        let mut interp = Interpreter::new();
        let lib = Rc::new(ModulePath(vec!["lib".to_string()]));
        // Empty scope: no name in this module resolves through scope.
        interp.module_scopes.insert((*lib).clone(), HashMap::new());
        interp.module_stack.push(Rc::clone(&lib));

        assert_eq!(interp.qualify("dangling"), "lib::dangling");
    }

    /// Mirror of the above for the entry module: a scope-miss inside
    /// the entry module qualifies-to-bare via `module_qualify`'s
    /// entry rule, so `qualify("foo")` round-trips to `"foo"` even
    /// when the scope is empty. This is what lets the multi-module
    /// path's entry frame produce the same registry keys as the
    /// single-file `run` path's bare-keyed registries.
    #[test]
    fn qualify_falls_back_to_bare_for_unknown_name_in_entry_scope() {
        let mut interp = Interpreter::new();
        let entry = Rc::new(ModulePath::entry());
        interp
            .module_scopes
            .insert((*entry).clone(), HashMap::new());
        interp.module_stack.push(Rc::clone(&entry));

        assert_eq!(interp.qualify("dangling"), "dangling");
    }

    /// Empty module stack (the single-file `run` path) returns the
    /// bare name verbatim — no scope is consulted, registries are
    /// expected to be bare-keyed, and the lookup hits directly. A
    /// regression that started consulting `module_scopes` even with
    /// an empty stack would trip this assert.
    #[test]
    fn qualify_returns_bare_name_when_module_stack_is_empty() {
        let interp = Interpreter::new();
        assert_eq!(interp.qualify("anything"), "anything");
    }

    /// Pin the scope-hit fast path: when the current module's scope
    /// maps `local → qualified`, `qualify` returns the qualified
    /// string from the scope (not the `module_qualify` fallback).
    /// The two agree for own-module decls; they diverge for imported
    /// items, where the scope says `User → models::User` but a
    /// `module_qualify` fallback against the *importing* module
    /// would yield the wrong key (`entry::User` if entry weren't
    /// special-cased).
    #[test]
    fn qualify_returns_scope_mapping_when_present() {
        let mut interp = Interpreter::new();
        let entry = Rc::new(ModulePath::entry());
        let mut scope = HashMap::new();
        // `User` is imported into the entry module from `models`; the
        // scope translates it to the qualified registry key.
        scope.insert("User".to_string(), "models::User".to_string());
        interp.module_scopes.insert((*entry).clone(), scope);
        interp.module_stack.push(Rc::clone(&entry));

        assert_eq!(interp.qualify("User"), "models::User");
    }
}
