//! Cranelift-based code generation for the Phoenix compiler.
//!
//! Translates a Phoenix [`IrModule`] into either a native object file
//! (`.o`) that can be linked with the Phoenix runtime library to produce
//! an executable, or — once Phase 2.4 lands — a WebAssembly module.
//! The choice is selected by [`Target`]; see `docs/design-decisions.md`
//! §Phase 2.4 WebAssembly compilation for the per-target contract.
//!
//! # Usage
//!
//! ```ignore
//! use phoenix_cranelift::Target;
//! let obj_bytes = phoenix_cranelift::compile(&ir_module, Target::Native)?;
//! std::fs::write("output.o", &obj_bytes)?;
//! // Then link: cc -o output output.o -lphoenix_runtime -L<runtime_dir>
//! ```
#![warn(missing_docs)]

mod abi;
mod builtins;
mod context;
mod error;

/// Crate-internal macro: panic with a recognisable "internal compiler
/// error" prefix so the failure is grep-able and clearly distinct from
/// user-facing diagnostics. Use at sites that are unreachable when the
/// IR is well-formed (i.e. would have been `unreachable!()`); the
/// message should name the dispatcher and what was expected so a hit
/// in the wild points at the right pass to debug. The macro is in
/// scope crate-wide via `macro_rules!` textual ordering — submodules
/// can invoke `ice!(...)` directly without importing.
///
/// Note: the `mod translate;` and `pub mod link;` declarations below
/// rely on this macro being defined first. If a future refactor moves
/// submodule declarations above this point, `ice!` will silently
/// disappear from those modules — keep this definition above any
/// submodule that uses it.
macro_rules! ice {
    ($($arg:tt)*) => {
        panic!("internal compiler error in cranelift backend: {}", format_args!($($arg)*))
    };
}

/// Runtime library discovery for linking.
pub mod link;
mod target;
mod translate;
mod type_tag;
mod types;

pub use error::CompileError;
pub use link::{LinkError, RUNTIME_LIB_NAME, find_runtime_lib, link_executable};
pub use target::Target;

use context::CompileContext;
use phoenix_ir::module::IrModule;

use cranelift_codegen::Context;
use cranelift_codegen::ir::Signature;
use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{AbiParam, InstBuilder};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module};

/// Compile a Phoenix IR module to a target-appropriate artifact.
///
/// For [`Target::Native`] this returns the raw bytes of an ELF / Mach-O
/// object file (`.o`) whose exported `main` calls the Phoenix `phx_main`.
/// For the WASM variants this returns the raw bytes of a `.wasm` module
/// once their codegen lands in Phase 2.4;
/// until then they return a "not yet implemented"
/// [`CompileError`] so the abstraction is callable but the gate is
/// loud.
pub fn compile(ir_module: &IrModule, target: Target) -> Result<Vec<u8>, CompileError> {
    match target {
        Target::Native => compile_native(ir_module),
        Target::Wasm32Linear | Target::Wasm32Gc => Err(CompileError::new(format!(
            "target `{}` is not yet implemented; \
             WASM codegen lands in Phase 2.4 (see \
             docs/design-decisions.md §Phase 2.4 WebAssembly compilation \
             for the per-target PR sequence)",
            target.as_cli()
        ))),
    }
}

fn compile_native(ir_module: &IrModule) -> Result<Vec<u8>, CompileError> {
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

    // Enable threshold-driven GC, run phx_main, then free everything
    // still tracked. See `phx_gc_enable` / `phx_gc_shutdown` for the
    // rationale of each call.
    translate::call_runtime(&mut builder, ctx, ctx.runtime.gc_enable, &[]);
    translate::call_runtime(&mut builder, ctx, phx_main_cl_id, &[]);
    translate::call_runtime(&mut builder, ctx, ctx.runtime.gc_shutdown, &[]);

    // Return 0.
    let zero = builder.ins().iconst(cl::I32, 0);
    builder.ins().return_(&[zero]);

    builder.finalize();

    ctx.module
        .define_function(c_main_id, &mut cl_ctx)
        .map_err(|e| CompileError::new(format!("failed to define C main: {e}")))?;

    Ok(())
}
