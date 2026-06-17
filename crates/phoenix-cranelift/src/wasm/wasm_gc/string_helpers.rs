//! Synthesized WASM-GC string helpers for the wasm32-gc backend.
//!
//! Each function here emits a raw `wasm-encoder` instruction stream
//! for one string runtime helper. PR 6 slice 1 added `phx_print_str`,
//! `phx_str_concat`, and `phx_str_eq`; PR 6 slice 2 adds
//! `phx_str_cmp` (lexicographic byte compare, fans out to
//! `Op::StringLt` / `Le` / `Gt` / `Ge`) and `phx_str_substring`
//! (char-boundary walk + clamping + view `struct.new`). They are
//! split out of [`super::module_builder`] because each is ~100 lines
//! of instruction emission with no module-state bookkeeping of its
//! own — [`ModuleBuilder::declare_string_helpers`] stays the thin
//! dispatcher that decides *which* helpers to emit and records their
//! indices, and this module owns *how* each one is built.
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
    IOVEC_OFFSET, ModuleBuilder, PRINT_STR_BUF_START, PRINT_STR_MAX_LEN, STR_DATA, STR_LEN,
    STR_OFFSET, emit_fd_write_call,
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
        field_index: STR_LEN,
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
        field_index: STR_OFFSET,
    });
    func.instruction(&Instruction::LocalSet(offset_local));
    // data = struct.get $string $data(0)
    func.instruction(&Instruction::LocalGet(s_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_DATA,
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
        field_index: STR_LEN,
    });
    func.instruction(&Instruction::LocalSet(len_a_local));
    // len_b = b.$len
    func.instruction(&Instruction::LocalGet(b_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_LEN,
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
        field_index: STR_DATA,
    });
    func.instruction(&Instruction::LocalGet(a_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_OFFSET,
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
        field_index: STR_DATA,
    });
    func.instruction(&Instruction::LocalGet(b_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_OFFSET,
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
        field_index: STR_LEN,
    });
    func.instruction(&Instruction::LocalTee(len_local));
    func.instruction(&Instruction::LocalGet(b_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_LEN,
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
        field_index: STR_OFFSET,
    });
    func.instruction(&Instruction::LocalSet(off_a_local));
    func.instruction(&Instruction::LocalGet(b_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_OFFSET,
    });
    func.instruction(&Instruction::LocalSet(off_b_local));
    func.instruction(&Instruction::LocalGet(a_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_DATA,
    });
    func.instruction(&Instruction::LocalSet(data_a_local));
    func.instruction(&Instruction::LocalGet(b_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_DATA,
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

/// `phx_str_cmp(a: (ref null $string), b: (ref null $string)) -> i32`
/// — lexicographic byte compare returning negative if `a < b`, zero
/// if equal, positive if `a > b`. The four lex ops (`Op::StringLt` /
/// `Le` / `Gt` / `Ge`) dispatch as `Call $phx_str_cmp` + `i32.const 0`
/// + `i32.lt_s` / `le_s` / `gt_s` / `ge_s`. See §Phase 2.4 decision K.3.
///
/// Algorithm (matches `[u8]` lexicographic ordering — the same shape
/// `Vec::cmp` uses):
///
/// 1. Compare overlapping bytes left-to-right; on the first differing
///    pair, return `byte_a as i32 - byte_b as i32` (an in-range
///    nonzero signed i32).
/// 2. If all overlapping bytes match, return `len_a - len_b` —
///    negative if `a` is a prefix of `b`, positive if vice versa,
///    zero if same length.
///
/// Params: `a`, `b` (locals 0, 1).
/// Locals: `len_a`, `len_b`, `min_len`, `i`, `byte_a`, `byte_b`,
/// `off_a`, `off_b` (i32, locals 2-9), `data_a`, `data_b`
/// (`(ref $bytes)`, locals 10, 11).
pub(super) fn synthesize_str_cmp(b: &mut ModuleBuilder) -> Result<u32, CompileError> {
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
        (8, ValType::I32),    // len_a, len_b, min_len, i, byte_a, byte_b, off_a, off_b
        (2, bytes_ref_local), // data_a, data_b
    ]);
    let a_local: u32 = 0;
    let b_local: u32 = 1;
    let len_a_local: u32 = 2;
    let len_b_local: u32 = 3;
    let min_len_local: u32 = 4;
    let i_local: u32 = 5;
    let byte_a_local: u32 = 6;
    let byte_b_local: u32 = 7;
    let off_a_local: u32 = 8;
    let off_b_local: u32 = 9;
    let data_a_local: u32 = 10;
    let data_b_local: u32 = 11;

    // Capture lengths, offsets, and data refs once before the loop.
    func.instruction(&Instruction::LocalGet(a_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_LEN,
    });
    func.instruction(&Instruction::LocalSet(len_a_local));
    func.instruction(&Instruction::LocalGet(b_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_LEN,
    });
    func.instruction(&Instruction::LocalSet(len_b_local));
    // min_len = min(len_a, len_b) — emit the branchless `if a < b { a } else { b }`
    // via select.
    func.instruction(&Instruction::LocalGet(len_a_local));
    func.instruction(&Instruction::LocalGet(len_b_local));
    func.instruction(&Instruction::LocalGet(len_a_local));
    func.instruction(&Instruction::LocalGet(len_b_local));
    func.instruction(&Instruction::I32LtU);
    func.instruction(&Instruction::Select);
    func.instruction(&Instruction::LocalSet(min_len_local));
    // offsets + data refs
    func.instruction(&Instruction::LocalGet(a_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_OFFSET,
    });
    func.instruction(&Instruction::LocalSet(off_a_local));
    func.instruction(&Instruction::LocalGet(b_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_OFFSET,
    });
    func.instruction(&Instruction::LocalSet(off_b_local));
    func.instruction(&Instruction::LocalGet(a_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_DATA,
    });
    func.instruction(&Instruction::LocalSet(data_a_local));
    func.instruction(&Instruction::LocalGet(b_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_DATA,
    });
    func.instruction(&Instruction::LocalSet(data_b_local));
    // i = 0
    func.instruction(&Instruction::I32Const(0));
    func.instruction(&Instruction::LocalSet(i_local));
    // Compare loop.
    func.instruction(&Instruction::Block(BlockType::Empty));
    func.instruction(&Instruction::Loop(BlockType::Empty));
    // if i >= min_len: exit loop → fall through to len_a - len_b path
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::LocalGet(min_len_local));
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
    func.instruction(&Instruction::LocalSet(byte_b_local));
    // if byte_a != byte_b: return (byte_a - byte_b) as signed i32
    func.instruction(&Instruction::LocalGet(byte_a_local));
    func.instruction(&Instruction::LocalGet(byte_b_local));
    func.instruction(&Instruction::I32Ne);
    func.instruction(&Instruction::If(BlockType::Empty));
    func.instruction(&Instruction::LocalGet(byte_a_local));
    func.instruction(&Instruction::LocalGet(byte_b_local));
    func.instruction(&Instruction::I32Sub);
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
    // All overlapping bytes matched — return len_a - len_b.
    func.instruction(&Instruction::LocalGet(len_a_local));
    func.instruction(&Instruction::LocalGet(len_b_local));
    func.instruction(&Instruction::I32Sub);
    func.instruction(&Instruction::End);
    Ok(b.add_and_emit_function(sig, &func))
}

