//! Synthesized WASM-GC Float helpers for the wasm32-gc backend.
//!
//! Holds the `phx_print_f64` synthesis (PR 6 slice 5, §Phase 2.4
//! decision K.6 — 2026-06-09 amendment locking ryu's shortest-
//! roundtrip format on both backends). The helper structure mirrors
//! `string_helpers::synthesize_print_str`: declare signatures, reserve
//! scratch space in linear memory, walk the algorithm, emit
//! `fd_write`, return the index.
//!
//! **Phase status (PR 6 slice 5):**
//! - **Phase 1:** NaN / ±inf special cases inline (ryu's
//!   `Buffer::format` checks them before calling `format64`; so do we).
//! - **Phase 2 (this file):** the Ryu d2s general case — a
//!   line-by-line port of the `ryu` crate's `d2d` (`src/d2s.rs`),
//!   `mul_shift_64` / `multiple_of_power_of_5` (`src/d2s_intrinsics.rs`)
//!   and pretty formatter (`src/pretty/mod.rs`, `format64`) into
//!   `wasm-encoder` bytecode. The power-of-5 tables are computed from
//!   their mathematical definitions in [`super::ryu_tables`] (trimmed
//!   to the f64-reachable index ranges) and materialized as active
//!   data segments; 128-bit multiplies are composed from i64 ops.
//! - **Phase 3:** the `float_print_*` tests in `compile_wasm_gc.rs`
//!   pin the output against the `ryu` crate (= native `format_f64`):
//!   a hand-picked adversarial corpus, the longhand f64 extremes,
//!   runtime-computed values, 200 deterministic random bit patterns,
//!   and a sweep of every IEEE binary exponent (which exercises every
//!   reachable entry of both power-of-5 tables).
//!
//! # Helper call graph
//!
//! ```text
//! phx_print_f64 ──► phx_ryu_d2d ──► phx_ryu_mul_shift_64 ──► phx_ryu_umul128_hi
//!      │                 └────────► phx_ryu_mult_pow5
//!      ├────────► phx_ryu_write_digits
//!      └────────► phx_ryu_write_exp3 ──► phx_ryu_write_digits
//! ```
//!
//! Every helper is immediate-emit (declared and code-emitted in one
//! call), so the function/code section parallelism that
//! `ModuleBuilder::finish` guards holds — `declare_print_f64_helper`
//! runs before any deferred-body Phoenix function is declared.

use wasm_encoder::{BlockType, Function, Instruction, MemArg, ValType};

use crate::error::CompileError;

use super::module_builder::{
    IOVEC_OFFSET, ModuleBuilder, PRINT_F64_BUF_START, RYU_POW5_INV_SPLIT_OFFSET,
    RYU_POW5_SPLIT_INDEX_BASE, RYU_POW5_SPLIT_OFFSET, emit_fd_write_call,
};
use super::ryu_tables::{double_pow5_inv_split, double_pow5_split};

// Ryu d2s constants (names match the `ryu` crate's `d2s.rs` /
// `d2s_intrinsics.rs`).
/// `(1 << 52) - 1` — IEEE-754 f64 mantissa mask.
const MANTISSA_MASK: i64 = (1 << 52) - 1;
/// `1 << 52` — the implicit leading mantissa bit.
const HIDDEN_BIT: i64 = 1 << 52;
/// `5 * M_INV_5 ≡ 1 (mod 2^64)` — modular inverse used by
/// `pow5_factor`'s divisibility loop. (`14757395258967641293u64` as
/// i64.)
const M_INV_5: i64 = 14757395258967641293u64 as i64;
/// `⌊(2^64 - 1) / 5⌋` — after a `wrapping_mul(M_INV_5)`, the value is
/// ≤ this iff the pre-multiplication value was divisible by 5.
const N_DIV_5: i64 = 3689348814741910323;

/// Emit a batch of instructions onto `func`. The d2s port is several
/// hundred instructions; batching keeps each algorithm step readable
/// as one `ins(...)` block mirroring the corresponding ryu source
/// lines.
fn ins(func: &mut Function, list: &[Instruction]) {
    for i in list {
        func.instruction(i);
    }
}

/// `i32.store8` at `[addr + off]` (addr from the stack).
fn store8(off: u64) -> Instruction<'static> {
    Instruction::I32Store8(MemArg {
        offset: off,
        align: 0,
        memory_index: 0,
    })
}

/// `i64.load` at `[addr + off]` (addr from the stack), 8-byte aligned —
/// the Ryu table base offsets are 16-aligned and entries are 16 bytes,
/// so every `lo` / `hi` word sits on a natural boundary.
fn load64(off: u64) -> Instruction<'static> {
    Instruction::I64Load(MemArg {
        offset: off,
        align: 3,
        memory_index: 0,
    })
}

