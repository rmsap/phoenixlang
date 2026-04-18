//! Match expression lowering.
//!
//! Lowers Phoenix `match` expressions into IR control flow.  Enum matches
//! use discriminant-based branching (one block per variant); non-enum
//! matches chain equality tests.
//!
//! Generic type substitution: for generic enums like `Option<Int>` or
//! `Result<Int, String>`, the enum layout uses `__generic` placeholders.
//! [`resolve_field_type`] substitutes these with concrete type arguments
//! at the match site.

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
    /// Block where all match arms jump after executing their body.
    merge_block: BlockId,
    /// Block parameter on `merge_block` that receives the match result
    /// value.  `None` if the match result is `Void`.
    result_param: Option<ValueId>,
    /// Source span of the `match` expression (for diagnostics).
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
        result_param.unwrap_or_else(|| self.void_placeholder(span))
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
                        let type_args = match subject_type {
                            Type::Generic(_, args) => args.clone(),
                            _ => Vec::new(),
                        };
                        // Count generic placeholders in all prior variants so we
                        // know where this variant's generics start in `type_args`.
                        //
                        // LIMITATION: this assumes a 1:1 correspondence between
                        // generic placeholders across the enum layout and the
                        // `type_args` list.  This works for Option<T> and
                        // Result<T, E> (each variant has at most one generic), but
                        // would produce wrong results for user-defined enums where
                        // the same type parameter appears in multiple variants
                        // (e.g., `enum Foo<T, U> { Baz(T), Bar(Int, T, U) }`).
                        // A proper fix requires storing type parameter index
                        // metadata in the enum layout (see A5).
                        let generic_offset: usize = variants
                            .iter()
                            .take(var_idx)
                            .map(|(_, fs)| fs.iter().filter(|t| t.is_generic_placeholder()).count())
                            .sum();
                        // Guard: if the computed offset would exceed type_args,
                        // the heuristic is wrong for this enum (e.g., a
                        // user-defined generic enum where the same type param
                        // appears in multiple variants).  Skip generic
                        // substitution rather than using the wrong concrete type.
                        // A proper fix requires storing type parameter index
                        // metadata in the enum layout (see architecture item A5).
                        let total_generics_in_variant = field_types
                            .iter()
                            .filter(|t| t.is_generic_placeholder())
                            .count();
                        let safe_type_args = if generic_offset + total_generics_in_variant
                            > type_args.len()
                            && !type_args.is_empty()
                        {
                            &[] as &[Type]
                        } else {
                            &type_args
                        };
                        for (j, binding_name) in vp.bindings.iter().enumerate() {
                            let field_type = resolve_field_type(
                                field_types,
                                j,
                                generic_offset,
                                safe_type_args,
                                self.check,
                            );
                            let field_val = self.emit(
                                Op::EnumGetField(subject, var_idx as u32, j as u32),
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
                        unreachable!(
                            "unknown enum variant `{}` in match lowering — \
                             sema should have rejected this",
                            vp.variant
                        );
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

    /// Lower a match arm body, pop scope, and jump to the merge block.
    fn lower_arm_body(&mut self, body: &MatchBody, ctx: &MatchCtx) {
        let arm_val = match body {
            MatchBody::Expr(expr) => self.lower_expr(expr),
            MatchBody::Block(block) => self
                .lower_block_implicit(block)
                .unwrap_or_else(|| self.void_placeholder(ctx.span)),
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

/// Resolve the concrete type for a variant field, substituting generic
/// placeholders with type arguments from the match subject.
///
/// `field_types` is the variant's field type list from the enum layout.
/// `field_idx` is the index of the current field within this variant.
/// `generic_offset` is the number of generic placeholders in all prior variants.
/// `type_args` is the concrete type arguments from the subject type
/// (e.g., `[Int]` for `Option<Int>`).
///
/// ## Worked example: `Result<Int, String>`
///
/// Layout: `[("Ok", [__generic]), ("Err", [__generic])]`
/// Type args: `[Int, String]`
///
/// For the `Ok` variant (index 0):
///   - `generic_offset` = 0 (no prior variants)
///   - field 0 is `__generic`, `prior_generics` = 0
///   - `arg_idx` = 0 + 0 = 0 → resolves to `Int`
///
/// For the `Err` variant (index 1):
///   - `generic_offset` = 1 (Ok has 1 generic placeholder)
///   - field 0 is `__generic`, `prior_generics` = 0
///   - `arg_idx` = 1 + 0 = 1 → resolves to `String`
fn resolve_field_type(
    field_types: &[IrType],
    field_idx: usize,
    generic_offset: usize,
    type_args: &[Type],
    check: &phoenix_sema::checker::CheckResult,
) -> IrType {
    let mut field_type = field_types.get(field_idx).cloned().unwrap_or(IrType::Void);
    if field_type.is_generic_placeholder() {
        let prior_generics = field_types
            .iter()
            .take(field_idx)
            .filter(|t| t.is_generic_placeholder())
            .count();
        let arg_idx = generic_offset + prior_generics;
        if let Some(concrete) = type_args.get(arg_idx) {
            field_type = crate::lower::lower_type(concrete, check);
        }
    }
    field_type
}
