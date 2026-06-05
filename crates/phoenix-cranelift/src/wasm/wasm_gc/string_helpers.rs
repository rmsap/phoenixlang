//! Synthesized WASM-GC string helpers for the wasm32-gc backend.
//!
//! The three functions here emit the raw `wasm-encoder` instruction
//! streams for the string runtime helpers that PR 6 slice 1 adds:
//! `phx_print_str`, `phx_str_concat`, and `phx_str_eq`. They are split
//! out of [`super::module_builder`] because each is ~100 lines of
//! instruction emission with no module-state bookkeeping of its own —
//! [`ModuleBuilder::declare_string_helpers`] stays the thin dispatcher
//! that decides *which* helpers to emit and records their indices, and
//! this module owns *how* each one is built.
//!
//! Each function declares its own signature via
//! [`ModuleBuilder::intern_signature`] and emits its body via
//! [`ModuleBuilder::add_and_emit_function`], returning the helper's WASM
//! function index. The `$bytes` / `$string` types must already be
//! declared (via `ModuleBuilder::declare_string_types`) — every helper
//! pulls their type-section indices through `require_*_type_idx`. See
//! §Phase 2.4 decision K.2 for the string representation.

use wasm_encoder::{BlockType, HeapType, Instruction, RefType, ValType};

use crate::error::CompileError;

use super::module_builder::{
    IOVEC_OFFSET, ModuleBuilder, PRINT_STR_BUF_START, PRINT_STR_MAX_LEN, emit_fd_write_call,
};

