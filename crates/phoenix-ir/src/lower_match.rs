//! Match expression lowering.
//!
//! Lowers Phoenix `match` expressions into IR control flow.  Enum matches
//! use discriminant-based branching; non-enum matches chain equality tests.

use crate::block::BlockId;
use crate::instruction::{Op, ValueId};
use crate::lower::{LoweringContext, VarBinding};
use crate::terminator::Terminator;
use crate::types::IrType;
use phoenix_common::span::Span;
use phoenix_parser::ast::{MatchBody, MatchExpr};
use phoenix_sema::types::Type;

/// Shared context for match arm lowering (avoids passing many parameters).
struct MatchCtx {
    merge_block: BlockId,
    result_param: Option<ValueId>,
    span: Option<Span>,
}

impl<'a> LoweringContext<'a> {
    /// Lower a match expression.
    pub(crate) fn lower_match(&mut self, m: &MatchExpr) -> ValueId {
        let subject = self.lower_expr(&m.subject);
        let span = Some(m.span);
        let result_type = self.expr_type(&m.span);

        let merge_block = self.create_block();
        let result_param = if result_type != IrType::Void {
            Some(self.add_block_param(merge_block, result_type.clone()))
        } else {
            None
        };

        // Determine if subject is an enum (for discriminant-based matching).
        let subject_type = self
            .source_type(&m.subject.span())
            .cloned()
            .unwrap_or(Type::Void);

        let enum_name = match &subject_type {
            Type::Named(name) | Type::Generic(name, _) => {
                if self.module.enum_layouts.contains_key(name) {
                    Some(name.clone())
                } else {
                    None
                }
            }
            _ => None,
        };

        // Create blocks for each arm.
        let arm_blocks: Vec<_> = m.arms.iter().map(|_| self.create_block()).collect();
        let ctx = MatchCtx {
            merge_block,
            result_param,
            span,
        };

        // Generate the dispatch chain.
        if let Some(ref enum_name) = enum_name {
            self.lower_match_enum(m, subject, &subject_type, enum_name, &arm_blocks, &ctx);
        } else {
            self.lower_match_non_enum(m, subject, &subject_type, &arm_blocks, &ctx);
        }

        self.switch_to_block(merge_block);
        result_param.unwrap_or_else(|| self.emit(Op::ConstBool(false), IrType::Bool, span))
    }

    /// Lower enum match arms using discriminant-based branching.
    fn lower_match_enum(
        &mut self,
        m: &MatchExpr,
        subject: ValueId,
        subject_type: &Type,
        enum_name: &str,
        arm_blocks: &[BlockId],
        ctx: &MatchCtx,
    ) {
        let disc = self.emit(Op::EnumDiscriminant(subject), IrType::I64, ctx.span);
        let variants = self
            .module
            .enum_layouts
            .get(enum_name)
            .cloned()
            .unwrap_or_default();

        for (i, arm) in m.arms.iter().enumerate() {
            let arm_block = arm_blocks[i];
            let is_last = i + 1 >= m.arms.len();

            // For both last and non-last arms, create a fresh block.
            // Non-last arms use it for the next discriminant test; the last
            // arm's block becomes an unreachable fallthrough (wired up below).
            let next_block = self.create_block();

            match &arm.pattern {
                phoenix_parser::ast::Pattern::Variant(vp) => {
                    if let Some((var_idx, (_, field_types))) = variants
                        .iter()
                        .enumerate()
                        .find(|(_, (name, _))| *name == vp.variant)
                    {
                        let var_idx_val =
                            self.emit(Op::ConstI64(var_idx as i64), IrType::I64, ctx.span);
                        let matches = self.emit(Op::IEq(disc, var_idx_val), IrType::Bool, ctx.span);
                        self.terminate(Terminator::Branch {
                            condition: matches,
                            true_block: arm_block,
                            true_args: Vec::new(),
                            false_block: next_block,
                            false_args: Vec::new(),
                        });

                        self.switch_to_block(arm_block);
                        self.push_scope();
                        for (j, binding_name) in vp.bindings.iter().enumerate() {
                            let field_type = field_types.get(j).cloned().unwrap_or(IrType::Void);
                            let field_val = self.emit(
                                Op::EnumGetField(subject, j as u32),
                                field_type.clone(),
                                ctx.span,
                            );
                            self.define_var(
                                binding_name.clone(),
                                VarBinding::Direct(field_val, field_type),
                            );
                        }
                    } else {
                        // Variant not found in layout — sema should have
                        // rejected this, but be defensive: jump past the arm
                        // and push a scope so the pop in lower_arm_body stays
                        // balanced.
                        debug_assert!(
                            false,
                            "unknown enum variant `{}` in match lowering",
                            vp.variant
                        );
                        self.terminate(Terminator::Jump {
                            target: next_block,
                            args: Vec::new(),
                        });
                        self.switch_to_block(arm_block);
                        self.push_scope();
                    }
                }
                phoenix_parser::ast::Pattern::Wildcard(_) => {
                    self.terminate(Terminator::Jump {
                        target: arm_block,
                        args: Vec::new(),
                    });
                    self.switch_to_block(arm_block);
                    self.push_scope();
                }
                phoenix_parser::ast::Pattern::Binding(name, _) => {
                    self.terminate(Terminator::Jump {
                        target: arm_block,
                        args: Vec::new(),
                    });
                    self.switch_to_block(arm_block);
                    self.push_scope();
                    let subject_ir_type = self.expr_type(&m.subject.span());
                    self.define_var(name.clone(), VarBinding::Direct(subject, subject_ir_type));
                }
                phoenix_parser::ast::Pattern::Literal(lit) => {
                    let lit_val = self.lower_literal(lit);
                    let matches =
                        self.emit_comparison_for_type(subject_type, subject, lit_val, ctx.span);
                    self.terminate(Terminator::Branch {
                        condition: matches,
                        true_block: arm_block,
                        true_args: Vec::new(),
                        false_block: next_block,
                        false_args: Vec::new(),
                    });
                    self.switch_to_block(arm_block);
                    self.push_scope();
                }
            }

            self.lower_arm_body(&arm.body, ctx);

            if is_last {
                // Terminate the unreachable fallthrough block.
                self.switch_to_block(next_block);
                self.terminate(Terminator::Unreachable);
            } else {
                self.switch_to_block(next_block);
            }
        }
    }

