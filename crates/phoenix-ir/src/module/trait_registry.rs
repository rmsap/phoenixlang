//! IR-level trait metadata and vtable types used by `dyn Trait` lowering
//! and dispatch.
//!
//! This submodule owns the data shapes the `dyn Trait` pipeline reads
//! from across crates — sema lowers into them, the verifier checks them,
//! the IR interpreter and the Cranelift backend dispatch through them.
//! The parent [`crate::module::IrModule`] holds instances:
//! `traits: HashMap<String, IrTraitInfo>` and
//! `dyn_vtables: HashMap<(String, String), DynVtable>`.

use crate::instruction::FuncId;
use crate::types::IrType;

/// IR-level trait metadata: method slot table for `dyn Trait` dispatch.
///
/// Constructed during IR lowering from sema's `TraitInfo` for object-safe
/// traits (non-object-safe traits are omitted — they cannot appear in
/// `DynRef` positions, so no IR consumer needs their signatures). See
/// [`crate::module::IrModule::traits`] for ownership and lifecycle.
#[derive(Debug, Clone)]
pub struct IrTraitInfo {
    /// Method signatures in declaration order.  The index of a method in
    /// this vector is its vtable slot index — the same index carried by
    /// every `Op::DynCall` and the same ordering contract pinned by
    /// [`crate::module::IrModule::dyn_vtables`] entries.  Do not sort,
    /// reorder, or de-duplicate.
    pub methods: Vec<IrTraitMethod>,
}

/// One method slot in an [`IrTraitInfo`].  Parameters exclude `self`.
#[derive(Debug, Clone)]
pub struct IrTraitMethod {
    /// The method name.
    pub name: String,
    /// The IR-level parameter types (excluding the implicit `self`
    /// receiver, which is always a pointer-sized data pointer at the
    /// vtable ABI level).
    pub param_types: Vec<IrType>,
    /// The IR-level return type.
    pub return_type: IrType,
}

impl IrTraitInfo {
    /// Number of methods declared on this trait — the slot count of any
    /// vtable emitted for it.
    pub fn slot_count(&self) -> usize {
        self.methods.len()
    }

    /// Look up the `(param_types, return_type)` of the method at
    /// `slot_idx`. Returns `None` for an out-of-range index.
    pub fn method_signature(&self, slot_idx: usize) -> Option<(&[IrType], &IrType)> {
        let m = self.methods.get(slot_idx)?;
        Some((&m.param_types, &m.return_type))
    }
}

/// One vtable entry in a [`DynVtable`]: the method name (kept alongside
/// the function pointer for debug display and runtime error messages)
/// and the [`FuncId`] of the concrete implementation.
pub type VtableEntry = (String, FuncId);

/// A `dyn Trait` vtable: ordered list of `(method_name, FuncId)` pairs,
/// one per trait-declared method, in declaration order.
///
/// **Slot-index contract:** `entries[i]` corresponds to
/// [`IrTraitInfo::methods`]`[i]` — i.e. the index of the entry IS the
/// vtable slot index. Every `Op::DynCall` carries this pre-resolved
/// index, and codegen is a direct `vtable_ptr[i * POINTER_SIZE]` load.
/// Do not sort, reorder, or de-duplicate entries.
pub type DynVtable = Vec<VtableEntry>;