/// `phx_ryu_format_f64(v: f64) -> i32` — format `v` into the linear-
/// memory scratch at `PRINT_F64_BUF_START` in exactly the bytes
/// native's `phoenix_runtime::format_f64` (= the `ryu` crate)
/// produces, returning the byte length. No trailing newline and no
/// I/O — `phx_print_f64` (the thin wrapper below) appends the newline
/// and calls `fd_write`; `phx_tostring_f64` copies the bytes into a
/// fresh `$string`. NaN / ±inf are handled inline up front; ±0.0 and
/// all other finite values flow through the ported `format64` + `d2d`.
pub(super) fn synthesize_format_f64(b: &mut ModuleBuilder) -> Result<u32, CompileError> {
    // Materialize the power-of-5 tables. Serialized as little-endian
    // `(lo: u64, hi: u64)` pairs — `phx_ryu_d2d` loads `lo` at
    // `base + idx*16` and `hi` at `base + idx*16 + 8`, where `base`
    // is the segment start for the inverse table and
    // `RYU_POW5_SPLIT_INDEX_BASE` for the pow5 table (its segment is
    // trimmed to the f64-reachable indices 1..=325).
    let inv_split = double_pow5_inv_split();
    let mut inv_bytes = Vec::with_capacity(inv_split.len() * 16);
    for (lo, hi) in inv_split {
        inv_bytes.extend_from_slice(&lo.to_le_bytes());
        inv_bytes.extend_from_slice(&hi.to_le_bytes());
    }
    b.declare_active_data(RYU_POW5_INV_SPLIT_OFFSET, &inv_bytes);
    let pow5_split = double_pow5_split();
    let mut pow5_bytes = Vec::with_capacity(pow5_split.len() * 16);
    for (lo, hi) in pow5_split {
        pow5_bytes.extend_from_slice(&lo.to_le_bytes());
        pow5_bytes.extend_from_slice(&hi.to_le_bytes());
    }
    b.declare_active_data(RYU_POW5_SPLIT_OFFSET, &pow5_bytes);

    // Sub-helpers, in dependency order.
    let umul_hi_idx = synthesize_umul128_hi(b);
    let mul_shift_idx = synthesize_mul_shift_64(b, umul_hi_idx);
    let mult_pow5_idx = synthesize_mult_pow5(b);
    let write_digits_idx = synthesize_write_digits(b);
    let write_exp3_idx = synthesize_write_exp3(b, write_digits_idx);
    let d2d_idx = synthesize_d2d(b, mul_shift_idx, mult_pow5_idx);

    let sig = b.intern_signature(&[ValType::F64], &[ValType::I32]);

    // Locals beyond the f64 param at 0.
    let mut func = Function::new([(2, ValType::I64), (6, ValType::I32)]);
    const V: u32 = 0; // f64 param
    const BITS: u32 = 1; // i64 — raw bits, then (reused) the IEEE mantissa
    const OUT: u32 = 2; // i64 — d2d's decimal mantissa ("output")
    const IDX: u32 = 3; // i32 — write cursor (absolute linear-memory address)
    const IEXP: u32 = 4; // i32 — IEEE biased exponent
    const LEN: u32 = 5; // i32 — decimal_length17(OUT)
    const K: u32 = 6; // i32 — d2d's decimal exponent
    const KK: u32 = 7; // i32 — LEN + K (10^(KK-1) <= |v| < 10^KK)
    const CUR: u32 = 8; // i32 — zero-fill cursor / exponent base scratch

    // ── Special case: NaN ────────────────────────────────────────
    // A f64 is NaN iff exponent bits are all 1 and mantissa is non-zero.
    // We use `v != v` — WASM's `f64.ne` returns 1 for NaN vs. itself
    // (IEEE-754 unordered), matching native's `val.is_nan()` check. Ryu
    // emits `"NaN"` for NaN inputs, so we match byte-for-byte.
    ins(
        &mut func,
        &[
            Instruction::LocalGet(V),
            Instruction::LocalGet(V),
            Instruction::F64Ne,
            Instruction::If(BlockType::Empty),
        ],
    );
    write_literal(&mut func, b"NaN");
    ins(
        &mut func,
        &[
            Instruction::I32Const(3),
            Instruction::Return,
            Instruction::End,
        ],
    );

    // ── Special case: ±Infinity ──────────────────────────────────
    // `f64.abs(v) == INFINITY` handles both with one comparison; the
    // sign branch distinguishes them. Ryu emits `"inf"` / `"-inf"`.
    ins(
        &mut func,
        &[
            Instruction::LocalGet(V),
            Instruction::F64Abs,
            Instruction::F64Const(f64::INFINITY.into()),
            Instruction::F64Eq,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(V),
            Instruction::F64Const(0.0.into()),
            Instruction::F64Lt,
            Instruction::If(BlockType::Empty),
        ],
    );
    write_literal(&mut func, b"-inf");
    ins(
        &mut func,
        &[
            Instruction::I32Const(4),
            Instruction::Return,
            Instruction::Else,
        ],
    );
    write_literal(&mut func, b"inf");
    ins(
        &mut func,
        &[
            Instruction::I32Const(3),
            Instruction::Return,
            Instruction::End,
            Instruction::End,
        ],
    );

    // ── General case: ryu `format64` (src/pretty/mod.rs) ─────────
    // bits = v.to_bits(); idx = buffer start; '-' if sign bit set.
    ins(
        &mut func,
        &[
            Instruction::LocalGet(V),
            Instruction::I64ReinterpretF64,
            Instruction::LocalSet(BITS),
            Instruction::I32Const(PRINT_F64_BUF_START as i32),
            Instruction::LocalSet(IDX),
            Instruction::LocalGet(BITS),
            Instruction::I64Const(0),
            Instruction::I64LtS, // sign bit set ⇔ bits < 0 as signed
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(IDX),
            Instruction::I32Const(b'-' as i32),
            store8(0),
            Instruction::LocalGet(IDX),
            Instruction::I32Const(1),
            Instruction::I32Add,
            Instruction::LocalSet(IDX),
            Instruction::End,
            // ieee_exponent = (bits >> 52) & 0x7FF
            Instruction::LocalGet(BITS),
            Instruction::I64Const(52),
            Instruction::I64ShrU,
            Instruction::I32WrapI64,
            Instruction::I32Const(0x7FF),
            Instruction::I32And,
            Instruction::LocalSet(IEXP),
            // BITS := ieee_mantissa = bits & ((1 << 52) - 1)
            Instruction::LocalGet(BITS),
            Instruction::I64Const(MANTISSA_MASK),
            Instruction::I64And,
            Instruction::LocalSet(BITS),
            // ±0.0 → "0.0" (sign already emitted above)
            Instruction::LocalGet(IEXP),
            Instruction::I32Eqz,
            Instruction::LocalGet(BITS),
            Instruction::I64Eqz,
            Instruction::I32And,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(IDX),
            Instruction::I32Const(b'0' as i32),
            store8(0),
            Instruction::LocalGet(IDX),
            Instruction::I32Const(b'.' as i32),
            store8(1),
            Instruction::LocalGet(IDX),
            Instruction::I32Const(b'0' as i32),
            store8(2),
            Instruction::LocalGet(IDX),
            Instruction::I32Const(3),
            Instruction::I32Add,
            Instruction::LocalSet(IDX),
            Instruction::Else,
            // (output, exp) = d2d(mantissa, exponent)
            Instruction::LocalGet(BITS),
            Instruction::LocalGet(IEXP),
            Instruction::Call(d2d_idx),
            Instruction::LocalSet(K),
            Instruction::LocalSet(OUT),
        ],
    );
    // LEN = decimal_length17(OUT): 1 + Σ (OUT >= 10^p) for p = 1..=16.
    // Same result as ryu's high-to-low comparison chain; OUT < 10^17
    // by d2d's contract (17 digits suffice for round-tripping).
    func.instruction(&Instruction::I32Const(1));
    let mut pow10: i64 = 10;
    for _ in 1..=16 {
        ins(
            &mut func,
            &[
                Instruction::LocalGet(OUT),
                Instruction::I64Const(pow10),
                Instruction::I64GeU,
                Instruction::I32Add,
            ],
        );
        pow10 *= 10;
    }
    ins(
        &mut func,
        &[
            Instruction::LocalSet(LEN),
            // KK = LEN + K
            Instruction::LocalGet(LEN),
            Instruction::LocalGet(K),
            Instruction::I32Add,
            Instruction::LocalSet(KK),
        ],
    );

    // The four pretty-format branches, in ryu's order. Worst-case
    // write extent is idx+24 plus the newline — see the
    // PRINT_F64_BUF_START doc in module_builder.rs.
    //
    // Branch 1: 0 <= k && kk <= 16 — integer with trailing ".0"
    // (e.g. 1234e7 → "12340000000.0").
    ins(
        &mut func,
        &[
            Instruction::LocalGet(K),
            Instruction::I32Const(0),
            Instruction::I32GeS,
            Instruction::LocalGet(KK),
            Instruction::I32Const(16),
            Instruction::I32LeS,
            Instruction::I32And,
            Instruction::If(BlockType::Empty),
            // digits of OUT end at idx+LEN
            Instruction::LocalGet(OUT),
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(LEN),
            Instruction::I32Add,
            Instruction::Call(write_digits_idx),
            // zero-fill [idx+LEN, idx+KK)
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(LEN),
            Instruction::I32Add,
            Instruction::LocalSet(CUR),
            Instruction::Block(BlockType::Empty),
            Instruction::Loop(BlockType::Empty),
            Instruction::LocalGet(CUR),
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(KK),
            Instruction::I32Add,
            Instruction::I32GeS,
            Instruction::BrIf(1),
            Instruction::LocalGet(CUR),
            Instruction::I32Const(b'0' as i32),
            store8(0),
            Instruction::LocalGet(CUR),
            Instruction::I32Const(1),
            Instruction::I32Add,
            Instruction::LocalSet(CUR),
            Instruction::Br(0),
            Instruction::End,
            Instruction::End,
            // ".0" suffix; idx = idx + kk + 2
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(KK),
            Instruction::I32Add,
            Instruction::I32Const(b'.' as i32),
            store8(0),
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(KK),
            Instruction::I32Add,
            Instruction::I32Const(b'0' as i32),
            store8(1),
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(KK),
            Instruction::I32Add,
            Instruction::I32Const(2),
            Instruction::I32Add,
            Instruction::LocalSet(IDX),
            Instruction::Else,
            // Branch 2: 0 < kk <= 16 — decimal point inside the digits
            // (e.g. 1234e-2 → "12.34"). Digits are staged one byte
            // right of final position, then the first KK shift left by
            // one (`memory.copy` has memmove semantics, so the
            // overlapping forward copy is well-defined), then '.' lands
            // in the gap. Mirrors ryu's write-then-ptr::copy.
            Instruction::LocalGet(KK),
            Instruction::I32Const(0),
            Instruction::I32GtS,
            Instruction::LocalGet(KK),
            Instruction::I32Const(16),
            Instruction::I32LeS,
            Instruction::I32And,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(OUT),
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(LEN),
            Instruction::I32Add,
            Instruction::I32Const(1),
            Instruction::I32Add,
            Instruction::Call(write_digits_idx),
            Instruction::LocalGet(IDX), // dst
            Instruction::LocalGet(IDX),
            Instruction::I32Const(1),
            Instruction::I32Add,       // src
            Instruction::LocalGet(KK), // size
            Instruction::MemoryCopy {
                src_mem: 0,
                dst_mem: 0,
            },
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(KK),
            Instruction::I32Add,
            Instruction::I32Const(b'.' as i32),
            store8(0),
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(LEN),
            Instruction::I32Add,
            Instruction::I32Const(1),
            Instruction::I32Add,
            Instruction::LocalSet(IDX),
            Instruction::Else,
            // Branch 3: -5 < kk <= 0 — leading "0." + zeros
            // (e.g. 1234e-6 → "0.001234").
            Instruction::LocalGet(KK),
            Instruction::I32Const(-5),
            Instruction::I32GtS,
            Instruction::LocalGet(KK),
            Instruction::I32Const(0),
            Instruction::I32LeS,
            Instruction::I32And,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(IDX),
            Instruction::I32Const(b'0' as i32),
            store8(0),
            Instruction::LocalGet(IDX),
            Instruction::I32Const(b'.' as i32),
            store8(1),
            // zero-fill [idx+2, idx+2-kk)
            Instruction::LocalGet(IDX),
            Instruction::I32Const(2),
            Instruction::I32Add,
            Instruction::LocalSet(CUR),
            Instruction::Block(BlockType::Empty),
            Instruction::Loop(BlockType::Empty),
            Instruction::LocalGet(CUR),
            Instruction::LocalGet(IDX),
            Instruction::I32Const(2),
            Instruction::I32Add,
            Instruction::LocalGet(KK),
            Instruction::I32Sub,
            Instruction::I32GeS,
            Instruction::BrIf(1),
            Instruction::LocalGet(CUR),
            Instruction::I32Const(b'0' as i32),
            store8(0),
            Instruction::LocalGet(CUR),
            Instruction::I32Const(1),
            Instruction::I32Add,
            Instruction::LocalSet(CUR),
            Instruction::Br(0),
            Instruction::End,
            Instruction::End,
            // digits end at idx + LEN + (2 - KK); idx = that end
            Instruction::LocalGet(OUT),
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(LEN),
            Instruction::I32Add,
            Instruction::I32Const(2),
            Instruction::I32Add,
            Instruction::LocalGet(KK),
            Instruction::I32Sub,
            Instruction::Call(write_digits_idx),
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(LEN),
            Instruction::I32Add,
            Instruction::I32Const(2),
            Instruction::I32Add,
            Instruction::LocalGet(KK),
            Instruction::I32Sub,
            Instruction::LocalSet(IDX),
            Instruction::Else,
            // Branch 4: single digit, scientific (e.g. 1e30 → "1e30").
            Instruction::LocalGet(LEN),
            Instruction::I32Const(1),
            Instruction::I32Eq,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(OUT),
            Instruction::I32WrapI64, // OUT <= 9 here
            Instruction::I32Const(b'0' as i32),
            Instruction::I32Add,
            store8(0),
            Instruction::LocalGet(IDX),
            Instruction::I32Const(b'e' as i32),
            store8(1),
            // idx = (idx + 2) + write_exp3(kk - 1, idx + 2)
            Instruction::LocalGet(IDX),
            Instruction::I32Const(2),
            Instruction::I32Add,
            Instruction::LocalTee(CUR),
            Instruction::LocalGet(KK),
            Instruction::I32Const(1),
            Instruction::I32Sub,
            Instruction::LocalGet(CUR),
            Instruction::Call(write_exp3_idx),
            Instruction::I32Add,
            Instruction::LocalSet(IDX),
            Instruction::Else,
            // Branch 5: scientific (e.g. 1234e30 → "1.234e33").
            // Digits staged at idx+1..idx+LEN+1; the first digit then
            // copies down to idx and '.' takes its place.
            Instruction::LocalGet(OUT),
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(LEN),
            Instruction::I32Add,
            Instruction::I32Const(1),
            Instruction::I32Add,
            Instruction::Call(write_digits_idx),
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(IDX),
            Instruction::I32Load8U(MemArg {
                offset: 1,
                align: 0,
                memory_index: 0,
            }),
            store8(0),
            Instruction::LocalGet(IDX),
            Instruction::I32Const(b'.' as i32),
            store8(1),
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(LEN),
            Instruction::I32Add,
            Instruction::I32Const(b'e' as i32),
            store8(1),
            // idx = (idx + LEN + 2) + write_exp3(kk - 1, idx + LEN + 2)
            Instruction::LocalGet(IDX),
            Instruction::LocalGet(LEN),
            Instruction::I32Add,
            Instruction::I32Const(2),
            Instruction::I32Add,
            Instruction::LocalTee(CUR),
            Instruction::LocalGet(KK),
            Instruction::I32Const(1),
            Instruction::I32Sub,
            Instruction::LocalGet(CUR),
            Instruction::Call(write_exp3_idx),
            Instruction::I32Add,
            Instruction::LocalSet(IDX),
            Instruction::End, // branch 4/5
            Instruction::End, // branch 3
            Instruction::End, // branch 2
            Instruction::End, // branch 1
            Instruction::End, // zero / d2d if-else
            // Return the formatted length.
            Instruction::LocalGet(IDX),
            Instruction::I32Const(PRINT_F64_BUF_START as i32),
            Instruction::I32Sub,
        ],
    );
    func.instruction(&Instruction::End);

    Ok(b.add_and_emit_function(sig, &func))
}

