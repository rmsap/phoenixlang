//! Synthesized per-module runtime helpers for the WebAssembly backend.
//!
//! Each helper is a small WASM function written in instruction form
//! that bottoms out in `wasi_snapshot_preview1.fd_write`. The
//! synthesizer runs once per `phoenix build --target wasm32-linear`
//! invocation; the resulting bodies are inlined into the emitted
//! `.wasm` module rather than linked from a separate object.
//!
//! Why inline rather than link? WebAssembly has no "static library"
//! step in the C-toolchain sense — every function in the final module
//! either lives in the module's own code section or is imported from
//! the host. Phoenix's native runtime (`libphoenix_runtime.a`) does
//! not exist as a `.wasm` object; rewriting `phoenix-runtime` to
//! compile-to-wasm32 is PR 3's scope (so the GC can come along for
//! the ride). PR 2 keeps the runtime surface tiny — just enough for
//! `print` of an `i64` — and synthesizes it inline so PR 2 does not
//! depend on PR 3's runtime port landing first.
//!
//! Memory layout used by the helpers (single page, see `mod.rs`
//! `SCRATCH_BASE`):
//!
//! | Offset                 | Contents                              |
//! |------------------------|---------------------------------------|
//! | `SCRATCH_BASE +  0`    | iovec.buf_ptr (i32)                   |
//! | `SCRATCH_BASE +  4`    | iovec.buf_len (i32)                   |
//! | `SCRATCH_BASE +  8`    | nwritten cell (i32, written by WASI)  |
//! | `SCRATCH_BASE + 16`    | itoa buffer (32 bytes)                |
//!
//! The itoa buffer holds an i64's decimal representation. The
//! widest case is `i64::MIN` printed via its two's-complement
//! unsigned magnitude (`0x8000000000000000` = 9223372036854775808),
//! which is 19 decimal digits, plus an optional `-` and a trailing
//! `\n` — 21 bytes worst case. Round up to 32 for natural alignment
//! of the next page.

use wasm_encoder::{Function, Instruction, MemArg, ValType};

use super::SCRATCH_BASE;
use super::module_builder::ModuleBuilder;

/// Alignment exponent for the i32 stores/loads used by the helpers
/// (WASM's `MemArg.align` is `log2(byte_alignment)`). `2` → 4-byte
/// alignment, which matches every scratch field's natural alignment.
const I32_ALIGN_LOG2: u32 = 2;

/// Byte offset within scratch of the iovec base-pointer field.
const IOVEC_BUF_PTR: u32 = 0;
/// Byte offset within scratch of the iovec length field.
const IOVEC_BUF_LEN: u32 = 4;
/// Byte offset within scratch of the WASI nwritten cell.
const NWRITTEN_OFFSET: u32 = 8;
/// Byte offset within scratch of the start of the itoa buffer.
const ITOA_BUF_OFFSET: u32 = 16;
/// Capacity (in bytes) of the itoa buffer. Sized for i64 worst case
/// (19 digits via the `i64::MIN` unsigned-magnitude path) + sign +
/// newline = 21 bytes; rounded up to 32 for natural alignment.
const ITOA_BUF_LEN: u32 = 32;

/// Process exit code surfaced when a `phx_print_*` call's underlying
/// `fd_write` returns a non-zero WASI errno (closed stdout, full
/// pipe, etc.). A fixed code rather than the raw errno keeps the
/// process-exit semantics conventional — WASI errnos occupy 0–76
/// and overlap the shell's general-purpose code range in confusing
/// ways. `1` matches "general failure" by convention.
const PRINT_FAILED_EXIT_CODE: i32 = 1;

/// Build a `MemArg` for an aligned i32 access at the given absolute
/// offset within the WASM default memory.
fn i32_at(offset: u32) -> MemArg {
    MemArg {
        offset: offset as u64,
        align: I32_ALIGN_LOG2,
        memory_index: 0,
    }
}

/// Build a `MemArg` for a 1-byte access at the given absolute offset.
/// Used by the itoa loop for digit-by-digit writes.
fn i8_at(offset: u32) -> MemArg {
    MemArg {
        offset: offset as u64,
        align: 0,
        memory_index: 0,
    }
}

/// Absolute address of a scratch-region field, as the i32 constant
/// to push onto the operand stack. Centralizes the `SCRATCH_BASE +
/// field_offset` arithmetic so call sites read one level closer to
/// the offset table at the top of this module.
fn scratch_addr(field_offset: u32) -> i32 {
    (SCRATCH_BASE + field_offset) as i32
}