/// `phx_str_substring(s: (ref null $string), start: i64, end: i64)
/// -> (ref $string)` — char-boundary walk over the receiver's bytes,
/// clamping start/end to the receiver's char count, then `struct.new`
/// returning a view into the parent's `$bytes`. See §Phase 2.4
/// decision K.3.
///
/// Phoenix's substring is **char-indexed** (UTF-8 code points), not
/// byte-indexed. The walk algorithm: scan bytes counting code-point
/// starts (`byte & 0xC0 != 0x80`); when the count reaches the target,
/// stop — `i` is the byte index of the start of the (target+1)-th code
/// point. Out-of-range clamping is automatic: hitting `i >= len`
/// before the target count is reached leaves `i = len`, naturally
/// matching the runtime's `start.min(char_count)` behavior.
///
/// Both walks share the same shape; we run it twice — once to find
/// `byte_start` (target = `start_chars`) and once to find `byte_end`
/// (target = `end_chars`, walking from offset 0 of the parent again
/// since `end_chars >= start_chars` after clamping).
///
/// Params: `s`, `start: i64`, `end: i64` (locals 0, 1, 2).
/// Locals: `start_i`, `end_i` (i32 clamped, 3-4), `byte_start`,
/// `byte_end` (i32, 5-6), `i`, `count`, `byte` (i32, 7-9),
/// `off`, `len` (i32, 10-11), `data` (`(ref $bytes)`, 12).
pub(super) fn synthesize_str_substring(b: &mut ModuleBuilder) -> Result<u32, CompileError> {
    let string_idx = b.require_string_type_idx()?;
    let bytes_idx = b.require_bytes_type_idx()?;
    let string_ref_param = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(string_idx),
    });
    let string_ref_ret = ValType::Ref(RefType {
        nullable: false,
        heap_type: HeapType::Concrete(string_idx),
    });
    let sig = b.intern_signature(
        &[string_ref_param, ValType::I64, ValType::I64],
        &[string_ref_ret],
    );

    let bytes_ref_local = ValType::Ref(RefType {
        nullable: false,
        heap_type: HeapType::Concrete(bytes_idx),
    });
    let mut func = wasm_encoder::Function::new([
        (9, ValType::I32), // start_i, end_i, byte_start, byte_end, i, count, byte, off, len
        (1, bytes_ref_local), // data
    ]);
    let s_local: u32 = 0;
    let start_i64_local: u32 = 1;
    let end_i64_local: u32 = 2;
    let start_i_local: u32 = 3;
    let end_i_local: u32 = 4;
    let byte_start_local: u32 = 5;
    let byte_end_local: u32 = 6;
    let i_local: u32 = 7;
    let count_local: u32 = 8;
    let byte_local: u32 = 9;
    let off_local: u32 = 10;
    let len_local: u32 = 11;
    let data_local: u32 = 12;

    // Lower start/end from i64 to i32, clamping each into `[0,
    // i32::MAX]` (see `emit_clamp_index_i64_to_i32` for why saturating
    // rather than wrapping matters). The char-walk applies the upper
    // `char_count` clamp afterward by stopping at `i >= len`.
    emit_clamp_index_i64_to_i32(&mut func, start_i64_local, start_i_local);
    emit_clamp_index_i64_to_i32(&mut func, end_i64_local, end_i_local);
    // end_i = max(end_i, start_i) — the runtime clamps `end_u =
    // end_u.max(start_u)` after the individual clamps, so empty
    // ranges (`end < start`) become `start..start`.
    //
    // WASM `select` pops `(val1, val2, cond)` and returns val1 if
    // cond != 0 else val2 — with cond = `end_i < start_i`, the
    // "branch is taken" case needs `start_i` (the larger) and the
    // fall-through needs `end_i` (already >= start_i). So val1 =
    // start_i, val2 = end_i.
    func.instruction(&Instruction::LocalGet(start_i_local));
    func.instruction(&Instruction::LocalGet(end_i_local));
    func.instruction(&Instruction::LocalGet(end_i_local));
    func.instruction(&Instruction::LocalGet(start_i_local));
    func.instruction(&Instruction::I32LtU);
    func.instruction(&Instruction::Select);
    func.instruction(&Instruction::LocalSet(end_i_local));
    // Capture parent fields once.
    func.instruction(&Instruction::LocalGet(s_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_OFFSET,
    });
    func.instruction(&Instruction::LocalSet(off_local));
    func.instruction(&Instruction::LocalGet(s_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_LEN,
    });
    func.instruction(&Instruction::LocalSet(len_local));
    func.instruction(&Instruction::LocalGet(s_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_DATA,
    });
    func.instruction(&Instruction::LocalSet(data_local));
    // Walk 1: find byte_start = byte index of start_i-th code-point
    // start (or len if start_i exceeds the char count).
    emit_char_walk(
        &mut func,
        bytes_idx,
        data_local,
        off_local,
        len_local,
        i_local,
        count_local,
        byte_local,
        start_i_local,
    );
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::LocalSet(byte_start_local));
    // Walk 2: find byte_end = byte index of end_i-th code-point start.
    // We restart from `i = 0` and walk to `end_i` rather than continuing
    // from byte_start because the loop emits the same instruction
    // sequence either way (chars_to_take = end_i - start_i is harder
    // to express without a separate counter); restart keeps the helper
    // as one shared walker.
    emit_char_walk(
        &mut func,
        bytes_idx,
        data_local,
        off_local,
        len_local,
        i_local,
        count_local,
        byte_local,
        end_i_local,
    );
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::LocalSet(byte_end_local));
    // struct.new $string  (data, off + byte_start, byte_end - byte_start)
    func.instruction(&Instruction::LocalGet(data_local));
    func.instruction(&Instruction::LocalGet(off_local));
    func.instruction(&Instruction::LocalGet(byte_start_local));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::LocalGet(byte_end_local));
    func.instruction(&Instruction::LocalGet(byte_start_local));
    func.instruction(&Instruction::I32Sub);
    func.instruction(&Instruction::StructNew(string_idx));
    func.instruction(&Instruction::End);
    Ok(b.add_and_emit_function(sig, &func))
}

