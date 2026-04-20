//! Layout knowledge for Phoenix reference types and collection containers.
//!
//! Two concerns are colocated here but live in separate submodules:
//!
//! - [`TypeLayout`] (in [`type_layout`]) — single source of truth for how
//!   each `IrType` maps onto Cranelift slots and memory (slot count,
//!   Cranelift-type expansion, load / store codegen).
//! - Container-header sizes (in [`containers`]) — `LIST_HEADER`,
//!   `MAP_HEADER`, and [`elem_size_bytes`] — constants that must match
//!   `phoenix-runtime`.
//!
//! Adding a new reference type is a single match-arm edit in
//! [`TypeLayout::of`]. Load and store are data-driven loops over
//! `cl_types()` so any slot count is handled uniformly.

mod containers;
mod type_layout;

pub(super) use containers::{LIST_HEADER, MAP_HEADER, elem_size_bytes};
pub(crate) use type_layout::TypeLayout;

/// Size of a single value slot in bytes. All IR values are stored in
/// 8-byte-aligned slots: scalars occupy 1 slot, `StringRef` fat pointers
/// occupy 2 slots.
///
/// This constant must match the alignment used by `phx_alloc` in the runtime
/// (which allocates with 8-byte alignment).
pub(super) const SLOT_SIZE: usize = 8;
