//! Heap-object layout computation for the wasm32-linear backend.
//!
//! Contains the pure layout machinery (size / alignment / offset math)
//! used by the body translator to lower `Op::StructAlloc` /
//! `Op::StructGetField` / `Op::StructSetField` and the enum-variant
//! family (`Op::EnumAlloc` / `Op::EnumDiscriminant` / `Op::EnumGetField`).
//! Kept separate from [`super::translate`] because none of this depends
//! on the instruction-emission context — it's deterministic over the
//! IR module's `struct_layouts` / `enum_layouts` tables.
//!
//! # Bool widening, 4-byte field alignment
//!
//! Every field is stored at ≥4-byte alignment: `Bool` widens to a
//! 4-byte i32 (0/1), GC pointers are 4 bytes, `StringRef` is two
//! consecutive i32s (8 bytes total), `I64`/`F64` are 8 bytes at 8-byte
//! alignment. This avoids any byte-level load/store at field
//! boundaries.
//!
//! # Discriminant placement (enums)
//!
//! Enums place a 4-byte i32 discriminant at offset 0 and start the
//! payload at offset 4, then naturally align each field. Variant
//! layouts are computed per-site (rather than max-of-variants) because
//! the IR's declared variant-field types can be `GENERIC_PLACEHOLDER`
//! for stdlib generic enums (`Option<T>` / single-payload `Result`),
//! and the alloc / get sides reconstruct identical offsets from the
//! same per-site walk. Multi-field variants with any placeholder are
//! explicitly rejected at the alloc and get sites because the alloc
//! side derives layouts from value-vid types while the get side uses
//! declared types — those can disagree on other-position offsets when
//! a placeholder's actual size differs from its declared one.

use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use crate::error::CompileError;

/// Bytes of bookkeeping at the head of every `phx_list_alloc`-produced
/// list buffer (length, capacity, element-size, GC-header fields).
/// The first element of the data region sits at `LIST_HEADER` from the
/// list pointer; subsequent elements are at `LIST_HEADER + i * elem_size`.
/// The value is pinned against the `phoenix_runtime::list_methods`
/// implementation by the unit test `wasm_list_header_matches_runtime`
/// below (a compile-time `const_assert!` inside the runtime can't see
/// this constant — codegen and runtime are separate crates — so the
/// test runs the lockstep check).
pub(super) const LIST_HEADER: u32 = 24;

/// `true` for IR types that flatten to a single `i32` GC pointer on
/// wasm32 — `StructRef` / `EnumRef` / `ListRef` / `MapRef` /
/// `ClosureRef`. Centralizing the list keeps [`super::translate::wasm_valtypes_for`],
/// [`phx_field_size_bytes`], [`phx_field_align_bytes`], and the
/// field-load / field-store match arms updated in lockstep when a
/// new GC-pointer variant lands.
pub(super) fn is_gc_pointer_type(ty: &IrType) -> bool {
    matches!(
        ty,
        IrType::StructRef(_, _)
            | IrType::EnumRef(_, _)
            | IrType::ListRef(_)
            | IrType::MapRef(_, _)
            | IrType::ClosureRef { .. }
    )
}

/// `true` for IR types whose heap-storage form is a single i32 — i.e.
/// `Bool` (widened to 0/1) and every GC-pointer reference type. The
/// load / store helpers in `super::translate` use this to collapse the
/// `Bool` and `GC-pointer` arms of `emit_field_load` / `emit_field_store`
/// into one guarded match arm so the two paths can't drift apart.
pub(super) fn is_i32_field(ty: &IrType) -> bool {
    matches!(ty, IrType::Bool) || is_gc_pointer_type(ty)
}

/// Size in bytes of a single Phoenix `IrType` when stored as a
/// struct/enum field on the GC heap. Used to compute field offsets
/// in [`compute_struct_layout`] and [`compute_variant_field_offsets`].
///
/// `Bool` widens to 4 bytes (stored as `i32` 0/1) rather than packing
/// to a single byte — keeps every field 4-byte-or-greater aligned,
/// matching the WASM-encoder's natural `i32.store` granularity and
/// removing the need for byte-level load/store at field boundaries.
/// `Void` has no field representation; callers should reject Void-
/// typed struct fields upstream.
pub(super) fn phx_field_size_bytes(ty: &IrType) -> Result<u32, CompileError> {
    match ty {
        IrType::I64 | IrType::F64 => Ok(8),
        IrType::Bool => Ok(4), // padded to i32 for natural alignment
        IrType::StringRef | IrType::DynRef(_) => Ok(8), // 2 × i32 (ptr+len, or data+vtable)
        ty if is_gc_pointer_type(ty) => Ok(4), // single i32 GC pointer
        IrType::Void => Err(CompileError::new(
            "wasm32-linear: `Void` has no field-storage representation \
             (internal: sema/IR should reject Void-typed struct/enum fields)",
        )),
        _ => Err(unsupported(ty, "field-storage size")),
    }
}

