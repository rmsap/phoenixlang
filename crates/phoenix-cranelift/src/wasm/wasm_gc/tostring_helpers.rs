//! Synthesized `toString` constructors for the wasm32-gc backend.
//!
//! The `toString` builtin returns a fresh Phoenix `String` whose bytes
//! match the other backends exactly: `toString(Int)` is Rust
//! `i64::to_string` (decimal), `toString(Float)` is
//! `phoenix_runtime::format_f64` (the ryu port — reused via
//! `phx_ryu_format_f64`), `toString(Bool)` is `"true"` / `"false"`,
//! and `toString(String)` is the identity (lowered as a plain local
//! copy at the dispatch site, no helper). String interpolation lowers
//! every non-String hole through `toString`, so these helpers are what
//! make `"x = {n}"` work on wasm32-gc.
//!
//! Unlike the print helpers there is no `fd_write` dependency —
//! `toString` is pure construction: digits (or formatter output, or a
//! literal) into a fresh `$bytes` array, wrapped in a `$string`
//! struct. A `toString`-only module carries no WASI import.

use wasm_encoder::{BlockType, Function, HeapType, Instruction, MemArg, RefType, ValType};

use crate::error::CompileError;

use super::module_builder::{ModuleBuilder, PRINT_F64_BUF_START};

/// Emit a batch of instructions onto `func` (same convention as
/// `float_helpers::ins`).
fn ins(func: &mut Function, list: &[Instruction]) {
    for i in list {
        func.instruction(i);
    }
}

/// The nullable `$string` ref ValType for helper signatures.
fn string_ref(b: &mut ModuleBuilder) -> Result<ValType, CompileError> {
    Ok(ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(b.require_string_type_idx()?),
    }))
}

