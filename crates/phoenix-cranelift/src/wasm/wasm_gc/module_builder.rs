//! Module-level assembly state for the wasm32-gc backend.
//!
//! `ModuleBuilder` here owns the per-section builders that
//! `wasm-encoder` exposes. Distinct from the wasm32-linear builder
//! because the pipelines diverge significantly:
//!
//! - **No runtime merge.** Per design-decisions §Phase 2.4 decision I,
//!   wasm32-gc emits all helpers inline rather than embedding a
//!   pre-compiled `phoenix-runtime.wasm`. The import section therefore
//!   carries only the WASI `fd_write` symbol, not the `phx_*` runtime
//!   surface.
//! - **Synthesized helpers.** The codegen crate ships its own print
//!   helpers as WASM bytecode (function indices assigned during
//!   [`Self::declare_print_helper`]). PR 5 introduced `phx_print_i64`;
//!   PR 6 slice 1 adds `phx_print_str`, `phx_str_concat`, and
//!   `phx_str_eq`. Each is synthesized only when the IR module
//!   actually needs it (a `BuiltinCall("print", String)` site for
//!   `phx_print_str`, an `Op::StringConcat` for `phx_str_concat`, etc.)
//!   — a module that uses no strings carries no string helpers and no
//!   `$bytes` / `$string` type declarations.
//! - **Small linear memory.** WASI's `fd_write` only reads linear
//!   memory, so `phx_print_str` copies the source string's bytes out of
//!   its WASM-GC `(array i8)` into a linear-memory scratch buffer
//!   before calling `fd_write`. The buffer sizes are sized for typical
//!   fixture output; oversized strings are rejected up-front rather
//!   than silently truncated.
//!
//! Section emission order follows the WASM spec: type → import →
//! function → table → memory → global → export → code → data.

use std::collections::HashMap;

use phoenix_ir::instruction::{FuncId, Op, ValueId};
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;
use wasm_encoder::ValType;

use crate::error::CompileError;

use super::super::type_interner::TypeInterner;
use super::enums::{self, EnumInstantiationKey};
use super::string_helpers;
use super::translate;

/// Linear-memory layout used by the synthesized print helper. Sizes
/// are tiny — wasm32-gc only uses linear memory for WASI iovec
/// staging and user string literals.
///
/// - `[0, IOVEC_OFFSET)` — NULL guard (never read or written).
/// - `[IOVEC_OFFSET, IOVEC_OFFSET + 8)` — single 8-byte iovec entry
///   (`iov_ptr: i32, iov_len: i32`).
/// - `[NWRITTEN_OFFSET, NWRITTEN_OFFSET + 4)` — i32 storage for
///   `fd_write`'s `nwritten` out-pointer.
/// - `[PRINT_I64_BUF_START, PRINT_I64_BUF_END)` — digit scratch.
/// - `[PRINT_STR_BUF_START, PRINT_STR_BUF_END)` — `phx_print_str`'s
///   array → linear-memory copy scratch.
/// - `[BOOL_TRUE_OFFSET, BOOL_FALSE_OFFSET + 6)` — the `"true\n"` /
///   `"false\n"` payloads pre-populated by `print(Bool)`'s active data
///   segments (see [`Self::declare_bool_data`]).
/// - `[PRINT_F64_BUF_START, PRINT_F64_BUF_END)` — `phx_print_f64`'s
///   digit / literal scratch (see [`super::float_helpers`]).
/// - `[RYU_POW5_INV_SPLIT_OFFSET, RYU_POW5_SPLIT_OFFSET)` and
///   `[RYU_POW5_SPLIT_OFFSET, …)` — the Ryu d2s power-of-5 tables
///   (`(u64 lo, u64 hi)` pairs, 16 bytes per entry, little-endian),
///   materialized by active data segments when `phx_print_f64` is
///   synthesized. See [`super::ryu_tables`] and §Phase 2.4 K.6.
pub(super) const IOVEC_OFFSET: u32 = 8;
pub(super) const NWRITTEN_OFFSET: u32 = 16;
/// Scratch buffer for `phx_print_i64`'s digit conversion. 32 bytes
/// holds the worst case i64 string representation (sign byte, 19
/// digits, trailing newline — 21 chars total) with comfortable
/// headroom. Exclusive end is [`PRINT_I64_BUF_END`]; the helper
/// writes from the end backward.
const PRINT_I64_BUF_START: u32 = 32;
const PRINT_I64_BUF_END: u32 = PRINT_I64_BUF_START + 32;
/// Scratch buffer for `phx_print_str`'s array → linear-memory copy.
/// `fd_write` only reads linear memory, so the helper must copy the
/// source string's bytes out of its `(array i8)` before staging the
/// iovec. 4096 bytes is more than any fixture's printed line needs
/// — fixtures print short labels and interpolated values, never
/// multi-KiB blobs — and the helper hard-rejects strings whose `len + 1`
/// (newline) would overflow the buffer rather than silently truncating.
pub(super) const PRINT_STR_BUF_START: u32 = PRINT_I64_BUF_END;
const PRINT_STR_BUF_END: u32 = PRINT_STR_BUF_START + 4096;
/// Largest string `phx_print_str` accepts in one call. Equal to the
/// buffer size minus one byte for the trailing newline. Sized to fit
/// inside one WASM page (alongside the iovec / nwritten staging and
/// the `phx_print_i64` scratch).
pub(super) const PRINT_STR_MAX_LEN: u32 = PRINT_STR_BUF_END - PRINT_STR_BUF_START - 1;

/// Fixed linear-memory offsets where `"true\n"` / `"false\n"` are
/// pre-populated by active data segments at module instantiation.
/// `translate_print`'s Bool arm emits a 5-instruction if/else that
/// stages an iovec at one of these offsets and calls `fd_write`. No
/// runtime allocation, no helper function — see §Phase 2.4 decision K.3.
pub(super) const BOOL_TRUE_OFFSET: u32 = PRINT_STR_BUF_END;
pub(super) const BOOL_TRUE_BYTES: &[u8] = b"true\n";
pub(super) const BOOL_FALSE_OFFSET: u32 = BOOL_TRUE_OFFSET + BOOL_TRUE_BYTES.len() as u32;
pub(super) const BOOL_FALSE_BYTES: &[u8] = b"false\n";

/// Scratch region for `phx_print_f64` — both the special-case
/// literals (`"NaN\n"`, `"inf\n"`, `"-inf\n"`) and the Ryu emission
/// buffer. Sits above the bool data segments. 32 bytes is comfortable:
/// ryu's f64 output is bounded by 24 characters — the worst case is a
/// negative small-magnitude value whose exponent needs 4 chars,
/// `"-2.2250738585072014e-308"` (= -f64::MIN_POSITIVE; ryu's own
/// `Buffer` is `[u8; 24]`) — plus the trailing `'\n'` = 25. The bound
/// is pinned by `format_f64_pins_ryu_output` in `phoenix-runtime`.
/// (The emitter's transient footprint also fits: the `12.34`-style
/// branch stages digits one byte right of their final position before
/// the `memory.copy` shift, peaking at offset 19 within the region.)
/// Per `docs/design-decisions.md`
/// §Phase 2.4 K.6 (2026-06-09 amendment) the helper targets ryu's
/// scientific format — not the pre-amendment fixed-point worst case
/// that would have needed ~340 bytes. Defined here alongside the other
/// linear-memory regions so the layout map above stays the single
/// source of truth; consumed by [`super::float_helpers`].
pub(super) const PRINT_F64_BUF_START: u32 = BOOL_FALSE_OFFSET + BOOL_FALSE_BYTES.len() as u32;
/// Exclusive end of the f64 scratch region.
pub(super) const PRINT_F64_BUF_END: u32 = PRINT_F64_BUF_START + 32;

/// Base offset of the Ryu `DOUBLE_POW5_INV_SPLIT` table (291 entries ×
/// 16 bytes, ~4.5 KiB; trimmed to the f64-reachable indices 0..=290 —
/// see `ryu_tables.rs`) — consulted by `phx_ryu_d2d` for binary
/// exponents ≥ 0. 16-aligned so every `(lo, hi)` pair sits at a
/// natural 8-byte boundary for the helper's `i64.load align=3` hints.
/// Populated by an active data segment only when the module prints a
/// Float (see [`Self::declare_active_data`] /
/// `float_helpers::synthesize_print_f64`); a Float-free module carries
/// neither segment.
pub(super) const RYU_POW5_INV_SPLIT_OFFSET: u32 = PRINT_F64_BUF_END.next_multiple_of(16);
/// Base offset of the Ryu `DOUBLE_POW5_SPLIT` table (325 entries × 16
/// bytes, ~5.1 KiB) — consulted by `phx_ryu_d2d` for binary exponents
/// < 0. Packed directly after the inverse table. The segment holds the
/// f64-reachable indices 1..=325, so entry i sits at
/// `RYU_POW5_SPLIT_OFFSET + (i − 1) · 16`; loads go through
/// [`RYU_POW5_SPLIT_INDEX_BASE`] to keep index arithmetic out of the
/// bytecode.
pub(super) const RYU_POW5_SPLIT_OFFSET: u32 =
    RYU_POW5_INV_SPLIT_OFFSET + (super::ryu_tables::DOUBLE_POW5_INV_TABLE_SIZE as u32) * 16;