/// `phx_print_str(s: (ref null $string))` — copy `s.$data[s.$offset
/// .. s.$offset + s.$len]` into the linear-memory scratch buffer,
/// append `'\n'`, stage an iovec, and call `fd_write`. Traps
/// (`unreachable`) if the string's length exceeds
/// [`PRINT_STR_MAX_LEN`] — the scratch buffer is fixed-size and
/// the helper hard-rejects oversized strings rather than silently
/// corrupting memory.
///
/// Param: `s` (`(ref null $string)`, local 0).
/// Locals: `len` (i32, local 1), `offset` (i32, local 2),
/// `i` (i32, local 3), `data` (`(ref $bytes)`, local 4).
pub(super) fn synthesize_print_str(
    b: &mut ModuleBuilder,
    fd_write_idx: u32,
) -> Result<u32, CompileError> {
    let string_idx = b.require_string_type_idx()?;
    let bytes_idx = b.require_bytes_type_idx()?;
    let string_ref_param = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(string_idx),
    });
    let sig = b.intern_signature(&[string_ref_param], &[]);

    let bytes_ref_local = ValType::Ref(RefType {
        nullable: false,
        heap_type: HeapType::Concrete(bytes_idx),
    });
    // Locals beyond the parameter: len, offset, data, i. wasm-encoder
    // wants `(count, type)` runs; group by ValType to keep the local
    // section compact.
    let mut func = wasm_encoder::Function::new([
        (3, ValType::I32),    // len, offset, i
        (1, bytes_ref_local), // data
    ]);
    let i32_memarg = wasm_encoder::MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    };
    let byte_memarg = wasm_encoder::MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    };
    let s_local: u32 = 0;
    let len_local: u32 = 1;
    let offset_local: u32 = 2;
    let i_local: u32 = 3;
    let data_local: u32 = 4;

    // len = struct.get $string $len(2)
    func.instruction(&Instruction::LocalGet(s_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 2,
    });
    func.instruction(&Instruction::LocalSet(len_local));
    // Reject oversized strings up front — the scratch buffer is
    // fixed-size, and silently writing past it would corrupt the
    // iovec staging area (which sits BELOW the buffer in memory,
    // so an overflow would smash the iovec on the next call).
    // Trap is preferable to a silent corruption bug.
    func.instruction(&Instruction::LocalGet(len_local));
    func.instruction(&Instruction::I32Const(PRINT_STR_MAX_LEN as i32));
    func.instruction(&Instruction::I32GtU);
    func.instruction(&Instruction::If(BlockType::Empty));
    func.instruction(&Instruction::Unreachable);
    func.instruction(&Instruction::End);
    // offset = struct.get $string $offset(1)
    func.instruction(&Instruction::LocalGet(s_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 1,
    });
    func.instruction(&Instruction::LocalSet(offset_local));
    // data = struct.get $string $data(0)
    func.instruction(&Instruction::LocalGet(s_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 0,
    });
    func.instruction(&Instruction::LocalSet(data_local));
    // i = 0
    func.instruction(&Instruction::I32Const(0));
    func.instruction(&Instruction::LocalSet(i_local));
    // Copy loop: while i < len { mem[BUF + i] = data[offset + i]; i++ }
    func.instruction(&Instruction::Block(BlockType::Empty));
    func.instruction(&Instruction::Loop(BlockType::Empty));
    // if i >= len: br 1 (exit block)
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::LocalGet(len_local));
    func.instruction(&Instruction::I32GeU);
    func.instruction(&Instruction::BrIf(1));
    // mem[BUF + i] = array.get_u $bytes data (offset + i)
    // address: BUF_START + i
    func.instruction(&Instruction::I32Const(PRINT_STR_BUF_START as i32));
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::I32Add);
    // value: array.get_u $bytes data (offset + i)
    func.instruction(&Instruction::LocalGet(data_local));
    func.instruction(&Instruction::LocalGet(offset_local));
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::ArrayGetU(bytes_idx));
    // store
    func.instruction(&Instruction::I32Store8(byte_memarg));
    // i += 1
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::I32Const(1));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::LocalSet(i_local));
    func.instruction(&Instruction::Br(0));
    func.instruction(&Instruction::End); // close loop
    func.instruction(&Instruction::End); // close block
    // mem[BUF + len] = '\n'
    func.instruction(&Instruction::I32Const(PRINT_STR_BUF_START as i32));
    func.instruction(&Instruction::LocalGet(len_local));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::I32Const(b'\n' as i32));
    func.instruction(&Instruction::I32Store8(byte_memarg));
    // Stage iovec at IOVEC_OFFSET: (ptr = BUF_START, len = len + 1)
    func.instruction(&Instruction::I32Const(IOVEC_OFFSET as i32));
    func.instruction(&Instruction::I32Const(PRINT_STR_BUF_START as i32));
    func.instruction(&Instruction::I32Store(i32_memarg));
    func.instruction(&Instruction::I32Const(IOVEC_OFFSET as i32 + 4));
    func.instruction(&Instruction::LocalGet(len_local));
    func.instruction(&Instruction::I32Const(1));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::I32Store(i32_memarg));
    emit_fd_write_call(&mut func, fd_write_idx);
    func.instruction(&Instruction::End);
    Ok(b.add_and_emit_function(sig, &func))
}