/// Natural alignment in bytes of a single Phoenix `IrType` when
/// stored on the GC heap. Mirror of [`phx_field_size_bytes`].
pub(super) fn phx_field_align_bytes(ty: &IrType) -> Result<u32, CompileError> {
    match ty {
        IrType::I64 | IrType::F64 => Ok(8),
        IrType::Bool | IrType::StringRef | IrType::DynRef(_) => Ok(4),
        ty if is_gc_pointer_type(ty) => Ok(4),
        IrType::Void => Err(CompileError::new(
            "wasm32-linear: `Void` has no alignment (internal: sema/IR \
             should reject Void-typed fields)",
        )),
        _ => Err(unsupported(ty, "field-storage alignment")),
    }
}

/// Build a `MemArg` for a `field_offset`-relative access of type `ty`,
/// deriving the WASM alignment hint from `ty`'s natural alignment.
/// Single chokepoint for `super::translate::emit_field_load` /
/// `emit_field_store` so both sides compute identical hints from the
/// same source.
pub(super) fn field_memarg(
    field_offset: u32,
    ty: &IrType,
) -> Result<wasm_encoder::MemArg, CompileError> {
    Ok(wasm_encoder::MemArg {
        offset: field_offset as u64,
        align: align_log2(phx_field_align_bytes(ty)?),
        memory_index: 0,
    })
}

/// Byte width of one `dyn Trait` vtable entry: a single i32
/// function-table index. Couples the two sites that must agree on the
/// layout — vtable emission (`ModuleBuilder::require_dyn_vtable`, one
/// `i32::to_le_bytes` per method) and the `Op::DynCall` dispatch load
/// (`i32.load` at `vtable_ptr + method_idx * DYN_VTABLE_ENTRY_SIZE`).
pub(super) const DYN_VTABLE_ENTRY_SIZE: u32 = 4;

/// `MemArg` for an `i32` access at `offset` (`align: 2` = log2(4)).
/// Used for the fat-pointer `len` half of `StringRef` field accesses
/// and for the enum discriminant at offset 0.
pub(super) fn i32_memarg(offset: u32) -> wasm_encoder::MemArg {
    wasm_encoder::MemArg {
        offset: offset as u64,
        align: 2,
        memory_index: 0,
    }
}

/// `log2` of the alignment used in WASM `MemArg::align` hints. WASM
/// expects the hint as `log2(byte_alignment)` — `i32` (4-byte) is
/// `align: 2`, `i64` (8-byte) is `align: 3`. Helper so call sites
/// don't have to re-derive the log per emission.
pub(super) fn align_log2(byte_align: u32) -> u32 {
    debug_assert!(
        byte_align.is_power_of_two(),
        "alignment must be a power of two: got {byte_align}"
    );
    byte_align.trailing_zeros()
}

/// Round `offset` up to the next multiple of `align`. Used to pad
/// field offsets before each field that requires natural alignment.
pub(super) fn align_up(offset: u32, align: u32) -> u32 {
    debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
    // `offset + align - 1` can overflow a `u32` for pathologically large
    // offsets; struct/enum allocations are bounded well below that in
    // practice, but a regression that fed unbounded offsets here would
    // silently wrap. The assert documents the precondition and catches
    // the wrap in debug builds.
    debug_assert!(
        offset.checked_add(align - 1).is_some(),
        "align_up({offset}, {align}) overflows u32"
    );
    (offset + align - 1) & !(align - 1)
}