/// Virtual index base for the pow5 table: `phx_ryu_d2d` addresses
/// entry i as `RYU_POW5_SPLIT_INDEX_BASE + i · 16`, exactly as if the
/// table still carried its unreachable entry 0. One entry-width below
/// the segment start, i.e. it points into the last inverse-table
/// entry — harmless, because every reachable i is ≥
/// `DOUBLE_POW5_SPLIT_FIRST_IDX` (= 1), which the exponent-sweep
/// differential test exercises end-to-end.
pub(super) const RYU_POW5_SPLIT_INDEX_BASE: u32 =
    RYU_POW5_SPLIT_OFFSET - (super::ryu_tables::DOUBLE_POW5_SPLIT_FIRST_IDX as u32) * 16;
/// Exclusive end of the Ryu table region — the high-water mark of the
/// fixed linear-memory layout. Must stay under `MEMORY_PAGES` × 64 KiB.
const RYU_TABLES_END: u32 =
    RYU_POW5_SPLIT_OFFSET + (super::ryu_tables::DOUBLE_POW5_TABLE_SIZE as u32) * 16;

/// Memory pages declared for the module. One 64-KiB page is more than
/// enough for the iovec staging (12 bytes), the `phx_print_i64`
/// scratch (32 bytes), the `phx_print_str` scratch (4096 bytes), the
/// `"true\n"` / `"false\n"` bool payloads (11 bytes), the
/// `phx_print_f64` scratch (32 bytes), and the two Ryu power-of-5
/// tables (~9.6 KiB) combined — the layout tops out under 14 KiB
/// (asserted below). Grows in a later slice if a longer-lines
/// requirement emerges.
const MEMORY_PAGES: u64 = 1;

/// The fixed layout must fit the declared memory; a future region
/// added past the Ryu tables that overflows the page should fail the
/// build here, not trap at runtime.
const _: () = assert!(RYU_TABLES_END <= MEMORY_PAGES as u32 * 65536);

pub(super) struct ModuleBuilder {
    /// Function-signature interning. Shared with the wasm32-linear
    /// backend via `super::super::type_interner` — dedup is target-
    /// independent.
    types: TypeInterner,
    imports: wasm_encoder::ImportSection,
    functions: wasm_encoder::FunctionSection,
    memories: wasm_encoder::MemorySection,
    exports: wasm_encoder::ExportSection,
    code: wasm_encoder::CodeSection,
    data: wasm_encoder::DataSection,

    /// Number of imported functions, used to translate "local
    /// function ordinal N" into "WASM function index N +
    /// import_func_count". Bumped by [`Self::declare_imports`].
    import_func_count: u32,

    /// WASM function index of the WASI `fd_write` import. Populated
    /// by [`Self::declare_imports`]. Consulted by
    /// [`Self::declare_print_helper`] when emitting the helper body's
    /// `call` instruction.
    fd_write_idx: Option<u32>,

    /// WASM function index of the synthesized `phx_print_i64` helper
    /// (digit conversion + `fd_write` with newline). Populated by
    /// [`Self::declare_print_helper`]; consulted by
    /// `translate::translate_print` for `Int` arguments.
    print_i64_idx: Option<u32>,

    /// WASM function index of the WASI-required `_start` entry.
    /// Populated by [`Self::declare_start`].
    start_idx: Option<u32>,

    /// Phoenix `main` function index in WASM's flat function space.
    /// `_start` calls this on entry. Populated by
    /// [`Self::declare_phoenix_functions`] on encountering a function
    /// named `main`.
    phx_main_idx: Option<u32>,

    /// Phoenix [`FuncId`] → merged-module WASM function index, populated
    /// by [`Self::declare_phoenix_functions`]. Consulted by
    /// `Op::Call` lowering so direct calls (including recursion) can
    /// resolve to a concrete WASM `call` target before the called
    /// function's body has been emitted.
    phx_user_funcs: HashMap<FuncId, u32>,

    /// Phoenix struct name (post-monomorphization, e.g. `Point` or
    /// `Container__i64`) → WASM type-section index of the nominal
    /// `(struct ...)` declaration. Populated by
    /// [`Self::declare_phoenix_structs`]; consulted by `Op::StructAlloc`
    /// lowering and by the `IrType::StructRef` → WASM `ValType` mapping
    /// in `translate::wasm_valtypes_for`. See §Phase 2.4 decision K.1
    /// for the one-WASM-struct-per-Phoenix-struct rationale.
    phx_structs: HashMap<String, u32>,

    /// WASM struct type-section index → declared field count. Lets
    /// `Op::StructGetField` / `Op::StructSetField` bounds-check the IR
    /// field index — recovered from the receiver's binding `ValType`,
    /// which carries the WASM index but not the Phoenix struct name —
    /// before emitting a `struct.get`/`struct.set`. An out-of-range
    /// index would otherwise yield a module `wasmparser` only rejects
    /// deep in binary decoding. Populated alongside [`Self::phx_structs`].
    phx_struct_field_counts: HashMap<u32, u32>,

    /// WASM type-section index of `(array (mut i8))` — the byte storage
    /// array for Phoenix strings. Populated by
    /// [`Self::declare_string_types`] when the module uses any string
    /// ops; consulted by `Op::ConstString` lowering (via
    /// `array.new_data`) and by the synthesized string helpers.
    /// See §Phase 2.4 decision K.2.
    bytes_type_idx: Option<u32>,

    /// WASM type-section index of `(struct (ref $bytes) (field $offset
    /// i32) (field $len i32))` — the nominal Phoenix `String` type.
    /// Populated by [`Self::declare_string_types`]; consulted by the
    /// `IrType::StringRef` → WASM `ValType` mapping and by every
    /// string op site.
    string_type_idx: Option<u32>,

    /// WASM function index of the synthesized `phx_print_str` helper
    /// — copies a string's bytes from `$data + $offset` for `$len`
    /// bytes into the linear-memory iovec scratch buffer, appends a
    /// newline, and calls `fd_write`. Populated by
    /// [`Self::declare_string_helpers`] when the IR module calls
    /// `print` with a String argument.
    print_str_idx: Option<u32>,

    /// WASM function index of the synthesized `phx_print_f64` helper
    /// — NaN/±inf/±0.0 special cases inline, plus the Ryu d2s general
    /// case (shortest-roundtrip digits via the precomputed power-of-5
    /// tables, emitted in ryu's positional/scientific format).
    /// Populated by [`Self::declare_print_f64_helper`] when the IR
    /// module calls `print` with a Float argument. See §Phase 2.4
    /// decision K.6 (2026-06-09 amendment).
    print_f64_idx: Option<u32>,

    /// WASM function index of the synthesized `phx_fmod` helper —
    /// IEEE-754 truncated remainder (musl `fmod` port), matching
    /// Rust's `f64 % f64` bit-for-bit. Populated by
    /// [`Self::declare_fmod_helper`] when the IR module emits
    /// `Op::FMod`. See §Phase 2.4 decision K.5.
    fmod_idx: Option<u32>,

    /// WASM function index of the synthesized `phx_str_concat` helper
    /// — allocates a fresh `$bytes` of combined length, `array.copy`s
    /// from each operand honoring `$offset`, and `struct.new`s the
    /// result. Populated by [`Self::declare_string_helpers`] when the
    /// IR module emits `Op::StringConcat`.
    str_concat_idx: Option<u32>,

    /// WASM function index of the synthesized `phx_str_eq` helper —
    /// length-equal check followed by a byte-by-byte loop with
    /// offset arithmetic on each side. Returns `1` if equal, `0`
    /// otherwise. Populated by [`Self::declare_string_helpers`] when
    /// the IR module emits `Op::StringEq` or `Op::StringNe`.
    str_eq_idx: Option<u32>,

    /// WASM function index of the synthesized `phx_str_cmp` helper —
    /// signed-i32 lexicographic byte compare returning negative if
    /// `a < b`, zero if equal, positive if `a > b`. The four lex ops
    /// `Op::StringLt` / `Le` / `Gt` / `Ge` lower to a `Call` here
    /// followed by `i32.const 0` and the matching signed-i32 cmp.
    /// Populated when `HelperNeeds::str_cmp` is set. See §Phase 2.4
    /// decision K.3.
    str_cmp_idx: Option<u32>,

    /// WASM function index of the synthesized `phx_str_substring`
    /// helper — char-boundary walk over the receiver's bytes,
    /// clamping start/end, then `struct.new $string` returning a view
    /// into the parent's `$bytes`. Populated when
    /// `HelperNeeds::str_substring` is set.
    str_substring_idx: Option<u32>,

    /// WASM function index of the synthesized `phx_str_length` helper
    /// — code-point-start walk returning the char count as i64.
    /// Populated when `HelperNeeds::str_length` is set.
    str_length_idx: Option<u32>,

    /// Concrete-Phoenix-enum-instantiation `(template_name, type_args)`
    /// → `(parent_type_idx, variant_type_indices)`. Each entry is one
    /// distinct concrete enum (`Option<Int>` and `Option<String>` are
    /// separate entries, even though their template `Option` is shared).
    /// `parent_type_idx` is the open `(sub (struct (field $tag i32)))`
    /// that holds only the discriminant; `variant_type_indices[i]` is
    /// the WASM type index for the i-th variant. Populated by
    /// [`Self::declare_phoenix_enums`] after a codegen-time
    /// monomorphization pass collects every concrete instantiation
    /// used in the IR. Consulted by `Op::EnumAlloc` (via the alloc
    /// result type's `EnumRef(name, args)`), `Op::EnumDiscriminant` /
    /// `Op::EnumGetField` (via the receiver binding's parent_idx →
    /// reverse-lookup), and the `IrType::EnumRef` → `ValType` mapping
    /// (via the field type's `(name, args)`). See §Phase 2.4 decision K.4.
    phx_enums: HashMap<EnumInstantiationKey, (u32, Vec<u32>)>,