/// `phx_print_f64(v: f64)` — `phx_ryu_format_f64` + trailing newline +
/// `fd_write`. The formatter owns every byte of the text (including
/// the NaN / ±inf literals); this wrapper owns the I/O.
pub(super) fn synthesize_print_f64(
    b: &mut ModuleBuilder,
    fd_write_idx: u32,
    format_f64_idx: u32,
) -> Result<u32, CompileError> {
    let sig = b.intern_signature(&[ValType::F64], &[]);
    let mut func = Function::new([(1, ValType::I32)]);
    const V: u32 = 0; // f64 param
    const LEN: u32 = 1; // i32 — formatted byte length
    let i32_memarg = MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    };
    ins(
        &mut func,
        &[
            Instruction::LocalGet(V),
            Instruction::Call(format_f64_idx),
            Instruction::LocalSet(LEN),
            // '\n' at START + len
            Instruction::I32Const(PRINT_F64_BUF_START as i32),
            Instruction::LocalGet(LEN),
            Instruction::I32Add,
            Instruction::I32Const(b'\n' as i32),
            store8(0),
            // iovec: (START, len + 1)
            Instruction::I32Const(IOVEC_OFFSET as i32),
            Instruction::I32Const(PRINT_F64_BUF_START as i32),
            Instruction::I32Store(i32_memarg),
            Instruction::I32Const(IOVEC_OFFSET as i32 + 4),
            Instruction::LocalGet(LEN),
            Instruction::I32Const(1),
            Instruction::I32Add,
            Instruction::I32Store(i32_memarg),
        ],
    );
    emit_fd_write_call(&mut func, fd_write_idx);
    func.instruction(&Instruction::End);
    Ok(b.add_and_emit_function(sig, &func))
}

/// `phx_ryu_umul128_hi(a: u64, b: u64) -> u64` — the high 64 bits of
/// the unsigned 128-bit product `a × b`, composed from four 32×32→64
/// partial products (WASM has no 128-bit integer ops). The standard
/// schoolbook decomposition: the `mid` accumulator collects the three
/// contributions to bits 32..95 (max 3 × (2³²−1), no overflow), and
/// its high word carries into the result.
fn synthesize_umul128_hi(b: &mut ModuleBuilder) -> u32 {
    let sig = b.intern_signature(&[ValType::I64, ValType::I64], &[ValType::I64]);
    let mut f = Function::new([(7, ValType::I64)]);
    const A: u32 = 0;
    const B: u32 = 1;
    const A_LO: u32 = 2;
    const A_HI: u32 = 3;
    const B_LO: u32 = 4;
    const B_HI: u32 = 5;
    const LH: u32 = 6; // a_lo * b_hi
    const HL: u32 = 7; // a_hi * b_lo
    const MID: u32 = 8;
    const MASK32: i64 = 0xFFFF_FFFF;
    ins(
        &mut f,
        &[
            Instruction::LocalGet(A),
            Instruction::I64Const(MASK32),
            Instruction::I64And,
            Instruction::LocalSet(A_LO),
            Instruction::LocalGet(A),
            Instruction::I64Const(32),
            Instruction::I64ShrU,
            Instruction::LocalSet(A_HI),
            Instruction::LocalGet(B),
            Instruction::I64Const(MASK32),
            Instruction::I64And,
            Instruction::LocalSet(B_LO),
            Instruction::LocalGet(B),
            Instruction::I64Const(32),
            Instruction::I64ShrU,
            Instruction::LocalSet(B_HI),
            Instruction::LocalGet(A_LO),
            Instruction::LocalGet(B_HI),
            Instruction::I64Mul,
            Instruction::LocalSet(LH),
            Instruction::LocalGet(A_HI),
            Instruction::LocalGet(B_LO),
            Instruction::I64Mul,
            Instruction::LocalSet(HL),
            // mid = (a_lo*b_lo >> 32) + (lh & MASK32) + (hl & MASK32)
            Instruction::LocalGet(A_LO),
            Instruction::LocalGet(B_LO),
            Instruction::I64Mul,
            Instruction::I64Const(32),
            Instruction::I64ShrU,
            Instruction::LocalGet(LH),
            Instruction::I64Const(MASK32),
            Instruction::I64And,
            Instruction::I64Add,
            Instruction::LocalGet(HL),
            Instruction::I64Const(MASK32),
            Instruction::I64And,
            Instruction::I64Add,
            Instruction::LocalSet(MID),
            // hi = a_hi*b_hi + (lh >> 32) + (hl >> 32) + (mid >> 32)
            Instruction::LocalGet(A_HI),
            Instruction::LocalGet(B_HI),
            Instruction::I64Mul,
            Instruction::LocalGet(LH),
            Instruction::I64Const(32),
            Instruction::I64ShrU,
            Instruction::I64Add,
            Instruction::LocalGet(HL),
            Instruction::I64Const(32),
            Instruction::I64ShrU,
            Instruction::I64Add,
            Instruction::LocalGet(MID),
            Instruction::I64Const(32),
            Instruction::I64ShrU,
            Instruction::I64Add,
            Instruction::End,
        ],
    );
    b.add_and_emit_function(sig, &f)
}

