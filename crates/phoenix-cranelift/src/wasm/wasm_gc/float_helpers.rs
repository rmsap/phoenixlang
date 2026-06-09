//! Synthesized WASM-GC Float helpers for the wasm32-gc backend.
//!
//! Currently holds the in-progress `phx_print_f64` synthesis (PR 6
//! slice 5, §Phase 2.4 decision K.6). The helper structure mirrors
//! `string_helpers::synthesize_print_str`: declare its signature,
//! reserve scratch space in linear memory, walk the algorithm,
//! emit `fd_write`, return the index.
//!
//! **Phase status (PR 6 slice 5):**
//! - **Phase 1 (this file, landed):** special cases (NaN, ±inf),
//!   integer fast-path (`val.fract() == 0.0 && val in i64 range` →
//!   reuse `phx_print_i64`; `±0.0` both print "0" here, matching
//!   native). General Ryu d2s case errors at runtime with
//!   `unreachable`.
//! - **Phase 2 (in progress):** Ryu d2s port — precomputed POW5 /
//!   POW5_INV tables in passive data segments, 128-bit mulShift
//!   composed from i64 ops, digit shortening + emission.
//! - **Phase 3 (planned):** adversarial test corpus comparing
//!   wasm32-gc output against `phoenix_runtime::format_f64` for a
//!   pinned set of f64 values.

use wasm_encoder::{BlockType, Instruction, MemArg, ValType};

use crate::error::CompileError;

use super::module_builder::{IOVEC_OFFSET, ModuleBuilder, PRINT_F64_BUF_START, emit_fd_write_call};

// The linear-memory scratch region `[PRINT_F64_BUF_START,
// PRINT_F64_BUF_END)` is defined in `module_builder.rs` alongside the
// other regions (see its layout map). Phase 1 only stages short
// special-case literals at `PRINT_F64_BUF_START`; the Phase 2 Ryu
// emitter will additionally use the buffer's full extent up to
// `PRINT_F64_BUF_END`.

