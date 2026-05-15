//! Statement lowering.
//!
//! Lowers Phoenix AST statements into IR instructions within the current
//! basic block.

use crate::block::BlockId;
use crate::instruction::{Op, ValueId};
use crate::lower::{LoweringContext, VarBinding, lower_type};
use crate::terminator::Terminator;
use crate::types::IrType;
use phoenix_parser::ast::{
    Block, Declaration, ForSource, ForStmt, Program, Statement, VarDecl, VarDeclTarget, WhileStmt,
};

impl<'a> LoweringContext<'a> {
    /// Pass 2: Lower all function bodies.
    ///
    /// All `function_index` / `method_index` lookups qualify the
    /// bare AST name against `self.current_module` so a multi-module
    /// build resolves each declaration to its mangled key (matching
    /// sema's tables). Single-file callers leave `current_module`
    /// at [`ModulePath::entry()`] and the qualification reduces to
    /// the bare name — no behavior change.
    pub(crate) fn lower_function_bodies(&mut self, program: &Program) {
        for decl in &program.declarations {
            match decl {
                Declaration::Function(f) => {
                    let qname = self.qualify(&f.name);
                    if let Some(&func_id) = self.module.function_index.get(qname.as_ref()) {
                        self.lower_function_body(func_id, &f.params, &f.body);
                    }
                }
                Declaration::Impl(imp) => {
                    let qtype = self.qualify(&imp.type_name).into_owned();
                    for method in &imp.methods {
                        let key = (qtype.clone(), method.name.clone());
                        if let Some(&func_id) = self.module.method_index.get(&key) {
                            self.lower_function_body(func_id, &method.params, &method.body);
                        }
                    }
                }
                Declaration::Struct(s) => {
                    let qtype = self.qualify(&s.name).into_owned();
                    for method in &s.methods {
                        let key = (qtype.clone(), method.name.clone());
                        if let Some(&func_id) = self.module.method_index.get(&key) {
                            self.lower_function_body(func_id, &method.params, &method.body);
                        }
                    }
                    for trait_impl in &s.trait_impls {
                        for method in &trait_impl.methods {
                            let key = (qtype.clone(), method.name.clone());
                            if let Some(&func_id) = self.module.method_index.get(&key) {
                                self.lower_function_body(func_id, &method.params, &method.body);
                            }
                        }
                    }
                }
                Declaration::Enum(e) => {
                    let qtype = self.qualify(&e.name).into_owned();
                    for method in &e.methods {
                        let key = (qtype.clone(), method.name.clone());
                        if let Some(&func_id) = self.module.method_index.get(&key) {
                            self.lower_function_body(func_id, &method.params, &method.body);
                        }
                    }
                    for trait_impl in &e.trait_impls {
                        for method in &trait_impl.methods {
                            let key = (qtype.clone(), method.name.clone());
                            if let Some(&func_id) = self.module.method_index.get(&key) {
                                self.lower_function_body(func_id, &method.params, &method.body);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Lower a function body: create the entry block, bind parameters,
    /// and walk the body statements.
    fn lower_function_body(
        &mut self,
        func_id: crate::instruction::FuncId,
        _ast_params: &[phoenix_parser::ast::Param],
        body: &Block,
    ) {
        // Set up per-function state.
        self.current_func_id = Some(func_id);
        self.var_scopes.clear();
        self.loop_stack.clear();
        self.pending_defers.clear();
        self.push_scope();

        // Create the entry block.
        let entry = self.create_block();
        self.switch_to_block(entry);

        // Bind parameters as variables.
        let func = self.module.functions[func_id.index()].func();
        let param_types = func.param_types.clone();
        let param_names = func.param_names.clone();

        // Allocate ValueIds for parameters by emitting Copy ops from
        // block parameters (parameters are the first values in the entry block).
        for (name, ty) in param_names.iter().zip(param_types.iter()) {
            let param_val = self.add_block_param(entry, ty.clone());
            // For now, all parameters are immutable SSA bindings.
            self.define_var(name.clone(), VarBinding::Direct(param_val, ty.clone()));
        }

        // Lower the body block as a function-exit boundary: defers
        // are emitted *before* the body scope is popped so deferred
        // expressions can resolve free variables defined inside the
        // body. (Plain `lower_block_implicit` pops the scope first,
        // which would leave deferred references dangling.)
        let result = self.lower_function_body_block(body);

        // If the function returns non-void and we have a result, return it.
        // Otherwise return void.
        let return_type = self.module.functions[func_id.index()]
            .func()
            .return_type
            .clone();

        // Check if the current block already has a terminator (e.g. from a return statement).
        let needs_terminator = {
            let block = self.current_block();
            matches!(
                self.current_func_mut().block(block).terminator,
                Terminator::None
            )
        };

        if needs_terminator {
            if return_type != IrType::Void {
                if let Some(val) = result {
                    // Implicit-return coercion: mirrors the explicit
                    // `Statement::Return(r)` path so a concrete value
                    // flowing out of a `-> dyn Trait` via a trailing
                    // expression is wrapped in a `(data_ptr,
                    // vtable_ptr)` pair.  Without this, lambdas /
                    // functions that use implicit return would leave
                    // the call site expecting two slots and getting
                    // one, failing Cranelift verification.
                    //
                    // Defers have already been lowered for this
                    // fall-through path by `lower_function_body_block`
                    // above. Early-exit paths (`Statement::Return`,
                    // `lower_try`) emit their own defers before their
                    // own terminators and don't reach this branch.
                    let val = self.coerce_value_to_expected(val, &return_type, body.span);
                    self.terminate(Terminator::Return(Some(val)));
                } else {
                    // Void function with no explicit return.
                    self.terminate(Terminator::Return(None));
                }
            } else {
                self.terminate(Terminator::Return(None));
            }
        }

        self.pop_scope();
        self.current_func_id = None;
        self.current_block = None;
    }

    /// Lower a block of statements.  Returns `None` — does not produce a
    /// value (use [`lower_block_implicit`] for implicit-return blocks).
    pub(crate) fn lower_block(&mut self, block: &Block) {
        self.push_scope();
        for stmt in &block.statements {
            self.lower_stmt(stmt);
        }
        self.pop_scope();
    }

    /// Lower a function-exit-boundary block (top-level function body
    /// or lambda body).
    ///
    /// Same iteration as [`lower_block_implicit`] with one extra
    /// step: when the body falls through (no explicit `return`
    /// terminated it), pending defers are lowered *before* the
    /// body's scope is popped, so deferred expressions can still
    /// resolve free variables declared inside the body.
    ///
    /// Not appropriate for `if`-arms, `match`-arms, or loop bodies —
    /// those do not exit a function frame, so their defers (if any)
    /// must remain attached to the enclosing function's
    /// `pending_defers` and fire at *that* function's exit.
    ///
    /// **Scope responsibility.** The caller (`lower_function_body` for
    /// free functions / methods, `lower_lambda` for closures) is
    /// expected to have already pushed the *parameter* scope and
    /// bound parameters / captures into it. This method then pushes a
    /// second scope on top of the parameter scope as the body's
    /// outermost-let scope, snapshots the post-push depth as the
    /// defer-masking boundary, lowers, and pops only its own scope.
    /// `defer_outer_scope_depth` therefore points just above both
    /// scopes, so [`lower_defers_for_exit`]'s `split_off` masks
    /// inner-block scopes (if-arms, loop bodies) without disturbing
    /// either the body or the parameter scope.
    pub(crate) fn lower_function_body_block(&mut self, block: &Block) -> Option<ValueId> {
        self.push_scope();
        // Snapshot the scope depth at the function/lambda body's
        // outermost level. `lower_defers_for_exit` consults this to
        // hide inner scopes during defer expression lowering — see
        // its doc-comment for why. Saved/restored across nested
        // function-body invocations (lambdas inside this body).
        let saved_defer_depth = self.defer_outer_scope_depth;
        self.defer_outer_scope_depth = self.var_scopes.len();

        let result = self.lower_implicit_block_body(block);

        // Fall-through exit: lower defers BEFORE pop_scope. Skipped
        // when the block is already terminated (an explicit `return`
        // already emitted defers via the `Statement::Return` arm of
        // `lower_stmt`).
        if !self.current_block_is_terminated() {
            self.lower_defers_for_exit();
        }

        self.defer_outer_scope_depth = saved_defer_depth;
        self.pop_scope();
        result
    }

    /// `true` if the current block already carries a terminator (i.e.
    /// the lowering has emitted a `Return`/`Branch`/`Jump`/`Unreachable`
    /// for it). Used by [`lower_function_body_block`] and
    /// [`lower_implicit_block_body`] to decide whether to keep
    /// emitting into the current block or break.
    fn current_block_is_terminated(&self) -> bool {
        let bb = self.current_block();
        !matches!(self.current_func().block(bb).terminator, Terminator::None)
    }

    /// Lower a block of statements with implicit return semantics.
    /// If the last statement is a bare expression, its value is returned.
    pub(crate) fn lower_block_implicit(&mut self, block: &Block) -> Option<ValueId> {
        self.push_scope();
        let result = self.lower_implicit_block_body(block);
        self.pop_scope();
        result
    }

    /// Shared statement loop for [`lower_block_implicit`] and
    /// [`lower_function_body_block`]: the caller owns scope push/pop
    /// (and the function-body variant injects defer lowering between
    /// the loop and `pop_scope`). Stops early if the current block
    /// gains a terminator. The last statement, if a bare expression,
    /// is returned as the implicit-return value.
    fn lower_implicit_block_body(&mut self, block: &Block) -> Option<ValueId> {
        let mut result = None;
        for (i, stmt) in block.statements.iter().enumerate() {
            let is_last = i == block.statements.len() - 1;

            if self.current_block_is_terminated() {
                break;
            }

            if is_last && let Statement::Expression(expr_stmt) = stmt {
                let val = self.lower_expr(&expr_stmt.expr);
                result = Some(val);
                continue;
            }

            self.lower_stmt(stmt);
        }
        result
    }

    /// Lower a single statement.
    pub(crate) fn lower_stmt(&mut self, stmt: &Statement) {
        match stmt {
            Statement::VarDecl(v) => self.lower_var_decl(v),
            Statement::Expression(e) => {
                self.lower_expr(&e.expr);
            }
            Statement::Return(r) => {
                if let Some(val_expr) = &r.value {
                    let val = self.lower_expr(val_expr);
                    // Coerce to the function's declared return type so a
                    // concrete value flowing out of a `-> dyn Trait`
                    // function is wrapped in a `(data_ptr, vtable_ptr)`
                    // pair.  Without this, the returned value would be
                    // single-slot at call sites that expect two.
                    let expected = self.current_func().return_type.clone();
                    let val = self.coerce_expr_to_expected(val, val_expr.span(), &expected, r.span);
                    self.lower_defers_for_exit();
                    self.terminate(Terminator::Return(Some(val)));
                } else {
                    self.lower_defers_for_exit();
                    self.terminate(Terminator::Return(None));
                }
            }
            Statement::While(w) => {
                self.lower_while_stmt(w);
            }
            Statement::For(f) => {
                self.lower_for_stmt(f);
            }
            Statement::Break(_span) => {
                if let Some(loop_ctx) = self.current_loop().cloned() {
                    self.terminate(Terminator::Jump {
                        target: loop_ctx.break_target,
                        args: Vec::new(),
                    });
                }
            }
            Statement::Continue(_span) => {
                if let Some(loop_ctx) = self.current_loop().cloned() {
                    self.terminate(Terminator::Jump {
                        target: loop_ctx.continue_target,
                        args: Vec::new(),
                    });
                }
            }
            // `defer expr` — append the expression to the current
            // function's pending-defers list. It is lowered (in reverse
            // order) at every `Return` exit path. Lazy capture
            // semantics: free variables resolve at exit, not at the
            // defer point. See Phase 2.3 decision G.
            Statement::Defer(d) => {
                self.pending_defers.push(d.expr.clone());
            }
        }
    }

    /// Lower every defer registered so far in this function, LIFO.
    ///
    /// Called by every `Return` exit path before emitting
    /// `Terminator::Return` — the explicit `return` statement, the
    /// fall-through return at the end of a function/lambda body, and
    /// the `?` (try) early-return in `lower_try`. Each defer's result
    /// is dropped (defer is a statement, not an expression). The
    /// pending list is left intact so other exit paths in the same
    /// function emit the same defers.
    ///
    /// **Sema preconditions** ([`Checker::check_defer_placement`] in
    /// the sema crate):
    /// - Every `Statement::Defer` lives at the function/lambda body's
    ///   outermost statement level. So a deferred expression's free
    ///   variables can only refer to function parameters or the
    ///   body's outermost-level lets (no inner-scope bindings).
    /// - The deferred expression contains neither `return` nor `?`
    ///   (try). Both lower to `Terminator::Return`; allowing them
    ///   would mid-defer terminate the block the caller's own
    ///   `Terminator::Return` is about to land on.
    ///
    /// Combined, those preconditions guarantee `lower_expr` here
    /// neither terminates the current block nor mutates
    /// `self.pending_defers` (the take-and-restore below is just to
    /// release the borrow for the `&mut self` recursion).
    ///
    /// **Scope masking.** Inner scopes (loop bodies, if-arms, match
    /// arms) that happen to be on the scope stack at this exit point
    /// can SHADOW outer bindings — `lookup_var` searches innermost
    /// first — so we temporarily detach them with [`Vec::split_off`]
    /// before lowering each defer expression and reattach them after.
    /// Without this masking, a deferred `print(x)` at a `return` or
    /// `?` reached from inside a `let x = ...` block would resolve
    /// to the inner-shadowed `x`, diverging from the AST interp
    /// (which pops inner scopes before firing defers).
    ///
    /// **IR-size caveat.** Each call lowers every pending defer at
    /// the call site, so a function with N exit paths and a defer
    /// that itself constructs a lambda emits N copies of the
    /// lambda's lowered function. The all-arms-return shape (e.g.
    /// `if c { return 1 } else { return 2 }`) additionally emits a
    /// dead copy on the unreachable post-merge block — both arms
    /// terminate, so [`lower_function_body_block`]'s fall-through
    /// path runs `lower_defers_for_exit` again into a block no
    /// control flow reaches. DCE'd by Cranelift / the C backend, so
    /// it's an emit-time waste rather than a runtime concern.
    /// Currently acceptable (most defers are small calls); a future
    /// optimisation could share the defer's lowered code across
    /// exits via a join block. Tracked alongside the rest of Phase
    /// 2.7's IR-size work.
    pub(crate) fn lower_defers_for_exit(&mut self) {
        if self.pending_defers.is_empty() {
            return;
        }

        // Hide inner scopes for the duration of defer lowering so
        // free-variable lookups in the defer expression resolve
        // against the function's outermost scopes only. See the
        // **Scope masking** section above.
        let depth = self.defer_outer_scope_depth;
        let inner_scopes = self.var_scopes.split_off(depth);

        // Take rather than clone — sema preconditions guarantee no
        // defer expression lowers to anything that mutates
        // `pending_defers`, and `take` avoids an AST deep-clone of
        // every pending defer on every exit path.
        let defers = std::mem::take(&mut self.pending_defers);
        for expr in defers.iter().rev() {
            self.lower_expr(expr);
        }
        self.pending_defers = defers;

        self.var_scopes.extend(inner_scopes);
    }

    /// Lower a variable declaration.
    fn lower_var_decl(&mut self, v: &VarDecl) {
        // Make the let-binding's annotation visible to nested lowering
        // (e.g. `Map.builder()` / `List.builder()` carve-out, which
        // sema types as `MapBuilder<TypeVar(K), TypeVar(V)>` regardless
        // of the surrounding annotation — the annotation here is the
        // only carrier of the concrete K/V types at this point).
        // `mem::replace` swaps in the new hint and hands back the
        // previous one in a single move; the restore at the end of
        // the body keeps nested initializers (RHS of inner lets,
        // assignments, lambda bodies) from observing this binding's
        // annotation.
        let new_target = self
            .check
            .var_annotation_types
            .get(&v.span)
            .map(|t| crate::lower::lower_type(t, self.check));
        let prev_target = std::mem::replace(&mut self.current_target_type, new_target);
        let init_val = self.lower_expr(&v.initializer);
        self.current_target_type = prev_target;

        match &v.target {
            VarDeclTarget::Simple(name) => {
                // Read the sema-resolved annotation (not the raw TypeExpr)
                // so aliases expanding to `dyn Trait` pick up coercion.
                //
                // Span-keying caveat: `var_annotation_types` is keyed by
                // the `VarDecl`'s parser span. This is sound under the
                // current single-pass lowering where `VarDecl` spans are
                // immutable and 1:1 with the sema pass that recorded
                // them. Any future pass that rewrites, reparents, or
                // synthesizes `VarDecl` nodes between sema and lowering
                // (macros, desugaring, cross-file inlining) will silently
                // lose entries — `.get(&v.span)` returns `None`, dyn
                // coercion is skipped, and a concrete value flows into a
                // `dyn Trait` slot unwrapped. The planned Phase-3 fix
                // re-keys both this map and `call_type_args` on a stable
                // `NodeId` assigned at parse time; see the field doc on
                // `phoenix-sema/src/checker.rs::ResolvedModule::var_annotation_types`.
                let (init_val, ir_type) = match self.check.var_annotation_types.get(&v.span) {
                    Some(annotated) => {
                        let expected = lower_type(annotated, self.check);
                        let coerced = self.coerce_expr_to_expected(
                            init_val,
                            v.initializer.span(),
                            &expected,
                            v.span,
                        );
                        (coerced, expected)
                    }
                    None => (init_val, self.expr_type(&v.initializer.span())),
                };

                if v.is_mut {
                    // Mutable: allocate a stack slot, store the initial value.
                    let slot = self.emit(Op::Alloca(ir_type.clone()), IrType::I64, Some(v.span));
                    self.emit_void(Op::Store(slot, init_val), Some(v.span));
                    self.define_var(name.clone(), VarBinding::Mutable(slot, ir_type));
                } else {
                    // Immutable: the SSA value IS the variable.
                    self.define_var(name.clone(), VarBinding::Direct(init_val, ir_type));
                }
            }
            VarDeclTarget::StructDestructure {
                type_name,
                field_names,
            } => {
                // Look up the struct layout to get field indices.
                if let Some(layout) = self.module.struct_layouts.get(type_name).cloned() {
                    for field_name in field_names {
                        if let Some(idx) = layout.iter().position(|(name, _)| name == field_name) {
                            let field_type = layout[idx].1.clone();
                            let field_val = self.emit(
                                Op::StructGetField(init_val, idx as u32),
                                field_type.clone(),
                                Some(v.span),
                            );
                            self.define_var(
                                field_name.clone(),
                                VarBinding::Direct(field_val, field_type),
                            );
                        }
                    }
                }
            }
        }
    }

    /// Lower a while loop into header/body/exit blocks.
    ///
    /// Produces: `header` (evaluate condition, branch) → `body` (loop back
    /// to header) → `exit`/`merge` (else clause if present, then continue).
    fn lower_while_stmt(&mut self, w: &WhileStmt) {
        let header_block = self.create_block();
        let body_block = self.create_block();
        let (break_block, exit_block, merge_block) =
            self.create_loop_exit_blocks(w.else_block.is_some());

        // Jump to the header.
        self.terminate(Terminator::Jump {
            target: header_block,
            args: Vec::new(),
        });

        // Header: evaluate condition.
        self.switch_to_block(header_block);
        let cond = self.lower_expr(&w.condition);
        self.terminate(Terminator::Branch {
            condition: cond,
            true_block: body_block,
            true_args: Vec::new(),
            false_block: exit_block,
            false_args: Vec::new(),
        });

        // Body.
        self.push_loop(crate::lower::LoopContext {
            continue_target: header_block,
            break_target: break_block,
        });
        self.switch_to_block(body_block);
        self.lower_block(&w.body);
        if self.block_needs_terminator() {
            self.terminate(Terminator::Jump {
                target: header_block,
                args: Vec::new(),
            });
        }
        self.pop_loop();

        // Else / merge.
        self.finish_loop_exit(w.else_block.as_ref(), break_block, exit_block, merge_block);
    }

    /// Lower a for loop.
    fn lower_for_stmt(&mut self, f: &ForStmt) {
        match &f.source {
            ForSource::Range { start, end } => {
                self.lower_for_range(f, start, end);
            }
            ForSource::Iterable(iter_expr) => {
                self.lower_for_iterable(f, iter_expr);
            }
        }
    }

    /// Lower a range-based for loop: `for i in start..end { body }`.
    ///
    /// Uses an alloca'd counter with header/body/latch blocks.  The latch
    /// increments the counter and jumps back to the header.
    fn lower_for_range(
        &mut self,
        f: &ForStmt,
        start: &phoenix_parser::ast::Expr,
        end: &phoenix_parser::ast::Expr,
    ) {
        let start_val = self.lower_expr(start);
        let end_val = self.lower_expr(end);

        // Allocate the loop variable as mutable.
        let counter_slot = self.emit(Op::Alloca(IrType::I64), IrType::I64, Some(f.span));
        self.emit_void(Op::Store(counter_slot, start_val), Some(f.span));

        let header_block = self.create_block();
        let body_block = self.create_block();
        let latch_block = self.create_block();
        let (break_block, exit_block, merge_block) =
            self.create_loop_exit_blocks(f.else_block.is_some());

        self.terminate(Terminator::Jump {
            target: header_block,
            args: Vec::new(),
        });

        // Header: check bounds.
        self.switch_to_block(header_block);
        let i_val = self.emit(Op::Load(counter_slot), IrType::I64, Some(f.span));
        let cond = self.emit(Op::ILt(i_val, end_val), IrType::Bool, Some(f.span));
        self.terminate(Terminator::Branch {
            condition: cond,
            true_block: body_block,
            true_args: Vec::new(),
            false_block: exit_block,
            false_args: Vec::new(),
        });

        // Body: bind loop variable.
        self.push_loop(crate::lower::LoopContext {
            continue_target: latch_block,
            break_target: break_block,
        });
        self.switch_to_block(body_block);
        self.push_scope();
        let i_in_body = self.emit(Op::Load(counter_slot), IrType::I64, Some(f.span));
        self.define_var(
            f.var_name.clone(),
            VarBinding::Direct(i_in_body, IrType::I64),
        );
        self.lower_block(&f.body);
        self.pop_scope();
        if self.block_needs_terminator() {
            self.terminate(Terminator::Jump {
                target: latch_block,
                args: Vec::new(),
            });
        }
        self.pop_loop();

        // Latch: increment counter.
        self.switch_to_block(latch_block);
        let i_cur = self.emit(Op::Load(counter_slot), IrType::I64, Some(f.span));
        let one = self.emit(Op::ConstI64(1), IrType::I64, Some(f.span));
        let i_next = self.emit(Op::IAdd(i_cur, one), IrType::I64, Some(f.span));
        self.emit_void(Op::Store(counter_slot, i_next), Some(f.span));
        self.terminate(Terminator::Jump {
            target: header_block,
            args: Vec::new(),
        });

        // Else / merge.
        self.finish_loop_exit(f.else_block.as_ref(), break_block, exit_block, merge_block);
    }

    /// Lower a collection-based for loop: `for item in list { body }`.
    ///
    /// Computes the list length up front, then uses an alloca'd index
    /// counter with `List.get` to fetch each element.
    fn lower_for_iterable(&mut self, f: &ForStmt, iter_expr: &phoenix_parser::ast::Expr) {
        let list_val = self.lower_expr(iter_expr);

        // Get list length.
        let len = self.emit(
            Op::BuiltinCall("List.length".to_string(), vec![list_val]),
            IrType::I64,
            Some(f.span),
        );

        // Allocate index counter.
        let idx_slot = self.emit(Op::Alloca(IrType::I64), IrType::I64, Some(f.span));
        let zero = self.emit(Op::ConstI64(0), IrType::I64, Some(f.span));
        self.emit_void(Op::Store(idx_slot, zero), Some(f.span));

        let header_block = self.create_block();
        let body_block = self.create_block();
        let latch_block = self.create_block();
        let (break_block, exit_block, merge_block) =
            self.create_loop_exit_blocks(f.else_block.is_some());

        self.terminate(Terminator::Jump {
            target: header_block,
            args: Vec::new(),
        });

        // Header.
        self.switch_to_block(header_block);
        let idx_val = self.emit(Op::Load(idx_slot), IrType::I64, Some(f.span));
        let cond = self.emit(Op::ILt(idx_val, len), IrType::Bool, Some(f.span));
        self.terminate(Terminator::Branch {
            condition: cond,
            true_block: body_block,
            true_args: Vec::new(),
            false_block: exit_block,
            false_args: Vec::new(),
        });

        // Body.
        self.push_loop(crate::lower::LoopContext {
            continue_target: latch_block,
            break_target: break_block,
        });
        self.switch_to_block(body_block);
        self.push_scope();
        let idx_in_body = self.emit(Op::Load(idx_slot), IrType::I64, Some(f.span));
        // Determine the element type from the list's type.
        let elem_type = {
            let list_ir_type = self.expr_type(&iter_expr.span());
            match list_ir_type {
                IrType::ListRef(elem) => *elem,
                _ => IrType::Void,
            }
        };
        let item = self.emit(
            Op::BuiltinCall("List.get".to_string(), vec![list_val, idx_in_body]),
            elem_type.clone(),
            Some(f.span),
        );
        self.define_var(f.var_name.clone(), VarBinding::Direct(item, elem_type));
        self.lower_block(&f.body);
        self.pop_scope();
        if self.block_needs_terminator() {
            self.terminate(Terminator::Jump {
                target: latch_block,
                args: Vec::new(),
            });
        }
        self.pop_loop();

        // Latch.
        self.switch_to_block(latch_block);
        let idx_cur = self.emit(Op::Load(idx_slot), IrType::I64, Some(f.span));
        let one = self.emit(Op::ConstI64(1), IrType::I64, Some(f.span));
        let idx_next = self.emit(Op::IAdd(idx_cur, one), IrType::I64, Some(f.span));
        self.emit_void(Op::Store(idx_slot, idx_next), Some(f.span));
        self.terminate(Terminator::Jump {
            target: header_block,
            args: Vec::new(),
        });

        // Else / merge.
        self.finish_loop_exit(f.else_block.as_ref(), break_block, exit_block, merge_block);
    }

    /// Returns `true` if the current block does not yet have a terminator.
    pub(crate) fn block_needs_terminator(&mut self) -> bool {
        let block = self.current_block();
        matches!(
            self.current_func_mut().block(block).terminator,
            Terminator::None
        )
    }

    /// Create exit/merge blocks for a loop with an optional else clause.
    ///
    /// Returns `(break_block, exit_block, merge_block)`.  When there is no
    /// else clause, all three are the same block.  When there is an else
    /// clause, `exit_block` holds the else body, `break_block` is the
    /// target for `break` statements, and `merge_block` is the join point.
    fn create_loop_exit_blocks(&mut self, has_else: bool) -> (BlockId, BlockId, BlockId) {
        let break_block = self.create_block();
        if has_else {
            let exit_block = self.create_block();
            let merge_block = self.create_block();
            (break_block, exit_block, merge_block)
        } else {
            (break_block, break_block, break_block)
        }
    }

    /// Wire up the else/merge blocks at the end of a loop.
    ///
    /// If an else block is present, lowers it into `exit_block` and adds
    /// jumps from both `exit_block` and `break_block` to `merge_block`.
    fn finish_loop_exit(
        &mut self,
        else_block: Option<&Block>,
        break_block: BlockId,
        exit_block: BlockId,
        merge_block: BlockId,
    ) {
        if let Some(else_body) = else_block {
            self.switch_to_block(exit_block);
            self.lower_block(else_body);
            if self.block_needs_terminator() {
                self.terminate(Terminator::Jump {
                    target: merge_block,
                    args: Vec::new(),
                });
            }

            self.switch_to_block(break_block);
            self.terminate(Terminator::Jump {
                target: merge_block,
                args: Vec::new(),
            });

            self.switch_to_block(merge_block);
        } else {
            self.switch_to_block(break_block);
        }
    }
}