/// Layout of a Phoenix struct on the GC heap: per-field byte offset
/// (declaration order) + total struct size in bytes (padded to the
/// max field alignment). Computed from `IrModule::struct_layouts`
/// at codegen time; no runtime side table.
///
/// `Clone` so callers can pull a layout out of the per-function cache
/// by value, freeing the cache's `&mut` borrow for subsequent
/// instruction emission.
#[derive(Clone)]
pub(super) struct StructLayout {
    pub(super) field_offsets: Vec<u32>,
    pub(super) field_types: Vec<IrType>,
    pub(super) total_size: u32,
}

/// Compute the WASM-side struct layout from the IR module's
/// `struct_layouts` table (`HashMap<name, Vec<(field_name, IrType)>>`).
/// Walks fields in declaration order, pads each to its natural
/// alignment, sums sizes. Returns an error if the struct isn't
/// registered or any field has an unrepresentable type.
pub(super) fn compute_struct_layout(
    ir_module: &IrModule,
    struct_name: &str,
) -> Result<StructLayout, CompileError> {
    let fields = ir_module.struct_layouts.get(struct_name).ok_or_else(|| {
        CompileError::new(format!(
            "wasm32-linear: struct `{struct_name}` has no registered layout \
             (internal compiler bug — IR lowering should populate struct_layouts \
             for every concrete struct before codegen)"
        ))
    })?;
    let mut offsets = Vec::with_capacity(fields.len());
    let mut field_types = Vec::with_capacity(fields.len());
    let mut cursor: u32 = 0;
    let mut max_align: u32 = 1;
    for (field_name, ty) in fields {
        // Defense in depth: today's struct monomorphizer
        // (`phoenix_ir::monomorphize::struct_mono`) rewrites every
        // generic struct ref to a concrete specialization before
        // codegen, so a placeholder field type shouldn't reach here.
        // Unlike `Op::EnumGetField`, the struct path has no per-site
        // type substitution to recover from one — `Op::StructAlloc` /
        // `Op::StructSetField` only see the declared field type — so
        // a future monomorphization regression that left a placeholder
        // here would silently size the field at 4 bytes (the ref-type
        // default in `phx_field_size_bytes`) and truncate any
        // I64/F64/StringRef field. Fail loud instead.
        if ty.is_generic_placeholder() {
            return Err(CompileError::new(format!(
                "wasm32-linear: struct `{struct_name}` field `{field_name}` is \
                 unresolved (`GENERIC_PLACEHOLDER`); the struct monomorphizer \
                 should have replaced this with a concrete specialization before \
                 codegen (internal compiler bug)"
            )));
        }
        let align = phx_field_align_bytes(ty)?;
        let size = phx_field_size_bytes(ty)?;
        if align > max_align {
            max_align = align;
        }
        let field_off = align_up(cursor, align);
        offsets.push(field_off);
        field_types.push(ty.clone());
        cursor = field_off + size;
    }
    // Pad the total to the max alignment so an array of this struct
    // would keep each element aligned. Not strictly required for single
    // allocations but cheap and conventional. `max_align` is seeded at
    // 1 and only grows, so it's safe to pass directly to `align_up`.
    let total_size = align_up(cursor, max_align);
    Ok(StructLayout {
        field_offsets: offsets,
        field_types,
        total_size,
    })
}

/// Result of walking one enum variant's field types: per-field byte
/// offsets relative to the allocation base, the end of the payload
/// (first byte past the last field), and the variant's max field
/// alignment folded with the discriminant's 4-byte alignment.
///
/// Callers (`Op::EnumAlloc`) round `payload_end` up to `max_align` to
/// get the total allocation size — matching the tail-padding policy
/// in [`compute_struct_layout`].
pub(super) struct VariantLayout {
    pub(super) field_offsets: Vec<u32>,
    pub(super) payload_end: u32,
    pub(super) max_align: u32,
}

/// Walk a list of concrete field types for one enum variant and
/// return its [`VariantLayout`]. Field 0 starts at offset 4 (after
/// the discriminant); each subsequent field is naturally aligned.
/// `max_align` is folded over `phx_field_align_bytes(ty)` and seeded
/// with the discriminant's 4-byte alignment, so callers can pad the
/// allocation's total size to it (matching the struct path).
///
/// Used by both `Op::EnumAlloc` (with the value vids' actual types)
/// and `Op::EnumGetField` (with the sema-annotated `instr.result_type`
/// substituted in for the requested field) so the alloc and get sides
/// compute identical offsets per use site.
pub(super) fn compute_variant_field_offsets(
    field_types: &[IrType],
) -> Result<VariantLayout, CompileError> {
    let mut offsets = Vec::with_capacity(field_types.len());
    let mut cursor: u32 = 4; // payload starts after the 4-byte discriminant
    let mut max_align: u32 = 4; // discriminant is i32, aligned to 4
    for ty in field_types {
        let size = phx_field_size_bytes(ty)?;
        let align = phx_field_align_bytes(ty)?;
        if align > max_align {
            max_align = align;
        }
        let off = align_up(cursor, align);
        offsets.push(off);
        cursor = off + size;
    }
    Ok(VariantLayout {
        field_offsets: offsets,
        payload_end: cursor,
        max_align,
    })
}