/// Clamp an i64 substring index (`src_i64_local`) into `[0, i32::MAX]`
/// and store the result in `dst_i32_local`.
///
/// Negative values become 0 (matching the runtime's `start.max(0)`).
/// Values above `i32::MAX` **saturate** to `i32::MAX` rather than
/// wrapping: a plain `i32.wrap_i64` would alias a huge index onto a
/// small in-range one (e.g. `2^32 + 5` → `5`), so `s.substring(2^32 +
/// 5, …)` would slice from char 5 instead of clamping to an empty span
/// — a silent divergence from the runtime's `.min(char_count)`. Any
/// index at or above the char count clamps identically, so `i32::MAX`
/// is a safe stand-in, and the char-walk then applies the real
/// `char_count` clamp by stopping at `i >= len`.
fn emit_clamp_index_i64_to_i32(
    func: &mut wasm_encoder::Function,
    src_i64_local: u32,
    dst_i32_local: u32,
) {
    func.instruction(&Instruction::LocalGet(src_i64_local));
    func.instruction(&Instruction::I64Const(0));
    func.instruction(&Instruction::I64LtS);
    func.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    // start < 0 → 0
    func.instruction(&Instruction::I32Const(0));
    func.instruction(&Instruction::Else);
    func.instruction(&Instruction::LocalGet(src_i64_local));
    func.instruction(&Instruction::I64Const(i32::MAX as i64));
    func.instruction(&Instruction::I64GtS);
    func.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    // start > i32::MAX → i32::MAX (saturate)
    func.instruction(&Instruction::I32Const(i32::MAX));
    func.instruction(&Instruction::Else);
    func.instruction(&Instruction::LocalGet(src_i64_local));
    func.instruction(&Instruction::I32WrapI64);
    func.instruction(&Instruction::End);
    func.instruction(&Instruction::End);
    func.instruction(&Instruction::LocalSet(dst_i32_local));
}