    /// Reverse index `parent_type_idx → instantiation key`, kept in
    /// lockstep with [`Self::phx_enums`] by [`Self::record_enum`]. Lets
    /// [`Self::enum_by_parent_idx`] resolve `Op::EnumGetField`'s
    /// parent-typed receiver back to its instantiation in O(1) rather
    /// than scanning every `phx_enums` entry per field read. Parent
    /// indices are unique (one `declare_open_struct` per instantiation),
    /// so the map is injective.
    phx_enum_parent_to_key: HashMap<u32, EnumInstantiationKey>,

    /// True once [`Self::declare_bool_data`] has emitted the
    /// `"true\n"` / `"false\n"` active data segments. The inline
    /// `print(Bool)` lowering in `translate.rs` stages iovecs at the
    /// fixed offsets those segments populate, so it must refuse to emit
    /// unless this is set — see [`Self::require_bool_data`]. Guards the
    /// `scan_helper_needs` (keys off `IrType::Bool`) ↔ `translate_print`
    /// (keys off `ValType::I32`) coupling: if a future i32-lowered type
    /// is ever printed without `scan_helper_needs` declaring the
    /// segments, this turns a silent garbage-print into a loud error.
    bool_data_declared: bool,

    /// Count of passive data segments emitted so far. Required up
    /// front for the WASM `DataCount` section, which the validator
    /// reads to know how many data segments exist before it sees the
    /// data section itself — `array.new_data` instructions need that
    /// count for validation. Bumped by [`Self::reserve_string_data`]
    /// and by [`Self::declare_bool_data`]. Active segments (used by
    /// the bool data path) and passive segments (used by string
    /// literals) both count.
    data_segment_count: u32,
}

impl ModuleBuilder {
    pub(super) fn new() -> Self {
        Self {
            types: TypeInterner::default(),
            imports: wasm_encoder::ImportSection::new(),
            functions: wasm_encoder::FunctionSection::new(),
            memories: wasm_encoder::MemorySection::new(),
            exports: wasm_encoder::ExportSection::new(),
            code: wasm_encoder::CodeSection::new(),
            data: wasm_encoder::DataSection::new(),
            import_func_count: 0,
            fd_write_idx: None,
            print_i64_idx: None,
            start_idx: None,
            phx_main_idx: None,
            phx_user_funcs: HashMap::new(),
            phx_structs: HashMap::new(),
            phx_struct_field_counts: HashMap::new(),
            bytes_type_idx: None,
            string_type_idx: None,
            print_str_idx: None,
            print_f64_idx: None,
            fmod_idx: None,
            str_concat_idx: None,
            str_eq_idx: None,
            str_cmp_idx: None,
            str_substring_idx: None,
            str_length_idx: None,
            phx_enums: HashMap::new(),
            phx_enum_parent_to_key: HashMap::new(),
            bool_data_declared: false,
            data_segment_count: 0,
        }
    }

    /// Emit the two active data segments backing `print(Bool)` —
    /// `"true\n"` at [`BOOL_TRUE_OFFSET`] and `"false\n"` at
    /// [`BOOL_FALSE_OFFSET`]. Active segments self-materialize into
    /// linear memory at module instantiation, so the per-call cost is
    /// just an iovec stage + `fd_write`. See §Phase 2.4 decision K.3.
    ///
    /// Bumps `data_segment_count` for the validator's `DataCount`
    /// section — active segments count alongside the passive segments
    /// used by string literals.
    pub(super) fn declare_bool_data(&mut self) {
        let true_expr = wasm_encoder::ConstExpr::i32_const(BOOL_TRUE_OFFSET as i32);
        self.data
            .active(0, &true_expr, BOOL_TRUE_BYTES.iter().copied());
        let false_expr = wasm_encoder::ConstExpr::i32_const(BOOL_FALSE_OFFSET as i32);
        self.data
            .active(0, &false_expr, BOOL_FALSE_BYTES.iter().copied());
        self.data_segment_count += 2;
        self.bool_data_declared = true;
    }

    /// Emit an active data segment that materializes `bytes` at
    /// `offset` in linear memory at module instantiation. Used by the
    /// Ryu power-of-5 tables (`float_helpers::synthesize_print_f64`);
    /// the caller owns the offset choice — the layout map at the top
    /// of this file is the single source of truth. Bumps
    /// `data_segment_count` for the validator's `DataCount` section.
    pub(super) fn declare_active_data(&mut self, offset: u32, bytes: &[u8]) {
        let offset_expr = wasm_encoder::ConstExpr::i32_const(offset as i32);
        self.data.active(0, &offset_expr, bytes.iter().copied());
        self.data_segment_count += 1;
    }

    /// Assert that the `print(Bool)` data segments were declared before
    /// the inline lowering stages an iovec at their offsets. Errors
    /// (rather than silently emitting a module that reads an
    /// unpopulated linear-memory region) if `scan_helper_needs` did not
    /// set `print_bool` for a value `translate_print` is treating as a
    /// `Bool` — the two predicates key off different type
    /// representations (`IrType::Bool` vs. `ValType::I32`) and this is
    /// the guard that keeps them honest. Mirrors the `require_*_idx`
    /// helpers' defensive pattern.
    pub(super) fn require_bool_data(&self) -> Result<(), CompileError> {
        if self.bool_data_declared {
            Ok(())
        } else {
            Err(CompileError::new(
                "wasm32-gc: `print(Bool)` lowering reached before \
                 `declare_bool_data` ran (internal compiler bug — \
                 `scan_helper_needs` did not set `print_bool` for a value \
                 lowered to WASM `i32`; the inline lowering and the data-\
                 segment scan must agree on what counts as a printable \
                 Bool)",
            ))
        }
    }