/// `phx_str_concat(a: (ref null $string), b: (ref null $string)) ->
/// (ref $string)` — allocate a fresh `$bytes` of length `a.$len +
/// b.$len`, `array.copy` each operand's bytes (honoring its
/// `$offset`), then `struct.new $string` wrapping the new array
/// with `$offset = 0` and `$len = a.$len + b.$len`.
///
/// Two `array.copy` instructions handle the bulk byte movement —
/// the host VM is responsible for vectorizing them. Total cost:
/// one byte-array allocation, two bulk copies, one struct
/// allocation.
///
/// Params: `a`, `b` (locals 0, 1).
/// Locals: `len_a`, `len_b`, `total` (i32, locals 2, 3, 4),
/// `data` (`(ref $bytes)`, local 5).
pub(super) fn synthesize_str_concat(b: &mut ModuleBuilder) -> Result<u32, CompileError> {
    let string_idx = b.require_string_type_idx()?;
    let bytes_idx = b.require_bytes_type_idx()?;
    let string_ref = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(string_idx),
    });
    let string_ref_nn = ValType::Ref(RefType {
        nullable: false,
        heap_type: HeapType::Concrete(string_idx),
    });
    let sig = b.intern_signature(&[string_ref, string_ref], &[string_ref_nn]);

    let bytes_ref_local = ValType::Ref(RefType {
        nullable: false,
        heap_type: HeapType::Concrete(bytes_idx),
    });
    let mut func = wasm_encoder::Function::new([
        (3, ValType::I32),    // len_a, len_b, total
        (1, bytes_ref_local), // data
    ]);
    let a_local: u32 = 0;
    let b_local: u32 = 1;
    let len_a_local: u32 = 2;
    let len_b_local: u32 = 3;
    let total_local: u32 = 4;
    let data_local: u32 = 5;

    // len_a = a.$len
    func.instruction(&Instruction::LocalGet(a_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 2,
    });
    func.instruction(&Instruction::LocalSet(len_a_local));
    // len_b = b.$len
    func.instruction(&Instruction::LocalGet(b_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 2,
    });
    func.instruction(&Instruction::LocalSet(len_b_local));
    // total = len_a + len_b
    func.instruction(&Instruction::LocalGet(len_a_local));
    func.instruction(&Instruction::LocalGet(len_b_local));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::LocalSet(total_local));
    // data = array.new_default $bytes total
    func.instruction(&Instruction::LocalGet(total_local));
    func.instruction(&Instruction::ArrayNewDefault(bytes_idx));
    func.instruction(&Instruction::LocalSet(data_local));
    // array.copy $bytes $bytes  dest=data dest_off=0  src=a.$data src_off=a.$offset size=len_a
    func.instruction(&Instruction::LocalGet(data_local));
    func.instruction(&Instruction::I32Const(0));
    func.instruction(&Instruction::LocalGet(a_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 0,
    });
    func.instruction(&Instruction::LocalGet(a_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 1,
    });
    func.instruction(&Instruction::LocalGet(len_a_local));
    func.instruction(&Instruction::ArrayCopy {
        array_type_index_dst: bytes_idx,
        array_type_index_src: bytes_idx,
    });
    // array.copy $bytes $bytes  dest=data dest_off=len_a  src=b.$data src_off=b.$offset size=len_b
    func.instruction(&Instruction::LocalGet(data_local));
    func.instruction(&Instruction::LocalGet(len_a_local));
    func.instruction(&Instruction::LocalGet(b_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 0,
    });
    func.instruction(&Instruction::LocalGet(b_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 1,
    });
    func.instruction(&Instruction::LocalGet(len_b_local));
    func.instruction(&Instruction::ArrayCopy {
        array_type_index_dst: bytes_idx,
        array_type_index_src: bytes_idx,
    });
    // struct.new $string  (data, 0, total) — leaves the result on
    // the stack as the function's return value (no LocalSet + Return
    // pair needed since wasm-encoder allows multi-value flow into
    // the function-end implicit return).
    func.instruction(&Instruction::LocalGet(data_local));
    func.instruction(&Instruction::I32Const(0));
    func.instruction(&Instruction::LocalGet(total_local));
    func.instruction(&Instruction::StructNew(string_idx));
    func.instruction(&Instruction::End);
    Ok(b.add_and_emit_function(sig, &func))
}

