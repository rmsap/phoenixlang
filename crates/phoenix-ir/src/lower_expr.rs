//! Expression lowering.
//!
//! Flattens nested Phoenix expressions into sequences of IR instructions,
//! each producing an SSA [`ValueId`].

use crate::instruction::{FuncId, Op, VOID_SENTINEL, ValueId};
use crate::lower::{LoweringContext, VarBinding, lower_type};
use crate::module::IrFunction;
use crate::terminator::Terminator;
use crate::types::IrType;
use phoenix_common::span::Span;
use phoenix_parser::ast::{
    AssignmentExpr, BinaryExpr, BinaryOp, CallExpr, ElseBranch, Expr, FieldAccessExpr,
    FieldAssignmentExpr, IfExpr, LambdaExpr, ListLiteralExpr, LiteralKind, MapLiteralExpr,
    MethodCallExpr, StringInterpolationExpr, StringSegment, StructLiteralExpr, TryExpr, UnaryExpr,
    UnaryOp,
};
use phoenix_sema::types::Type;
use std::collections::HashMap;

impl<'a> LoweringContext<'a> {
    /// Lower a slice of sema [`Type`]s to their IR counterparts. Used when
    /// propagating enum generic args (e.g. `Option<Int>` → `EnumRef("Option",
    /// [I64])`) through enum-variant construction sites.
    fn lower_type_args(&self, args: &[Type]) -> Vec<IrType> {
        args.iter().map(|t| lower_type(t, self.check)).collect()
    }

    /// If sema resolved the type at `span` to a known enum, return its name
    /// and sema-level type args. Returns `None` for non-enum types, unresolved
    /// spans, or names absent from `enum_layouts`.
    ///
    /// Callers still do variant-name lookup against the layout; this helper
    /// unifies the "is the thing at this span an enum construction, and if so
    /// with what name + args" question that `lower_ident`, `lower_call`, and
    /// `lower_struct_literal` all need to answer.
    fn enum_type_at(&self, span: &Span) -> Option<(String, Vec<Type>)> {
        let source_ty = self.source_type(span)?;
        let (name, args) = match source_ty {
            Type::Generic(n, a) => (n.clone(), a.clone()),
            Type::Named(n) => (n.clone(), Vec::new()),
            _ => return None,
        };
        if self.module.enum_layouts.contains_key(&name) {
            Some((name, args))
        } else {
            None
        }
    }

    /// If sema resolved the type at `span` to a known struct, return its
    /// name and sema-level type args. Mirrors [`Self::enum_type_at`] —
    /// the struct-construction lowering paths use it to thread concrete
    /// type arguments into `IrType::StructRef(name, args)` so
    /// struct-monomorphization can specialize per-instantiation.
    ///
    /// Returns `None` for non-struct types, unresolved spans, or names
    /// absent from `struct_layouts`.
    fn struct_type_at(&self, span: &Span) -> Option<(String, Vec<Type>)> {
        let source_ty = self.source_type(span)?;
        let (name, args) = match source_ty {
            Type::Generic(n, a) => (n.clone(), a.clone()),
            Type::Named(n) => (n.clone(), Vec::new()),
            _ => return None,
        };
        if self.module.struct_layouts.contains_key(&name) {
            Some((name, args))
        } else {
            None
        }
    }

    /// Resolve a struct field's IR type at a concrete use site.
    ///
    /// The layout in `struct_layouts` is keyed by the template's bare
    /// name and its field types may contain `IrType::TypeVar(T)` for
    /// generic structs.  When the receiver's sema type carries concrete
    /// args (e.g. `Container<Int>`), substitute each `TypeVar` with the
    /// corresponding concrete type so callers see a fully-resolved field
    /// type.
    ///
    /// This is the *lowering-time* half of Phoenix's dual TypeVar
    /// substitution (see the module-level doc in `monomorphize.rs`).
    /// Template method bodies — where the receiver's sema type is a bare
    /// `Named("Foo")` with no args — take the no-op path here and are
    /// substituted later by [`crate::monomorphize::substitute_types_in_fn`]
    /// when struct-mono clones and specializes the method body.
    fn resolve_field_type(
        &self,
        struct_name: &str,
        sema_args: &[Type],
        raw_field_ty: &IrType,
    ) -> IrType {
        if sema_args.is_empty() {
            return raw_field_ty.clone();
        }
        let Some(type_params) = self.module.struct_type_params.get(struct_name) else {
            return raw_field_ty.clone();
        };
        if type_params.len() != sema_args.len() {
            return raw_field_ty.clone();
        }
        let subst: std::collections::HashMap<String, IrType> = type_params
            .iter()
            .cloned()
            .zip(sema_args.iter().map(|t| lower_type(t, self.check)))
            .collect();
        crate::monomorphize::substitute(raw_field_ty, &subst)
    }

    /// Lower an expression and return the [`ValueId`] of its result.
    pub(crate) fn lower_expr(&mut self, expr: &Expr) -> ValueId {
        match expr {
            Expr::Literal(lit) => self.lower_literal(lit),
            Expr::Ident(ident) => self.lower_ident(ident),
            Expr::Binary(binary) => self.lower_binary(binary),
            Expr::Unary(unary) => self.lower_unary(unary),
            Expr::Call(call) => self.lower_call(call),
            Expr::Assignment(assign) => self.lower_assignment(assign),
            Expr::FieldAssignment(fa) => self.lower_field_assignment(fa),
            Expr::FieldAccess(fa) => self.lower_field_access(fa),
            Expr::MethodCall(mc) => self.lower_method_call(mc),
            Expr::StructLiteral(sl) => self.lower_struct_literal(sl),
            Expr::Match(m) => self.lower_match(m),
            Expr::If(if_expr) => self.lower_if(if_expr),
            Expr::Lambda(lambda) => self.lower_lambda(lambda),
            Expr::ListLiteral(list) => self.lower_list_literal(list),
            Expr::MapLiteral(map) => self.lower_map_literal(map),
            Expr::StringInterpolation(si) => self.lower_string_interpolation(si),
            Expr::Try(try_expr) => self.lower_try(try_expr),
        }
    }