/// Emit `phx_print_str(ptr: i32, len: i32)` — the leaf helper that
/// every other `phx_print_*` bottoms out in.
///
/// Body: stage `(ptr, len)` as a single-element iovec at scratch,
/// call `fd_write(fd=1, iovs=scratch, iovs_len=1, nwritten=...)`,
/// and on a non-zero WASI errno return, terminate the module via
/// `proc_exit(PRINT_FAILED_EXIT_CODE)`. The fixed exit code (rather
/// than the raw errno) keeps the failure visible — runtime IO
/// failures (closed stdout, full pipe) crash the process instead of
/// silently corrupting program output — while keeping the exit-code
/// convention conventional. The user-facing `print` in Phoenix has
/// no return value to thread the errno through.
///
/// **Coverage gaps (intentional, recorded here so the next reviewer
/// doesn't assume otherwise):**
///
/// 1. The `errno != 0` branch has no integration test. Driving
///    `fd_write` to a non-zero return from a hermetic test would
///    require an environment where stdout is closed at the WASI host
///    layer; PR 2 doesn't ship that harness, and PR 3 has no easier
///    shape either. The branch is small and audited by inspection.
/// 2. Partial-write handling is also absent: a successful `fd_write`
///    that returns `nwritten < buf_len` (the iovec was only partially
///    consumed by the host) would silently truncate the output. Real
///    WASI hosts (`wasmtime`, browsers) don't short-write to stdout
///    in practice, but a correct implementation would loop until
///    `nwritten` reaches `buf_len` or an `errno` fires. PR 3+ should
///    add the loop when the runtime port grows real IO surface; PR 2
///    deliberately keeps the helper one straight-line block.
pub(super) fn emit_print_str(b: &mut ModuleBuilder) {
    // No extra locals beyond the two i32 parameters (ptr, len). The
    // errno value lives on the operand stack only as long as it
    // takes to compare it to 0 — failure path uses a fixed exit
    // code rather than the errno.
    let mut f = Function::new([]);

    // *(scratch + IOVEC_BUF_PTR) = ptr  (param 0)
    //   Push the *exact* target address for each store (rather than
    //   sharing one base + relying on `MemArg.offset` to pick the
    //   field). Symmetric per-store addressing reads more like a
    //   struct-field assignment and avoids the "why does the second
    //   store use the same base?" head-scratch.
    f.instruction(&Instruction::I32Const(scratch_addr(IOVEC_BUF_PTR)));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Store(i32_at(0)));

    // *(scratch + IOVEC_BUF_LEN) = len  (param 1)
    f.instruction(&Instruction::I32Const(scratch_addr(IOVEC_BUF_LEN)));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Store(i32_at(0)));

    // errno = fd_write(fd=1, iovs=scratch, iovs_len=1, nwritten=scratch+NWRITTEN_OFFSET)
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Const(scratch_addr(IOVEC_BUF_PTR)));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Const(scratch_addr(NWRITTEN_OFFSET)));
    f.instruction(&Instruction::Call(b.fd_write_idx()));

    // if errno != 0 { proc_exit(PRINT_FAILED_EXIT_CODE) }
    //   `proc_exit` is noreturn at the WASI semantic level (its WASM
    //   signature is `(i32) -> ()`, but control never returns past
    //   the call), so the `then` arm needs no explicit branch out.
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(PRINT_FAILED_EXIT_CODE));
    f.instruction(&Instruction::Call(b.proc_exit_idx()));
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::End);
    b.push_runtime_body(&f);
}