/// `phx_tostring_i64(n: i64) -> (ref null $string)` — decimal text of
/// `n`, matching Rust's `i64::to_string` byte-for-byte for all values
/// the digit loop handles. Digits are written right-to-left straight
/// into the fresh `$bytes` array (no linear-memory staging): count the
/// digits first, size the array exactly, then fill.
///
/// KNOWN GAP (shared with `phx_print_i64` — grep `i64::MIN`): the
/// unary negation `0 - n` wraps for `i64::MIN`, so its digits come out
/// garbled. Same accepted MVP divergence, same fix point when it's
/// closed.
pub(super) fn synthesize_tostring_i64(b: &mut ModuleBuilder) -> Result<u32, CompileError> {
    let bytes_idx = b.require_bytes_type_idx()?;
    let string_idx = b.require_string_type_idx()?;
    let ret = string_ref(b)?;
    let sig = b.intern_signature(&[ValType::I64], &[ret]);

    // Locals: param n=0 (i64); m=1 (i64 digit scratch); i32 group:
    // neg=2, len=3, i=4; ref group: arr=5.
    // Non-nullable local (the string-helpers house pattern): the array
    // is definitely assigned before every read, and `$string`'s `$data`
    // field is `(ref $bytes)`, so no null-cast is ever needed.
    let arr_ref_ty = ValType::Ref(RefType {
        nullable: false,
        heap_type: HeapType::Concrete(bytes_idx),
    });
    let mut f = Function::new([(1, ValType::I64), (3, ValType::I32), (1, arr_ref_ty)]);
    const N: u32 = 0;
    const M: u32 = 1;
    const NEG: u32 = 2;
    const LEN: u32 = 3;
    const I: u32 = 4;
    const ARR: u32 = 5;

    ins(
        &mut f,
        &[
            // neg = n < 0; m = abs(n)  (wraps on i64::MIN — see doc)
            Instruction::LocalGet(N),
            Instruction::I64Const(0),
            Instruction::I64LtS,
            Instruction::LocalSet(NEG),
            Instruction::LocalGet(NEG),
            Instruction::If(BlockType::Empty),
            Instruction::I64Const(0),
            Instruction::LocalGet(N),
            Instruction::I64Sub,
            Instruction::LocalSet(M),
            Instruction::Else,
            Instruction::LocalGet(N),
            Instruction::LocalSet(M),
            Instruction::End,
            // len = neg + digit count of m. The count loop divides M
            // down to zero (destroying it); M is re-derived from N for
            // the fill pass below — one i64 scratch instead of two.
            Instruction::LocalGet(NEG),
            Instruction::LocalSet(LEN),
            Instruction::Block(BlockType::Empty),
            Instruction::Loop(BlockType::Empty),
            Instruction::LocalGet(LEN),
            Instruction::I32Const(1),
            Instruction::I32Add,
            Instruction::LocalSet(LEN),
            Instruction::LocalGet(M),
            Instruction::I64Const(10),
            Instruction::I64DivU,
            Instruction::LocalTee(M),
            Instruction::I64Eqz,
            Instruction::BrIf(1),
            Instruction::Br(0),
            Instruction::End,
            Instruction::End,
            // m = abs(n) again for the fill pass
            Instruction::LocalGet(NEG),
            Instruction::If(BlockType::Empty),
            Instruction::I64Const(0),
            Instruction::LocalGet(N),
            Instruction::I64Sub,
            Instruction::LocalSet(M),
            Instruction::Else,
            Instruction::LocalGet(N),
            Instruction::LocalSet(M),
            Instruction::End,
            // arr = array.new_default $bytes (len)
            Instruction::LocalGet(LEN),
            Instruction::ArrayNewDefault(bytes_idx),
            Instruction::LocalSet(ARR),
            // fill right-to-left: i = len; do { i -= 1; arr[i] = '0' + m % 10; m /= 10 } while m != 0
            Instruction::LocalGet(LEN),
            Instruction::LocalSet(I),
            Instruction::Loop(BlockType::Empty),
            Instruction::LocalGet(I),
            Instruction::I32Const(1),
            Instruction::I32Sub,
            Instruction::LocalSet(I),
            Instruction::LocalGet(ARR),
            Instruction::LocalGet(I),
            Instruction::LocalGet(M),
            Instruction::I64Const(10),
            Instruction::I64RemU,
            Instruction::I32WrapI64,
            Instruction::I32Const(b'0' as i32),
            Instruction::I32Add,
            Instruction::ArraySet(bytes_idx),
            Instruction::LocalGet(M),
            Instruction::I64Const(10),
            Instruction::I64DivU,
            Instruction::LocalTee(M),
            Instruction::I64Const(0),
            Instruction::I64Ne,
            Instruction::BrIf(0),
            Instruction::End,
            // '-' sign at index 0
            Instruction::LocalGet(NEG),
            Instruction::If(BlockType::Empty),
            Instruction::LocalGet(ARR),
            Instruction::I32Const(0),
            Instruction::I32Const(b'-' as i32),
            Instruction::ArraySet(bytes_idx),
            Instruction::End,
            // struct.new $string (arr, offset = 0, len)
            Instruction::LocalGet(ARR),
            Instruction::I32Const(0),
            Instruction::LocalGet(LEN),
            Instruction::StructNew(string_idx),
            Instruction::End,
        ],
    );
    Ok(b.add_and_emit_function(sig, &f))
}

/// `phx_tostring_bool(b: i32) -> (ref null $string)` — `"true"` /
/// `"false"` from two passive data segments via `array.new_data`,
/// exactly the `Op::ConstString` materialization pattern (these are
/// just two compiler-supplied literals without newlines — distinct
/// from `print(Bool)`'s active `"true\n"` / `"false\n"` segments).
pub(super) fn synthesize_tostring_bool(b: &mut ModuleBuilder) -> Result<u32, CompileError> {
    let bytes_idx = b.require_bytes_type_idx()?;
    let string_idx = b.require_string_type_idx()?;
    let true_seg = b.reserve_string_data(b"true");
    let false_seg = b.reserve_string_data(b"false");
    let ret = string_ref(b)?;
    let sig = b.intern_signature(&[ValType::I32], &[ret]);

    let mut f = Function::new([]);
    const COND: u32 = 0;
    ins(
        &mut f,
        &[
            Instruction::LocalGet(COND),
            Instruction::If(BlockType::FunctionType(sig_result_only(b, string_idx))),
            Instruction::I32Const(0),
            Instruction::I32Const(4),
            Instruction::ArrayNewData {
                array_type_index: bytes_idx,
                array_data_index: true_seg,
            },
            Instruction::I32Const(0),
            Instruction::I32Const(4),
            Instruction::StructNew(string_idx),
            Instruction::Else,
            Instruction::I32Const(0),
            Instruction::I32Const(5),
            Instruction::ArrayNewData {
                array_type_index: bytes_idx,
                array_data_index: false_seg,
            },
            Instruction::I32Const(0),
            Instruction::I32Const(5),
            Instruction::StructNew(string_idx),
            Instruction::End,
            Instruction::End,
        ],
    );
    Ok(b.add_and_emit_function(sig, &f))
}