    /// Lower an `if`/`else if`/`else` expression into IR control flow.
    ///
    /// Mirrors [`Self::lower_match`]: creates a merge block with an optional
    /// parameter for the result value, branches on the condition, lowers each
    /// branch as a value-producing block, and threads the branch value through
    /// to the merge block via `Jump { args: [val] }`.
    ///
    /// `else if` chains recurse: the nested `if` produces its own merge block
    /// and `ValueId`, which the outer `if` then forwards to its own merge.
    /// `if` without `else` has type `Void`; the false branch goes directly to
    /// the merge block.
    pub(crate) fn lower_if(&mut self, if_expr: &IfExpr) -> ValueId {
        let span = Some(if_expr.span);
        let result_type = self.expr_type(&if_expr.span);

        let merge_block = self.create_block();
        let result_param = if result_type != IrType::Void {
            Some(self.add_block_param(merge_block, result_type.clone()))
        } else {
            None
        };

        let cond = self.lower_expr(&if_expr.condition);
        let then_block = self.create_block();
        let else_block = if if_expr.else_branch.is_some() {
            self.create_block()
        } else {
            merge_block
        };

        self.terminate(Terminator::Branch {
            condition: cond,
            true_block: then_block,
            true_args: Vec::new(),
            false_block: else_block,
            false_args: Vec::new(),
        });

        // Then branch: lower, thread value to merge.
        self.switch_to_block(then_block);
        let then_val = self.lower_block_implicit(&if_expr.then_block);
        let then_reaches_merge = self.jump_to_merge(merge_block, result_param, then_val, span);

        // Else branch: lower, thread value to merge.  Tracks whether any
        // branch actually jumped to the merge block so we can terminate an
        // unreachable merge with `Unreachable` instead of letting an upstream
        // `Return` pick up a VOID_SENTINEL operand.
        let else_reaches_merge = match &if_expr.else_branch {
            Some(ElseBranch::Block(block)) => {
                self.switch_to_block(else_block);
                let else_val = self.lower_block_implicit(block);
                self.jump_to_merge(merge_block, result_param, else_val, span)
            }
            Some(ElseBranch::ElseIf(nested)) => {
                self.switch_to_block(else_block);
                let nested_val = self.lower_if(nested);
                self.jump_to_merge(merge_block, result_param, Some(nested_val), span)
            }
            // No `else`: the false-branch target IS the merge block, so the
            // Branch terminator itself reaches merge.
            None => true,
        };

        self.switch_to_block(merge_block);

        // If every branch diverged (e.g., `if c { return a } else { return b }`),
        // the merge block has no predecessors.  Terminate it as unreachable so
        // the function-level terminator logic doesn't try to emit a return
        // using a stale result_param or VOID_SENTINEL.
        if !then_reaches_merge && !else_reaches_merge {
            self.terminate(Terminator::Unreachable);
        }

        // For Void if-expressions, return a sentinel rather than emitting a
        // dead `const_bool` into the merge block.  Callers of a Void if never
        // use the resulting ValueId.
        result_param.unwrap_or(VOID_SENTINEL)
    }

    /// Emits a `Jump` from the current block to `merge_block`, passing the
    /// branch's value as a block argument when the merge block takes a result
    /// parameter.  Returns `true` if the jump was emitted (merge is reachable
    /// from this branch), `false` if the current block was already terminated
    /// (e.g. the branch ended in `return`).
    fn jump_to_merge(
        &mut self,
        merge_block: crate::block::BlockId,
        result_param: Option<ValueId>,
        branch_val: Option<ValueId>,
        span: Option<Span>,
    ) -> bool {
        if !self.block_needs_terminator() {
            return false;
        }
        let args = if result_param.is_some() {
            let val = branch_val.unwrap_or_else(|| self.void_placeholder(span));
            vec![val]
        } else {
            Vec::new()
        };
        self.terminate(Terminator::Jump {
            target: merge_block,
            args,
        });
        true
    }

    /// Lower a literal expression.
    pub(crate) fn lower_literal(&mut self, lit: &phoenix_parser::ast::Literal) -> ValueId {
        let span = Some(lit.span);
        match &lit.kind {
            LiteralKind::Int(v) => self.emit(Op::ConstI64(*v), IrType::I64, span),
            LiteralKind::Float(v) => self.emit(Op::ConstF64(*v), IrType::F64, span),
            LiteralKind::String(s) => {
                self.emit(Op::ConstString(s.clone()), IrType::StringRef, span)
            }
            LiteralKind::Bool(v) => self.emit(Op::ConstBool(*v), IrType::Bool, span),
        }
    }

    /// Lower an identifier expression.
    ///
    /// Identifiers fall into three buckets, checked in order:
    ///
    /// 1. **Zero-field enum variant** (`None`, `Some` via `None` is elsewhere,
    ///    user-defined `enum Color { Red, Green }` → `Red`). Sema tags the
    ///    span with a `Type::Generic(enum_name, args)` or `Type::Named(enum)`;
    ///    combined with an `enum_layouts` hit on a zero-field variant named
    ///    `ident.name`, we emit an `EnumAlloc` with the concrete args carried
    ///    forward on the resulting `EnumRef`. This handles `None` uniformly
    ///    with every other zero-field variant — no separate `None` branch.
    /// 2. **Bound variable** — standard lookup in the current scope chain.
    /// 3. **Otherwise**: sema should have rejected the program; panic.
    fn lower_ident(&mut self, ident: &phoenix_parser::ast::IdentExpr) -> ValueId {
        if let Some((enum_name, type_args)) = self.enum_type_at(&ident.span)
            && let Some(variants) = self.module.enum_layouts.get(&enum_name).cloned()
            && let Some((idx, _)) = variants
                .iter()
                .enumerate()
                .find(|(_, (name, fields))| *name == ident.name && fields.is_empty())
        {
            let ir_args = self.lower_type_args(&type_args);
            return self.emit(
                Op::EnumAlloc(enum_name.clone(), idx as u32, Vec::new()),
                IrType::EnumRef(enum_name, ir_args),
                Some(ident.span),
            );
        }

        if let Some(binding) = self.lookup_var(&ident.name).cloned() {
            match binding {
                VarBinding::Direct(val, _) => val,
                VarBinding::Mutable(slot, ty) => self.emit(Op::Load(slot), ty, Some(ident.span)),
            }
        } else {
            // Sema should have caught this — including `None` without a
            // resolvable `Option<T>` context, which the zero-field-variant
            // branch above handles when sema does its job.
            unreachable!(
                "unknown identifier `{}` at {:?} — sema should have rejected this \
                 (or failed to resolve an Option<T> type for a bare `None`)",
                ident.name, ident.span
            )
        }
    }

