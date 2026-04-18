//! Shared layout constants and sizing helpers for collection types.
//!
//! These must be kept in sync with the runtime's `list_methods.rs`
//! (`HEADER_SIZE = 24`) and `map_methods.rs` (`HEADER_SIZE = 32`).

use phoenix_ir::types::IrType;

use super::helpers::slots_for_type;

/// Size of a single value slot in bytes.  All IR values are stored in
/// 8-byte-aligned slots: scalars occupy 1 slot, `StringRef` fat pointers
/// occupy 2 slots.
///
/// This constant must match the alignment used by `phx_alloc` in the runtime
/// (which allocates with 8-byte alignment).
pub(super) const SLOT_SIZE: usize = 8;

/// Compute the element size in bytes for a list/map element type.
///
/// `StringRef` is a fat pointer (ptr + len = 16 bytes).
/// All other types occupy a single 8-byte slot.
///
/// # Note
///
/// If `ty` is a generic placeholder (`StructRef("__generic")`), this returns
/// 8 (one slot).  This is correct for empty collections that never access
/// elements, but would be wrong for `StringRef` (which needs 16).  The
/// `slots_for_type` function's explicit match ensures new multi-slot types
/// cause a compile error rather than silently returning 1.
pub(super) fn elem_size_bytes(ty: &IrType) -> i64 {
    (slots_for_type(ty) * SLOT_SIZE) as i64
}

/// List header size in bytes (length + capacity + elem_size).
pub(super) const LIST_HEADER: i32 = 24;

/// Map header size in bytes (length + capacity + key_size + val_size).
pub(super) const MAP_HEADER: i32 = 32;

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the compiler's header constants match the runtime's.
    /// A mismatch would cause silent memory corruption at runtime.
    #[test]
    fn layout_constants_match_runtime() {
        assert_eq!(
            LIST_HEADER as usize,
            phoenix_runtime::list_header_size(),
            "LIST_HEADER mismatch between compiler and runtime"
        );
        assert_eq!(
            MAP_HEADER as usize,
            phoenix_runtime::map_header_size(),
            "MAP_HEADER mismatch between compiler and runtime"
        );
    }
}
