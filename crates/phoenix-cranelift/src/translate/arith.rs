//! Translation of constants, arithmetic, and comparison operations.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{InstBuilder, Value};
use cranelift_frontend::FunctionBuilder;
use cranelift_module::Module;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::types::POINTER_TYPE;
use phoenix_ir::instruction::Op;

use super::helpers::emit_div_zero_check;
use super::{FuncState, get_val1};

/// Translate a constant operation to a Cranelift value.
pub(super) fn translate_const(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    op: &Op,
) -> Result<Vec<Value>, CompileError> {
    match op {
        Op::ConstI64(n) => Ok(vec![builder.ins().iconst(cl::I64, *n)]),
        Op::ConstF64(n) => Ok(vec![builder.ins().f64const(*n)]),
        Op::ConstBool(b) => Ok(vec![builder.ins().iconst(cl::I8, *b as i64)]),
        Op::ConstString(s) => {
            let data_id = ctx
                .module
                .declare_anonymous_data(false, false)
                .map_err(CompileError::from_display)?;

            let mut data_desc = cranelift_module::DataDescription::new();
            data_desc.define(s.as_bytes().to_vec().into_boxed_slice());
            ctx.module
                .define_data(data_id, &data_desc)
                .map_err(CompileError::from_display)?;

            let gv = ctx.module.declare_data_in_func(data_id, builder.func);
            let ptr = builder.ins().global_value(POINTER_TYPE, gv);
            let len = builder.ins().iconst(cl::I64, s.len() as i64);
            Ok(vec![ptr, len])
        }
        _ => unreachable!(),
    }
}

/// Translate an integer arithmetic operation.
pub(super) fn translate_int_arith(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    op: &Op,
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    match op {
        Op::IAdd(a, b) => Ok(vec![
            builder
                .ins()
                .iadd(get_val1(state, *a)?, get_val1(state, *b)?),
        ]),
        Op::ISub(a, b) => Ok(vec![
            builder
                .ins()
                .isub(get_val1(state, *a)?, get_val1(state, *b)?),
        ]),
        Op::IMul(a, b) => Ok(vec![
            builder
                .ins()
                .imul(get_val1(state, *a)?, get_val1(state, *b)?),
        ]),
        Op::IDiv(a, b) => {
            let va = get_val1(state, *a)?;
            let vb = get_val1(state, *b)?;
            emit_div_zero_check(builder, ctx, vb)?;
            Ok(vec![builder.ins().sdiv(va, vb)])
        }
        Op::IMod(a, b) => {
            let va = get_val1(state, *a)?;
            let vb = get_val1(state, *b)?;
            emit_div_zero_check(builder, ctx, vb)?;
            Ok(vec![builder.ins().srem(va, vb)])
        }
        Op::INeg(a) => Ok(vec![builder.ins().ineg(get_val1(state, *a)?)]),
        _ => unreachable!(),
    }
}

/// Translate a floating-point arithmetic operation.
pub(super) fn translate_float_arith(
    builder: &mut FunctionBuilder,
    op: &Op,
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    match op {
        Op::FAdd(a, b) => Ok(vec![
            builder
                .ins()
                .fadd(get_val1(state, *a)?, get_val1(state, *b)?),
        ]),
        Op::FSub(a, b) => Ok(vec![
            builder
                .ins()
                .fsub(get_val1(state, *a)?, get_val1(state, *b)?),
        ]),
        Op::FMul(a, b) => Ok(vec![
            builder
                .ins()
                .fmul(get_val1(state, *a)?, get_val1(state, *b)?),
        ]),
        Op::FDiv(a, b) => Ok(vec![
            builder
                .ins()
                .fdiv(get_val1(state, *a)?, get_val1(state, *b)?),
        ]),
        Op::FMod(a, b) => {
            // Cranelift has no native fmod.  The interpreter uses Rust's `%`
            // which is truncation-toward-zero (C `fmod` semantics), so we must
            // use `trunc` (not `floor`) to match: a - trunc(a/b) * b.
            let va = get_val1(state, *a)?;
            let vb = get_val1(state, *b)?;
            let div = builder.ins().fdiv(va, vb);
            let truncated = builder.ins().trunc(div);
            let product = builder.ins().fmul(truncated, vb);
            Ok(vec![builder.ins().fsub(va, product)])
        }
        Op::FNeg(a) => Ok(vec![builder.ins().fneg(get_val1(state, *a)?)]),
        _ => unreachable!(),
    }
}

/// Translate a comparison or boolean operation.
pub(super) fn translate_cmp(
    builder: &mut FunctionBuilder,
    op: &Op,
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    match op {
        // Integer comparisons
        Op::IEq(a, b) => Ok(vec![builder.ins().icmp(
            IntCC::Equal,
            get_val1(state, *a)?,
            get_val1(state, *b)?,
        )]),
        Op::INe(a, b) => Ok(vec![builder.ins().icmp(
            IntCC::NotEqual,
            get_val1(state, *a)?,
            get_val1(state, *b)?,
        )]),
        Op::ILt(a, b) => Ok(vec![builder.ins().icmp(
            IntCC::SignedLessThan,
            get_val1(state, *a)?,
            get_val1(state, *b)?,
        )]),
        Op::IGt(a, b) => Ok(vec![builder.ins().icmp(
            IntCC::SignedGreaterThan,
            get_val1(state, *a)?,
            get_val1(state, *b)?,
        )]),
        Op::ILe(a, b) => Ok(vec![builder.ins().icmp(
            IntCC::SignedLessThanOrEqual,
            get_val1(state, *a)?,
            get_val1(state, *b)?,
        )]),
        Op::IGe(a, b) => Ok(vec![builder.ins().icmp(
            IntCC::SignedGreaterThanOrEqual,
            get_val1(state, *a)?,
            get_val1(state, *b)?,
        )]),

        // Float comparisons
        Op::FEq(a, b) => Ok(vec![builder.ins().fcmp(
            FloatCC::Equal,
            get_val1(state, *a)?,
            get_val1(state, *b)?,
        )]),
        Op::FNe(a, b) => Ok(vec![builder.ins().fcmp(
            FloatCC::NotEqual,
            get_val1(state, *a)?,
            get_val1(state, *b)?,
        )]),
        Op::FLt(a, b) => Ok(vec![builder.ins().fcmp(
            FloatCC::LessThan,
            get_val1(state, *a)?,
            get_val1(state, *b)?,
        )]),
        Op::FGt(a, b) => Ok(vec![builder.ins().fcmp(
            FloatCC::GreaterThan,
            get_val1(state, *a)?,
            get_val1(state, *b)?,
        )]),
        Op::FLe(a, b) => Ok(vec![builder.ins().fcmp(
            FloatCC::LessThanOrEqual,
            get_val1(state, *a)?,
            get_val1(state, *b)?,
        )]),
        Op::FGe(a, b) => Ok(vec![builder.ins().fcmp(
            FloatCC::GreaterThanOrEqual,
            get_val1(state, *a)?,
            get_val1(state, *b)?,
        )]),

        // Boolean operations
        Op::BoolEq(a, b) => Ok(vec![builder.ins().icmp(
            IntCC::Equal,
            get_val1(state, *a)?,
            get_val1(state, *b)?,
        )]),
        Op::BoolNe(a, b) => Ok(vec![builder.ins().icmp(
            IntCC::NotEqual,
            get_val1(state, *a)?,
            get_val1(state, *b)?,
        )]),
        Op::BoolNot(a) => {
            let v = get_val1(state, *a)?;
            Ok(vec![builder.ins().bxor_imm(v, 1)])
        }

        _ => unreachable!(),
    }
}