    /// Lower a binary expression.
    fn lower_binary(&mut self, binary: &BinaryExpr) -> ValueId {
        let span = Some(binary.span);

        // Short-circuit: `and`/`or` are lowered to control flow.
        match binary.op {
            BinaryOp::And => return self.lower_short_circuit(binary, true),
            BinaryOp::Or => return self.lower_short_circuit(binary, false),
            _ => {}
        }

        let lhs = self.lower_expr(&binary.left);
        let rhs = self.lower_expr(&binary.right);

        // Determine the operand type from the left operand's sema type.
        let left_type = self.require_source_type(&binary.left.span());

        let op = match (&left_type, binary.op) {
            // Int arithmetic
            (Type::Int, BinaryOp::Add) => Op::IAdd(lhs, rhs),
            (Type::Int, BinaryOp::Sub) => Op::ISub(lhs, rhs),
            (Type::Int, BinaryOp::Mul) => Op::IMul(lhs, rhs),
            (Type::Int, BinaryOp::Div) => Op::IDiv(lhs, rhs),
            (Type::Int, BinaryOp::Mod) => Op::IMod(lhs, rhs),

            // Float arithmetic
            (Type::Float, BinaryOp::Add) => Op::FAdd(lhs, rhs),
            (Type::Float, BinaryOp::Sub) => Op::FSub(lhs, rhs),
            (Type::Float, BinaryOp::Mul) => Op::FMul(lhs, rhs),
            (Type::Float, BinaryOp::Div) => Op::FDiv(lhs, rhs),
            (Type::Float, BinaryOp::Mod) => Op::FMod(lhs, rhs),

            // Int comparison
            (Type::Int, BinaryOp::Eq) => Op::IEq(lhs, rhs),
            (Type::Int, BinaryOp::NotEq) => Op::INe(lhs, rhs),
            (Type::Int, BinaryOp::Lt) => Op::ILt(lhs, rhs),
            (Type::Int, BinaryOp::Gt) => Op::IGt(lhs, rhs),
            (Type::Int, BinaryOp::LtEq) => Op::ILe(lhs, rhs),
            (Type::Int, BinaryOp::GtEq) => Op::IGe(lhs, rhs),

            // Float comparison
            (Type::Float, BinaryOp::Eq) => Op::FEq(lhs, rhs),
            (Type::Float, BinaryOp::NotEq) => Op::FNe(lhs, rhs),
            (Type::Float, BinaryOp::Lt) => Op::FLt(lhs, rhs),
            (Type::Float, BinaryOp::Gt) => Op::FGt(lhs, rhs),
            (Type::Float, BinaryOp::LtEq) => Op::FLe(lhs, rhs),
            (Type::Float, BinaryOp::GtEq) => Op::FGe(lhs, rhs),

            // String comparison
            (Type::String, BinaryOp::Eq) => Op::StringEq(lhs, rhs),
            (Type::String, BinaryOp::NotEq) => Op::StringNe(lhs, rhs),
            (Type::String, BinaryOp::Lt) => Op::StringLt(lhs, rhs),
            (Type::String, BinaryOp::Gt) => Op::StringGt(lhs, rhs),
            (Type::String, BinaryOp::LtEq) => Op::StringLe(lhs, rhs),
            (Type::String, BinaryOp::GtEq) => Op::StringGe(lhs, rhs),

            // String concatenation
            (Type::String, BinaryOp::Add) => Op::StringConcat(lhs, rhs),

            // Bool comparison
            (Type::Bool, BinaryOp::Eq) => Op::BoolEq(lhs, rhs),
            (Type::Bool, BinaryOp::NotEq) => Op::BoolNe(lhs, rhs),

            // Fallback: treat as int operation
            _ => match binary.op {
                BinaryOp::Add => Op::IAdd(lhs, rhs),
                BinaryOp::Sub => Op::ISub(lhs, rhs),
                BinaryOp::Mul => Op::IMul(lhs, rhs),
                BinaryOp::Div => Op::IDiv(lhs, rhs),
                BinaryOp::Mod => Op::IMod(lhs, rhs),
                BinaryOp::Eq => Op::IEq(lhs, rhs),
                BinaryOp::NotEq => Op::INe(lhs, rhs),
                BinaryOp::Lt => Op::ILt(lhs, rhs),
                BinaryOp::Gt => Op::IGt(lhs, rhs),
                BinaryOp::LtEq => Op::ILe(lhs, rhs),
                BinaryOp::GtEq => Op::IGe(lhs, rhs),
                BinaryOp::And | BinaryOp::Or => unreachable!("handled above"),
            },
        };

        // Determine result type.
        let result_type = match binary.op {
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
                match &left_type {
                    Type::String => IrType::StringRef,
                    Type::Float => IrType::F64,
                    _ => IrType::I64,
                }
            }
            _ => IrType::Bool, // all comparisons produce Bool
        };