/// `phx_str_eq(a: (ref null $string), b: (ref null $string)) -> i32`
/// — return `1` if both strings have equal length and byte
/// contents (offset-adjusted), `0` otherwise.
///
/// Fast path: if `a.$len != b.$len`, return `0` immediately.
/// Otherwise loop byte-by-byte. Pre-loop offset capture keeps the
/// hot path to two `LocalGet`s + an `ArrayGetU` + an `i32.ne` +
/// `BrIf` per byte.
///
/// Params: `a`, `b` (locals 0, 1).
/// Locals: `len`, `off_a`, `off_b`, `i`, `byte_a` (i32, locals
/// 2-6), `data_a`, `data_b` (`(ref $bytes)`, locals 7, 8).
pub(super) fn synthesize_str_eq(b: &mut ModuleBuilder) -> Result<u32, CompileError> {
    let string_idx = b.require_string_type_idx()?;
    let bytes_idx = b.require_bytes_type_idx()?;
    let string_ref = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(string_idx),
    });
    let sig = b.intern_signature(&[string_ref, string_ref], &[ValType::I32]);

    let bytes_ref_local = ValType::Ref(RefType {
        nullable: false,
        heap_type: HeapType::Concrete(bytes_idx),
    });
    let mut func = wasm_encoder::Function::new([
        (5, ValType::I32),    // len, off_a, off_b, i, byte_a
        (2, bytes_ref_local), // data_a, data_b
    ]);
    let a_local: u32 = 0;
    let b_local: u32 = 1;
    let len_local: u32 = 2;
    let off_a_local: u32 = 3;
    let off_b_local: u32 = 4;
    let i_local: u32 = 5;
    let byte_a_local: u32 = 6;
    let data_a_local: u32 = 7;
    let data_b_local: u32 = 8;

    // if a.$len != b.$len: return 0
    func.instruction(&Instruction::LocalGet(a_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 2,
    });
    func.instruction(&Instruction::LocalTee(len_local));
    func.instruction(&Instruction::LocalGet(b_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 2,
    });
    func.instruction(&Instruction::I32Ne);
    func.instruction(&Instruction::If(BlockType::Empty));
    func.instruction(&Instruction::I32Const(0));
    func.instruction(&Instruction::Return);
    func.instruction(&Instruction::End);
    // Capture offsets and data refs once before the loop.
    func.instruction(&Instruction::LocalGet(a_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 1,
    });
    func.instruction(&Instruction::LocalSet(off_a_local));
    func.instruction(&Instruction::LocalGet(b_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 1,
    });
    func.instruction(&Instruction::LocalSet(off_b_local));
    func.instruction(&Instruction::LocalGet(a_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 0,
    });
    func.instruction(&Instruction::LocalSet(data_a_local));
    func.instruction(&Instruction::LocalGet(b_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: 0,
    });
    func.instruction(&Instruction::LocalSet(data_b_local));
    // i = 0
    func.instruction(&Instruction::I32Const(0));
    func.instruction(&Instruction::LocalSet(i_local));
    // Compare loop.
    func.instruction(&Instruction::Block(BlockType::Empty));
    func.instruction(&Instruction::Loop(BlockType::Empty));
    // if i >= len: exit loop with equal-so-far
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::LocalGet(len_local));
    func.instruction(&Instruction::I32GeU);
    func.instruction(&Instruction::BrIf(1));
    // byte_a = data_a[off_a + i]
    func.instruction(&Instruction::LocalGet(data_a_local));
    func.instruction(&Instruction::LocalGet(off_a_local));
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::ArrayGetU(bytes_idx));
    func.instruction(&Instruction::LocalSet(byte_a_local));
    // byte_b = data_b[off_b + i]
    func.instruction(&Instruction::LocalGet(data_b_local));
    func.instruction(&Instruction::LocalGet(off_b_local));
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::ArrayGetU(bytes_idx));
    // if byte_a != byte_b: return 0
    func.instruction(&Instruction::LocalGet(byte_a_local));
    func.instruction(&Instruction::I32Ne);
    func.instruction(&Instruction::If(BlockType::Empty));
    func.instruction(&Instruction::I32Const(0));
    func.instruction(&Instruction::Return);
    func.instruction(&Instruction::End);
    // i += 1
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::I32Const(1));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::LocalSet(i_local));
    func.instruction(&Instruction::Br(0));
    func.instruction(&Instruction::End); // close loop
    func.instruction(&Instruction::End); // close block
    // All bytes matched.
    func.instruction(&Instruction::I32Const(1));
    func.instruction(&Instruction::End);
    Ok(b.add_and_emit_function(sig, &func))
}