    /// Declare the two nominal WASM-GC types that back Phoenix's
    /// `String`: `(array (mut i8))` for byte storage and
    /// `(struct (ref $bytes) (field $offset i32) (field $len i32))` for
    /// the three-field wrapper.  See §Phase 2.4 decision K.2 for the
    /// shape rationale (substring views + StringBuilder.finalize() as
    /// O(1) struct.new operations).
    ///
    /// Must run *before* any function signature is interned, because
    /// a signature whose param or return is `IrType::StringRef` encodes
    /// `HeapType::Concrete($string_idx)` inline — declaring the string
    /// types afterwards would have the signature reference an
    /// unallocated type-section slot. (The struct types declared by
    /// `declare_phoenix_structs` are still emitted first, so the type
    /// section reads: Phoenix structs → `$bytes` → `$string` → function
    /// signatures.)
    pub(super) fn declare_string_types(&mut self) {
        debug_assert!(
            self.bytes_type_idx.is_none() && self.string_type_idx.is_none(),
            "wasm32-gc: `declare_string_types` called twice"
        );
        // `$bytes` — mutable byte array. Mutability is required so the
        // future `StringBuilder` can grow its array in place; sema
        // enforces Phoenix-level immutability of finalized strings.
        let bytes_field = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::I8,
            mutable: true,
        };
        let bytes_idx = self.types.declare_array(bytes_field);
        self.bytes_type_idx = Some(bytes_idx);
        // `$string` — (ref $bytes, $offset i32, $len i32). Phoenix-level
        // immutability of a finalized String is a sema invariant, not a
        // WASM one: no IR op emits `struct.set` against a `$string` (even
        // StringBuilder.finalize() produces a fresh struct), so the WASM
        // field mutability is free to choose. Pick `mutable: true`
        // uniformly so the shape mirrors the structs declared by K.1 and
        // a future builder-finalize that reuses the slot works without
        // retyping.
        let data_field = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::Val(wasm_encoder::ValType::Ref(
                wasm_encoder::RefType {
                    nullable: false,
                    heap_type: wasm_encoder::HeapType::Concrete(bytes_idx),
                },
            )),
            mutable: true,
        };
        let i32_field = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::Val(wasm_encoder::ValType::I32),
            mutable: true,
        };
        let string_idx = self
            .types
            .declare_struct(&[data_field, i32_field, i32_field]);
        self.string_type_idx = Some(string_idx);
    }

    /// Non-erroring peek at the `$string` type-section index. Used by
    /// `translate_print`'s WASM-`ValType`-based dispatch: it matches
    /// the receiver's `HeapType::Concrete(idx)` against this, falling
    /// through to a generic "unsupported print arg" diagnostic for
    /// any other ref type. Returns `None` if the module never declared
    /// the string types (i.e. doesn't use strings).
    pub(super) fn string_type_idx_if_set(&self) -> Option<u32> {
        self.string_type_idx
    }

    /// WASM type-section index of the `$bytes` array. Required by
    /// `Op::ConstString` (for the `array.new_data` instruction) and by
    /// every string-helper synthesizer.
    pub(super) fn require_bytes_type_idx(&self) -> Result<u32, CompileError> {
        self.bytes_type_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `$bytes` type requested before \
                 `declare_string_types` ran — the IR module uses strings \
                 but the helper-needs scan missed it (internal compiler bug)",
            )
        })
    }

    /// WASM type-section index of the `$string` struct. Required by
    /// `IrType::StringRef` → WASM ValType mapping and by every
    /// string-producing op.
    pub(super) fn require_string_type_idx(&self) -> Result<u32, CompileError> {
        self.string_type_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `$string` type requested before \
                 `declare_string_types` ran — the IR module uses strings \
                 but the helper-needs scan missed it (internal compiler bug)",
            )
        })
    }

    /// Reserve a passive data segment carrying `bytes`, return its
    /// segment index. `Op::ConstString` lowering then emits
    /// `array.new_data $bytes_type_idx $segment_idx` which allocates a
    /// fresh array and copies bytes from the segment into it.
    /// One segment per literal — no interning of identical strings
    /// in the MVP; revisit if module-size pressure surfaces.
    pub(super) fn reserve_string_data(&mut self, bytes: &[u8]) -> u32 {
        let idx = self.data_segment_count;
        self.data.passive(bytes.iter().copied());
        self.data_segment_count += 1;
        idx
    }

    /// Declare one nominal WASM-GC struct type per Phoenix struct in
    /// the IR module, in `struct_layouts` iteration order. Each
    /// declaration takes the type-section index that the order assigns
    /// and is recorded in [`Self::phx_structs`] so subsequent function
    /// signatures (and `Op::StructAlloc` lowering) can reference the
    /// index without re-walking the section.
    ///
    /// Must run *before* any function signature is interned, because
    /// signatures that take or return a `(ref null $struct_idx)` encode
    /// the index inline — declaring the struct after such a signature
    /// would have the signature reference an unallocated type-section
    /// slot. See §Phase 2.4 decision K.1.
    ///
    /// **Field-type restriction for slice 3.** Slice 3's fixtures only
    /// exercise primitive-typed fields (`Int`, `Float`, `Bool`). Nested
    /// struct fields, list / map / enum / closure fields, and string
    /// fields all error here — they need follow-up slices that pin
    /// their own type mappings before they can lower correctly. The
    /// error keeps a fixture-driven slice from silently producing a
    /// malformed module on inputs the slice hasn't been designed for.
    pub(super) fn declare_phoenix_structs(
        &mut self,
        ir_module: &IrModule,
    ) -> Result<(), CompileError> {
        // Iterate in sorted name order so the type-section layout is
        // deterministic across runs (HashMap iteration is otherwise
        // arbitrary, and a non-deterministic type section would make
        // golden-byte diffs in tests untrustworthy).
        let mut names: Vec<&String> = ir_module.struct_layouts.keys().collect();
        names.sort();
        for name in names {
            let layout = &ir_module.struct_layouts[name];
            let mut fields = Vec::with_capacity(layout.len());
            for (field_name, field_ty) in layout {
                fields.push(wasm_field_type_for(name, field_name, field_ty)?);
            }
            let idx = self.types.declare_struct(&fields);
            self.phx_structs.insert(name.clone(), idx);
            self.phx_struct_field_counts
                .insert(idx, fields.len() as u32);
        }
        Ok(())
    }

    /// Look up the WASM type-section index of a Phoenix struct's
    /// nominal `(struct …)` declaration. Used by `Op::StructAlloc`
    /// lowering and by the `IrType::StructRef` → WASM `ValType`
    /// mapping.
    pub(super) fn require_phx_struct(&self, name: &str) -> Result<u32, CompileError> {
        self.phx_structs.get(name).copied().ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: struct `{name}` was not declared by \
                 `declare_phoenix_structs` — either an `Op::StructAlloc` / \
                 `IrType::StructRef` references a struct missing from \
                 `IrModule::struct_layouts`, or the pipeline declared a \
                 function signature touching it before the struct itself \
                 (internal compiler bug)"
            ))
        })
    }

    /// Number of fields declared for the WASM struct type at
    /// `struct_idx`, or `None` if no struct was declared at that index.
    /// Used by `Op::StructGetField` / `Op::StructSetField` to
    /// bounds-check the IR field index.
    pub(super) fn struct_field_count(&self, struct_idx: u32) -> Option<u32> {
        self.phx_struct_field_counts.get(&struct_idx).copied()
    }

    /// Declare WASM-GC types for every Phoenix enum in the IR module
    /// per §Phase 2.4 decision K.4. Thin wrapper over [`enums::declare`]
    /// — the collection / monomorphization / declaration machinery lives
    /// in [`super::enums`]; this method is the builder-side entry point
    /// the pipeline in `mod.rs` calls, mirroring how
    /// [`Self::declare_string_helpers`] dispatches to
    /// [`super::string_helpers`].
    ///
    /// Must run *after* [`Self::declare_phoenix_structs`] and
    /// [`Self::declare_string_types`] (so variant fields of those types
    /// can encode their indices) and *before* any function signature
    /// touching `IrType::EnumRef` is interned (so the signature can
    /// encode the parent's `HeapType::Concrete(idx)`).
    pub(super) fn declare_phoenix_enums(
        &mut self,
        ir_module: &IrModule,
    ) -> Result<(), CompileError> {
        enums::declare(self, ir_module)
    }

    /// Declare an enum's open parent struct — `(sub (struct (field $tag
    /// i32)))`, non-final so variants can subtype it — and return its
    /// type-section index. Narrow wrapper over the private
    /// [`TypeInterner`] so [`enums::declare`] can emit parent types
    /// without reaching into the interner directly (same pattern as
    /// [`Self::intern_signature`]). See §Phase 2.4 decision K.4.
    pub(super) fn declare_enum_parent_struct(&mut self, fields: &[wasm_encoder::FieldType]) -> u32 {
        self.types.declare_open_struct(fields)
    }

    /// Declare an enum's final variant struct subtyping the parent at
    /// `parent_idx`, and return its type-section index. `fields` must
    /// start with the parent's `$tag` field (a WASM-GC subtype
    /// requirement) followed by the variant's payload. Narrow wrapper
    /// over the private [`TypeInterner`] for [`enums::declare`].
    pub(super) fn declare_enum_variant_struct(
        &mut self,
        fields: &[wasm_encoder::FieldType],
        parent_idx: u32,
    ) -> u32 {
        self.types.declare_subtype_struct(fields, parent_idx)
    }

    /// Record a fully-declared enum instantiation: its parent
    /// type-section index and the per-variant type indices. Keeps
    /// [`Self::phx_enums`] and the [`Self::phx_enum_parent_to_key`]
    /// reverse index in lockstep so the query accessors stay
    /// consistent. Called once per instantiation by [`enums::declare`].
    pub(super) fn record_enum(
        &mut self,
        key: EnumInstantiationKey,
        parent_idx: u32,
        variant_indices: Vec<u32>,
    ) {
        self.phx_enum_parent_to_key.insert(parent_idx, key.clone());
        self.phx_enums.insert(key, (parent_idx, variant_indices));
    }

    /// Read-only view of the Phoenix-struct-name → WASM-type-index map.
    /// Used by [`enums::declare`] to resolve `StructRef` variant fields
    /// to the struct's already-declared type index.
    pub(super) fn phx_struct_indices(&self) -> &HashMap<String, u32> {
        &self.phx_structs
    }

    /// WASM type-section index of the parent type for the concrete
    /// enum instantiation `(name, type_args)`. Used by
    /// `Op::EnumDiscriminant` (reads `$tag` through this type) and by
    /// the `IrType::EnumRef` → WASM `ValType` mapping. Different
    /// `type_args` for the same template are *separate* WASM enums
    /// per K.4 codegen-time monomorphization.
    pub(super) fn require_enum_parent_idx(
        &self,
        name: &str,
        type_args: &[IrType],
    ) -> Result<u32, CompileError> {
        let key = (name.to_string(), type_args.to_vec());
        self.phx_enums
            .get(&key)
            .map(|(parent, _)| *parent)
            .ok_or_else(|| {
                CompileError::new(format!(
                    "wasm32-gc: enum instantiation `{name}{type_args:?}` was \
                     not declared by `declare_phoenix_enums` — either an \
                     `Op::EnumAlloc` / `IrType::EnumRef` references an enum \
                     missing from `IrModule::enum_layouts`, or the \
                     enum-collection pass missed this instantiation \
                     (internal compiler bug)"
                ))
            })
    }

    /// WASM type-section index of a variant struct for the concrete
    /// enum instantiation `(name, type_args)`. Used by
    /// `Op::EnumAlloc` (the `struct.new` target) and
    /// `Op::EnumGetField` (the `ref.cast` target before the field load).
    pub(super) fn require_enum_variant_idx(
        &self,
        name: &str,
        type_args: &[IrType],
        variant_idx: u32,
    ) -> Result<u32, CompileError> {
        let key = (name.to_string(), type_args.to_vec());
        let (_, variants) = self.phx_enums.get(&key).ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: enum instantiation `{name}{type_args:?}` was not \
                 declared by `declare_phoenix_enums` (internal compiler bug)"
            ))
        })?;
        variants.get(variant_idx as usize).copied().ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: enum `{name}{type_args:?}` has {} variants but \
                     variant index {variant_idx} was requested (IR verifier \
                     should have caught this)",
                variants.len()
            ))
        })
    }

    /// Reverse-lookup the `(name, type_args)` instantiation and the
    /// variant-index list from a WASM parent type-section index. Used
    /// by `Op::EnumGetField`: the receiver's binding `ValType` carries
    /// the parent index, so the field-read path recovers the
    /// instantiation from that without needing the IR to thread the
    /// `(name, args)` tuple through the op.
    ///
    /// O(1) via the [`Self::phx_enum_parent_to_key`] reverse index
    /// (built by [`Self::record_enum`]) rather than a scan over every
    /// declared enum per field read. Returns `None` if `parent_idx`
    /// isn't a recorded enum parent (e.g. it's a plain struct index).
    pub(super) fn enum_by_parent_idx(
        &self,
        parent_idx: u32,
    ) -> Option<(&EnumInstantiationKey, &[u32])> {
        let key = self.phx_enum_parent_to_key.get(&parent_idx)?;
        let (_, variants) = self.phx_enums.get(key)?;
        Some((key, variants.as_slice()))
    }

    /// Number of fields (excluding the inherited `$tag`) in the
    /// variant at `variant_idx` of the enum named `name`. The
    /// template's variant arity is type-arg-independent (each
    /// instantiation has the same number of fields per variant; only
    /// the field types differ), so this consults `enum_layouts`
    /// directly without needing the `type_args`.
    pub(super) fn enum_variant_field_count(
        &self,
        ir_module: &IrModule,
        name: &str,
        variant_idx: u32,
    ) -> Option<u32> {
        ir_module
            .enum_layouts
            .get(name)
            .and_then(|variants| variants.get(variant_idx as usize))
            .map(|(_, fields)| fields.len() as u32)
    }

    /// Declare the WASI imports the synthesized helpers and `_start`
    /// need. WASI's module name is `wasi_snapshot_preview1` and the
    /// signature is fixed by the spec:
    /// - `fd_write(fd: i32, iovs_ptr: i32, iovs_len: i32, nwritten_ptr: i32) -> i32`
    ///
    /// Function indices are assigned in declaration order, so
    /// `fd_write` lands at index 0. `proc_exit` is *not* imported yet:
    /// `_start` returns normally and the MVP has no panic path, so
    /// importing a symbol nothing calls would only burden the host.
    /// The panic-routing slice adds it back alongside its first caller.
    pub(super) fn declare_imports(&mut self) {
        let fd_write_ty = self.types.intern(
            &[
                wasm_encoder::ValType::I32, // fd
                wasm_encoder::ValType::I32, // iovs_ptr
                wasm_encoder::ValType::I32, // iovs_len
                wasm_encoder::ValType::I32, // nwritten_ptr
            ],
            &[wasm_encoder::ValType::I32],
        );
        self.imports.import(
            "wasi_snapshot_preview1",
            "fd_write",
            wasm_encoder::EntityType::Function(fd_write_ty),
        );
        self.fd_write_idx = Some(self.import_func_count);
        self.import_func_count += 1;
    }

    /// Declare the single linear memory used by the WASI iovec
    /// staging area and user string literals. See the module-level
    /// constants for the layout.
    pub(super) fn declare_memory(&mut self) {
        self.memories.memory(wasm_encoder::MemoryType {
            minimum: MEMORY_PAGES,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
    }

    /// Synthesize the `print` helper(s) the MVP needs and record their
    /// WASM function indices. Slice 1 only prints `Int`, so only
    /// `phx_print_i64` is emitted; `phx_print_str` synthesis is
    /// deferred to the String slice so we don't emit an uncallable
    /// function into every module.
    pub(super) fn declare_print_helper(&mut self) -> Result<(), CompileError> {
        let fd_write_idx = self.fd_write_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `declare_print_helper` called before \
                 `declare_imports` (internal compiler bug)",
            )
        })?;
        self.print_i64_idx = Some(self.synthesize_print_i64_helper(fd_write_idx)?);
        Ok(())
    }

    /// Body: convert the i64 parameter to a decimal ASCII string with
    /// a trailing newline, stage an iovec entry pointing at it, and
    /// call `fd_write(1, iovec_ptr, 1, nwritten_ptr)`.
    ///
    /// Layout: a scratch buffer at `[PRINT_I64_BUF_START,
    /// PRINT_I64_BUF_END)`. The helper writes from the end backward
    /// — first `'\n'` at `BUF_END - 1`, then digits, then an optional
    /// `'-'` — leaving the final string at `[ptr, BUF_END)`.
    ///
    /// Locals (beyond the i64 parameter at local index 0):
    /// - local 1: `ptr` (i32) — current write cursor.
    /// - local 2: `digit` (i32) — scratch for one ASCII digit.
    /// - local 3: `is_neg` (i32) — set when `n < 0`.
    /// - local 4: `len` (i32) — total bytes to write.
    ///
    /// `i64::MIN` overflows the unary-negation step and prints garbage
    /// on this path. Phoenix's `Int` is i64, the same as the runtime
    /// uses, and the wasm32-linear backend relies on the runtime's
    /// Rust-side formatting which doesn't have this gap. Accepting
    /// the divergence for the MVP — fibonacci's outputs are all well
    /// within i64 range and we re-evaluate if a fixture hits the edge.
    fn synthesize_print_i64_helper(&mut self, fd_write_idx: u32) -> Result<u32, CompileError> {
        let print_ty = self.types.intern(&[wasm_encoder::ValType::I64], &[]);
        let print_idx = self.add_local_function(print_ty);

        // 4 i32 locals beyond the param: ptr, digit, is_neg, len.
        let mut func = wasm_encoder::Function::new([(4, wasm_encoder::ValType::I32)]);
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
        let n_local: u32 = 0;
        let ptr_local: u32 = 1;
        let digit_local: u32 = 2;
        let is_neg_local: u32 = 3;
        let len_local: u32 = 4;

        // ptr = BUF_END - 1; [ptr] = '\n'
        func.instruction(&wasm_encoder::Instruction::I32Const(
            PRINT_I64_BUF_END as i32 - 1,
        ));
        func.instruction(&wasm_encoder::Instruction::LocalTee(ptr_local));
        func.instruction(&wasm_encoder::Instruction::I32Const(b'\n' as i32));
        func.instruction(&wasm_encoder::Instruction::I32Store8(byte_memarg));

        // if n == 0:
        //   ptr -= 1; [ptr] = '0'
        // else:
        //   if n < 0: is_neg = 1; n = -n
        //   while n > 0:
        //     ptr -= 1
        //     digit = (n % 10) as i32 + '0'
        //     [ptr] = digit
        //     n /= 10
        //   if is_neg: ptr -= 1; [ptr] = '-'
        func.instruction(&wasm_encoder::Instruction::LocalGet(n_local));
        func.instruction(&wasm_encoder::Instruction::I64Eqz);
        func.instruction(&wasm_encoder::Instruction::If(
            wasm_encoder::BlockType::Empty,
        ));
        // Zero path.
        func.instruction(&wasm_encoder::Instruction::LocalGet(ptr_local));
        func.instruction(&wasm_encoder::Instruction::I32Const(1));
        func.instruction(&wasm_encoder::Instruction::I32Sub);
        func.instruction(&wasm_encoder::Instruction::LocalTee(ptr_local));
        func.instruction(&wasm_encoder::Instruction::I32Const(b'0' as i32));
        func.instruction(&wasm_encoder::Instruction::I32Store8(byte_memarg));
        func.instruction(&wasm_encoder::Instruction::Else);
        // Non-zero path: handle sign.
        func.instruction(&wasm_encoder::Instruction::LocalGet(n_local));
        func.instruction(&wasm_encoder::Instruction::I64Const(0));
        func.instruction(&wasm_encoder::Instruction::I64LtS);
        func.instruction(&wasm_encoder::Instruction::If(
            wasm_encoder::BlockType::Empty,
        ));
        func.instruction(&wasm_encoder::Instruction::I32Const(1));
        func.instruction(&wasm_encoder::Instruction::LocalSet(is_neg_local));
        // KNOWN GAP: i64::MIN — `0 - n` wraps back to i64::MIN (still
        // negative), so the digit loop below then runs `I64RemS` on a
        // negative value and prints garbage. Accepted for the MVP; see
        // the function doc comment and `print_negative_runs_under_wasmtime_gc`
        // (which deliberately stays within ±i64 range). Grep `i64::MIN`
        // to find every site that has to change when this is fixed.
        func.instruction(&wasm_encoder::Instruction::I64Const(0));
        func.instruction(&wasm_encoder::Instruction::LocalGet(n_local));
        func.instruction(&wasm_encoder::Instruction::I64Sub);
        func.instruction(&wasm_encoder::Instruction::LocalSet(n_local));
        func.instruction(&wasm_encoder::Instruction::End); // close inner if
        // Digit loop: emit each digit by walking backward.
        func.instruction(&wasm_encoder::Instruction::Block(
            wasm_encoder::BlockType::Empty,
        ));
        func.instruction(&wasm_encoder::Instruction::Loop(
            wasm_encoder::BlockType::Empty,
        ));
        // n == 0 → exit
        func.instruction(&wasm_encoder::Instruction::LocalGet(n_local));
        func.instruction(&wasm_encoder::Instruction::I64Eqz);
        func.instruction(&wasm_encoder::Instruction::BrIf(1));
        // ptr -= 1
        func.instruction(&wasm_encoder::Instruction::LocalGet(ptr_local));
        func.instruction(&wasm_encoder::Instruction::I32Const(1));
        func.instruction(&wasm_encoder::Instruction::I32Sub);
        func.instruction(&wasm_encoder::Instruction::LocalSet(ptr_local));
        // digit = (n % 10) as i32 + '0'
        func.instruction(&wasm_encoder::Instruction::LocalGet(n_local));
        func.instruction(&wasm_encoder::Instruction::I64Const(10));
        func.instruction(&wasm_encoder::Instruction::I64RemS);
        func.instruction(&wasm_encoder::Instruction::I32WrapI64);
        func.instruction(&wasm_encoder::Instruction::I32Const(b'0' as i32));
        func.instruction(&wasm_encoder::Instruction::I32Add);
        func.instruction(&wasm_encoder::Instruction::LocalSet(digit_local));
        // [ptr] = digit
        func.instruction(&wasm_encoder::Instruction::LocalGet(ptr_local));
        func.instruction(&wasm_encoder::Instruction::LocalGet(digit_local));
        func.instruction(&wasm_encoder::Instruction::I32Store8(byte_memarg));
        // n /= 10
        func.instruction(&wasm_encoder::Instruction::LocalGet(n_local));
        func.instruction(&wasm_encoder::Instruction::I64Const(10));
        func.instruction(&wasm_encoder::Instruction::I64DivS);
        func.instruction(&wasm_encoder::Instruction::LocalSet(n_local));
        func.instruction(&wasm_encoder::Instruction::Br(0));
        func.instruction(&wasm_encoder::Instruction::End); // close loop
        func.instruction(&wasm_encoder::Instruction::End); // close block
        // if is_neg: ptr -= 1; [ptr] = '-'
        func.instruction(&wasm_encoder::Instruction::LocalGet(is_neg_local));
        func.instruction(&wasm_encoder::Instruction::If(
            wasm_encoder::BlockType::Empty,
        ));
        func.instruction(&wasm_encoder::Instruction::LocalGet(ptr_local));
        func.instruction(&wasm_encoder::Instruction::I32Const(1));
        func.instruction(&wasm_encoder::Instruction::I32Sub);
        func.instruction(&wasm_encoder::Instruction::LocalTee(ptr_local));
        func.instruction(&wasm_encoder::Instruction::I32Const(b'-' as i32));
        func.instruction(&wasm_encoder::Instruction::I32Store8(byte_memarg));
        func.instruction(&wasm_encoder::Instruction::End); // close is_neg if
        func.instruction(&wasm_encoder::Instruction::End); // close outer if/else

        // len = BUF_END - ptr
        func.instruction(&wasm_encoder::Instruction::I32Const(
            PRINT_I64_BUF_END as i32,
        ));
        func.instruction(&wasm_encoder::Instruction::LocalGet(ptr_local));
        func.instruction(&wasm_encoder::Instruction::I32Sub);
        func.instruction(&wasm_encoder::Instruction::LocalSet(len_local));

        // Stage iovec and call fd_write.
        func.instruction(&wasm_encoder::Instruction::I32Const(IOVEC_OFFSET as i32));
        func.instruction(&wasm_encoder::Instruction::LocalGet(ptr_local));
        func.instruction(&wasm_encoder::Instruction::I32Store(i32_memarg));
        func.instruction(&wasm_encoder::Instruction::I32Const(
            IOVEC_OFFSET as i32 + 4,
        ));
        func.instruction(&wasm_encoder::Instruction::LocalGet(len_local));
        func.instruction(&wasm_encoder::Instruction::I32Store(i32_memarg));
        emit_fd_write_call(&mut func, fd_write_idx);
        func.instruction(&wasm_encoder::Instruction::End);
        self.code.function(&func);
        Ok(print_idx)
    }

    /// Append a Phoenix-function declaration to the function section
    /// and return its WASM function index. Used by
    /// [`Self::declare_phoenix_functions`] and
    /// [`Self::declare_start`].
    fn add_local_function(&mut self, sig: u32) -> u32 {
        let idx = self.import_func_count + self.functions.len();
        self.functions.function(sig);
        idx
    }

    /// Intern a function signature and return its type-section index.
    /// Exposed so the string-helper synthesizers in
    /// [`super::string_helpers`] can declare their own signatures
    /// without reaching into the private [`TypeInterner`].
    pub(super) fn intern_signature(&mut self, params: &[ValType], returns: &[ValType]) -> u32 {
        self.types.intern(params, returns)
    }

    /// Append an immediate-emit helper: declare a function with
    /// signature `sig` in the function section, emit `body` into the
    /// code section, and return the helper's WASM function index. The
    /// function/code parallelism that [`Self::finish`] guards holds as
    /// long as callers emit the body in the same call that declares the
    /// signature — which this method enforces. Exposed for
    /// [`super::string_helpers`].
    pub(super) fn add_and_emit_function(&mut self, sig: u32, body: &wasm_encoder::Function) -> u32 {
        let idx = self.add_local_function(sig);
        self.code.function(body);
        idx
    }

    /// Declare every concrete Phoenix function (assign it a WASM
    /// function index + a type-section signature) and record `main`'s
    /// index for `_start` to call. MVP scope: every Phoenix function's
    /// signature is built from its IR `param_types` / `return_type`
    /// through the shared `wasm_valtypes_for` helper. (Slice 2 adds a
    /// `FuncId → wasm_idx` map for `Op::Call` resolution; slice 1 has no
    /// inter-function calls beyond `_start → main`.)
    pub(super) fn declare_phoenix_functions(
        &mut self,
        ir_module: &IrModule,
    ) -> Result<(), CompileError> {
        for func in ir_module.concrete_functions() {
            let params = translate::flatten_param_types(&func.param_types, self)?;
            let returns = translate::wasm_return_valtypes(&func.return_type, self)?;
            let sig = self.types.intern(&params, &returns);
            let wasm_idx = self.add_local_function(sig);
            // A duplicate `FuncId` would silently overwrite the map
            // entry, so `Op::Call` lowering would resolve recursion /
            // direct calls to the wrong WASM target. This invariant
            // is as load-bearing as the dispatcher's
            // `blocks[i].id == BlockId(i)` check.
            if let Some(prev_idx) = self.phx_user_funcs.insert(func.id, wasm_idx) {
                return Err(CompileError::new(format!(
                    "wasm32-gc: duplicate FuncId {:?} declared (WASM indices \
                     {prev_idx} and {wasm_idx}) — `declare_phoenix_functions` \
                     expects each concrete function exactly once (internal \
                     compiler bug)",
                    func.id
                )));
            }
            if func.name == "main" {
                // The synthesized `_start` (typed `[] -> []`) calls
                // `main` with no arguments and discards no result, so
                // `main` must be `() -> Void`. Reject anything else with
                // a clear diagnostic rather than emitting a `_start`
                // that leaves an operand on the stack (a structurally
                // invalid module). Phoenix's sema enforces this today;
                // the check keeps the backend honest if that changes.
                if !func.param_types.is_empty() {
                    return Err(CompileError::new(format!(
                        "wasm32-gc: `main` must take no parameters, but it \
                         declares {} (the synthesized `_start` calls `main` \
                         with no arguments)",
                        func.param_types.len()
                    )));
                }
                if !matches!(func.return_type, IrType::Void) {
                    return Err(CompileError::new(format!(
                        "wasm32-gc: `main` must return `Void`, but returns \
                         `{:?}` (the synthesized `_start` is typed `[] -> []` \
                         and discards no value)",
                        func.return_type
                    )));
                }
                self.phx_main_idx = Some(wasm_idx);
            }
        }
        // A single pass over `concrete_functions()` both declares every
        // function and records `main`'s index; if `main` was never seen,
        // `phx_main_idx` is still unset here. (Sema doesn't require an
        // entry point, so a `main`-less program reaches the backend — this
        // is the layer that rejects it.) A `main` with the wrong signature
        // returns earlier inside the loop with a more specific diagnostic.
        if self.phx_main_idx.is_none() {
            return Err(CompileError::new("wasm32-gc: no `main` function found"));
        }
        Ok(())
    }

    /// Declare the WASI-required `_start` entry. Its body is emitted
    /// later by [`Self::emit_start_body`]; this just reserves the
    /// function index.
    pub(super) fn declare_start(&mut self) {
        let start_ty = self.types.intern(&[], &[]);
        let idx = self.add_local_function(start_ty);
        self.start_idx = Some(idx);
    }

    /// Export `memory` (for host iovec readback) and `_start` (the
    /// WASI entry point). Phoenix functions are not exported — they
    /// only exist for internal call resolution.
    pub(super) fn emit_exports(&mut self) {
        self.exports
            .export("memory", wasm_encoder::ExportKind::Memory, 0);
        if let Some(start_idx) = self.start_idx {
            self.exports
                .export("_start", wasm_encoder::ExportKind::Func, start_idx);
        }
    }

    /// Emit each concrete Phoenix function's body in declaration
    /// order. Delegates per-function lowering to
    /// [`translate::translate_function`].
    pub(super) fn emit_phoenix_bodies(&mut self, ir_module: &IrModule) -> Result<(), CompileError> {
        for func in ir_module.concrete_functions() {
            let body = translate::translate_function(self, ir_module, func)?;
            self.code.function(&body);
        }
        Ok(())
    }

    /// Emit `_start`'s body — call `main`, then return cleanly.
    /// (Future: import `proc_exit` and route panics through it with a
    /// non-zero code. Today main returns void and any internal trap
    /// aborts the instance, which is the right behavior for the MVP.)
    pub(super) fn emit_start_body(&mut self) -> Result<(), CompileError> {
        let main_idx = self
            .phx_main_idx
            .ok_or_else(|| CompileError::new("wasm32-gc: `main` function index not resolved"))?;
        let mut func = wasm_encoder::Function::new([]);
        func.instruction(&wasm_encoder::Instruction::Call(main_idx));
        func.instruction(&wasm_encoder::Instruction::End);
        self.code.function(&func);
        Ok(())
    }

    /// Look up the WASM function index of a Phoenix user function by
    /// its [`FuncId`]. Used by `Op::Call` lowering.
    pub(super) fn require_phx_user_func(&self, id: FuncId) -> Result<u32, CompileError> {
        self.phx_user_funcs.get(&id).copied().ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: `Op::Call({id:?})` references an unknown user function \
                 (internal compiler bug — `declare_phoenix_functions` should have \
                 registered every concrete function before any body is emitted)"
            ))
        })
    }

    /// WASM function index of the WASI `fd_write` import. Used by the
    /// inline `print(Bool)` lowering in `translate.rs` to call
    /// `fd_write` directly without going through a synthesized helper.
    pub(super) fn require_fd_write_idx(&self) -> Result<u32, CompileError> {
        self.fd_write_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `fd_write` index requested before \
                 `declare_imports` ran (internal compiler bug — the \
                 pipeline must call `declare_imports` for any module that \
                 prints, including `print(Bool)`)",
            )
        })
    }

    /// Index of the synthesized `phx_print_i64` helper.
    pub(super) fn require_print_i64_idx(&self) -> Result<u32, CompileError> {
        self.print_i64_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `phx_print_i64` helper index requested before \
                 `declare_print_helper` ran (internal compiler bug)",
            )
        })
    }

    /// Index of the synthesized `phx_print_f64` helper. See §Phase 2.4
    /// decision K.6.
    pub(super) fn require_print_f64_idx(&self) -> Result<u32, CompileError> {
        self.print_f64_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `phx_print_f64` helper index requested before \
                 `declare_print_f64_helper` ran with `needs_print_f64 = true` \
                 (internal compiler bug — `scan_helper_needs` missed a \
                 `print(Float)` call site)",
            )
        })
    }

    /// Synthesize the `phx_print_f64` helper (and its Ryu d2s
    /// sub-helpers + power-of-5 data segments) if `needs.print_f64`.
    /// The helper depends on `fd_write` (for emitting digits), so this
    /// method runs after `declare_imports`.
    pub(super) fn declare_print_f64_helper(
        &mut self,
        needs: HelperNeeds,
    ) -> Result<(), CompileError> {
        if !needs.print_f64 {
            return Ok(());
        }
        let fd_write_idx = self.fd_write_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `phx_print_f64` needs `fd_write`, but \
                 `declare_imports` did not run (internal compiler bug)",
            )
        })?;
        self.print_f64_idx = Some(super::float_helpers::synthesize_print_f64(
            self,
            fd_write_idx,
        )?);
        Ok(())
    }

    /// Index of the synthesized `phx_fmod` helper.
    pub(super) fn require_fmod_idx(&self) -> Result<u32, CompileError> {
        self.fmod_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `phx_fmod` helper index requested before \
                 `declare_fmod_helper` ran with `needs.fmod = true` \
                 (internal compiler bug — `scan_helper_needs` missed an \
                 `Op::FMod` site)",
            )
        })
    }

    /// Synthesize the `phx_fmod` helper if `needs.fmod`. Unlike the
    /// print helpers it has no `fd_write` dependency (pure function),
    /// so it can be declared whether or not the module prints — the
    /// only ordering constraint is the immediate-emit-before-deferred-
    /// body invariant shared by every helper.
    pub(super) fn declare_fmod_helper(&mut self, needs: HelperNeeds) -> Result<(), CompileError> {
        if !needs.fmod {
            return Ok(());
        }
        self.fmod_idx = Some(super::float_helpers::synthesize_fmod(self));
        Ok(())
    }

    /// Index of the synthesized `phx_print_str` helper.
    pub(super) fn require_print_str_idx(&self) -> Result<u32, CompileError> {
        self.print_str_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `phx_print_str` helper index requested before \
                 `declare_string_helpers` ran with `needs_print_str = true` \
                 (internal compiler bug — `scan_helper_needs` missed a \
                 `print(String)` call site)",
            )
        })
    }

    /// Index of the synthesized `phx_str_concat` helper.
    pub(super) fn require_str_concat_idx(&self) -> Result<u32, CompileError> {
        self.str_concat_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `phx_str_concat` helper index requested before \
                 `declare_string_helpers` ran with `needs_str_concat = true` \
                 (internal compiler bug — `scan_helper_needs` missed an \
                 `Op::StringConcat` site)",
            )
        })
    }

    /// Index of the synthesized `phx_str_cmp` helper. Returns
    /// negative / zero / positive (lex byte compare with offsets).
    pub(super) fn require_str_cmp_idx(&self) -> Result<u32, CompileError> {
        self.str_cmp_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `phx_str_cmp` helper index requested before \
                 `declare_string_helpers` ran with `needs_str_cmp = true` \
                 (internal compiler bug — `scan_helper_needs` missed an \
                 `Op::StringLt` / `Le` / `Gt` / `Ge` site)",
            )
        })
    }

    /// Index of the synthesized `phx_str_substring` helper.
    pub(super) fn require_str_substring_idx(&self) -> Result<u32, CompileError> {
        self.str_substring_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `phx_str_substring` helper index requested before \
                 `declare_string_helpers` ran with `needs_str_substring = true` \
                 (internal compiler bug — `scan_helper_needs` missed a \
                 `BuiltinCall(\"String.substring\")` site)",
            )
        })
    }

    /// Index of the synthesized `phx_str_length` helper — returns the
    /// char count (code-point count) as i64. See the K.2 correction
    /// note for why this is a helper, not an inline `struct.get`.
    pub(super) fn require_str_length_idx(&self) -> Result<u32, CompileError> {
        self.str_length_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `phx_str_length` helper index requested before \
                 `declare_string_helpers` ran with `needs_str_length = true` \
                 (internal compiler bug — `scan_helper_needs` missed a \
                 `BuiltinCall(\"String.length\")` site)",
            )
        })
    }

    /// Index of the synthesized `phx_str_eq` helper.
    pub(super) fn require_str_eq_idx(&self) -> Result<u32, CompileError> {
        self.str_eq_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `phx_str_eq` helper index requested before \
                 `declare_string_helpers` ran with `needs_str_eq = true` \
                 (internal compiler bug — `scan_helper_needs` missed an \
                 `Op::StringEq` / `Op::StringNe` site)",
            )
        })
    }

    /// Synthesize whichever string helpers `needs` flags. Each helper
    /// is emitted in dependency order: `phx_print_str` and
    /// `phx_str_concat` are independent of each other but both depend
    /// on the `$bytes` / `$string` types being declared (so
    /// [`Self::declare_string_types`] must run first). The function
    /// indices are stored on `self` for the translator to look up at
    /// call sites.
    ///
    /// **Helper synthesis order matters** because the function /
    /// code-section invariant (every immediate-emit helper precedes
    /// every declare-now / emit-later function) requires helpers to
    /// land before `declare_phoenix_functions` and `declare_start`.
    /// All four string helpers are declare-and-emit-in-one-call, so
    /// the order among them is free; pick a consistent one for stable
    /// diffs.
    ///
    /// The instruction-level synthesis lives in
    /// [`super::string_helpers`]; this method is the thin dispatcher
    /// that decides which helpers to emit and records their indices.
    pub(super) fn declare_string_helpers(
        &mut self,
        needs: HelperNeeds,
    ) -> Result<(), CompileError> {
        if needs.print_str {
            let fd_write_idx = self.fd_write_idx.ok_or_else(|| {
                CompileError::new(
                    "wasm32-gc: `phx_print_str` needs `fd_write`, but \
                     `declare_imports` did not run (internal compiler bug)",
                )
            })?;
            self.print_str_idx = Some(string_helpers::synthesize_print_str(self, fd_write_idx)?);
        }
        if needs.str_concat {
            self.str_concat_idx = Some(string_helpers::synthesize_str_concat(self)?);
        }
        if needs.str_eq {
            self.str_eq_idx = Some(string_helpers::synthesize_str_eq(self)?);
        }
        if needs.str_cmp {
            self.str_cmp_idx = Some(string_helpers::synthesize_str_cmp(self)?);
        }
        if needs.str_substring {
            self.str_substring_idx = Some(string_helpers::synthesize_str_substring(self)?);
        }
        if needs.str_length {
            self.str_length_idx = Some(string_helpers::synthesize_str_length(self)?);
        }
        Ok(())
    }

    /// Finalize the module and return the raw bytes. Section order
    /// follows the WASM spec.
    pub(super) fn finish(self) -> Result<Vec<u8>, CompileError> {
        // The function section (signatures) and code section (bodies)
        // must stay positionally parallel: `code[i]` is the body of the
        // i-th local function. That holds only because every
        // immediate-emit helper (`phx_print_i64`, declared *and*
        // code-emitted in one call) precedes the declare-now/emit-later
        // functions (`main`, `_start`). Guard the invariant as a hard
        // error (not a `debug_assert!`) so a future helper that breaks
        // the ordering fails identically in release rather than silently
        // emitting a module whose signatures and bodies are misaligned.
        if self.functions.len() != self.code.len() {
            return Err(CompileError::new(format!(
                "wasm32-gc: function/code section length mismatch ({} sigs vs \
                 {} bodies) — an immediate-emit helper was likely declared after \
                 a deferred-body function (internal compiler bug)",
                self.functions.len(),
                self.code.len(),
            )));
        }
        let mut module = wasm_encoder::Module::new();
        module.section(self.types.section());
        module.section(&self.imports);
        module.section(&self.functions);
        module.section(&self.memories);
        module.section(&self.exports);
        // The DataCount section is REQUIRED by WASM validation when
        // any instruction references a data segment (`array.new_data`,
        // `memory.init`, `data.drop`). Validators read DataCount
        // up-front so they can verify segment-index operands before
        // they encounter the Data section itself. Emit only when we
        // actually have data segments — a module with no string
        // literals carries none.
        if self.data_segment_count > 0 {
            module.section(&wasm_encoder::DataCountSection {
                count: self.data_segment_count,
            });
        }
        module.section(&self.code);
        module.section(&self.data);
        Ok(module.finish())
    }
}