/// `phx_ryu_mul_shift_64(m: u64, mul_lo: u64, mul_hi: u64, j: i32) ->
/// u64` — ryu's `mul_shift_64`:
/// `(((m·mul_lo >> 64) + m·mul_hi) >> (j − 64))` truncated to u64.
///
/// The 128-bit sum is carried explicitly (`sum_lo < lo(m·mul_hi)`
/// after the add ⇔ the add wrapped). The final shift composes the two
/// words; ryu guarantees `64 < j < 128` (its `j` stays within a few
/// bits of the 125-bit table precision), so `s = j − 64 ∈ [1, 63]` and
/// the `64 − s` left shift never degenerates to a mod-64 = 0 shift.
fn synthesize_mul_shift_64(b: &mut ModuleBuilder, umul_hi_idx: u32) -> u32 {
    let sig = b.intern_signature(
        &[ValType::I64, ValType::I64, ValType::I64, ValType::I32],
        &[ValType::I64],
    );
    let mut f = Function::new([(3, ValType::I64)]);
    const M: u32 = 0;
    const MUL_LO: u32 = 1;
    const MUL_HI: u32 = 2;
    const J: u32 = 3;
    const SUM_LO: u32 = 4;
    const SUM_HI: u32 = 5;
    const S: u32 = 6;
    ins(
        &mut f,
        &[
            // sum_lo = hi(m·mul_lo) + lo(m·mul_hi)   (may wrap)
            Instruction::LocalGet(M),
            Instruction::LocalGet(MUL_LO),
            Instruction::Call(umul_hi_idx),
            Instruction::LocalGet(M),
            Instruction::LocalGet(MUL_HI),
            Instruction::I64Mul,
            Instruction::I64Add,
            Instruction::LocalSet(SUM_LO),
            // sum_hi = hi(m·mul_hi) + carry(sum_lo < lo(m·mul_hi))
            Instruction::LocalGet(M),
            Instruction::LocalGet(MUL_HI),
            Instruction::Call(umul_hi_idx),
            Instruction::LocalGet(SUM_LO),
            Instruction::LocalGet(M),
            Instruction::LocalGet(MUL_HI),
            Instruction::I64Mul,
            Instruction::I64LtU,
            Instruction::I64ExtendI32U,
            Instruction::I64Add,
            Instruction::LocalSet(SUM_HI),
            // s = j - 64
            Instruction::LocalGet(J),
            Instruction::I32Const(64),
            Instruction::I32Sub,
            Instruction::I64ExtendI32U,
            Instruction::LocalSet(S),
            // (sum_lo >> s) | (sum_hi << (64 - s))
            Instruction::LocalGet(SUM_LO),
            Instruction::LocalGet(S),
            Instruction::I64ShrU,
            Instruction::LocalGet(SUM_HI),
            Instruction::I64Const(64),
            Instruction::LocalGet(S),
            Instruction::I64Sub,
            Instruction::I64Shl,
            Instruction::I64Or,
            Instruction::End,
        ],
    );
    b.add_and_emit_function(sig, &f)
}

/// `phx_ryu_mult_pow5(value: u64, p: u32) -> i32` — ryu's
/// `multiple_of_power_of_5`: 1 iff `value` is divisible by `5^p`.
/// The loop is `pow5_factor`: each `wrapping_mul(M_INV_5)` divides by
/// 5 exactly when the value was a multiple of 5 (result ≤ N_DIV_5),
/// and scrambles it above N_DIV_5 otherwise. `value` is never 0 at
/// the call sites (mv or mv±small, with mv = 4·m2 and m2 ≥ 1).
fn synthesize_mult_pow5(b: &mut ModuleBuilder) -> u32 {
    let sig = b.intern_signature(&[ValType::I64, ValType::I32], &[ValType::I32]);
    let mut f = Function::new([(1, ValType::I32)]);
    const VAL: u32 = 0;
    const P: u32 = 1;
    const COUNT: u32 = 2;
    ins(
        &mut f,
        &[
            Instruction::Block(BlockType::Empty),
            Instruction::Loop(BlockType::Empty),
            Instruction::LocalGet(VAL),
            Instruction::I64Const(M_INV_5),
            Instruction::I64Mul,
            Instruction::LocalTee(VAL),
            Instruction::I64Const(N_DIV_5),
            Instruction::I64GtU,
            Instruction::BrIf(1),
            Instruction::LocalGet(COUNT),
            Instruction::I32Const(1),
            Instruction::I32Add,
            Instruction::LocalSet(COUNT),
            Instruction::Br(0),
            Instruction::End,
            Instruction::End,
            Instruction::LocalGet(COUNT),
            Instruction::LocalGet(P),
            Instruction::I32GeU,
            Instruction::End,
        ],
    );
    b.add_and_emit_function(sig, &f)
}

/// `phx_ryu_write_digits(value: u64, end: i32)` — write `value`'s
/// decimal digits right-to-left so the last digit lands at `end - 1`.
/// The caller pre-computes the digit count (`decimal_length17` for the
/// mantissa, a ≤3-digit length for exponents), so the start position
/// is known and no return value is needed. Emits at least one digit
/// (`value == 0` → `"0"`), matching a do-while.
fn synthesize_write_digits(b: &mut ModuleBuilder) -> u32 {
    let sig = b.intern_signature(&[ValType::I64, ValType::I32], &[]);
    let mut f = Function::new([]);
    const VAL: u32 = 0;
    const END: u32 = 1;
    ins(
        &mut f,
        &[
            Instruction::Loop(BlockType::Empty),
            // end -= 1; [end] = '0' + value % 10
            Instruction::LocalGet(END),
            Instruction::I32Const(1),
            Instruction::I32Sub,
            Instruction::LocalTee(END),
            Instruction::LocalGet(VAL),
            Instruction::I64Const(10),
            Instruction::I64RemU,
            Instruction::I32WrapI64,
            Instruction::I32Const(b'0' as i32),
            Instruction::I32Add,
            store8(0),
            // value /= 10; continue while value != 0
            Instruction::LocalGet(VAL),
            Instruction::I64Const(10),
            Instruction::I64DivU,
            Instruction::LocalTee(VAL),
            Instruction::I64Const(0),
            Instruction::I64Ne,
            Instruction::BrIf(0),
            Instruction::End,
            Instruction::End,
        ],
    );
    b.add_and_emit_function(sig, &f)
}

/// `phx_ryu_write_exp3(k: i32, pos: i32) -> i32` — ryu's
/// `write_exponent3`: emit the decimal exponent (`'-'` for negative,
/// no `'+'`, no zero padding; |k| ≤ 324 so at most 3 digits) at `pos`,
/// returning the byte count written. Digit bytes are identical to
/// ryu's `DIGIT_TABLE` lookups — that table is just packed decimal
/// pairs.
fn synthesize_write_exp3(b: &mut ModuleBuilder, write_digits_idx: u32) -> u32 {
    let sig = b.intern_signature(&[ValType::I32, ValType::I32], &[ValType::I32]);
    let mut f = Function::new([(2, ValType::I32)]);
    const K: u32 = 0;
    const POS: u32 = 1;
    const SIGN: u32 = 2;
    const LEN: u32 = 3;
    ins(
        &mut f,
        &[
            Instruction::LocalGet(K),
            Instruction::I32Const(0),
            Instruction::I32LtS,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(POS),
            Instruction::I32Const(b'-' as i32),
            store8(0),
            Instruction::LocalGet(POS),
            Instruction::I32Const(1),
            Instruction::I32Add,
            Instruction::LocalSet(POS),
            Instruction::I32Const(0),
            Instruction::LocalGet(K),
            Instruction::I32Sub,
            Instruction::LocalSet(K),
            Instruction::I32Const(1),
            Instruction::LocalSet(SIGN),
            Instruction::End,
            // len = 1 + (k >= 10) + (k >= 100)
            Instruction::I32Const(1),
            Instruction::LocalGet(K),
            Instruction::I32Const(10),
            Instruction::I32GeS,
            Instruction::I32Add,
            Instruction::LocalGet(K),
            Instruction::I32Const(100),
            Instruction::I32GeS,
            Instruction::I32Add,
            Instruction::LocalSet(LEN),
            Instruction::LocalGet(K),
            Instruction::I64ExtendI32U,
            Instruction::LocalGet(POS),
            Instruction::LocalGet(LEN),
            Instruction::I32Add,
            Instruction::Call(write_digits_idx),
            Instruction::LocalGet(SIGN),
            Instruction::LocalGet(LEN),
            Instruction::I32Add,
            Instruction::End,
        ],
    );
    b.add_and_emit_function(sig, &f)
}

