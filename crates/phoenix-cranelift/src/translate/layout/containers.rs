//! List container-header size and element sizing.
//!
//! `LIST_HEADER` is referenced by codegen and pinned against
//! `phoenix_runtime::list_header_size()` in the test below — a mismatch
//! would cause silent memory corruption. The map header has no codegen
//! caller (every map op is a runtime call), so the runtime owns its
//! header-size invariant.

use phoenix_ir::types::IrType;

use super::TypeLayout;

/// List header size in bytes (length + capacity + elem_size).
pub(in crate::translate) const LIST_HEADER: i32 = 24;

/// Compute the element size in bytes for a list/map element type.
///
/// `StringRef` is a fat pointer (ptr + len = 16 bytes). All other types
/// occupy a single 8-byte slot.
///
/// # Note
///
/// If `ty` is a generic placeholder (`StructRef("__generic")`), this
/// returns 8 (one slot). This is correct for empty collections that never
/// access elements, but would be wrong for `StringRef` (which needs 16).
/// The exhaustive match in [`TypeLayout::of`] ensures a new multi-slot IR
/// variant forces a compile error rather than silently returning 1.
pub(in crate::translate) fn elem_size_bytes(ty: &IrType) -> i64 {
    TypeLayout::of(ty).size_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elem_size_bytes_matches_type_layout() {
        assert_eq!(elem_size_bytes(&IrType::I64), 8);
        assert_eq!(elem_size_bytes(&IrType::F64), 8);
        assert_eq!(elem_size_bytes(&IrType::Bool), 8);
        assert_eq!(elem_size_bytes(&IrType::StringRef), 16);
        assert_eq!(
            elem_size_bytes(&IrType::StructRef("X".into(), Vec::new())),
            8
        );
        assert_eq!(elem_size_bytes(&IrType::Void), 0);
    }

    /// Verify that the compiler's `LIST_HEADER` constant matches the
    /// runtime's. A mismatch would cause silent memory corruption at
    /// runtime. The map header used to be mirrored here too, but
    /// codegen no longer indexes past it (every map op is a runtime
    /// call), so the runtime owns that invariant and pins it in its
    /// own tests.
    #[test]
    fn list_header_matches_runtime() {
        assert_eq!(
            LIST_HEADER as usize,
            phoenix_runtime::list_header_size(),
            "LIST_HEADER mismatch between compiler and runtime"
        );
    }
}