/// What synthesized helpers / type declarations a given IR module
/// needs — string machinery, `phx_print_f64`, and `phx_fmod` alike.
/// Populated by [`scan_helper_needs`]. The fields are strict subsets
/// — `print_str` implies `string_types`, `str_concat` implies
/// `string_types`, etc. — so the field-by-field check at synthesis
/// time stays simple (each helper checks only its own flag) and the
/// scanner is the single place that knows the dependency graph.
#[derive(Default, Clone, Copy)]
pub(super) struct HelperNeeds {
    /// True iff `$bytes` and `$string` must be declared. Set whenever
    /// any `IrType::StringRef` appears anywhere in the module — as an
    /// instruction's `result_type`, a function param/return, or a
    /// block param.
    pub(super) string_types: bool,
    /// True iff at least one `BuiltinCall("print", args)` site has an
    /// `args[0]` whose IR type is `StringRef`.
    pub(super) print_str: bool,
    /// True iff at least one `Op::StringConcat` appears.
    pub(super) str_concat: bool,
    /// True iff at least one `Op::StringEq` or `Op::StringNe` appears.
    pub(super) str_eq: bool,
    /// True iff at least one `Op::StringLt` / `Le` / `Gt` / `Ge`
    /// appears. Drives synthesis of `phx_str_cmp`. See §Phase 2.4
    /// decision K.3.
    pub(super) str_cmp: bool,
    /// True iff at least one `BuiltinCall("String.substring", _)`
    /// appears. Drives synthesis of `phx_str_substring`.
    pub(super) str_substring: bool,
    /// True iff at least one `BuiltinCall("String.length", _)` appears.
    /// Drives synthesis of `phx_str_length` (char-count walk — matches
    /// the runtime's char-indexed length semantics; see the K.2
    /// correction note).
    pub(super) str_length: bool,
    /// True iff at least one `BuiltinCall("print", args)` site has an
    /// `args[0]` whose IR type is `Bool`. Drives emission of the
    /// `"true\n"` / `"false\n"` active data segments and gates the
    /// inline if/else lowering in `translate_print`.
    pub(super) print_bool: bool,
    /// True iff at least one `BuiltinCall("print", args)` site has an
    /// `args[0]` whose IR type is `F64`. Drives synthesis of the
    /// inline `phx_print_f64` helper (Ryu d2s + special cases) — see
    /// §Phase 2.4 decision K.6.
    pub(super) print_f64: bool,
    /// True iff at least one `Op::FMod` (Float `%`) appears. Drives
    /// synthesis of the `phx_fmod` helper (musl `fmod` port — WASM has
    /// no `f64.rem` instruction). See §Phase 2.4 decision K.5.
    pub(super) fmod: bool,
}