/// Emit a code-point-counting walk over the bytes at `data[off ..
/// off+len]`. Walks until `count` has reached `target`, leaving `i`
/// positioned at the byte index of the start of the `target`-th code
/// point (0-indexed). If `target > char_count`, the walk exits when
/// `i >= len`, leaving `i = len` — the natural clamp.
///
/// Caller owns `i` and `count` locals; this helper sets `i = 0`,
/// `count = 0` and runs the loop. After the walk, `i` is the byte
/// index the caller wants; `count` is left as the target reached
/// (or as the final code-point count if the string was shorter).
///
/// Used twice by `synthesize_str_substring` (once each for `byte_start`
/// and `byte_end`).
#[allow(clippy::too_many_arguments)]
fn emit_char_walk(
    func: &mut wasm_encoder::Function,
    bytes_idx: u32,
    data_local: u32,
    off_local: u32,
    len_local: u32,
    i_local: u32,
    count_local: u32,
    byte_local: u32,
    target_local: u32,
) {
    func.instruction(&Instruction::I32Const(0));
    func.instruction(&Instruction::LocalSet(i_local));
    func.instruction(&Instruction::I32Const(0));
    func.instruction(&Instruction::LocalSet(count_local));
    func.instruction(&Instruction::Block(BlockType::Empty));
    func.instruction(&Instruction::Loop(BlockType::Empty));
    // if i >= len: exit (clamping case)
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::LocalGet(len_local));
    func.instruction(&Instruction::I32GeU);
    func.instruction(&Instruction::BrIf(1));
    // byte = data[off + i]
    func.instruction(&Instruction::LocalGet(data_local));
    func.instruction(&Instruction::LocalGet(off_local));
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::ArrayGetU(bytes_idx));
    func.instruction(&Instruction::LocalSet(byte_local));
    // is code-point start? byte & 0xC0 != 0x80
    func.instruction(&Instruction::LocalGet(byte_local));
    func.instruction(&Instruction::I32Const(0xC0));
    func.instruction(&Instruction::I32And);
    func.instruction(&Instruction::I32Const(0x80));
    func.instruction(&Instruction::I32Ne);
    func.instruction(&Instruction::If(BlockType::Empty));
    // if count == target: break — i is the target-th code-point start
    func.instruction(&Instruction::LocalGet(count_local));
    func.instruction(&Instruction::LocalGet(target_local));
    func.instruction(&Instruction::I32Eq);
    func.instruction(&Instruction::BrIf(2));
    // count++
    func.instruction(&Instruction::LocalGet(count_local));
    func.instruction(&Instruction::I32Const(1));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::LocalSet(count_local));
    func.instruction(&Instruction::End);
    // i++
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::I32Const(1));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::LocalSet(i_local));
    func.instruction(&Instruction::Br(0));
    func.instruction(&Instruction::End); // close loop
    func.instruction(&Instruction::End); // close block
}

