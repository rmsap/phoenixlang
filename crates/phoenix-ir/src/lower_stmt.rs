//! Statement lowering.
//!
//! Lowers Phoenix AST statements into IR instructions within the current
//! basic block.

use crate::block::BlockId;
use crate::instruction::{Op, ValueId};
use crate::lower::{LoweringContext, VarBinding};
use crate::terminator::Terminator;
use crate::types::IrType;
use phoenix_parser::ast::{
    Block, Declaration, ForSource, ForStmt, Program, Statement, VarDecl, VarDeclTarget, WhileStmt,
};

impl<'a> LoweringContext<'a> {
    /// Pass 2: Lower all function bodies.
    pub(crate) fn lower_function_bodies(&mut self, program: &Program) {
        for decl in &program.declarations {
            match decl {
                Declaration::Function(f) => {
                    if let Some(&func_id) = self.module.function_index.get(&f.name) {
                        self.lower_function_body(func_id, &f.params, &f.body);
                    }
                }
                Declaration::Impl(imp) => {
                    for method in &imp.methods {
                        let key = (imp.type_name.clone(), method.name.clone());
                        if let Some(&func_id) = self.module.method_index.get(&key) {
                            self.lower_function_body(func_id, &method.params, &method.body);
                        }
                    }
                }
                Declaration::Struct(s) => {
                    for method in &s.methods {
                        let key = (s.name.clone(), method.name.clone());
                        if let Some(&func_id) = self.module.method_index.get(&key) {
                            self.lower_function_body(func_id, &method.params, &method.body);
                        }
                    }
                    for trait_impl in &s.trait_impls {
                        for method in &trait_impl.methods {
                            let key = (s.name.clone(), method.name.clone());
                            if let Some(&func_id) = self.module.method_index.get(&key) {
                                self.lower_function_body(func_id, &method.params, &method.body);
                            }
                        }
                    }
                }
                Declaration::Enum(e) => {
                    for method in &e.methods {
                        let key = (e.name.clone(), method.name.clone());
                        if let Some(&func_id) = self.module.method_index.get(&key) {
                            self.lower_function_body(func_id, &method.params, &method.body);
                        }
                    }
                    for trait_impl in &e.trait_impls {
                        for method in &trait_impl.methods {
                            let key = (e.name.clone(), method.name.clone());
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
        self.push_scope();

        // Create the entry block.
        let entry = self.create_block();
        self.switch_to_block(entry);

        // Bind parameters as variables.
        let param_types = self.module.functions[func_id.0 as usize]
            .param_types
            .clone();
        let param_names = self.module.functions[func_id.0 as usize]
            .param_names
            .clone();

        // Allocate ValueIds for parameters by emitting Copy ops from
        // block parameters (parameters are the first values in the entry block).
        for (name, ty) in param_names.iter().zip(param_types.iter()) {
            let param_val = self.add_block_param(entry, ty.clone());
            // For now, all parameters are immutable SSA bindings.
            self.define_var(name.clone(), VarBinding::Direct(param_val, ty.clone()));
        }

        // Lower the body block.
        let result = self.lower_block_implicit(body);

        // If the function returns non-void and we have a result, return it.
        // Otherwise return void.
        let return_type = self.module.functions[func_id.0 as usize]
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

    /// Lower a block of statements with implicit return semantics.
    /// If the last statement is a bare expression, its value is returned.
    pub(crate) fn lower_block_implicit(&mut self, block: &Block) -> Option<ValueId> {
        self.push_scope();
        let mut result = None;

        for (i, stmt) in block.statements.iter().enumerate() {
            let is_last = i == block.statements.len() - 1;

            // Check if the current block already has a terminator.
            let has_terminator = {
                let bb = self.current_block();
                !matches!(
                    self.current_func_mut().block(bb).terminator,
                    Terminator::None
                )
            };
            if has_terminator {
                break;
            }

            if is_last && let Statement::Expression(expr_stmt) = stmt {
                // Last expression — its value is the implicit return.
                let val = self.lower_expr(&expr_stmt.expr);
                result = Some(val);
                continue;
            }

            self.lower_stmt(stmt);
        }

        self.pop_scope();
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
                    self.terminate(Terminator::Return(Some(val)));
                } else {
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
        }
    }

    /// Lower a variable declaration.
    fn lower_var_decl(&mut self, v: &VarDecl) {
        let init_val = self.lower_expr(&v.initializer);

        match &v.target {
            VarDeclTarget::Simple(name) => {
                if v.is_mut {
                    // Mutable: allocate a stack slot, store the initial value.
                    let ir_type = self.expr_type(&v.initializer.span());
                    let slot = self.emit(Op::Alloca(ir_type.clone()), IrType::I64, Some(v.span));
                    self.emit_void(Op::Store(slot, init_val), Some(v.span));
                    self.define_var(name.clone(), VarBinding::Mutable(slot, ir_type));
                } else {
                    // Immutable: the SSA value IS the variable.
                    let ir_type = self.expr_type(&v.initializer.span());
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