/// Scan the IR module to determine which synthesized helpers and
/// types need to be emitted. Walks every concrete function once and
/// flips the relevant flag for each op kind encountered.
///
/// Resolving a `print(String)` site's argument type needs a
/// `ValueId → IrType` map for the function, but most functions never
/// print (and many never touch strings at all), so the map is built
/// lazily — only the first `print(...)` call site in a function pays
/// for it, via [`build_vid_type_map`].
///
/// A module that doesn't use strings returns `HelperNeeds::default()`
/// — all fields `false` — which the pipeline reads as "skip all
/// string-related declarations entirely" so a string-free module
/// carries no `$bytes` / `$string` types and no dead helper bodies.
pub(super) fn scan_helper_needs(ir_module: &IrModule) -> HelperNeeds {
    let mut needs = HelperNeeds::default();
    for func in ir_module.concrete_functions() {
        // Built on first use (see the lazy `get_or_insert_with` below).
        let mut vid_types: Option<HashMap<ValueId, IrType>> = None;
        // Function-level signatures contribute too: if a function takes
        // or returns a `StringRef`, the string types have to be
        // declared so the signature can encode them.
        if func.return_type == IrType::StringRef || func.param_types.contains(&IrType::StringRef) {
            needs.string_types = true;
        }
        for block in &func.blocks {
            if block.params.iter().any(|(_, t)| *t == IrType::StringRef) {
                needs.string_types = true;
            }
            for instr in &block.instructions {
                if instr.result_type == IrType::StringRef {
                    needs.string_types = true;
                }
                match &instr.op {
                    Op::ConstString(_) => {
                        needs.string_types = true;
                    }
                    Op::FMod(_, _) => {
                        needs.fmod = true;
                    }
                    Op::StringConcat(_, _) => {
                        needs.string_types = true;
                        needs.str_concat = true;
                    }
                    Op::StringEq(_, _) | Op::StringNe(_, _) => {
                        needs.string_types = true;
                        needs.str_eq = true;
                    }
                    Op::StringLt(_, _)
                    | Op::StringLe(_, _)
                    | Op::StringGt(_, _)
                    | Op::StringGe(_, _) => {
                        needs.string_types = true;
                        needs.str_cmp = true;
                    }
                    Op::BuiltinCall(name, args) if name == "print" => {
                        let vid_types = vid_types.get_or_insert_with(|| build_vid_type_map(func));
                        if let Some(arg_vid) = args.first() {
                            match vid_types.get(arg_vid) {
                                Some(IrType::StringRef) => {
                                    needs.string_types = true;
                                    needs.print_str = true;
                                }
                                Some(IrType::Bool) => {
                                    needs.print_bool = true;
                                }
                                Some(IrType::F64) => {
                                    needs.print_f64 = true;
                                }
                                _ => {}
                            }
                        }
                    }
                    Op::BuiltinCall(name, _) if name == "String.length" => {
                        // `String.length` returns the code-point count
                        // (Phoenix's char-indexed semantics, matching
                        // the runtime's `s.chars().count()`), so it
                        // needs the walk helper, not the field load.
                        // See the K.2 correction note in
                        // docs/design-decisions.md.
                        needs.string_types = true;
                        needs.str_length = true;
                    }
                    Op::BuiltinCall(name, _) if name == "String.substring" => {
                        needs.string_types = true;
                        needs.str_substring = true;
                    }
                    _ => {}
                }
            }
            // Block-params and per-block scanning above covers all
            // value-producing sites; terminators carry no types.
        }
    }
    needs
}