/// Emit `phx_print_i64(value: i64)` — converts to decimal ASCII and
/// hands the result to `phx_print_str`.
///
/// Algorithm: fill the itoa buffer from the right (high addresses to
/// low) so the digit order comes out correct without an explicit
/// reverse. Sign handled separately after the magnitude loop.
///
/// Local map:
///
/// | Index | Type | Role                                       |
/// |-------|------|--------------------------------------------|
/// | 0     | i64  | function parameter `value`                 |
/// | 1     | i64  | working magnitude (positive)               |
/// | 2     | i32  | write cursor (offset into itoa buffer)     |
/// | 3     | i32  | `is_negative` flag (0/1)                   |
pub(super) fn emit_print_i64(b: &mut ModuleBuilder) {
    let mut f = Function::new([
        (1, ValType::I64), // magnitude
        (1, ValType::I32), // cursor
        (1, ValType::I32), // is_negative
    ]);

    let buf_end = scratch_addr(ITOA_BUF_OFFSET + ITOA_BUF_LEN);

    // mag = value  (unconditional initialization)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::LocalSet(1));

    // is_negative = (value < 0) ? 1 : 0
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(0));
    f.instruction(&Instruction::I64LtS);
    f.instruction(&Instruction::LocalSet(3));

    // if is_negative { mag = 0 - mag }
    //   We use `0 - mag` rather than `i64.neg` because WASM's MVP does
    //   not have a dedicated i64.neg instruction. For `value == i64::MIN`,
    //   `0 - mag` overflows in two's-complement and wraps back to
    //   `i64::MIN` (bit pattern 0x8000000000000000). That's fine here:
    //   the digit loop below uses `I64DivU`/`I64RemU` (unsigned), which
    //   read that bit pattern as 2^63 and yield the correct decimal
    //   digits — so `i64::MIN` prints as `-9223372036854775808` without
    //   a special case.
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I64Const(0));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I64Sub);
    f.instruction(&Instruction::LocalSet(1));
    f.instruction(&Instruction::End);

    // cursor = buf_end - 1; store '\n'
    //   `LocalTee` sets the local *and* leaves the value on the stack
    //   for the immediate `I32Store8` address — saves a `LocalGet`
    //   relative to a plain `LocalSet` + reload.
    f.instruction(&Instruction::I32Const(buf_end - 1));
    f.instruction(&Instruction::LocalTee(2));
    f.instruction(&Instruction::I32Const(b'\n' as i32));
    f.instruction(&Instruction::I32Store8(i8_at(0)));

    // do { cursor -= 1; *cursor = '0' + (mag % 10); mag /= 10 } while mag != 0
    //   Encoded as a `loop` because we want do-while semantics: the
    //   `0` case still needs to emit the '0' digit once.
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

    // cursor -= 1; (`LocalTee` keeps the new cursor on the stack as
    // the store address for the next block — same shape as the '\n'
    // and '-' stores)
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalTee(2));

    // digit = (mag % 10) as i32; *cursor = '0' + digit
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I64Const(10));
    f.instruction(&Instruction::I64RemU);
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::I32Const(b'0' as i32));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Store8(i8_at(0)));

    // mag /= 10
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I64Const(10));
    f.instruction(&Instruction::I64DivU);
    f.instruction(&Instruction::LocalTee(1));

    // continue while mag != 0
    //   Inside a `loop`, `br_if 0` branches *back to the loop header*
    //   (i.e. continues the loop). This differs from `block`, where
    //   `br_if 0` would branch *out* of the block. The do-while shape
    //   here relies on the `loop` semantics — switching to `block`
    //   would invert the control flow.
    f.instruction(&Instruction::I64Const(0));
    f.instruction(&Instruction::I64Ne);
    f.instruction(&Instruction::BrIf(0));
    f.instruction(&Instruction::End);

    // if is_negative { cursor -= 1; *cursor = '-' }
    //   Same `LocalTee` trick as the '\n' store: drop the cursor into
    //   the local and reuse it as the store address in one step.
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalTee(2));
    f.instruction(&Instruction::I32Const(b'-' as i32));
    f.instruction(&Instruction::I32Store8(i8_at(0)));
    f.instruction(&Instruction::End);

    // phx_print_str(cursor, buf_end - cursor)
    let print_str_idx = b
        .print_str_idx
        .expect("phx_print_str must be declared whenever phx_print_i64 is");
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(buf_end));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::Call(print_str_idx));

    f.instruction(&Instruction::End);
    b.push_runtime_body(&f);
}

/// Emit `phx_print_bool(value: i32)` — writes the literal `true` or
/// `false` (with trailing newline) via `phx_print_str`. The literal
/// bytes are appended to the data section here so they're only
/// present when the module actually contains a `print(bool)` call.
pub(super) fn emit_print_bool(b: &mut ModuleBuilder) {
    let (true_off, true_len) = b.reserve_data(b"true\n");
    let (false_off, false_len) = b.reserve_data(b"false\n");
    let print_str_idx = b
        .print_str_idx
        .expect("phx_print_str must be declared whenever phx_print_bool is");

    let mut f = Function::new([]);
    // if value { print_str(true_lit, 5) } else { print_str(false_lit, 6) }
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(true_off as i32));
    f.instruction(&Instruction::I32Const(true_len as i32));
    f.instruction(&Instruction::Call(print_str_idx));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::I32Const(false_off as i32));
    f.instruction(&Instruction::I32Const(false_len as i32));
    f.instruction(&Instruction::Call(print_str_idx));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    b.push_runtime_body(&f);
}
