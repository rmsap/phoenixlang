//! Cranelift-based native code generation for the Phoenix compiler.
//!
//! Translates a Phoenix [`IrModule`] into a native object file (`.o`) that
//! can be linked with the Phoenix runtime library to produce an executable.
//!
//! # Usage
//!
//! ```ignore
//! let obj_bytes = phoenix_cranelift::compile(&ir_module)?;
//! std::fs::write("output.o", &obj_bytes)?;
//! // Then link: cc -o output output.o -lphoenix_runtime -L<runtime_dir>
//! ```
#![warn(missing_docs)]

mod abi;
mod builtins;
mod context;
mod error;
/// Runtime library discovery for linking.
pub mod link;
mod translate;
mod types;

pub use error::CompileError;
pub use link::{RUNTIME_LIB_NAME, find_runtime_lib};

use context::CompileContext;
use phoenix_ir::module::IrModule;

use cranelift_codegen::Context;
use cranelift_codegen::ir::Signature;
use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{AbiParam, InstBuilder};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module};

/// Compile a Phoenix IR module to a native object file.
///
/// Returns the raw bytes of an ELF object file (`.o`).  The object exports
/// a `main` function (the C entry point) which calls the Phoenix `main`.
pub fn compile(ir_module: &IrModule) -> Result<Vec<u8>, CompileError> {
    let mut ctx = CompileContext::new(ir_module)?;

    // Translate all Phoenix IR functions to Cranelift IR.
    translate::translate_module(&mut ctx, ir_module)?;

    // Generate a C-compatible `main` that calls the Phoenix `main`.
    generate_c_main(&mut ctx, ir_module)?;

    // Emit the object file.
    let product = ctx.module.finish();
    product.emit().map_err(CompileError::from_display)
}

/// Generate a C `main` function that calls the Phoenix `phx_main`.
fn generate_c_main(ctx: &mut CompileContext, ir_module: &IrModule) -> Result<(), CompileError> {
    let phx_main_id = ir_module
        .function_index
        .get("main")
        .ok_or_else(|| CompileError::new("no main function found"))?;

    let phx_main_cl_id = ctx.func_ids[phx_main_id];

    // Declare `main` with C calling convention: int main(int argc, char **argv)
    let mut sig = Signature::new(ctx.call_conv);
    sig.params.push(AbiParam::new(cl::I32)); // argc
    sig.params.push(AbiParam::new(cl::I64)); // argv
    sig.returns.push(AbiParam::new(cl::I32)); // return code

    let c_main_id = ctx.module.declare_function("main", Linkage::Export, &sig)?;

    let mut cl_ctx = Context::new();
    cl_ctx.func.signature = sig;

    let mut fb_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut cl_ctx.func, &mut fb_ctx);

    let block = builder.create_block();
    builder.append_block_params_for_function_params(block);
    builder.switch_to_block(block);
    builder.seal_block(block);

    // Call phx_main().
    let func_ref = ctx
        .module
        .declare_func_in_func(phx_main_cl_id, builder.func);
    builder.ins().call(func_ref, &[]);

    // Return 0.
    let zero = builder.ins().iconst(cl::I32, 0);
    builder.ins().return_(&[zero]);

    builder.finalize();

    ctx.module
        .define_function(c_main_id, &mut cl_ctx)
        .map_err(|e| CompileError::new(format!("failed to define C main: {e}")))?;

    Ok(())
}
