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
use phoenix_parser::ast::{Declaration, Program};

impl Checker {
    /// Allocate a fresh `FuncId` for a free function with the given
    /// name (or return the existing id if one was already allocated).
    /// Used by pre-pass A.  Sema rejects duplicate function names at
    /// registration; this helper keeps the first-allocated id and
    /// returns it on subsequent calls so the ill-formed program still
    /// has well-defined ids.
    pub(crate) fn function_id_for(&mut self, name: &str) -> FuncId {
        if let Some(id) = self.pending_function_ids.get(name) {
            return *id;
        }
        let id = FuncId(self.next_func_id);
        self.next_func_id += 1;
        self.pending_function_ids.insert(name.to_string(), id);
        id
    }

    /// Walk the AST in declaration order to allocate `FuncId`s for
    /// every free function.  Inserts each id into
    /// [`Checker::pending_function_ids`]; the registration pass reads
    /// these back when it constructs each `FunctionInfo`.
    pub(crate) fn pre_allocate_function_ids(&mut self, program: &Program) {
        for decl in &program.declarations {
            if let Declaration::Function(func) = decl {
                self.function_id_for(&func.name);
            }
        }
    }

    /// Allocate a fresh `FuncId` for a user-declared method.  Used by
    /// pre-pass B.  First allocation wins on duplicate names within
    /// the same `(type_name, method_name)` pair (sema later emits a
    /// duplicate-method diagnostic).
    pub(crate) fn user_method_id_for(&mut self, type_name: &str, method_name: &str) -> FuncId {
        let key = (type_name.to_string(), method_name.to_string());
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
    pub(crate) fn pre_allocate_user_method_ids(&mut self, program: &Program) {
        for decl in &program.declarations {
            match decl {
                Declaration::Struct(s) => {
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
