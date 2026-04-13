//! Expression lowering.
//!
//! Flattens nested Phoenix expressions into sequences of IR instructions,
//! each producing an SSA [`ValueId`].

use crate::instruction::{Op, ValueId};
use crate::lower::{LoweringContext, VarBinding};
use crate::module::IrFunction;
use crate::terminator::Terminator;
use crate::types::IrType;
use phoenix_common::span::Span;
use phoenix_parser::ast::{
    AssignmentExpr, BinaryExpr, BinaryOp, CallExpr, Expr, FieldAccessExpr, FieldAssignmentExpr,
    LambdaExpr, ListLiteralExpr, LiteralKind, MapLiteralExpr, MethodCallExpr,
    StringInterpolationExpr, StringSegment, StructLiteralExpr, TryExpr, UnaryExpr, UnaryOp,
};
use phoenix_sema::types::Type;

impl<'a> LoweringContext<'a> {
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
            Expr::Lambda(lambda) => self.lower_lambda(lambda),
            Expr::ListLiteral(list) => self.lower_list_literal(list),
            Expr::MapLiteral(map) => self.lower_map_literal(map),
            Expr::StringInterpolation(si) => self.lower_string_interpolation(si),
            Expr::Try(try_expr) => self.lower_try(try_expr),
        }
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
    fn lower_ident(&mut self, ident: &phoenix_parser::ast::IdentExpr) -> ValueId {
        // Check if it is a zero-field enum variant (e.g. `None`, `True`).
        // The sema checker resolves these as identifiers.
        if let Some(source_ty) = self.source_type(&ident.span) {
            let enum_name = match source_ty {
                Type::Generic(name, _) => Some(name.clone()),
                Type::Named(name) => Some(name.clone()),
                _ => None,
            };
            if let Some(enum_name) = enum_name
                && let Some(variants) = self.module.enum_layouts.get(&enum_name).cloned()
                && let Some((idx, _)) = variants
                    .iter()
                    .enumerate()
                    .find(|(_, (name, fields))| *name == ident.name && fields.is_empty())
            {
                return self.emit(
                    Op::EnumAlloc(enum_name.clone(), idx as u32, Vec::new()),
                    IrType::EnumRef(enum_name),
                    Some(ident.span),
                );
            }
        }

        // Regular variable lookup.
        if let Some(binding) = self.lookup_var(&ident.name).cloned() {
            match binding {
                VarBinding::Direct(val, _) => val,
                VarBinding::Mutable(slot, ty) => self.emit(Op::Load(slot), ty, Some(ident.span)),
            }
        } else if ident.name == "None" {
            // Built-in Option::None — emit as a zero-field struct so that
            // EnumDiscriminant and format() work uniformly with Some().
            self.emit(
                Op::StructAlloc("None".to_string(), Vec::new()),
                IrType::StructRef("None".to_string()),
                Some(ident.span),
            )
        } else {
            // Unknown variable — emit a placeholder constant.
            // This shouldn't happen if sema passed, but be defensive.
            self.emit(Op::ConstI64(0), IrType::I64, Some(ident.span))
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
        let left_type = self
            .source_type(&binary.left.span())
            .cloned()
            .unwrap_or_else(|| {
                debug_assert!(false, "missing sema type for binary left operand");
                Type::Int
            });

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
                let operand_type = self
                    .source_type(&unary.operand.span())
                    .cloned()
                    .unwrap_or_else(|| {
                        debug_assert!(false, "missing sema type for unary operand");
                        Type::Int
                    });
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

        // Lower arguments.
        let args: Vec<ValueId> = call.args.iter().map(|a| self.lower_expr(a)).collect();
        // TODO: handle named arguments by reordering based on function signature

        // Determine the callee.
        if let Expr::Ident(ident) = &call.callee {
            // Check for built-in functions.
            match ident.name.as_str() {
                "print" => {
                    return self.emit(
                        Op::BuiltinCall("print".to_string(), args),
                        IrType::Void,
                        span,
                    );
                }
                "toString" => {
                    return self.emit(
                        Op::BuiltinCall("toString".to_string(), args),
                        IrType::StringRef,
                        span,
                    );
                }
                _ => {}
            }

            // Check for a known function.
            if let Some(&func_id) = self.module.function_index.get(&ident.name) {
                let result_type = self.expr_type(&call.span);
                return self.emit(Op::Call(func_id, args), result_type, span);
            }

            // Check for an enum variant constructor (e.g. `Some(42)`).
            if let Some(source_ty) = self.source_type(&call.span).cloned()
                && let Type::Generic(ref enum_name, _) = source_ty
                && let Some(variants) = self.module.enum_layouts.get(enum_name).cloned()
                && let Some((idx, _)) = variants
                    .iter()
                    .enumerate()
                    .find(|(_, (name, _))| *name == ident.name)
            {
                let result_type = IrType::EnumRef(enum_name.clone());
                return self.emit(
                    Op::EnumAlloc(enum_name.clone(), idx as u32, args),
                    result_type,
                    span,
                );
            }

            // Check for a struct constructor (e.g. `Point(1, 2)` parsed as a call).
            if self.module.struct_layouts.contains_key(&ident.name) {
                let result_type = IrType::StructRef(ident.name.clone());
                return self.emit(Op::StructAlloc(ident.name.clone(), args), result_type, span);
            }
        }

        // Indirect call through a closure value.
        let callee_val = self.lower_expr(&call.callee);
        let result_type = self.expr_type(&call.span);
        self.emit(Op::CallIndirect(callee_val, args), result_type, span)
    }

    /// Lower a variable assignment.
    fn lower_assignment(&mut self, assign: &AssignmentExpr) -> ValueId {
        let val = self.lower_expr(&assign.value);

        if let Some(binding) = self.lookup_var(&assign.name).cloned()
            && let VarBinding::Mutable(slot, _) = binding
        {
            self.emit_void(Op::Store(slot, val), Some(assign.span));
        }

        val
    }

    /// Lower a field assignment.
    fn lower_field_assignment(&mut self, fa: &FieldAssignmentExpr) -> ValueId {
        let obj = self.lower_expr(&fa.object);
        let val = self.lower_expr(&fa.value);

        // Look up the field index from the struct layout.
        let obj_type = self
            .source_type(&fa.object.span())
            .cloned()
            .unwrap_or_else(|| {
                debug_assert!(false, "missing sema type for field assignment object");
                Type::Void
            });
        if let Type::Named(ref struct_name) = obj_type
            && let Some(layout) = self.module.struct_layouts.get(struct_name).cloned()
            && let Some(idx) = layout.iter().position(|(name, _)| *name == fa.field)
        {
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
            .unwrap_or_else(|| {
                debug_assert!(false, "missing sema type for field access object");
                Type::Void
            });

        if let Type::Named(ref struct_name) = obj_type
            && let Some(layout) = self.module.struct_layouts.get(struct_name).cloned()
            && let Some(idx) = layout.iter().position(|(name, _)| *name == fa.field)
        {
            let field_type = layout[idx].1.clone();
            return self.emit(Op::StructGetField(obj, idx as u32), field_type, span);
        }

        // Fallback: struct layout lookup failed — the object may be a
        // generic type or type alias that sema resolved differently.
        debug_assert!(
            false,
            "field access on unknown struct layout: field `{}` on type {:?}",
            fa.field, obj_type
        );
        let result_type = self.expr_type(&fa.span);
        self.emit(Op::StructGetField(obj, 0), result_type, span)
    }

    /// Lower a method call.
    fn lower_method_call(&mut self, mc: &MethodCallExpr) -> ValueId {
        let obj = self.lower_expr(&mc.object);
        let mut args: Vec<ValueId> = mc.args.iter().map(|a| self.lower_expr(a)).collect();
        let span = Some(mc.span);

        let obj_type = self
            .source_type(&mc.object.span())
            .cloned()
            .unwrap_or_else(|| {
                debug_assert!(false, "missing sema type for method call object");
                Type::Void
            });

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

        // Check for a user-defined method.
        let key = (type_name.clone(), mc.method.clone());
        if let Some(&func_id) = self.module.method_index.get(&key) {
            // Pass `self` as the first argument.
            args.insert(0, obj);
            let result_type = self.expr_type(&mc.span);
            return self.emit(Op::Call(func_id, args), result_type, span);
        }

        // Built-in method: emit as BuiltinCall.
        args.insert(0, obj);
        let builtin_name = format!("{type_name}.{}", mc.method);
        let result_type = self.expr_type(&mc.span);
        self.emit(Op::BuiltinCall(builtin_name, args), result_type, span)
    }

    /// Lower a struct literal expression.
    fn lower_struct_literal(&mut self, sl: &StructLiteralExpr) -> ValueId {
        let args: Vec<ValueId> = sl.args.iter().map(|a| self.lower_expr(a)).collect();
        let span = Some(sl.span);

        // Check if this is an enum variant constructor.
        let source_ty = self.source_type(&sl.span).cloned();
        let enum_name = match &source_ty {
            Some(Type::Generic(name, _)) | Some(Type::Named(name)) => self
                .module
                .enum_layouts
                .contains_key(name)
                .then(|| name.clone()),
            _ => None,
        };
        if let Some(enum_name) = enum_name
            && let Some(variants) = self.module.enum_layouts.get(&enum_name).cloned()
            && let Some((idx, _)) = variants
                .iter()
                .enumerate()
                .find(|(_, (name, _))| *name == sl.name)
        {
            return self.emit(
                Op::EnumAlloc(enum_name.clone(), idx as u32, args),
                IrType::EnumRef(enum_name),
                span,
            );
        }

        // Regular struct constructor.
        let result_type = IrType::StructRef(sl.name.clone());
        self.emit(Op::StructAlloc(sl.name.clone(), args), result_type, span)
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

        let func_id = crate::instruction::FuncId(self.module.functions.len() as u32);

        // Build parameter types for the closure function.
        // Captures come first, then the actual parameters.
        let mut param_types: Vec<IrType> = capture_types;
        let mut param_names: Vec<String> = captures
            .iter()
            .map(|cap| format!("__cap_{}", cap.name))
            .collect();

        // Add the actual lambda parameters.
        for p in &lambda.params {
            let ty = self.expr_type(&p.span);
            param_types.push(ty);
            param_names.push(p.name.clone());
        }

        let return_type = lambda
            .return_type
            .as_ref()
            .map(|_| self.expr_type(&lambda.span))
            .and_then(|t| {
                if let IrType::ClosureRef { return_type, .. } = t {
                    Some(*return_type)
                } else {
                    None
                }
            })
            .unwrap_or(IrType::Void);

        let closure_func = IrFunction::new(
            func_id,
            closure_name,
            param_types,
            param_names.clone(),
            return_type,
            Some(lambda.span),
        );
        self.module.functions.push(closure_func);

        // Save current function state and lower the lambda body.
        let saved_func_id = self.current_func_id;
        let saved_block = self.current_block;
        let saved_scopes = std::mem::take(&mut self.var_scopes);
        let saved_loops = std::mem::take(&mut self.loop_stack);

        self.current_func_id = Some(func_id);
        self.push_scope();

        let entry = self.create_block();
        self.switch_to_block(entry);

        // Bind captures and parameters.
        for (i, name) in param_names.iter().enumerate() {
            let param_type = self.module.functions[func_id.0 as usize].param_types[i].clone();
            let val = self.add_block_param(entry, param_type.clone());
            let clean_name = name.strip_prefix("__cap_").unwrap_or(name);
            self.define_var(clean_name.to_string(), VarBinding::Direct(val, param_type));
        }

        // Lower the body.
        let body_result = self.lower_block_implicit(&lambda.body);
        if self.block_needs_terminator() {
            if let Some(val) = body_result {
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
        let operand_type = self
            .source_type(&try_expr.operand.span())
            .cloned()
            .unwrap_or(Type::Void);

        let (ok_index, unwrap_type) = match &operand_type {
            Type::Generic(name, args) if name == "Result" => {
                let inner = args
                    .first()
                    .map(|t| self.lower_type(t))
                    .unwrap_or(IrType::Void);
                (0i64, inner) // Ok is variant 0
            }
            Type::Generic(name, args) if name == "Option" => {
                let inner = args
                    .first()
                    .map(|t| self.lower_type(t))
                    .unwrap_or(IrType::Void);
                (0i64, inner) // Some is variant 0
            }
            _ => (0, IrType::Void),
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
        let unwrapped = self.emit(Op::EnumGetField(operand, 0), unwrap_type, span);

        // Early return path: return the error/none value.
        let continue_block = self.create_block();
        self.terminate(Terminator::Jump {
            target: continue_block,
            args: Vec::new(),
        });

        self.switch_to_block(early_return_block);
        self.terminate(Terminator::Return(Some(operand)));

        self.switch_to_block(continue_block);
        unwrapped
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
