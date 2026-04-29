//! Pre-allocation passes (pre-pass A and pre-pass B) that stamp every
//! user-declared callable with a stable [`FuncId`] before the
//! registration pass runs.
//!
//! These helpers walk the AST *before* registration so that
//! [`Checker::register_function`](crate::checker::Checker::register_function)
//! and [`Checker::register_impl`](crate::checker::Checker::register_impl)
//! can stamp each `FunctionInfo` / `MethodInfo` with the id IR lowering
//! will adopt.  They mutate `Checker` state (`next_func_id`,
//! `pending_function_ids`, `pending_user_method_ids`,
//! `user_method_offset`) that the registration pass then consumes.
//!
//! Free functions occupy `FuncId(0..N)` in AST-declaration order.
//! User-declared methods occupy `FuncId(N..N+M)` in the order
//! IR lowering's registration walks them — inline methods at the
//! struct/enum declaration site (inherent first, then trait impls in
//! source order), and standalone `impl` blocks at their own
//! declaration site.

use crate::checker::Checker;
use phoenix_common::ids::FuncId;
use phoenix_common::module_path::module_qualify;
use phoenix_parser::ast::{Declaration, Program};

impl Checker {
    /// Allocate a fresh `FuncId` for a free function with the given
    /// **bare** name in the current module (`Checker::current_module`).
    /// The bare name is qualified into the canonical key
    /// (`module_qualify(&self.current_module, name)`) before being
    /// stored, so two modules can both declare `foo` without colliding
    /// in `pending_function_ids`. Returns the existing id if one was
    /// already allocated for the same qualified key (within-module
    /// duplicate; sema's `register_function` later emits a
    /// "function `foo` is already defined" diagnostic).
    pub(crate) fn function_id_for(&mut self, name: &str) -> FuncId {
        let qualified = module_qualify(&self.current_module, name);
        if let Some(id) = self.pending_function_ids.get(&qualified) {
            return *id;
        }
        let id = FuncId(self.next_func_id);
        self.next_func_id += 1;
        self.pending_function_ids.insert(qualified, id);
        id
    }

    /// Walk the AST in declaration order to allocate `FuncId`s for
    /// every free function.  Inserts each id into
    /// [`Checker::pending_function_ids`] under the module-qualified
    /// key; the registration pass reads these back when it constructs
    /// each `FunctionInfo`.
    ///
    /// Skips functions whose name shadows a builtin: registration will
    /// reject them up front (see `register_function`'s
    /// `is_builtin_name` guard), and an unallocated id keeps the
    /// `pending_function_ids → functions` invariant in
    /// `build_functions` intact.
    pub(crate) fn pre_allocate_function_ids(&mut self, program: &Program) {
        for decl in &program.declarations {
            if let Declaration::Function(func) = decl {
                if self.is_builtin_name(&func.name) {
                    continue;
                }
                self.function_id_for(&func.name);
            }
        }
    }

    /// Allocate a fresh `FuncId` for a user-declared method. Used by
    /// pre-pass B. The receiver type-name is qualified against the
    /// current module; the method name stays bare relative to its
    /// type. First allocation wins on duplicate `(qualified_type,
    /// method_name)` pairs within the same module (within-module
    /// duplicates are diagnosed in `register_impl`).
    pub(crate) fn user_method_id_for(&mut self, type_name: &str, method_name: &str) -> FuncId {
        let qualified_type = module_qualify(&self.current_module, type_name);
        let key = (qualified_type, method_name.to_string());
        if let Some(id) = self.pending_user_method_ids.get(&key) {
            return *id;
        }
        let id = FuncId(self.next_func_id);
        self.next_func_id += 1;
        self.pending_user_method_ids.insert(key, id);
        id
    }

    /// Walk the AST in IR-lowering's registration order to allocate
    /// `FuncId`s for every user-declared method.  Inline methods on a
    /// struct/enum are visited at the struct/enum declaration site
    /// (inherent methods first, then trait impls in source order);
    /// standalone `impl` blocks are visited at their own declaration
    /// site.  This shape must match `IrModule`'s registration order
    /// so that `IrModule.functions[id.0]` corresponds to the same
    /// callable as `ResolvedModule.user_methods[id.0 - offset]`.
    ///
    /// **Invariant:** every pre-allocated `FuncId` must end up filled
    /// by the time `build_user_and_builtin_methods` runs. The two
    /// arms below maintain this invariant via *different* mechanisms
    /// and must not be confused:
    ///
    /// - **Struct/Enum arms** skip allocation when the parent is
    ///   builtin-named, mirroring `register_struct` /
    ///   `register_enum`'s `is_builtin_name` rejection (which also
    ///   skips registration of the parent's methods). No allocation,
    ///   no fill — symmetric.
    /// - **Impl arm** does *not* skip on builtin-named receivers —
    ///   `impl Option { … }` allocates ids for its methods, then
    ///   `register_impl`'s `is_builtin_name` rejection routes those
    ///   ids through the orphan path
    ///   ([`Self::consume_orphan_methods`]), which fills the slots
    ///   without inserting into `self.methods`.
    ///
    /// A future change that adds an `is_builtin_name` skip to the
    /// impl arm here would also need to remove the orphan-path call
    /// from `register_impl` (and vice versa) — touch both sides
    /// together.
    pub(crate) fn pre_allocate_user_method_ids(&mut self, program: &Program) {
        for decl in &program.declarations {
            match decl {
                Declaration::Struct(s) => {
                    // Builtin-named parent will be rejected at
                    // registration; skip allocating ids for its
                    // methods so the post-registration "every
                    // pre-allocated id is filled" invariant holds.
                    if self.is_builtin_name(&s.name) {
                        continue;
                    }
                    for m in &s.methods {
                        self.user_method_id_for(&s.name, &m.name);
                    }
                    for ti in &s.trait_impls {
                        for m in &ti.methods {
                            self.user_method_id_for(&s.name, &m.name);
                        }
                    }
                }
                Declaration::Enum(e) => {
                    if self.is_builtin_name(&e.name) {
                        continue;
                    }
                    for m in &e.methods {
                        self.user_method_id_for(&e.name, &m.name);
                    }
                    for ti in &e.trait_impls {
                        for m in &ti.methods {
                            self.user_method_id_for(&e.name, &m.name);
                        }
                    }
                }
                Declaration::Impl(imp) => {
                    for m in &imp.methods {
                        self.user_method_id_for(&imp.type_name, &m.name);
                    }
                }
                _ => {}
            }
        }
    }
}