    /// Lower non-enum match arms using chained equality tests.
    fn lower_match_non_enum(
        &mut self,
        m: &MatchExpr,
        subject: ValueId,
        subject_type: &Type,
        arm_blocks: &[BlockId],
        ctx: &MatchCtx,
    ) {
        for (i, arm) in m.arms.iter().enumerate() {
            let arm_block = arm_blocks[i];
            let is_last = i + 1 >= m.arms.len();

            let next_block = self.create_block();

            match &arm.pattern {
                phoenix_parser::ast::Pattern::Literal(lit) => {
                    let lit_val = self.lower_literal(lit);
                    let matches =
                        self.emit_comparison_for_type(subject_type, subject, lit_val, ctx.span);
                    self.terminate(Terminator::Branch {
                        condition: matches,
                        true_block: arm_block,
                        true_args: Vec::new(),
                        false_block: next_block,
                        false_args: Vec::new(),
                    });
                }
                phoenix_parser::ast::Pattern::Wildcard(_)
                | phoenix_parser::ast::Pattern::Binding(_, _)
                | phoenix_parser::ast::Pattern::Variant(_) => {
                    self.terminate(Terminator::Jump {
                        target: arm_block,
                        args: Vec::new(),
                    });
                }
            }

            self.switch_to_block(arm_block);
            self.push_scope();

            if let phoenix_parser::ast::Pattern::Binding(name, _) = &arm.pattern {
                let subject_ir_type = self.expr_type(&m.subject.span());
                self.define_var(name.clone(), VarBinding::Direct(subject, subject_ir_type));
            }

            self.lower_arm_body(&arm.body, ctx);

            if is_last {
                self.switch_to_block(next_block);
                self.terminate(Terminator::Unreachable);
            } else {
                self.switch_to_block(next_block);
            }
        }
    }

    /// Lower a match arm body and jump to the merge block.
    fn lower_arm_body(&mut self, body: &MatchBody, ctx: &MatchCtx) {
        let arm_val = match body {
            MatchBody::Expr(expr) => self.lower_expr(expr),
            MatchBody::Block(block) => self
                .lower_block_implicit(block)
                .unwrap_or_else(|| self.emit(Op::ConstBool(false), IrType::Bool, ctx.span)),
        };
        self.pop_scope();

        if self.block_needs_terminator() {
            let args = if ctx.result_param.is_some() {
                vec![arm_val]
            } else {
                Vec::new()
            };
            self.terminate(Terminator::Jump {
                target: ctx.merge_block,
                args,
            });
        }
    }
}
