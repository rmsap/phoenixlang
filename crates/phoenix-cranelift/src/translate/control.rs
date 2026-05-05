//! Translation of terminators (control flow instructions).

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{self, InstBuilder, Value};
use cranelift_frontend::FunctionBuilder;

use crate::context::CompileContext;
use crate::error::CompileError;
use phoenix_ir::module::IrFunction;
use phoenix_ir::terminator::Terminator;

use super::gc_roots;
use super::{FuncState, get_val, get_val1};

/// Translate a Phoenix terminator into Cranelift control flow instructions.
pub(super) fn translate_terminator(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    term: &Terminator,
    state: &FuncState,
    _func: &IrFunction,
) -> Result<(), CompileError> {
    match term {
        Terminator::Return(val) => {
            // Compute the return values *before* popping the frame, so
            // that any ref-typed return value remains rooted while we
            // emit the pop_frame call.
            let return_vals: Vec<Value> = if let Some(vid) = val {
                get_val(state, *vid)?
            } else {
                Vec::new()
            };
            if let Some(frame) = state.gc_frame.as_ref() {
                gc_roots::emit_frame_pop(builder, ctx, frame);
            }
            builder.ins().return_(&return_vals);
        }
        Terminator::Jump { target, args } => {
            let cl_target = state.block_map[target];
            let mut cl_args = Vec::new();
            for arg in args {
                cl_args.extend(get_val(state, *arg)?);
            }
            builder.ins().jump(cl_target, &cl_args);
        }
        Terminator::Branch {
            condition,
            true_block,
            true_args,
            false_block,
            false_args,
        } => {
            let cond = get_val1(state, *condition)?;
            let cl_true = state.block_map[true_block];
            let cl_false = state.block_map[false_block];
            let mut cl_true_args = Vec::new();
            for a in true_args {
                cl_true_args.extend(get_val(state, *a)?);
            }
            let mut cl_false_args = Vec::new();
            for a in false_args {
                cl_false_args.extend(get_val(state, *a)?);
            }
            builder
                .ins()
                .brif(cond, cl_true, &cl_true_args, cl_false, &cl_false_args);
        }
        Terminator::Switch {
            value,
            cases,
            default,
            default_args,
        } => {
            let disc = get_val1(state, *value)?;
            let cl_default = state.block_map[default];
            let mut cl_default_args = Vec::new();
            for a in default_args {
                cl_default_args.extend(get_val(state, *a)?);
            }

            if cases.is_empty() {
                builder.ins().jump(cl_default, &cl_default_args);
            } else {
                for (i, (disc_val, target, args)) in cases.iter().enumerate() {
                    let cmp_val = builder.ins().iconst(cl::I64, *disc_val as i64);
                    let cond = builder.ins().icmp(IntCC::Equal, disc, cmp_val);
                    let cl_target = state.block_map[target];
                    let mut cl_args: Vec<Value> = Vec::new();
                    for a in args {
                        cl_args.extend(get_val(state, *a)?);
                    }

                    if i == cases.len() - 1 {
                        builder
                            .ins()
                            .brif(cond, cl_target, &cl_args, cl_default, &cl_default_args);
                    } else {
                        let next_check = builder.create_block();
                        builder
                            .ins()
                            .brif(cond, cl_target, &cl_args, next_check, &[]);
                        builder.seal_block(next_check);
                        builder.switch_to_block(next_check);
                    }
                }
            }
        }
        Terminator::Unreachable => {
            builder.ins().trap(ir::TrapCode::unwrap_user(2));
        }
        Terminator::None => {
            return Err(CompileError::new(
                "encountered Terminator::None — IR is incomplete",
            ));
        }
    }
    Ok(())
}