/// Build a `ValueId → IrType` map for one function by combining (a)
/// entry-block params (which alias function parameters), (b)
/// non-entry block params, and (c) every instruction's `result_type`.
/// SSA guarantees each ValueId is defined exactly once, so the map
/// is unambiguous.
fn build_vid_type_map(func: &phoenix_ir::module::IrFunction) -> HashMap<ValueId, IrType> {
    let mut map = HashMap::new();
    for block in &func.blocks {
        for (vid, ty) in &block.params {
            map.insert(*vid, ty.clone());
        }
        for instr in &block.instructions {
            if let Some(vid) = instr.result {
                map.insert(vid, instr.result_type.clone());
            }
        }
    }
    map
}

/// Map one Phoenix field's `IrType` to a WASM-GC `FieldType` for the
/// containing struct's nominal declaration. Slice 3 only supports
/// primitive-typed fields (Int / Float / Bool); nested struct / list /
/// map / enum / closure / string field types are rejected with a
/// per-slice diagnostic — each needs its own follow-up sub-decision
/// before the layout can be pinned (e.g. nested struct fields require
/// the inner struct to be declared first in the type section; lists
/// need the `(array T)` mapping settled). Mutability is unconditional:
/// Phoenix supports `p.x = 5` and has no syntax to mark a field
/// immutable. See §Phase 2.4 decision K.1.
fn wasm_field_type_for(
    struct_name: &str,
    field_name: &str,
    field_ty: &IrType,
) -> Result<wasm_encoder::FieldType, CompileError> {
    let val_type = match field_ty {
        IrType::I64 => wasm_encoder::ValType::I64,
        IrType::F64 => wasm_encoder::ValType::F64,
        IrType::Bool => wasm_encoder::ValType::I32,
        other => {
            return Err(CompileError::new(format!(
                "wasm32-gc slice 3: struct `{struct_name}` field \
                 `{field_name}` has type `{other:?}`, but the slice only \
                 supports primitive fields (Int / Float / Bool). Nested \
                 struct / list / map / enum / closure / string fields \
                 land in follow-up slices (each carries its own \
                 type-mapping sub-decision under §Phase 2.4 decision K)"
            )));
        }
    };
    Ok(wasm_encoder::FieldType {
        element_type: wasm_encoder::StorageType::Val(val_type),
        mutable: true,
    })
}

/// Emit a `fd_write(1, IOVEC_OFFSET, 1, NWRITTEN_OFFSET); drop`
/// sequence onto `func`. Factored out so the staging-area constants and
/// the `drop`-of-result convention live in one place; shared by
/// `phx_print_i64` here and `phx_print_str` in [`super::string_helpers`].
pub(super) fn emit_fd_write_call(func: &mut wasm_encoder::Function, fd_write_idx: u32) {
    func.instruction(&wasm_encoder::Instruction::I32Const(1)); // stdout
    func.instruction(&wasm_encoder::Instruction::I32Const(IOVEC_OFFSET as i32));
    func.instruction(&wasm_encoder::Instruction::I32Const(1));
    func.instruction(&wasm_encoder::Instruction::I32Const(NWRITTEN_OFFSET as i32));
    func.instruction(&wasm_encoder::Instruction::Call(fd_write_idx));
    func.instruction(&wasm_encoder::Instruction::Drop);
}