/// `phx_ryu_d2d(ieee_mantissa: u64, ieee_exponent: u32) -> (u64, i32)`
/// — ryu's `d2d` (`src/d2s.rs`): the shortest decimal mantissa /
/// exponent pair that round-trips. A line-by-line port; comments cite
/// the ryu steps. Returns `(output, exp)` as a multi-value result
/// (mantissa below exponent on the stack).
fn synthesize_d2d(b: &mut ModuleBuilder, mul_shift_idx: u32, mult_pow5_idx: u32) -> u32 {
    let sig = b.intern_signature(&[ValType::I64, ValType::I32], &[ValType::I64, ValType::I32]);
    let mut f = Function::new([(10, ValType::I64), (13, ValType::I32)]);
    const MANT: u32 = 0; // i64 param
    const EXP: u32 = 1; // i32 param
    const M2: u32 = 2;
    const MV: u32 = 3; // 4 * m2
    const VR: u32 = 4; // also holds `output` at the end
    const VP: u32 = 5;
    const VM: u32 = 6;
    const MUL_LO: u32 = 7;
    const MUL_HI: u32 = 8;
    const VPD: u32 = 9; // vp / 10 (or / 100) per loop iteration
    const VMD: u32 = 10;
    const VRD: u32 = 11;
    const E2: u32 = 12;
    const MMS: u32 = 13; // mm_shift
    const ACC: u32 = 14; // accept_bounds (= even)
    const Q: u32 = 15;
    const E10: u32 = 16;
    const J: u32 = 17; // mul_shift's shift parameter
    const REM: u32 = 18; // removed
    const LRD: u32 = 19; // last_removed_digit; round_up on the common path
    const VMT: u32 = 20; // vm_is_trailing_zeros
    const VRT: u32 = 21; // vr_is_trailing_zeros
    const TMP: u32 = 22; // vm_mod10 scratch / table index i (e2 < 0 arm)
    const NEGE2: u32 = 23; // -e2 (e2 < 0 arm)
    const TIDX: u32 = 24; // byte offset into the active table

    // Step 1: decode subnormal vs. normal (ryu d2s.rs:92-103). The
    // "- 2" gives the bounds computation two extra bits.
    ins(
        &mut f,
        &[
            Instruction::LocalGet(EXP),
            Instruction::I32Eqz,
            Instruction::If(BlockType::Empty),
            Instruction::I32Const(1 - 1023 - 52 - 2),
            Instruction::LocalSet(E2),
            Instruction::LocalGet(MANT),
            Instruction::LocalSet(M2),
            Instruction::Else,
            Instruction::LocalGet(EXP),
            Instruction::I32Const(1023 + 52 + 2),
            Instruction::I32Sub,
            Instruction::LocalSet(E2),
            Instruction::LocalGet(MANT),
            Instruction::I64Const(HIDDEN_BIT),
            Instruction::I64Or,
            Instruction::LocalSet(M2),
            Instruction::End,
            // accept_bounds = even = (m2 & 1) == 0
            Instruction::LocalGet(M2),
            Instruction::I64Const(1),
            Instruction::I64And,
            Instruction::I64Eqz,
            Instruction::LocalSet(ACC),
            // Step 2: mv = 4 * m2; mm_shift = (mantissa != 0 || exponent <= 1)
            Instruction::LocalGet(M2),
            Instruction::I64Const(2),
            Instruction::I64Shl,
            Instruction::LocalSet(MV),
            Instruction::LocalGet(MANT),
            Instruction::I64Const(0),
            Instruction::I64Ne,
            Instruction::LocalGet(EXP),
            Instruction::I32Const(1),
            Instruction::I32LeU,
            Instruction::I32Or,
            Instruction::LocalSet(MMS),
            // (vm/vr_is_trailing_zeros, removed, last_removed_digit
            // start at 0 — WASM locals are zero-initialized.)
            //
            // Step 3: convert to decimal base (ryu d2s.rs:124-210).
            Instruction::LocalGet(E2),
            Instruction::I32Const(0),
            Instruction::I32GeS,
            Instruction::If(BlockType::Empty),
            // q = log10_pow2(e2) - (e2 > 3); log10_pow2(e) = (e * 78913) >> 18
            Instruction::LocalGet(E2),
            Instruction::I32Const(78913),
            Instruction::I32Mul,
            Instruction::I32Const(18),
            Instruction::I32ShrU,
            Instruction::LocalGet(E2),
            Instruction::I32Const(3),
            Instruction::I32GtS,
            Instruction::I32Sub,
            Instruction::LocalSet(Q),
            Instruction::LocalGet(Q),
            Instruction::LocalSet(E10),
            // j = -e2 + q + k, k = 125 + pow5bits(q) - 1
            //   = ((q * 1217359) >> 19) + 125 + q - e2
            Instruction::LocalGet(Q),
            Instruction::I32Const(1217359),
            Instruction::I32Mul,
            Instruction::I32Const(19),
            Instruction::I32ShrU,
            Instruction::I32Const(125),
            Instruction::I32Add,
            Instruction::LocalGet(Q),
            Instruction::I32Add,
            Instruction::LocalGet(E2),
            Instruction::I32Sub,
            Instruction::LocalSet(J),
            // DOUBLE_POW5_INV_SPLIT[q]
            Instruction::LocalGet(Q),
            Instruction::I32Const(4),
            Instruction::I32Shl,
            Instruction::LocalTee(TIDX),
            load64(RYU_POW5_INV_SPLIT_OFFSET as u64),
            Instruction::LocalSet(MUL_LO),
            Instruction::LocalGet(TIDX),
            load64(RYU_POW5_INV_SPLIT_OFFSET as u64 + 8),
            Instruction::LocalSet(MUL_HI),
            // vp = mul_shift(mv + 2); vm = mul_shift(mv - 1 - mm_shift);
            // vr = mul_shift(mv) — ryu's mul_shift_all_64
            Instruction::LocalGet(MV),
            Instruction::I64Const(2),
            Instruction::I64Add,
            Instruction::LocalGet(MUL_LO),
            Instruction::LocalGet(MUL_HI),
            Instruction::LocalGet(J),
            Instruction::Call(mul_shift_idx),
            Instruction::LocalSet(VP),
            Instruction::LocalGet(MV),
            Instruction::I64Const(1),
            Instruction::I64Sub,
            Instruction::LocalGet(MMS),
            Instruction::I64ExtendI32U,
            Instruction::I64Sub,
            Instruction::LocalGet(MUL_LO),
            Instruction::LocalGet(MUL_HI),
            Instruction::LocalGet(J),
            Instruction::Call(mul_shift_idx),
            Instruction::LocalSet(VM),
            Instruction::LocalGet(MV),
            Instruction::LocalGet(MUL_LO),
            Instruction::LocalGet(MUL_HI),
            Instruction::LocalGet(J),
            Instruction::Call(mul_shift_idx),
            Instruction::LocalSet(VR),
            // if q <= 21: trailing-zero bookkeeping (only one of
            // mp/mv/mm can be a multiple of 5, if any)
            Instruction::LocalGet(Q),
            Instruction::I32Const(21),
            Instruction::I32LeU,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(MV),
            Instruction::I64Const(5),
            Instruction::I64RemU,
            Instruction::I64Eqz,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(MV),
            Instruction::LocalGet(Q),
            Instruction::Call(mult_pow5_idx),
            Instruction::LocalSet(VRT),
            Instruction::Else,
            Instruction::LocalGet(ACC),
            Instruction::If(BlockType::Empty),
            // vm_is_trailing_zeros = multiple_of_power_of_5(mv - 1 - mm_shift, q)
            Instruction::LocalGet(MV),
            Instruction::I64Const(1),
            Instruction::I64Sub,
            Instruction::LocalGet(MMS),
            Instruction::I64ExtendI32U,
            Instruction::I64Sub,
            Instruction::LocalGet(Q),
            Instruction::Call(mult_pow5_idx),
            Instruction::LocalSet(VMT),
            Instruction::Else,
            // vp -= multiple_of_power_of_5(mv + 2, q)
            Instruction::LocalGet(VP),
            Instruction::LocalGet(MV),
            Instruction::I64Const(2),
            Instruction::I64Add,
            Instruction::LocalGet(Q),
            Instruction::Call(mult_pow5_idx),
            Instruction::I64ExtendI32U,
            Instruction::I64Sub,
            Instruction::LocalSet(VP),
            Instruction::End,
            Instruction::End,
            Instruction::End,
            Instruction::Else, // e2 < 0
            Instruction::I32Const(0),
            Instruction::LocalGet(E2),
            Instruction::I32Sub,
            Instruction::LocalSet(NEGE2),
            // q = log10_pow5(-e2) - (-e2 > 1); log10_pow5(e) = (e * 732923) >> 20
            Instruction::LocalGet(NEGE2),
            Instruction::I32Const(732923),
            Instruction::I32Mul,
            Instruction::I32Const(20),
            Instruction::I32ShrU,
            Instruction::LocalGet(NEGE2),
            Instruction::I32Const(1),
            Instruction::I32GtS,
            Instruction::I32Sub,
            Instruction::LocalSet(Q),
            // e10 = q + e2
            Instruction::LocalGet(Q),
            Instruction::LocalGet(E2),
            Instruction::I32Add,
            Instruction::LocalSet(E10),
            // i = -e2 - q (table index, in TMP)
            Instruction::LocalGet(NEGE2),
            Instruction::LocalGet(Q),
            Instruction::I32Sub,
            Instruction::LocalSet(TMP),
            // j = q - k, k = pow5bits(i) - 125
            //   = q + 124 - ((i * 1217359) >> 19)
            Instruction::LocalGet(Q),
            Instruction::I32Const(124),
            Instruction::I32Add,
            Instruction::LocalGet(TMP),
            Instruction::I32Const(1217359),
            Instruction::I32Mul,
            Instruction::I32Const(19),
            Instruction::I32ShrU,
            Instruction::I32Sub,
            Instruction::LocalSet(J),
            // DOUBLE_POW5_SPLIT[i] — the segment is trimmed to
            // indices ≥ 1, so loads go through the index base one
            // entry-width below the segment start (i ≥ 1 always).
            Instruction::LocalGet(TMP),
            Instruction::I32Const(4),
            Instruction::I32Shl,
            Instruction::LocalTee(TIDX),
            load64(RYU_POW5_SPLIT_INDEX_BASE as u64),
            Instruction::LocalSet(MUL_LO),
            Instruction::LocalGet(TIDX),
            load64(RYU_POW5_SPLIT_INDEX_BASE as u64 + 8),
            Instruction::LocalSet(MUL_HI),
            Instruction::LocalGet(MV),
            Instruction::I64Const(2),
            Instruction::I64Add,
            Instruction::LocalGet(MUL_LO),
            Instruction::LocalGet(MUL_HI),
            Instruction::LocalGet(J),
            Instruction::Call(mul_shift_idx),
            Instruction::LocalSet(VP),
            Instruction::LocalGet(MV),
            Instruction::I64Const(1),
            Instruction::I64Sub,
            Instruction::LocalGet(MMS),
            Instruction::I64ExtendI32U,
            Instruction::I64Sub,
            Instruction::LocalGet(MUL_LO),
            Instruction::LocalGet(MUL_HI),
            Instruction::LocalGet(J),
            Instruction::Call(mul_shift_idx),
            Instruction::LocalSet(VM),
            Instruction::LocalGet(MV),
            Instruction::LocalGet(MUL_LO),
            Instruction::LocalGet(MUL_HI),
            Instruction::LocalGet(J),
            Instruction::Call(mul_shift_idx),
            Instruction::LocalSet(VR),
            // if q <= 1: mv = 4·m2 always has ≥ 2 trailing zero bits
            Instruction::LocalGet(Q),
            Instruction::I32Const(1),
            Instruction::I32LeU,
            Instruction::If(BlockType::Empty),
            Instruction::I32Const(1),
            Instruction::LocalSet(VRT),
            Instruction::LocalGet(ACC),
            Instruction::If(BlockType::Empty),
            // mm has a trailing zero bit iff mm_shift == 1
            Instruction::LocalGet(MMS),
            Instruction::I32Const(1),
            Instruction::I32Eq,
            Instruction::LocalSet(VMT),
            Instruction::Else,
            // mp = mv + 2 always has one; vp -= 1
            Instruction::LocalGet(VP),
            Instruction::I64Const(1),
            Instruction::I64Sub,
            Instruction::LocalSet(VP),
            Instruction::End,
            Instruction::Else,
            Instruction::LocalGet(Q),
            Instruction::I32Const(63),
            Instruction::I32LtU,
            Instruction::If(BlockType::Empty),
            // vr_is_trailing_zeros = (mv & ((1 << q) - 1)) == 0
            Instruction::LocalGet(MV),
            Instruction::I64Const(1),
            Instruction::LocalGet(Q),
            Instruction::I64ExtendI32U,
            Instruction::I64Shl,
            Instruction::I64Const(1),
            Instruction::I64Sub,
            Instruction::I64And,
            Instruction::I64Eqz,
            Instruction::LocalSet(VRT),
            Instruction::End,
            Instruction::End,
            Instruction::End,
            // Step 4: shortest decimal in the interval (ryu d2s.rs:212-295).
            Instruction::LocalGet(VMT),
            Instruction::LocalGet(VRT),
            Instruction::I32Or,
            Instruction::If(BlockType::Empty),
            // ── rare path (~0.7%): track trailing zeros ──
            Instruction::Block(BlockType::Empty),
            Instruction::Loop(BlockType::Empty),
            Instruction::LocalGet(VP),
            Instruction::I64Const(10),
            Instruction::I64DivU,
            Instruction::LocalSet(VPD),
            Instruction::LocalGet(VM),
            Instruction::I64Const(10),
            Instruction::I64DivU,
            Instruction::LocalSet(VMD),
            Instruction::LocalGet(VPD),
            Instruction::LocalGet(VMD),
            Instruction::I64LeU,
            Instruction::BrIf(1),
            // vm_mod10 → TMP
            Instruction::LocalGet(VM),
            Instruction::LocalGet(VMD),
            Instruction::I64Const(10),
            Instruction::I64Mul,
            Instruction::I64Sub,
            Instruction::I32WrapI64,
            Instruction::LocalSet(TMP),
            Instruction::LocalGet(VR),
            Instruction::I64Const(10),
            Instruction::I64DivU,
            Instruction::LocalSet(VRD),
            // vm_trail &= vm_mod10 == 0; vr_trail &= last_removed == 0
            Instruction::LocalGet(VMT),
            Instruction::LocalGet(TMP),
            Instruction::I32Eqz,
            Instruction::I32And,
            Instruction::LocalSet(VMT),
            Instruction::LocalGet(VRT),
            Instruction::LocalGet(LRD),
            Instruction::I32Eqz,
            Instruction::I32And,
            Instruction::LocalSet(VRT),
            // last_removed = vr_mod10 (VR still pre-division here)
            Instruction::LocalGet(VR),
            Instruction::LocalGet(VRD),
            Instruction::I64Const(10),
            Instruction::I64Mul,
            Instruction::I64Sub,
            Instruction::I32WrapI64,
            Instruction::LocalSet(LRD),
            Instruction::LocalGet(VRD),
            Instruction::LocalSet(VR),
            Instruction::LocalGet(VPD),
            Instruction::LocalSet(VP),
            Instruction::LocalGet(VMD),
            Instruction::LocalSet(VM),
            Instruction::LocalGet(REM),
            Instruction::I32Const(1),
            Instruction::I32Add,
            Instruction::LocalSet(REM),
            Instruction::Br(0),
            Instruction::End,
            Instruction::End,
            // second loop: keep stripping while vm ends in 0
            Instruction::LocalGet(VMT),
            Instruction::If(BlockType::Empty),
            Instruction::Block(BlockType::Empty),
            Instruction::Loop(BlockType::Empty),
            Instruction::LocalGet(VM),
            Instruction::I64Const(10),
            Instruction::I64DivU,
            Instruction::LocalSet(VMD),
            // break when vm_mod10 != 0 (the mod value is the br_if condition)
            Instruction::LocalGet(VM),
            Instruction::LocalGet(VMD),
            Instruction::I64Const(10),
            Instruction::I64Mul,
            Instruction::I64Sub,
            Instruction::I32WrapI64,
            Instruction::BrIf(1),
            Instruction::LocalGet(VP),
            Instruction::I64Const(10),
            Instruction::I64DivU,
            Instruction::LocalSet(VPD),
            Instruction::LocalGet(VR),
            Instruction::I64Const(10),
            Instruction::I64DivU,
            Instruction::LocalSet(VRD),
            Instruction::LocalGet(VRT),
            Instruction::LocalGet(LRD),
            Instruction::I32Eqz,
            Instruction::I32And,
            Instruction::LocalSet(VRT),
            Instruction::LocalGet(VR),
            Instruction::LocalGet(VRD),
            Instruction::I64Const(10),
            Instruction::I64Mul,
            Instruction::I64Sub,
            Instruction::I32WrapI64,
            Instruction::LocalSet(LRD),
            Instruction::LocalGet(VRD),
            Instruction::LocalSet(VR),
            Instruction::LocalGet(VPD),
            Instruction::LocalSet(VP),
            Instruction::LocalGet(VMD),
            Instruction::LocalSet(VM),
            Instruction::LocalGet(REM),
            Instruction::I32Const(1),
            Instruction::I32Add,
            Instruction::LocalSet(REM),
            Instruction::Br(0),
            Instruction::End,
            Instruction::End,
            Instruction::End,
            // round even if the exact number is ….50…0
            Instruction::LocalGet(VRT),
            Instruction::LocalGet(LRD),
            Instruction::I32Const(5),
            Instruction::I32Eq,
            Instruction::I32And,
            Instruction::LocalGet(VR),
            Instruction::I64Const(1),
            Instruction::I64And,
            Instruction::I64Eqz,
            Instruction::I32And,
            Instruction::If(BlockType::Empty),
            Instruction::I32Const(4),
            Instruction::LocalSet(LRD),
            Instruction::End,
            // output = vr + (((vr == vm) && (!accept || !vm_trail)) || last_removed >= 5)
            Instruction::LocalGet(VR),
            Instruction::LocalGet(VR),
            Instruction::LocalGet(VM),
            Instruction::I64Eq,
            Instruction::LocalGet(ACC),
            Instruction::I32Eqz,
            Instruction::LocalGet(VMT),
            Instruction::I32Eqz,
            Instruction::I32Or,
            Instruction::I32And,
            Instruction::LocalGet(LRD),
            Instruction::I32Const(5),
            Instruction::I32GeU,
            Instruction::I32Or,
            Instruction::I64ExtendI32U,
            Instruction::I64Add,
            Instruction::LocalSet(VR),
            Instruction::Else,
            // ── common path (~99.3%); LRD doubles as round_up ──
            // optimization: remove two digits at a time
            Instruction::LocalGet(VP),
            Instruction::I64Const(100),
            Instruction::I64DivU,
            Instruction::LocalSet(VPD),
            Instruction::LocalGet(VM),
            Instruction::I64Const(100),
            Instruction::I64DivU,
            Instruction::LocalSet(VMD),
            Instruction::LocalGet(VPD),
            Instruction::LocalGet(VMD),
            Instruction::I64GtU,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(VR),
            Instruction::I64Const(100),
            Instruction::I64DivU,
            Instruction::LocalSet(VRD),
            Instruction::LocalGet(VR),
            Instruction::LocalGet(VRD),
            Instruction::I64Const(100),
            Instruction::I64Mul,
            Instruction::I64Sub,
            Instruction::I32WrapI64,
            Instruction::I32Const(50),
            Instruction::I32GeU,
            Instruction::LocalSet(LRD),
            Instruction::LocalGet(VRD),
            Instruction::LocalSet(VR),
            Instruction::LocalGet(VPD),
            Instruction::LocalSet(VP),
            Instruction::LocalGet(VMD),
            Instruction::LocalSet(VM),
            Instruction::LocalGet(REM),
            Instruction::I32Const(2),
            Instruction::I32Add,
            Instruction::LocalSet(REM),
            Instruction::End,
            Instruction::Block(BlockType::Empty),
            Instruction::Loop(BlockType::Empty),
            Instruction::LocalGet(VP),
            Instruction::I64Const(10),
            Instruction::I64DivU,
            Instruction::LocalSet(VPD),
            Instruction::LocalGet(VM),
            Instruction::I64Const(10),
            Instruction::I64DivU,
            Instruction::LocalSet(VMD),
            Instruction::LocalGet(VPD),
            Instruction::LocalGet(VMD),
            Instruction::I64LeU,
            Instruction::BrIf(1),
            Instruction::LocalGet(VR),
            Instruction::I64Const(10),
            Instruction::I64DivU,
            Instruction::LocalSet(VRD),
            Instruction::LocalGet(VR),
            Instruction::LocalGet(VRD),
            Instruction::I64Const(10),
            Instruction::I64Mul,
            Instruction::I64Sub,
            Instruction::I32WrapI64,
            Instruction::I32Const(5),
            Instruction::I32GeU,
            Instruction::LocalSet(LRD),
            Instruction::LocalGet(VRD),
            Instruction::LocalSet(VR),
            Instruction::LocalGet(VPD),
            Instruction::LocalSet(VP),
            Instruction::LocalGet(VMD),
            Instruction::LocalSet(VM),
            Instruction::LocalGet(REM),
            Instruction::I32Const(1),
            Instruction::I32Add,
            Instruction::LocalSet(REM),
            Instruction::Br(0),
            Instruction::End,
            Instruction::End,
            // output = vr + ((vr == vm) || round_up)
            Instruction::LocalGet(VR),
            Instruction::LocalGet(VR),
            Instruction::LocalGet(VM),
            Instruction::I64Eq,
            Instruction::LocalGet(LRD),
            Instruction::I32Or,
            Instruction::I64ExtendI32U,
            Instruction::I64Add,
            Instruction::LocalSet(VR),
            Instruction::End,
            // return (output, e10 + removed)
            Instruction::LocalGet(VR),
            Instruction::LocalGet(E10),
            Instruction::LocalGet(REM),
            Instruction::I32Add,
            Instruction::End,
        ],
    );
    b.add_and_emit_function(sig, &f)
}