        self.emit(op, result_type, span)
    }

    /// Lower short-circuit `and`/`or` to conditional branches.
    fn lower_short_circuit(&mut self, binary: &BinaryExpr, is_and: bool) -> ValueId {
        let span = Some(binary.span);
        let lhs = self.lower_expr(&binary.left);

        let eval_rhs_block = self.create_block();
        let merge_block = self.create_block();
        let short_circuit_val = if is_and {
            // `and`: if lhs is false, short-circuit with false
            self.emit(Op::ConstBool(false), IrType::Bool, span)
        } else {
            // `or`: if lhs is true, short-circuit with true
            self.emit(Op::ConstBool(true), IrType::Bool, span)
        };

        if is_and {
            // and: lhs true -> eval rhs; lhs false -> merge(false)
            self.terminate(Terminator::Branch {
                condition: lhs,
                true_block: eval_rhs_block,
                true_args: Vec::new(),
                false_block: merge_block,
                false_args: vec![short_circuit_val],
            });
        } else {
            // or: lhs true -> merge(true); lhs false -> eval rhs
            self.terminate(Terminator::Branch {
                condition: lhs,
                true_block: merge_block,
                true_args: vec![short_circuit_val],
                false_block: eval_rhs_block,
                false_args: Vec::new(),
            });
        }

        // Evaluate RHS.
        self.switch_to_block(eval_rhs_block);
        let rhs = self.lower_expr(&binary.right);
        self.terminate(Terminator::Jump {
            target: merge_block,
            args: vec![rhs],
        });

        // Merge.
        let result = self.add_block_param(merge_block, IrType::Bool);
        self.switch_to_block(merge_block);
        result
    }

    /// Lower a unary expression.
    fn lower_unary(&mut self, unary: &UnaryExpr) -> ValueId {
        let operand = self.lower_expr(&unary.operand);
        let span = Some(unary.span);

        match unary.op {
            UnaryOp::Neg => {
                let operand_type = self.require_source_type(&unary.operand.span());
                match operand_type {
                    Type::Float => self.emit(Op::FNeg(operand), IrType::F64, span),
                    _ => self.emit(Op::INeg(operand), IrType::I64, span),
                }
            }
            UnaryOp::Not => self.emit(Op::BoolNot(operand), IrType::Bool, span),
        }
    }

    /// Lower a function call expression.
    fn lower_call(&mut self, call: &CallExpr) -> ValueId {
        let span = Some(call.span);

        // Lower positional arguments.
        let positional: Vec<ValueId> = call.args.iter().map(|a| self.lower_expr(a)).collect();
        // Lower named arguments (values only — placement is deferred until we
        // know the callee's parameter names).
        let named: Vec<(String, ValueId)> = call
            .named_args
            .iter()
            .map(|(name, expr)| (name.clone(), self.lower_expr(expr)))
            .collect();

        // Determine the callee.
        if let Expr::Ident(ident) = &call.callee {
            // Check for built-in functions (no named-arg support).
            match ident.name.as_str() {
                "print" => {
                    return self.emit(
                        Op::BuiltinCall("print".to_string(), positional),
                        IrType::Void,
                        span,
                    );
                }
                "toString" => {
                    return self.emit(
                        Op::BuiltinCall("toString".to_string(), positional),
                        IrType::StringRef,
                        span,
                    );
                }
                _ => {}
            }

            // Qualify the bare AST name against `current_module` to match
            // sema's mangled `function_index` keys.
            let qname = self.qualify(&ident.name);
            if let Some(&func_id) = self.module.function_index.get(qname.as_ref()) {
                let result_type = self.expr_type(&call.span);
                let args = self.merge_call_args(func_id, &positional, &named, call.span);
                let args = self.coerce_call_args(func_id, args, call.span);
                let type_args = self.resolve_call_type_args(call.span);
                return self.emit(Op::Call(func_id, type_args, args), result_type, span);
            }

            // Check for an enum variant constructor (e.g. `Some(42)`).
            if let Some((enum_name, type_args)) = self.enum_type_at(&call.span)
                && let Some(variants) = self.module.enum_layouts.get(&enum_name).cloned()
                && let Some((idx, (_, field_types))) = variants
                    .iter()
                    .enumerate()
                    .find(|(_, (name, _))| *name == ident.name)
            {
                let ir_args = self.lower_type_args(&type_args);
                let args = self.coerce_args_to_expected(positional, field_types, call.span);
                let result_type = IrType::EnumRef(enum_name.clone(), ir_args);
                return self.emit(
                    Op::EnumAlloc(enum_name, idx as u32, args),
                    result_type,
                    span,
                );
            }

            // Check for a struct constructor (e.g. `Point(1, 2)` parsed as a call).
            if let Some(args) = self.coerce_struct_ctor_args(&ident.name, &positional, call.span) {
                let type_args: Vec<IrType> = self
                    .struct_type_at(&call.span)
                    .map(|(_, sema_args)| self.lower_type_args(&sema_args))
                    .unwrap_or_default();
                // Module-qualify so `IrType::StructRef` / `Op::StructAlloc`
                // match `struct_layouts`'s keying.
                let qname = self.qualify(&ident.name).into_owned();
                let result_type = IrType::StructRef(qname.clone(), type_args);
                return self.emit(Op::StructAlloc(qname, args), result_type, span);
            }
        }

        // Indirect call through a closure value.
        let callee_val = self.lower_expr(&call.callee);
        let result_type = self.expr_type(&call.span);
        self.emit(Op::CallIndirect(callee_val, positional), result_type, span)
    }

    /// Shared slot-filling core for both free-function and method calls.
    ///
    /// Builds a fully-ordered argument list of length `total`, filling
    /// from:
    /// 1. `positional` in source order (first wins),
    /// 2. each `named` by resolving its key via `named_to_slot`
    ///    (`None` = unknown named arg; debug-asserts since sema should
    ///    have rejected it), and
    /// 3. `defaults.get(&slot)` for any remaining empty slot, lowered
    ///    into the current block via `lower_expr`.
    ///
    /// Caller-site materialization contract (see [design-decisions.md:
    /// *Default-argument lowering strategy*]): each missing slot's
    /// default expression is lowered fresh at this call site, so
    /// side-effectful defaults (e.g. `= randomInt()`) re-evaluate per
    /// call.
    ///
    /// `site_desc` identifies the callee for panic messages (e.g.
    /// `"free function `foo`"` or `"method `Counter.bump`"`).
    ///
    /// **Default materialization.** For every missing slot, this
    /// helper first probes
    /// [`crate::module::IrModule::default_wrapper_index`] for a
    /// `(callee_id, slot_idx)` entry; if present, emits a zero-arg
    /// call to the synthesized wrapper, otherwise inlines the AST
    /// default expression at the caller. `callee_id = None` (built-in
    /// methods like `Option.unwrap`) skips the probe — those carry no
    /// `FuncId` and have no defaults today, so the missing-slot loop
    /// is a no-op for them. See [`crate::default_wrappers`] for the
    /// full design and the "Default-expression visibility across module
    /// boundaries" bug-closure entry in `docs/phases/phase-2.md` (§2.6)
    /// for the privacy rationale.
    #[allow(clippy::too_many_arguments)] // parameters are all distinct inputs; grouping would obscure, not clarify
    fn assemble_call_args(
        &mut self,
        callee_id: Option<FuncId>,
        total: usize,
        positional: &[ValueId],
        named: &[(String, ValueId)],
        named_to_slot: impl Fn(&str) -> Option<usize>,
        defaults: &HashMap<usize, Expr>,
        site_desc: &str,
        call_span: Span,
    ) -> Vec<ValueId> {
        // Over-long positional is a sema bug — silent truncation would
        // lose caller-side side effects (already lowered into the
        // current block) and leave a ValueId dangling.  Hard assert.
        assert!(
            positional.len() <= total,
            "assemble_call_args: {} positional args for {site_desc} which has {total} \
             params (call at {call_span:?}) — sema should reject over-long arg lists",
            positional.len(),
        );

        // Fast path: fully positional with correct arity and no named.
        if named.is_empty() && positional.len() == total {
            return positional.to_vec();
        }

        let mut slots: Vec<Option<ValueId>> = vec![None; total];
        for (i, val) in positional.iter().enumerate() {
            slots[i] = Some(*val);
        }
        for (name, val) in named {
            match named_to_slot(name) {
                Some(idx) => slots[idx] = Some(*val),
                None => debug_assert!(
                    false,
                    "assemble_call_args: sema let through named arg `{name}` for \
                     {site_desc} at {call_span:?} — should have been rejected with \
                     an 'unknown named arg' error",
                ),
            }
        }

        // Pre-collect missing slots. For each, check whether sema +
        // default-wrapper synthesis registered a wrapper for
        // `(callee_id, idx)` — if so, emit a zero-arg call to the
        // wrapper. Otherwise fall back to the inline path: lower the
        // AST default expression at the caller. The inline path is
        // the live (not legacy) materialization for pure-literal
        // defaults — privacy-safe by construction, cheaper than an
        // extra function-call hop. `defaults` was cloned out of
        // whatever owning borrow the caller held, so the loop
        // doesn't hold anything conflicting with `&mut self` below.
        let mut missing: Vec<(usize, MissingFill)> = Vec::new();
        for (i, slot) in slots.iter().enumerate() {
            if slot.is_some() {
                continue;
            }
            // Wrapper-index probe is only meaningful when we have a
            // real callee FuncId; built-in methods carry `None` and
            // skip straight to the inline path.
            let wrapper_id =
                callee_id.and_then(|cid| self.module.default_wrapper_index.get(&(cid, i)).copied());
            if let Some(wrapper_id) = wrapper_id {
                missing.push((i, MissingFill::Wrapper(wrapper_id)));
            } else if let Some(expr) = defaults.get(&i) {
                missing.push((i, MissingFill::Inline(expr.clone())));
            }
        }
        for (idx, fill) in missing {
            let val = match fill {
                MissingFill::Wrapper(wrapper_id) => {
                    // Zero-arg call to the synthesized wrapper. Result
                    // type comes from the wrapper's IR signature so the
                    // SSA value is correctly typed at the call site.
                    //
                    // Invariant: this `return_type` equals the
                    // callee's `param_types[slot_idx]` — by
                    // construction, `register_wrapper_stub` derives
                    // the wrapper's return type from the same sema
                    // type that produced the slot. Once
                    // monomorphization extends to default wrappers,
                    // that invariant must be re-established at
                    // specialization time; if it ever drifts, it will
                    // surface here as the SSA value carrying a
                    // different type than the callee expects.
                    let result_type = self.module.functions[wrapper_id.index()]
                        .func()
                        .return_type
                        .clone();
                    self.emit(
                        Op::Call(wrapper_id, Vec::new(), Vec::new()),
                        result_type,
                        Some(call_span),
                    )
                }
                MissingFill::Inline(expr) => self.lower_expr(&expr),
            };
            slots[idx] = Some(val);
        }

        let result: Vec<ValueId> = slots.into_iter().flatten().collect();
        // Hard assert — release-build underfill trips the Cranelift
        // verifier at best, silent miscompile at worst.
        assert_eq!(
            result.len(),
            total,
            "assemble_call_args: {} of {total} slots filled for {site_desc} at \
             {call_span:?} — sema should ensure every slot is either passed or has \
             a default",
            result.len(),
        );
        result
    }

    /// Free-function call wrapper around [`Self::assemble_call_args`].
    ///
    /// Reads param names from the IR module (already cloned at
    /// definition time) and defaults from
    /// [`FunctionInfo::default_param_exprs`].
    ///
    /// **Scope: free-function calls only.**  The direct-call branch of
    /// [`Self::lower_call`] is the sole caller.  The method-call path
    /// routes through [`Self::merge_method_call_args`] instead — same
    /// caller-site materialization rule, different lookup source
    /// (sema's `methods` registry) and slot space (excludes `self`).
    /// Indirect calls through closures ([`Op::CallIndirect`]) are
    /// unreachable because closure params cannot carry defaults; if
    /// that ever changes, the indirect path would need its own
    /// synthesis site (the callee's defaults are not known at the
    /// closure's call site — they'd have to be baked into the closure
    /// body at construction time).
    fn merge_call_args(
        &mut self,
        func_id: FuncId,
        positional: &[ValueId],
        named: &[(String, ValueId)],
        call_span: Span,
    ) -> Vec<ValueId> {
        // Clone owning copies to drop the borrow on `self.module`
        // before we call `lower_expr(&mut self)`.
        let func = self.module.functions[func_id.index()].func();
        let total = func.param_names.len();
        let param_names = func.param_names.clone();
        let func_name = func.name.clone();
        let defaults = self
            .check
            .function_info_by_name(&func_name)
            .map(|info| info.default_param_exprs.clone())
            .unwrap_or_default();

        let site_desc = format!("free function `{func_name}`");
        self.assemble_call_args(
            Some(func_id),
            total,
            positional,
            named,
            |name| param_names.iter().position(|p| p == name),
            &defaults,
            &site_desc,
            call_span,
        )
    }

    /// Method-call counterpart of [`Self::merge_call_args`] — operates
    /// on the *user-visible* slot space (`self` excluded) and reads
    /// defaults from sema's `methods` registry rather than `functions`.
    ///
    /// Returns a vector of length `method_info.params.len()` — still
    /// without `self`. Callers in [`Self::lower_method_call`] insert
    /// the receiver at slot 0 after this helper returns.
    ///
    /// Positional-only today: `MethodCallExpr` carries no named-arg
    /// syntax.  The shared [`Self::assemble_call_args`] core accepts a
    /// `named` slice, so adding named-args on methods is a
    /// parser-level change only.
    fn merge_method_call_args(
        &mut self,
        type_name: &str,
        method_name: &str,
        positional: &[ValueId],
        call_span: Span,
    ) -> Vec<ValueId> {
        // Single MethodInfo lookup; clone out what we need so the
        // immutable borrow on `self.check` releases before
        // `assemble_call_args(&mut self)` runs.
        //
        // Built-in methods (`Option.unwrap`, etc.) carry `func_id =
        // None` because the Cranelift backend inlines them — passing
        // `None` skips the wrapper-index probe entirely. Built-ins
        // ship with no defaults today (`default_param_exprs` is always
        // empty), so the missing-slot loop in `assemble_call_args`
        // never iterates and the inline-vs-wrapper decision is moot.
        let (callee_id, total, param_names, defaults) = self
            .check
            .method_info_by_name(type_name, method_name)
            .map(|info| {
                (
                    info.func_id,
                    info.params.len(),
                    info.param_names.clone(),
                    info.default_param_exprs.clone(),
                )
            })
            .unwrap_or_else(|| {
                unreachable!(
                    "merge_method_call_args: no MethodInfo for `{type_name}.{method_name}` \
                     — sema's `check_method_call` must reject unknown methods before \
                     lowering runs"
                )
            });

        let site_desc = format!("method `{type_name}.{method_name}`");
        self.assemble_call_args(
            callee_id,
            total,
            positional,
            &[],
            |name| param_names.iter().position(|p| p == name),
            &defaults,
            &site_desc,
            call_span,
        )
    }

    /// Lower a variable assignment.
    fn lower_assignment(&mut self, assign: &AssignmentExpr) -> ValueId {
        let val = self.lower_expr(&assign.value);

        if let Some(binding) = self.lookup_var(&assign.name).cloned()
            && let VarBinding::Mutable(slot, ty) = binding
        {
            // Coerce against the binding's declared type so a `let mut x:
            // dyn Trait` reassigned to a concrete value wraps in DynAlloc.
            let val = self.coerce_expr_to_expected(val, assign.value.span(), &ty, assign.span);
            self.emit_void(Op::Store(slot, val), Some(assign.span));
        }

        val
    }

    /// Lower a field assignment.
    ///
    /// Coerces `val` against the field's declared IR type so a concrete
    /// value flowing into a `dyn Trait` field is wrapped in `Op::DynAlloc`
    /// — mirrors the constructor path in
    /// [`Self::coerce_struct_ctor_args`]. Without this step, a mutable
    /// `dyn Trait` field assigned from a concrete value would land in a
    /// 2-slot field as a 1-slot value, which miscompiles at codegen.
    fn lower_field_assignment(&mut self, fa: &FieldAssignmentExpr) -> ValueId {
        let obj = self.lower_expr(&fa.object);
        let val = self.lower_expr(&fa.value);

        // Look up the field index from the struct layout.
        let obj_type = self
            .source_type(&fa.object.span())
            .cloned()
            .unwrap_or_else(|| unreachable!("missing sema type for field assignment object"));
        let (struct_name, sema_args) = match &obj_type {
            Type::Named(n) => (Some(n.clone()), Vec::new()),
            Type::Generic(n, args) => (Some(n.clone()), args.clone()),
            _ => (None, Vec::new()),
        };
        if let Some(struct_name) = struct_name
            && let Some(layout) = self.module.struct_layouts.get(&struct_name).cloned()
            && let Some(idx) = layout.iter().position(|(name, _)| *name == fa.field)
        {
            let field_type = self.resolve_field_type(&struct_name, &sema_args, &layout[idx].1);
            let val = self.coerce_expr_to_expected(val, fa.value.span(), &field_type, fa.span);
            self.emit_void(Op::StructSetField(obj, idx as u32, val), Some(fa.span));
        }

        val
    }

    /// Lower a field access.
    fn lower_field_access(&mut self, fa: &FieldAccessExpr) -> ValueId {
        let obj = self.lower_expr(&fa.object);
        let span = Some(fa.span);

        let obj_type = self
            .source_type(&fa.object.span())
            .cloned()
            .unwrap_or_else(|| unreachable!("missing sema type for field access object"));

        // Extract the struct name and any concrete type args from either
        // `Named("Foo")` or `Generic("Foo", [...])`. The latter arises on
        // field accesses against generic-struct values (`c.value` where
        // `c: Container<Int>`). Layouts are still keyed by bare name
        // pre-mono — struct-monomorphization rewrites post-mono
        // StructRefs to mangled names — but the *sema* type carries the
        // original Generic form, so lowering must accept both.
        // `resolve_field_type` handles the per-use-site TypeVar
        // substitution.
        let (struct_name, sema_args) = match &obj_type {
            Type::Named(n) => (Some(n.clone()), Vec::new()),
            Type::Generic(n, args) => (Some(n.clone()), args.clone()),
            _ => (None, Vec::new()),
        };
        if let Some(struct_name) = struct_name
            && let Some(layout) = self.module.struct_layouts.get(&struct_name).cloned()
            && let Some(idx) = layout.iter().position(|(name, _)| *name == fa.field)
        {
            let field_type = self.resolve_field_type(&struct_name, &sema_args, &layout[idx].1);
            return self.emit(Op::StructGetField(obj, idx as u32), field_type, span);
        }

        // Struct layout lookup failed — sema should have caught this.
        unreachable!(
            "field access on unknown struct layout: field `{}` on type {:?}",
            fa.field, obj_type
        )
    }

    /// Lower a method call.
    ///
    /// Four dispatch branches:
    /// - **`dyn Trait`** receiver: delegated to [`Self::lower_dyn_method_call`]
    ///   (vtable slot indirection).
    /// - **Trait-bounded type-variable** receiver (`<T: Trait>`): emits
    ///   an `Op::UnresolvedTraitMethod` placeholder; monomorphization
    ///   rewrites it to a concrete `Op::Call` after substitution.
    /// - **User-defined method** on a concrete type: delegated to
    ///   [`Self::lower_user_method_call`], which is the *only* branch
    ///   today that synthesizes default-argument values (via
    ///   [`Self::merge_method_call_args`]).
    /// - **Built-in method**: emits `Op::BuiltinCall`.
    ///
    /// When trait-method defaults land (see
    /// `docs/known-issues.md`: "Default arguments on trait-method
    /// calls"), the dyn and trait-bounded branches will also thread
    /// defaults — at monomorphization time for the trait-bounded
    /// branch, since the concrete impl is not known here.
    fn lower_method_call(&mut self, mc: &MethodCallExpr) -> ValueId {
        let obj = self.lower_expr(&mc.object);
        let mut args: Vec<ValueId> = mc.args.iter().map(|a| self.lower_expr(a)).collect();
        let span = Some(mc.span);

        let obj_type = self
            .source_type(&mc.object.span())
            .cloned()
            .unwrap_or_else(|| unreachable!("missing sema type for method call object"));

        // Trait-object method call: receiver is `dyn Trait`. Dispatched
        // via a pre-resolved slot index — see `lower_dyn.rs`.
        if let Type::Dyn(trait_name) = &obj_type {
            return self.lower_dyn_method_call(trait_name, obj, &mc.method, args, mc.span);
        }

        // Trait-bounded method call on a type variable receiver
        // (`x.method()` where `x: T` with `<T: Trait>`). The concrete
        // impl is not known yet — emit the typed placeholder and let
        // monomorphization rewrite it to `Op::Call` after substitution.
        if matches!(&obj_type, Type::TypeVar(_)) {
            args.insert(0, obj);
            let result_type = self.expr_type(&mc.span);
            let type_args = self.resolve_call_type_args(mc.span);
            return self.emit(
                Op::UnresolvedTraitMethod(mc.method.clone(), type_args, args),
                result_type,
                span,
            );
        }

        // Determine the type name for method lookup.
        let type_name = match &obj_type {
            Type::Named(name) => name.clone(),
            Type::String => "String".to_string(),
            Type::Int => "Int".to_string(),
            Type::Float => "Float".to_string(),
            Type::Bool => "Bool".to_string(),
            Type::Generic(name, _) => name.clone(),
            _ => String::new(),
        };

        // User-defined method on a concrete type. Qualify the receiver
        // against `current_module` to match sema's `method_index` keys
        // (`qualify_resolved` is a no-op when sema already emitted a
        // qualified `Type::Named` — see its doc).
        let key = (
            self.qualify_resolved(&type_name).into_owned(),
            mc.method.clone(),
        );
        if let Some(&func_id) = self.module.method_index.get(&key) {
            return self.lower_user_method_call(func_id, &type_name, mc, obj, args, span);
        }

        // Built-in method: emit as BuiltinCall.
        args.insert(0, obj);
        let builtin_name = format!("{type_name}.{}", mc.method);
        let result_type = self.expr_type(&mc.span);
        self.emit(Op::BuiltinCall(builtin_name, args), result_type, span)
    }

    /// User-method branch of [`Self::lower_method_call`].
    ///
    /// Split out so it's visibly the only dispatch path that threads
    /// defaults today.  Synthesizes any omitted trailing positional
    /// slots via [`Self::merge_method_call_args`] (caller-site
    /// materialization — see `merge_call_args`'s doc), then prepends
    /// the receiver `obj` at slot 0 before emitting `Op::Call`.
    fn lower_user_method_call(
        &mut self,
        func_id: FuncId,
        type_name: &str,
        mc: &MethodCallExpr,
        obj: ValueId,
        user_args: Vec<ValueId>,
        span: Option<Span>,
    ) -> ValueId {
        let mut merged = self.merge_method_call_args(type_name, &mc.method, &user_args, mc.span);
        merged.insert(0, obj);
        let result_type = self.expr_type(&mc.span);
        let type_args = self.resolve_call_type_args(mc.span);
        self.emit(Op::Call(func_id, type_args, merged), result_type, span)
    }

    /// Lower a struct literal expression.
    fn lower_struct_literal(&mut self, sl: &StructLiteralExpr) -> ValueId {
        let args: Vec<ValueId> = sl.args.iter().map(|a| self.lower_expr(a)).collect();
        let span = Some(sl.span);

        // Check if this is an enum variant constructor.
        if let Some((enum_name, type_args)) = self.enum_type_at(&sl.span)
            && let Some(variants) = self.module.enum_layouts.get(&enum_name).cloned()
            && let Some((idx, (_, field_types))) = variants
                .iter()
                .enumerate()
                .find(|(_, (name, _))| *name == sl.name)
        {
            let ir_args = self.lower_type_args(&type_args);
            let args = self.coerce_args_to_expected(args, field_types, sl.span);
            return self.emit(
                Op::EnumAlloc(enum_name.clone(), idx as u32, args),
                IrType::EnumRef(enum_name, ir_args),
                span,
            );
        }

        // Regular struct constructor. See `coerce_struct_ctor_args` for
        // the dyn-coercion rationale; it mirrors the paren-call path.
        let args = self
            .coerce_struct_ctor_args(&sl.name, &args, sl.span)
            .unwrap_or(args);
        let type_args: Vec<IrType> = self
            .struct_type_at(&sl.span)
            .map(|(_, sema_args)| self.lower_type_args(&sema_args))
            .unwrap_or_default();
        // Carry the module-qualified key so `IrType::StructRef` and
        // `Op::StructAlloc` agree with `struct_layouts`'s keying
        // (qualified post Phase 2.6). Single-file callers qualify to
        // bare names, preserving existing IR snapshots.
        let qname = self.qualify(&sl.name).into_owned();
        let result_type = IrType::StructRef(qname.clone(), type_args);
        self.emit(Op::StructAlloc(qname, args), result_type, span)
    }

    /// Coerce positional arguments of a struct constructor against the
    /// struct's declared field types, wrapping any concrete value that
    /// flows into a `dyn Trait` field in `Op::DynAlloc`. Returns `None`
    /// when `name` is not a registered struct (caller falls through to
    /// the next constructor shape or reports an error).
    fn coerce_struct_ctor_args(
        &mut self,
        name: &str,
        args: &[ValueId],
        span: Span,
    ) -> Option<Vec<ValueId>> {
        let field_layout = self.module.struct_layouts.get(name)?.clone();
        let expected: Vec<IrType> = field_layout.into_iter().map(|(_, ty)| ty).collect();
        Some(self.coerce_args_to_expected(args.to_vec(), &expected, span))
    }

    // NOTE: `lower_match` is in lower_match.rs

    /// Lower a lambda expression.
    fn lower_lambda(&mut self, lambda: &LambdaExpr) -> ValueId {
        let span = Some(lambda.span);
        let result_type = self.expr_type(&lambda.span);

        // Get capture info from sema.
        let captures = self
            .check
            .lambda_captures
            .get(&lambda.span)
            .cloned()
            .unwrap_or_default();

        // Capture the current values and types of captured variables.
        let mut capture_vals: Vec<ValueId> = Vec::new();
        let mut capture_types: Vec<IrType> = Vec::new();
        for cap in &captures {
            if let Some(binding) = self.lookup_var(&cap.name).cloned() {
                let (val, ty) = match binding {
                    VarBinding::Direct(val, ty) => (val, ty),
                    VarBinding::Mutable(slot, ty) => {
                        let loaded = self.emit(Op::Load(slot), ty.clone(), span);
                        (loaded, ty)
                    }
                };
                capture_vals.push(val);
                capture_types.push(ty);
            }
        }

        // Create a separate function for the lambda body.
        let closure_name = format!("__closure_{}", self.closure_counter);
        self.closure_counter += 1;

        // Env-pointer calling convention: the closure function's first
        // parameter is the closure env pointer (typed as the closure's
        // own ClosureRef so the heap object's slot 0 fn-ptr load and
        // slot 1+ capture loads have a coherent type), followed by the
        // user-visible params. Captures are NOT in the parameter list —
        // they live in the env and are read via Op::ClosureLoadCapture.

        // Single source of truth for the lambda's user-facing signature
        // `(param_types, return_type)`. In the well-typed case sema
        // gives us an `IrType::ClosureRef` and we read both fields off
        // it directly. The fallback (sema gave a non-ClosureRef result
        // type, which should not happen in well-formed input) recovers
        // each parameter type from its own span and leaves the return
        // type as `Void` — a conservative default rather than the
        // ill-defined "type of the lambda expression itself", which
        // would double-wrap into a nested `ClosureRef`.
        let (user_param_types, user_return_type): (Vec<IrType>, IrType) = match &result_type {
            IrType::ClosureRef {
                param_types: pt,
                return_type: rt,
            } => (pt.clone(), (**rt).clone()),
            _ => {
                let pts: Vec<IrType> = lambda
                    .params
                    .iter()
                    .map(|p| self.expr_type(&p.span))
                    .collect();
                (pts, IrType::Void)
            }
        };
        let closure_self_type = IrType::ClosureRef {
            param_types: user_param_types.clone(),
            return_type: Box::new(user_return_type.clone()),
        };

        // Build full parameter list: env first, then user params. Each
        // user param's type comes from `user_param_types`; the indexed
        // access is in-bounds by construction (parallel with
        // `lambda.params`).
        let mut param_types: Vec<IrType> = vec![closure_self_type];
        let mut param_names: Vec<String> = vec!["__env".to_string()];
        for (i, p) in lambda.params.iter().enumerate() {
            param_types.push(user_param_types[i].clone());
            param_names.push(p.name.clone());
        }

        // `id` is overwritten by `push_concrete` to match the slot
        // index; passing `FuncId(u32::MAX)` here makes that explicit.
        // Closures are always concrete — they have no type parameters
        // of their own. (Their bodies may still carry `IrType::TypeVar`
        // from an enclosing generic; monomorphization substitutes those
        // via the same `for_each_type_mut` walk that visits regular
        // functions.) `new_closure` is used over `new` to record the
        // capture-type vector on the function in one step — backends
        // and the verifier index it when handling
        // `Op::ClosureLoadCapture`.
        let closure_func = IrFunction::new_closure(
            crate::instruction::FuncId(u32::MAX),
            closure_name,
            param_types,
            param_names.clone(),
            user_return_type,
            Some(lambda.span),
            capture_types.clone(),
        );
        let func_id = self.module.push_concrete(closure_func);

        // Save current function state and lower the lambda body.
        let saved_func_id = self.current_func_id;
        let saved_block = self.current_block;
        let saved_scopes = std::mem::take(&mut self.var_scopes);
        let saved_loops = std::mem::take(&mut self.loop_stack);

        self.current_func_id = Some(func_id);
        self.push_scope();

        let entry = self.create_block();
        self.switch_to_block(entry);

        // Bind the env parameter and the user parameters as block params.
        let entry_param_types: Vec<IrType> = self.module.functions[func_id.index()]
            .func()
            .param_types
            .clone();
        let env_vid = self.add_block_param(entry, entry_param_types[0].clone());
        // Skip the env at index 0; the rest are user params (parallel to
        // `lambda.params`).
        for (i, name) in param_names.iter().enumerate().skip(1) {
            let param_type = entry_param_types[i].clone();
            let val = self.add_block_param(entry, param_type.clone());
            self.define_var(name.clone(), VarBinding::Direct(val, param_type));
        }
        // Bind each capture by emitting an Op::ClosureLoadCapture from
        // the env pointer at the capture's ordinal index. Captures are
        // loaded eagerly at function entry whether the body uses them
        // or not — this keeps lowering simple (no use-site discovery
        // pass). Cost in compiled mode is nil: Cranelift's DCE drops
        // any load whose result is never consumed. Cost in the IR
        // interpreter is one extra `Op::ClosureLoadCapture` dispatch
        // per unused capture (a vector index plus a clone) — small
        // enough that we accept it rather than complicate lowering
        // with a use-site walk.
        for (cap_idx, cap) in captures.iter().enumerate() {
            let cap_ty = capture_types[cap_idx].clone();
            let cap_vid = self.emit(
                Op::ClosureLoadCapture(env_vid, cap_idx as u32),
                cap_ty.clone(),
                span,
            );
            self.define_var(cap.name.clone(), VarBinding::Direct(cap_vid, cap_ty));
        }

        // Lower the body.
        let body_result = self.lower_block_implicit(&lambda.body);
        if self.block_needs_terminator() {
            if let Some(val) = body_result {
                // Implicit-return coercion — same contract as the
                // top-level function-body path in
                // `lower_stmt.rs::lower_function_body`: a concrete
                // trailing expression flowing out of a `-> dyn Trait`
                // lambda must be wrapped in a `(data_ptr, vtable_ptr)`
                // pair at the function boundary.
                let expected = self.module.functions[func_id.index()]
                    .func()
                    .return_type
                    .clone();
                let val = self.coerce_value_to_expected(val, &expected, lambda.body.span);
                self.terminate(Terminator::Return(Some(val)));
            } else {
                self.terminate(Terminator::Return(None));
            }
        }

        self.pop_scope();

        // Restore parent function state.
        self.current_func_id = saved_func_id;
        self.current_block = saved_block;
        self.var_scopes = saved_scopes;
        self.loop_stack = saved_loops;

        // Emit the closure allocation in the parent function.
        self.emit(Op::ClosureAlloc(func_id, capture_vals), result_type, span)
    }

    /// Lower a list literal.
    fn lower_list_literal(&mut self, list: &ListLiteralExpr) -> ValueId {
        let elems: Vec<ValueId> = list.elements.iter().map(|e| self.lower_expr(e)).collect();
        let result_type = self.expr_type(&list.span);
        self.emit(Op::ListAlloc(elems), result_type, Some(list.span))
    }

    /// Lower a map literal.
    fn lower_map_literal(&mut self, map: &MapLiteralExpr) -> ValueId {
        let pairs: Vec<(ValueId, ValueId)> = map
            .entries
            .iter()
            .map(|(k, v)| (self.lower_expr(k), self.lower_expr(v)))
            .collect();
        let result_type = self.expr_type(&map.span);
        self.emit(Op::MapAlloc(pairs), result_type, Some(map.span))
    }

    /// Lower string interpolation.
    fn lower_string_interpolation(&mut self, si: &StringInterpolationExpr) -> ValueId {
        let span = Some(si.span);
        let mut result: Option<ValueId> = None;

        for segment in &si.segments {
            let seg_val = match segment {
                StringSegment::Literal(s) => {
                    self.emit(Op::ConstString(s.clone()), IrType::StringRef, span)
                }
                StringSegment::Expr(expr) => {
                    let val = self.lower_expr(expr);
                    let expr_type = self
                        .source_type(&expr.span())
                        .cloned()
                        .unwrap_or(Type::String);
                    if expr_type == Type::String {
                        val
                    } else {
                        // Convert to string via toString.
                        self.emit(
                            Op::BuiltinCall("toString".to_string(), vec![val]),
                            IrType::StringRef,
                            span,
                        )
                    }
                }
            };

            result = Some(match result {
                Some(prev) => self.emit(Op::StringConcat(prev, seg_val), IrType::StringRef, span),
                None => seg_val,
            });
        }

        result.unwrap_or_else(|| self.emit(Op::ConstString(String::new()), IrType::StringRef, span))
    }

    /// Lower the try operator (`?`).
    fn lower_try(&mut self, try_expr: &TryExpr) -> ValueId {
        let operand = self.lower_expr(&try_expr.operand);
        let span = Some(try_expr.span);

        // Get the discriminant to check Ok/Some vs Err/None.
        let disc = self.emit(Op::EnumDiscriminant(operand), IrType::I64, span);

        // Determine if this is Result or Option from the sema type.
        let operand_type = self.require_source_type(&try_expr.operand.span());

        let (ok_index, unwrap_type) = match &operand_type {
            Type::Generic(name, args) if name == crate::types::RESULT_ENUM => {
                let inner = args
                    .first()
                    .map(|t| self.lower_type(t))
                    .unwrap_or(IrType::Void);
                (0i64, inner) // Ok is variant 0
            }
            Type::Generic(name, args) if name == crate::types::OPTION_ENUM => {
                let inner = args
                    .first()
                    .map(|t| self.lower_type(t))
                    .unwrap_or(IrType::Void);
                (0i64, inner) // Some is variant 0
            }
            _ => unreachable!(
                "? operator applied to non-Result/Option type {:?} — sema should reject this",
                operand_type
            ),
        };

        let ok_const = self.emit(Op::ConstI64(ok_index), IrType::I64, span);
        let is_ok = self.emit(Op::IEq(disc, ok_const), IrType::Bool, span);

        let unwrap_block = self.create_block();
        let early_return_block = self.create_block();

        self.terminate(Terminator::Branch {
            condition: is_ok,
            true_block: unwrap_block,
            true_args: Vec::new(),
            false_block: early_return_block,
            false_args: Vec::new(),
        });

        // Unwrap path: extract the inner value.
        self.switch_to_block(unwrap_block);
        let unwrapped = self.emit(
            Op::EnumGetField(operand, ok_index as u32, 0),
            unwrap_type.clone(),
            span,
        );

        // Early return path: return the error/none value.
        let continue_block = self.create_block();
        self.terminate(Terminator::Jump {
            target: continue_block,
            args: vec![unwrapped],
        });

        self.switch_to_block(early_return_block);
        self.terminate(Terminator::Return(Some(operand)));

        // Merge: receive the unwrapped value via block parameter so that
        // the definition properly dominates all uses in continue_block.
        let result = self.add_block_param(continue_block, unwrap_type);
        self.switch_to_block(continue_block);
        result
    }

    /// Emit an equality comparison appropriate for the given source type.
    ///
    /// Dispatches to `IEq`, `FEq`, `StringEq`, or `BoolEq` based on `ty`.
    pub(crate) fn emit_comparison_for_type(
        &mut self,
        ty: &Type,
        lhs: ValueId,
        rhs: ValueId,
        span: Option<Span>,
    ) -> ValueId {
        let op = match ty {
            Type::Float => Op::FEq(lhs, rhs),
            Type::String => Op::StringEq(lhs, rhs),
            Type::Bool => Op::BoolEq(lhs, rhs),
            _ => Op::IEq(lhs, rhs),
        };
        self.emit(op, IrType::Bool, span)
    }
}

/// How to fill an empty argument slot during default materialization.
/// Used by `assemble_call_args` to choose between the wrapper-call
/// path and the legacy inline-default-expression path. See the
/// default-materialization comment in `assemble_call_args` for the
/// full design and the `default_wrappers` module for the wrapper
/// synthesis pass.
enum MissingFill {
    /// A synthesized wrapper exists for this `(callee, slot_idx)` —
    /// emit `Op::Call(wrapper_id, [], [])` to evaluate the default
    /// once in the callee's module's scope.
    Wrapper(FuncId),
    /// No wrapper — fall back to inlining the AST default expression
    /// at the caller's site. Used for pure-literal defaults (the
    /// common case in single-file programs).
    Inline(Expr),
}