/// `phx_str_length(s: (ref null $string)) -> i64` — count code-point
/// starts in `s.$data[s.$offset .. s.$offset + s.$len]` and return the
/// result widened to i64 (Phoenix `Int`). Matches the runtime's
/// `s.chars().count()` semantics. See the K.2 correction note for why
/// this is a helper rather than an inline `struct.get $len`.
///
/// Param: `s` (`(ref null $string)`, local 0).
/// Locals: `count`, `i`, `byte`, `off`, `len` (i32, locals 1-5),
/// `data` (`(ref $bytes)`, local 6).
pub(super) fn synthesize_str_length(b: &mut ModuleBuilder) -> Result<u32, CompileError> {
    let string_idx = b.require_string_type_idx()?;
    let bytes_idx = b.require_bytes_type_idx()?;
    let string_ref = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(string_idx),
    });
    let sig = b.intern_signature(&[string_ref], &[ValType::I64]);

    let bytes_ref_local = ValType::Ref(RefType {
        nullable: false,
        heap_type: HeapType::Concrete(bytes_idx),
    });
    let mut func = wasm_encoder::Function::new([
        (5, ValType::I32),    // count, i, byte, off, len
        (1, bytes_ref_local), // data
    ]);
    let s_local: u32 = 0;
    let count_local: u32 = 1;
    let i_local: u32 = 2;
    let byte_local: u32 = 3;
    let off_local: u32 = 4;
    let len_local: u32 = 5;
    let data_local: u32 = 6;

    // off, len, data from struct.
    func.instruction(&Instruction::LocalGet(s_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_OFFSET,
    });
    func.instruction(&Instruction::LocalSet(off_local));
    func.instruction(&Instruction::LocalGet(s_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_LEN,
    });
    func.instruction(&Instruction::LocalSet(len_local));
    func.instruction(&Instruction::LocalGet(s_local));
    func.instruction(&Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_DATA,
    });
    func.instruction(&Instruction::LocalSet(data_local));
    // count = 0; i = 0;
    func.instruction(&Instruction::I32Const(0));
    func.instruction(&Instruction::LocalSet(count_local));
    func.instruction(&Instruction::I32Const(0));
    func.instruction(&Instruction::LocalSet(i_local));
    // Walk every byte, incrementing count on each code-point start.
    // Differs from `emit_char_walk` in that we always walk to len and
    // never break early — there's no target.
    func.instruction(&Instruction::Block(BlockType::Empty));
    func.instruction(&Instruction::Loop(BlockType::Empty));
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::LocalGet(len_local));
    func.instruction(&Instruction::I32GeU);
    func.instruction(&Instruction::BrIf(1));
    func.instruction(&Instruction::LocalGet(data_local));
    func.instruction(&Instruction::LocalGet(off_local));
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::ArrayGetU(bytes_idx));
    func.instruction(&Instruction::LocalSet(byte_local));
    func.instruction(&Instruction::LocalGet(byte_local));
    func.instruction(&Instruction::I32Const(0xC0));
    func.instruction(&Instruction::I32And);
    func.instruction(&Instruction::I32Const(0x80));
    func.instruction(&Instruction::I32Ne);
    func.instruction(&Instruction::If(BlockType::Empty));
    func.instruction(&Instruction::LocalGet(count_local));
    func.instruction(&Instruction::I32Const(1));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::LocalSet(count_local));
    func.instruction(&Instruction::End);
    func.instruction(&Instruction::LocalGet(i_local));
    func.instruction(&Instruction::I32Const(1));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::LocalSet(i_local));
    func.instruction(&Instruction::Br(0));
    func.instruction(&Instruction::End); // close loop
    func.instruction(&Instruction::End); // close block
    // Return count widened to i64.
    func.instruction(&Instruction::LocalGet(count_local));
    func.instruction(&Instruction::I64ExtendI32U);
    func.instruction(&Instruction::End);
    Ok(b.add_and_emit_function(sig, &func))
}
