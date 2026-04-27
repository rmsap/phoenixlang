//! Type-safe [`ValueId`] allocation paired with the per-value type index.
//!
//! [`ValueIdAllocator`] owns both the `ValueId` counter and the parallel
//! `Vec<IrType>` that stores each value's type. The only way to mint a
//! fresh `ValueId` is via [`ValueIdAllocator::alloc`], which atomically
//! bumps the counter *and* records the type. This makes it structurally
//! impossible for a pass to allocate a `ValueId` without recording its
//! type — the historical `next_value_id` / `value_types` parallel-index
//! invariant becomes a type-level guarantee.

use crate::instruction::ValueId;
use crate::types::IrType;

/// Allocator for SSA [`ValueId`]s within an [`IrFunction`](crate::module::IrFunction).
///
/// Owns the per-value type index. The vector's length *is* the next
/// `ValueId` to be handed out, so allocation and type-recording are a
/// single operation.
#[derive(Debug, Clone, Default)]
pub struct ValueIdAllocator {
    /// Per-value type index: `types[i]` is the IR type of `ValueId(i)`.
    types: Vec<IrType>,
}

impl ValueIdAllocator {
    /// Creates an empty allocator.
    pub fn new() -> Self {
        Self { types: Vec::new() }
    }

    /// Allocate a fresh [`ValueId`] and record its type. The only way to
    /// mint a `ValueId`.
    pub fn alloc(&mut self, ty: IrType) -> ValueId {
        let id = ValueId(self.types.len() as u32);
        self.types.push(ty);
        id
    }

    /// The [`ValueId`] that the next call to [`Self::alloc`] will
    /// hand out. Equivalently: the count of `ValueId`s already
    /// allocated, but expressed in the domain type so callers
    /// don't have to translate between `usize` / `u32` / `ValueId`.
    pub fn next_value_id(&self) -> ValueId {
        ValueId(self.types.len() as u32)
    }

    /// `true` if no [`ValueId`]s have been allocated.
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    /// Look up the recorded type of a [`ValueId`]. Returns `None` only
    /// for an out-of-range index (i.e. a `ValueId` that belongs to a
    /// different function). Within a function, every allocated id has a
    /// type by construction.
    pub fn type_of(&self, value: ValueId) -> Option<&IrType> {
        self.types.get(value.0 as usize)
    }

    /// Apply `f` to every recorded type. Used by passes that substitute
    /// types after the fact (monomorphization).
    pub fn for_each_type_mut(&mut self, mut f: impl FnMut(&mut IrType)) {
        for ty in &mut self.types {
            f(ty);
        }
    }
}
