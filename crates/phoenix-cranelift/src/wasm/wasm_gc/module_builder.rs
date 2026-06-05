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
///
/// User string literals (`Op::ConstString`) will claim a region above
/// the scratch buffer once the `String` slice lands; that slice adds
/// the data cursor and the reservation helper back.
pub(super) const IOVEC_OFFSET: u32 = 8;
const NWRITTEN_OFFSET: u32 = 16;
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

/// Memory pages declared for the module. One 64-KiB page is more than
/// enough for the iovec staging (12 bytes), the `phx_print_i64`
/// scratch (32 bytes), and the `phx_print_str` scratch (4096 bytes)
/// combined. Grows in a later slice if a longer-lines requirement
/// emerges.
const MEMORY_PAGES: u64 = 1;

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

    /// Count of passive data segments emitted so far. Required up
    /// front for the WASM `DataCount` section, which the validator
    /// reads to know how many data segments exist before it sees the
    /// data section itself — `array.new_data` instructions need that
    /// count for validation. Bumped by [`Self::reserve_string_data`].
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
            str_concat_idx: None,
            str_eq_idx: None,
            data_segment_count: 0,
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

    /// Index of the synthesized `phx_print_i64` helper.
    pub(super) fn require_print_i64_idx(&self) -> Result<u32, CompileError> {
        self.print_i64_idx.ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `phx_print_i64` helper index requested before \
                 `declare_print_helper` ran (internal compiler bug)",
            )
        })
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

/// What string helpers / type declarations a given IR module needs.
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
}

/// Scan the IR module to determine which string helpers and types
/// need to be emitted. Walks every concrete function once and flips the
/// relevant flag for each op kind encountered.
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
                    Op::StringConcat(_, _) => {
                        needs.string_types = true;
                        needs.str_concat = true;
                    }
                    Op::StringEq(_, _) | Op::StringNe(_, _) => {
                        needs.string_types = true;
                        needs.str_eq = true;
                    }
                    Op::BuiltinCall(name, args) if name == "print" => {
                        let vid_types = vid_types.get_or_insert_with(|| build_vid_type_map(func));
                        if let Some(arg_vid) = args.first()
                            && vid_types.get(arg_vid) == Some(&IrType::StringRef)
                        {
                            needs.string_types = true;
                            needs.print_str = true;
                        }
                    }
                    Op::BuiltinCall(name, _) if name == "String.length" => {
                        // `String.length` lowers inline as `struct.get
                        // $string $len` — no helper, but it does need
                        // the `$string` type declared.
                        needs.string_types = true;
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