/// Write a fixed byte sequence to the start of the f64 scratch
/// buffer (no iovec, no I/O — the formatter's contract is "bytes in
/// the buffer + a length", and the NaN / ±inf literals satisfy it the
/// same way the digit paths do). Byte-at-a-time stores: the literals
/// are ≤ 4 bytes, so coalescing would buy nothing.
fn write_literal(func: &mut Function, bytes: &[u8]) {
    for (i, &byte) in bytes.iter().enumerate() {
        func.instruction(&Instruction::I32Const(
            PRINT_F64_BUF_START as i32 + i as i32,
        ));
        func.instruction(&Instruction::I32Const(byte as i32));
        func.instruction(&Instruction::I32Store8(MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
    }
}

/// `phx_fmod(x: f64, y: f64) -> f64` — IEEE-754 truncated remainder
/// (sign of the dividend), matching Rust's `f64 % f64` (which lowers
/// to the platform `fmod`) bit-for-bit on every finite result. A port
/// of musl's `fmod` (`src/math/fmod.c`, MIT license) — like the Ryu
/// d2s port above, the algorithm is pure integer manipulation of the
/// f64 bit pattern (align exponents, repeated subtraction on the
/// mantissas, renormalize), so it is *exact*: the true remainder is
/// always representable and unique, and no rounding step exists to
/// diverge from native. NaN outcomes agree by class (every case below
/// yields NaN exactly when Rust `%` does) but their payload bits are
/// not pinned to any particular platform's.
///
/// Special cases (handled by musl's `(x*y)/(x*y)` NaN funnel and the
/// `|x| <= |y|` early-outs, all preserved here):
/// - `y == ±0`, `y` NaN, or `x` ±inf/NaN → NaN
/// - `|x| == |y|` → ±0 with x's sign;  `|x| < |y|` → x unchanged
/// - `x == ±0` → x (via the `|x| < |y|` early-out)
///
/// Synthesized when `HelperNeeds::fmod` is set (an `Op::FMod` site
/// exists). Unlike the print helpers it needs no `fd_write` import —
/// it is a pure function. See §Phase 2.4 decision K.5. musl's
/// copyright notice is preserved in `THIRD-PARTY-NOTICES.md` at the
/// repo root, as the MIT license requires for ports.
pub(super) fn synthesize_fmod(b: &mut ModuleBuilder) -> u32 {
    let sig = b.intern_signature(&[ValType::F64, ValType::F64], &[ValType::F64]);
    let mut f = Function::new([(4, ValType::I64), (2, ValType::I32)]);
    const X: u32 = 0; // f64 param (dividend)
    const Y: u32 = 1; // f64 param (divisor)
    const UXI: u32 = 2; // i64 — x bits, then the running mantissa
    const UYI: u32 = 3; // i64 — y bits, then y's aligned mantissa
    const I: u32 = 4; // i64 — subtraction scratch (musl's `i`)
    const SX: u32 = 5; // i64 — x's sign bit, isolated, re-OR'd at the end
    const EX: u32 = 6; // i32 — x's biased exponent (goes negative during alignment)
    const EY: u32 = 7; // i32 — y's biased exponent

    ins(
        &mut f,
        &[
            // Decompose both operands.
            Instruction::LocalGet(X),
            Instruction::I64ReinterpretF64,
            Instruction::LocalSet(UXI),
            Instruction::LocalGet(Y),
            Instruction::I64ReinterpretF64,
            Instruction::LocalSet(UYI),
            Instruction::LocalGet(UXI),
            Instruction::I64Const(52),
            Instruction::I64ShrU,
            Instruction::I32WrapI64,
            Instruction::I32Const(0x7FF),
            Instruction::I32And,
            Instruction::LocalSet(EX),
            Instruction::LocalGet(UYI),
            Instruction::I64Const(52),
            Instruction::I64ShrU,
            Instruction::I32WrapI64,
            Instruction::I32Const(0x7FF),
            Instruction::I32And,
            Instruction::LocalSet(EY),
            Instruction::LocalGet(UXI),
            Instruction::I64Const(i64::MIN),
            Instruction::I64And,
            Instruction::LocalSet(SX),
            // Special cases: y == ±0 || y is NaN || x is ±inf/NaN.
            // musl returns (x*y)/(x*y) — always NaN here (0/0, inf/inf,
            // or NaN propagation), matching Rust `%` for each case.
            Instruction::LocalGet(UYI),
            Instruction::I64Const(1),
            Instruction::I64Shl,
            Instruction::I64Eqz,
            Instruction::LocalGet(Y),
            Instruction::LocalGet(Y),
            Instruction::F64Ne,
            Instruction::I32Or,
            Instruction::LocalGet(EX),
            Instruction::I32Const(0x7FF),
            Instruction::I32Eq,
            Instruction::I32Or,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(X),
            Instruction::LocalGet(Y),
            Instruction::F64Mul,
            Instruction::LocalGet(X),
            Instruction::LocalGet(Y),
            Instruction::F64Mul,
            Instruction::F64Div,
            Instruction::Return,
            Instruction::End,
            // |x| <= |y| early-outs (compare bits with the sign shifted
            // out): equal magnitudes → ±0 with x's sign; smaller → x.
            Instruction::LocalGet(UXI),
            Instruction::I64Const(1),
            Instruction::I64Shl,
            Instruction::LocalGet(UYI),
            Instruction::I64Const(1),
            Instruction::I64Shl,
            Instruction::I64LeU,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(UXI),
            Instruction::I64Const(1),
            Instruction::I64Shl,
            Instruction::LocalGet(UYI),
            Instruction::I64Const(1),
            Instruction::I64Shl,
            Instruction::I64Eq,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(SX),
            Instruction::F64ReinterpretI64,
            Instruction::Return,
            Instruction::End,
            Instruction::LocalGet(X),
            Instruction::Return,
            Instruction::End,
        ],
    );
    emit_fmod_normalize(&mut f, UXI, EX, I);
    emit_fmod_normalize(&mut f, UYI, EY, I);
    ins(
        &mut f,
        &[
            // x mod y: align exponents by shifting x's mantissa left,
            // subtracting y's whenever it fits. An exact-zero
            // intermediate means y divides x → ±0 with x's sign.
            Instruction::Block(BlockType::Empty),
            Instruction::Loop(BlockType::Empty),
            Instruction::LocalGet(EX),
            Instruction::LocalGet(EY),
            Instruction::I32LeS,
            Instruction::BrIf(1),
            Instruction::LocalGet(UXI),
            Instruction::LocalGet(UYI),
            Instruction::I64Sub,
            Instruction::LocalTee(I),
            Instruction::I64Const(0),
            Instruction::I64GeS,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(I),
            Instruction::I64Eqz,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(SX),
            Instruction::F64ReinterpretI64,
            Instruction::Return,
            Instruction::End,
            Instruction::LocalGet(I),
            Instruction::LocalSet(UXI),
            Instruction::End,
            Instruction::LocalGet(UXI),
            Instruction::I64Const(1),
            Instruction::I64Shl,
            Instruction::LocalSet(UXI),
            Instruction::LocalGet(EX),
            Instruction::I32Const(1),
            Instruction::I32Sub,
            Instruction::LocalSet(EX),
            Instruction::Br(0),
            Instruction::End,
            Instruction::End,
            // Final aligned subtraction (ex == ey).
            Instruction::LocalGet(UXI),
            Instruction::LocalGet(UYI),
            Instruction::I64Sub,
            Instruction::LocalTee(I),
            Instruction::I64Const(0),
            Instruction::I64GeS,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(I),
            Instruction::I64Eqz,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(SX),
            Instruction::F64ReinterpretI64,
            Instruction::Return,
            Instruction::End,
            Instruction::LocalGet(I),
            Instruction::LocalSet(UXI),
            Instruction::End,
            // Renormalize the remainder's leading bit back to position 52.
            Instruction::Block(BlockType::Empty),
            Instruction::Loop(BlockType::Empty),
            Instruction::LocalGet(UXI),
            Instruction::I64Const(52),
            Instruction::I64ShrU,
            Instruction::I64Eqz,
            Instruction::I32Eqz, // (uxi >> 52) != 0 → done
            Instruction::BrIf(1),
            Instruction::LocalGet(UXI),
            Instruction::I64Const(1),
            Instruction::I64Shl,
            Instruction::LocalSet(UXI),
            Instruction::LocalGet(EX),
            Instruction::I32Const(1),
            Instruction::I32Sub,
            Instruction::LocalSet(EX),
            Instruction::Br(0),
            Instruction::End,
            Instruction::End,
            // Scale back: positive exponent re-encodes normally; ex <= 0
            // shifts down into the subnormal encoding.
            Instruction::LocalGet(EX),
            Instruction::I32Const(0),
            Instruction::I32GtS,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(UXI),
            Instruction::I64Const(HIDDEN_BIT),
            Instruction::I64Sub,
            Instruction::LocalGet(EX),
            Instruction::I64ExtendI32U,
            Instruction::I64Const(52),
            Instruction::I64Shl,
            Instruction::I64Or,
            Instruction::LocalSet(UXI),
            Instruction::Else,
            Instruction::LocalGet(UXI),
            Instruction::I32Const(1),
            Instruction::LocalGet(EX),
            Instruction::I32Sub,
            Instruction::I64ExtendI32U,
            Instruction::I64ShrU,
            Instruction::LocalSet(UXI),
            Instruction::End,
            // Reapply the dividend's sign.
            Instruction::LocalGet(UXI),
            Instruction::LocalGet(SX),
            Instruction::I64Or,
            Instruction::F64ReinterpretI64,
            Instruction::End,
        ],
    );
    b.add_and_emit_function(sig, &f)
}

/// Emit `phx_fmod`'s mantissa normalization for one operand (musl's
/// two identical "normalize x"/"normalize y" stanzas, parameterized by
/// local index): subnormals (`exp == 0`) shift their mantissa up until
/// the implicit-bit position is occupied — scanning a copy in
/// `scratch` and tracking the deficit in `exp`, which goes ≤ 0 — while
/// normals just mask in the hidden bit. On entry `bits` holds the raw
/// f64 bit pattern; on exit it holds the mantissa with its leading bit
/// at position 52 (the sign bit, already captured by the caller, is
/// shifted out along the way for subnormals).
fn emit_fmod_normalize(f: &mut Function, bits: u32, exp: u32, scratch: u32) {
    ins(
        f,
        &[
            Instruction::LocalGet(exp),
            Instruction::I32Eqz,
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(bits),
            Instruction::I64Const(12),
            Instruction::I64Shl,
            Instruction::LocalSet(scratch),
            Instruction::Block(BlockType::Empty),
            Instruction::Loop(BlockType::Empty),
            Instruction::LocalGet(scratch),
            Instruction::I64Const(0),
            Instruction::I64LtS, // top bit set → normalized scan done
            Instruction::BrIf(1),
            Instruction::LocalGet(exp),
            Instruction::I32Const(1),
            Instruction::I32Sub,
            Instruction::LocalSet(exp),
            Instruction::LocalGet(scratch),
            Instruction::I64Const(1),
            Instruction::I64Shl,
            Instruction::LocalSet(scratch),
            Instruction::Br(0),
            Instruction::End,
            Instruction::End,
            // bits <<= 1 - exp  (exp <= 0 here, so the amount is >= 1)
            Instruction::LocalGet(bits),
            Instruction::I32Const(1),
            Instruction::LocalGet(exp),
            Instruction::I32Sub,
            Instruction::I64ExtendI32U,
            Instruction::I64Shl,
            Instruction::LocalSet(bits),
            Instruction::Else,
            Instruction::LocalGet(bits),
            Instruction::I64Const(MANTISSA_MASK),
            Instruction::I64And,
            Instruction::I64Const(HIDDEN_BIT),
            Instruction::I64Or,
            Instruction::LocalSet(bits),
            Instruction::End,
        ],
    );
}