/// `phx_print_f64(v: f64)` — handle special cases and the integer
/// fast-path inline; defer the general Ryu d2s to Phase 2 with an
/// `unreachable` trap that surfaces clearly until the algorithm lands.
///
/// Param: `v` (f64, local 0). Phase 1 needs no extra locals — every
/// special case and the integer fast-path read `v` directly.
pub(super) fn synthesize_print_f64(
    b: &mut ModuleBuilder,
    fd_write_idx: u32,
    print_i64_idx: u32,
) -> Result<u32, CompileError> {
    let sig = b.intern_signature(&[ValType::F64], &[]);

    let mut func = wasm_encoder::Function::new([]);
    let v_local: u32 = 0;

    let i32_memarg = MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    };
    let byte_memarg = MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    };

    // ── Special case: NaN ────────────────────────────────────────
    // A f64 is NaN iff exponent bits are all 1 and mantissa is non-zero.
    // Equivalent test: `(bits & 0x7FFF_FFFF_FFFF_FFFF) > 0x7FF0_0000_0000_0000`.
    // We use `v != v` instead — WASM's `f64.ne` returns 1 for NaN
    // vs. itself (IEEE-754 unordered), and matches the native check.
    func.instruction(&Instruction::LocalGet(v_local));
    func.instruction(&Instruction::LocalGet(v_local));
    func.instruction(&Instruction::F64Ne);
    func.instruction(&Instruction::If(BlockType::Empty));
    emit_print_literal(&mut func, fd_write_idx, b"NaN\n", &i32_memarg, &byte_memarg);
    func.instruction(&Instruction::Return);
    func.instruction(&Instruction::End);

    // ── Special case: +/- infinity ───────────────────────────────
    // Test: v == f64::INFINITY or v == f64::NEG_INFINITY. Use
    // `f64.abs(v) == f64::INFINITY` to handle both with one comparison.
    func.instruction(&Instruction::LocalGet(v_local));
    func.instruction(&Instruction::F64Abs);
    func.instruction(&Instruction::F64Const(f64::INFINITY.into()));
    func.instruction(&Instruction::F64Eq);
    func.instruction(&Instruction::If(BlockType::Empty));
    // If v is negative infinity, print "-"
    func.instruction(&Instruction::LocalGet(v_local));
    func.instruction(&Instruction::F64Const(0.0.into()));
    func.instruction(&Instruction::F64Lt);
    func.instruction(&Instruction::If(BlockType::Empty));
    emit_print_literal(
        &mut func,
        fd_write_idx,
        b"-inf\n",
        &i32_memarg,
        &byte_memarg,
    );
    func.instruction(&Instruction::Else);
    emit_print_literal(&mut func, fd_write_idx, b"inf\n", &i32_memarg, &byte_memarg);
    func.instruction(&Instruction::End);
    func.instruction(&Instruction::Return);
    func.instruction(&Instruction::End);

    // ── Integer fast-path ────────────────────────────────────────
    // Native's `format_f64`:
    //   if val.fract() == 0.0 && val.is_finite()
    //      && val >= i64::MIN as f64
    //      && val < i64::MAX as f64 {
    //       (val as i64).to_string()
    //   }
    //
    // `-0.0` flows through here deliberately: native takes the same
    // fast-path for it (`(-0.0).fract() == 0.0`, finite, in range),
    // casts to `0i64`, and prints "0" — the sign bit is dropped. We
    // match that exactly rather than special-casing "-0".
    //
    // After the NaN / inf special cases above, `v` is finite here.
    // Test:
    //   - `f64.trunc(v) == v` (integer-valued)
    //   - `v >= i64::MIN as f64`
    //   - `v < i64::MAX as f64`
    // If all true, cast to i64 and delegate to phx_print_i64.
    func.instruction(&Instruction::LocalGet(v_local));
    func.instruction(&Instruction::F64Trunc);
    func.instruction(&Instruction::LocalGet(v_local));
    func.instruction(&Instruction::F64Eq);
    func.instruction(&Instruction::LocalGet(v_local));
    func.instruction(&Instruction::F64Const((i64::MIN as f64).into()));
    func.instruction(&Instruction::F64Ge);
    func.instruction(&Instruction::I32And);
    func.instruction(&Instruction::LocalGet(v_local));
    func.instruction(&Instruction::F64Const((i64::MAX as f64).into()));
    func.instruction(&Instruction::F64Lt);
    func.instruction(&Instruction::I32And);
    func.instruction(&Instruction::If(BlockType::Empty));
    func.instruction(&Instruction::LocalGet(v_local));
    func.instruction(&Instruction::I64TruncF64S);
    func.instruction(&Instruction::Call(print_i64_idx));
    func.instruction(&Instruction::Return);
    func.instruction(&Instruction::End);

    // ── General case: Ryu d2s ────────────────────────────────────
    // Phase 2 (in progress). Trap clearly until the algorithm lands.
    // The diagnostic is observable: WASM `unreachable` instruction
    // halts the instance, and the test harness reports it as a
    // failure with stderr noting the trap location.
    func.instruction(&Instruction::Unreachable);
    func.instruction(&Instruction::End);

    Ok(b.add_and_emit_function(sig, &func))
}

/// Emit an iovec-staged `fd_write` for a fixed byte sequence. The bytes
/// are written to a scratch region in linear memory at the start of
/// each call (the helper's scratch buffer doubles as the literal
/// staging area), then fd_write is called.
fn emit_print_literal(
    func: &mut wasm_encoder::Function,
    fd_write_idx: u32,
    bytes: &[u8],
    i32_memarg: &MemArg,
    byte_memarg: &MemArg,
) {
    // Write bytes into the f64 print buffer one at a time. For the
    // short literals we use (NaN, ±inf) this is fine; an
    // optimization would coalesce 4/8-byte writes but the literals
    // are short enough that the byte loop costs no measurable time.
    for (i, &byte) in bytes.iter().enumerate() {
        func.instruction(&Instruction::I32Const(
            PRINT_F64_BUF_START as i32 + i as i32,
        ));
        func.instruction(&Instruction::I32Const(byte as i32));
        func.instruction(&Instruction::I32Store8(*byte_memarg));
    }
    // Stage iovec.
    func.instruction(&Instruction::I32Const(IOVEC_OFFSET as i32));
    func.instruction(&Instruction::I32Const(PRINT_F64_BUF_START as i32));
    func.instruction(&Instruction::I32Store(*i32_memarg));
    func.instruction(&Instruction::I32Const(IOVEC_OFFSET as i32 + 4));
    func.instruction(&Instruction::I32Const(bytes.len() as i32));
    func.instruction(&Instruction::I32Store(*i32_memarg));
    emit_fd_write_call(func, fd_write_idx);
}