/// Intern a `[] -> [(ref null $string)]` block signature for the
/// bool helper's value-producing `if`.
fn sig_result_only(b: &mut ModuleBuilder, string_idx: u32) -> u32 {
    b.intern_signature(
        &[],
        &[ValType::Ref(RefType {
            nullable: true,
            heap_type: HeapType::Concrete(string_idx),
        })],
    )
}

/// `phx_tostring_f64(v: f64) -> (ref null $string)` — call
/// `phx_ryu_format_f64` (text into the f64 scratch buffer, length
/// returned), then copy the bytes into a fresh `$bytes` array and wrap
/// it. The copy is a per-byte loop: WASM-GC has no
/// linear-memory → managed-array bulk instruction, and the text is
/// ≤ 24 bytes.
pub(super) fn synthesize_tostring_f64(
    b: &mut ModuleBuilder,
    format_f64_idx: u32,
) -> Result<u32, CompileError> {
    let bytes_idx = b.require_bytes_type_idx()?;
    let string_idx = b.require_string_type_idx()?;
    let ret = string_ref(b)?;
    let sig = b.intern_signature(&[ValType::F64], &[ret]);

    // Non-nullable local (the string-helpers house pattern): the array
    // is definitely assigned before every read, and `$string`'s `$data`
    // field is `(ref $bytes)`, so no null-cast is ever needed.
    let arr_ref_ty = ValType::Ref(RefType {
        nullable: false,
        heap_type: HeapType::Concrete(bytes_idx),
    });
    let mut f = Function::new([(2, ValType::I32), (1, arr_ref_ty)]);
    const V: u32 = 0;
    const LEN: u32 = 1;
    const I: u32 = 2;
    const ARR: u32 = 3;
    ins(
        &mut f,
        &[
            Instruction::LocalGet(V),
            Instruction::Call(format_f64_idx),
            Instruction::LocalSet(LEN),
            Instruction::LocalGet(LEN),
            Instruction::ArrayNewDefault(bytes_idx),
            Instruction::LocalSet(ARR),
            // for i in 0..len: arr[i] = mem[START + i]
            Instruction::I32Const(0),
            Instruction::LocalSet(I),
            Instruction::Block(BlockType::Empty),
            Instruction::Loop(BlockType::Empty),
            Instruction::LocalGet(I),
            Instruction::LocalGet(LEN),
            Instruction::I32GeS,
            Instruction::BrIf(1),
            Instruction::LocalGet(ARR),
            Instruction::LocalGet(I),
            Instruction::LocalGet(I),
            Instruction::I32Load8U(MemArg {
                offset: PRINT_F64_BUF_START as u64,
                align: 0,
                memory_index: 0,
            }),
            Instruction::ArraySet(bytes_idx),
            Instruction::LocalGet(I),
            Instruction::I32Const(1),
            Instruction::I32Add,
            Instruction::LocalSet(I),
            Instruction::Br(0),
            Instruction::End,
            Instruction::End,
            // struct.new $string (arr, 0, len)
            Instruction::LocalGet(ARR),
            Instruction::I32Const(0),
            Instruction::LocalGet(LEN),
            Instruction::StructNew(string_idx),
            Instruction::End,
        ],
    );
    Ok(b.add_and_emit_function(sig, &f))
}