/// Layout of a Phoenix enum on the GC heap, indexed by variant.
///
/// `Op::EnumAlloc` and `Op::EnumGetField` both size and offset their
/// access per-site via [`compute_variant_field_offsets`] (the IR's
/// declared variant field types can be `GENERIC_PLACEHOLDER` for
/// stdlib `Option`/`Result`, so the alloc side uses the value vids'
/// actual types and the get side substitutes `instr.result_type` for
/// the requested field). The EnumLayout itself only needs to surface
/// the declared field types for that per-site walk and to bounds-check
/// variant/field indices — no static `total_size` or per-variant
/// offset table is consulted, and a partial recompute here would
/// silently disagree with the per-site path.
///
/// # Multi-field placeholder variants are rejected at alloc time
///
/// `Op::EnumAlloc` rejects any multi-field variant whose declared
/// field list contains *any* placeholder, because the alloc side
/// derives offsets from value-vid types while the later get side
/// derives them from declared types — those layouts agree only when
/// the variant is either fully concrete or has just one field
/// (single-field placeholder variants are the supported shape:
/// `Option<T>::Some(T)`, `Result<T,_>::Ok(T)`, etc.). `Op::EnumGetField`
/// keeps a matching guard in case any future alloc path constructs a
/// value through a different lowering — the guard makes the failure
/// local to the read site instead of garbled bytes.
#[derive(Clone)]
pub(super) struct EnumLayout {
    /// For each variant: the per-field IR types from the module's
    /// declared layout. May contain `GENERIC_PLACEHOLDER` entries for
    /// stdlib generic enums; callers substitute their own types per
    /// site before handing this list to `compute_variant_field_offsets`.
    pub(super) variant_field_types: Vec<Vec<IrType>>,
}

/// Look up the enum's declared variant-field types from the module.
pub(super) fn compute_enum_layout(
    ir_module: &IrModule,
    enum_name: &str,
) -> Result<EnumLayout, CompileError> {
    let variants = ir_module.enum_layouts.get(enum_name).ok_or_else(|| {
        CompileError::new(format!(
            "wasm32-linear: enum `{enum_name}` has no registered layout \
             (internal compiler bug — IR lowering should populate enum_layouts \
             for every concrete enum before codegen)"
        ))
    })?;
    let variant_field_types = variants
        .iter()
        .map(|(_variant_name, field_types)| field_types.clone())
        .collect();
    Ok(EnumLayout {
        variant_field_types,
    })
}

fn unsupported(ty: &IrType, where_: &str) -> CompileError {
    CompileError::new(format!(
        "wasm32-linear: IR type `{ty:?}` not yet supported in {where_} \
         (Phase 2.4 PR 3 — see docs/design-decisions.md §Phase 2.4)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lockstep check that the wasm32-linear backend's `LIST_HEADER`
    /// matches the runtime's list-header size. The wasm constant is a
    /// separate value from the native backend's `LIST_HEADER` (pinned
    /// by `translate::layout::containers::list_header_matches_runtime`),
    /// so it needs its own assertion: a runtime `HEADER_SIZE` change
    /// would otherwise leave this one silently drifted, causing every
    /// `Op::ListAlloc` store and `phx_list_get_raw` load to disagree on
    /// the data-region offset (silent memory corruption). `LIST_HEADER`
    /// is `pub(super)`, so this lives as a crate-internal unit test
    /// rather than in `tests/compile_wasm_linear.rs`.
    #[test]
    fn wasm_list_header_matches_runtime() {
        assert_eq!(
            LIST_HEADER as usize,
            phoenix_runtime::list_header_size(),
            "wasm32-linear LIST_HEADER mismatch between compiler and runtime"
        );
    }
}
