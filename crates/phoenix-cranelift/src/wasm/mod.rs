//! WebAssembly backend for the Phoenix compiler.
//!
//! Translates a Phoenix [`IrModule`] into a `.wasm` module via the
//! Bytecode Alliance's [`wasm-encoder`] crate. Cranelift's `wasm32`
//! ISA support is *input*-side only (it consumes WASM for wasmtime),
//! so the native backend's Cranelift IR machinery is not reused — this
//! module is a parallel translator targeting the WebAssembly
//! instruction set directly. See
//! `docs/design-decisions.md` §Phase 2.4 decision A0 for the rationale.
//!
//! # Scope
//!
//! PR 2 of Phase 2.4 lands the *scaffolding*: WASM module structure,
//! WASI preview1 host imports (`fd_write`, `proc_exit`), per-module
//! emit of a small synthesized runtime (`phx_print_i64` and friends),
//! IR → WASM function translation, and a `_start` entry that calls
//! Phoenix's `main`. The translator's IR-op coverage is intentionally
//! narrow: enough to compile `tests/fixtures/hello.phx`
//! (`function main() { let x: Int = 42; print(x) }`) end-to-end through
//! `wasmtime`. Other IR ops produce a clean
//! [`CompileError`] pointing at the PR 3 follow-up that extends
//! coverage alongside the linear-memory `MarkSweepHeap` port.
//!
//! # Memory layout (linear-memory variant, PR 2)
//!
//! - `[0, bool_literals_end)` — fixed-position bool literals
//!   (`"true\n"`, `"false\n"`). Reserved only when at least one
//!   `print(bool)` call appears in the module; absent for fixtures
//!   like hello.phx that print only integers.
//! - `[bool_literals_end, data_end)` — general string-constant area,
//!   advanced by `ModuleBuilder::reserve_data`. PR 2 doesn't append
//!   anything here yet; PR 3's `Op::ConstString` lands the first
//!   entries.
//! - `[SCRATCH_BASE, SCRATCH_BASE + 64)` — per-call scratch. Used by
//!   the synthesized `phx_print_*` helpers to stage an iovec, an
//!   `nwritten` cell, and a 32-byte itoa buffer before invoking WASI
//!   `fd_write`. Single-threaded model: each `phx_print_*` call
//!   consumes and releases the scratch within its own body, so
//!   sequential `print` calls don't interfere.
//!
//! Both regions live in the first 64 KiB page; PR 3 makes the page
//! count dynamic when the GC needs a heap.
//!
//! # File layout
//!
//! - [`module_builder`] — `ModuleBuilder`, the per-section assembler
//!   driving the declare/emit pipeline.
//! - [`type_interner`] — `TypeInterner`, the WASM type-section
//!   deduplicator.
//! - [`helper_usage`] — `HelperUsage`, the pre-scan that decides which
//!   synthesized print helpers a module needs.
//! - [`runtime`] — bodies for the synthesized `phx_print_*` helpers.
//! - [`translate`] — Phoenix IR → WASM function-body translation.

use phoenix_ir::module::IrModule;

use crate::error::CompileError;

mod helper_usage;
mod module_builder;
mod runtime;
mod translate;
mod type_interner;

use helper_usage::HelperUsage;
use module_builder::ModuleBuilder;

/// Top of the first 64 KiB page, minus 64 bytes of scratch. The
/// scratch sits at the high end of memory in PR 2 so the data section
/// (string constants, growing from offset 0) and the scratch never
/// overlap — even when PR 3 starts emitting non-trivial string data.
/// Aligned to 8 bytes so the embedded iovec / nwritten / itoa-buffer
/// fields stay naturally aligned.
pub(super) const SCRATCH_BASE: u32 = 65472;

/// Compile a Phoenix IR module to a linear-memory WebAssembly module.
///
/// Returns the raw bytes of a `.wasm` module that:
/// - Imports `wasi_snapshot_preview1.fd_write` and
///   `wasi_snapshot_preview1.proc_exit`.
/// - Defines and exports a single linear memory (`memory`).
/// - Synthesizes only the per-module runtime helpers the IR actually
///   needs (`phx_print_i64`, `phx_print_bool`, `phx_print_str`).
/// - Translates each concrete Phoenix function into a WASM function.
/// - Exports a WASI-compatible `_start` that calls Phoenix's `main`.
///
/// The output is well-formed enough to load under `wasmtime` and pass
/// `wasmparser` validation; the integration test in
/// `crates/phoenix-cranelift/tests/compile_wasm_linear.rs` exercises
/// both.
pub(super) fn compile_wasm_linear(ir_module: &IrModule) -> Result<Vec<u8>, CompileError> {
    let helpers = HelperUsage::scan(ir_module);
    let mut builder = ModuleBuilder::new();
    builder.declare_imports();
    builder.declare_memory();
    builder.declare_runtime_helpers(helpers);
    builder.declare_phoenix_functions(ir_module)?;
    builder.declare_start();
    builder.emit_exports();
    builder.emit_runtime_bodies(helpers);
    builder.emit_phoenix_bodies(ir_module)?;
    builder.emit_start_body();
    Ok(builder.finish())
}
