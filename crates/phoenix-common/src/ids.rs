//! Stable post-sema identifiers shared across the compiler pipeline.
//!
//! These newtypes index the `Vec`-backed tables on
//! `phoenix_sema::ResolvedModule` (functions, structs, enums, traits)
//! and flow unchanged through IR lowering, monomorphization, the IR
//! interpreter, and the Cranelift backend.
//!
//! # Allocation contract
//!
//! Allocation happens inside `phoenix_sema::checker::Checker` during
//! the registration pass (specifically, `pre_allocate_function_ids`
//! and `pre_allocate_user_method_ids`, both invoked by
//! `Checker::check_program` before any signature is registered).
//! Ids are assigned in AST-declaration order and are never
//! reassigned: free functions occupy `FuncId(0..N)` in source order,
//! user-declared methods occupy `FuncId(N..N+M)` in source order
//! (with inline methods on a struct/enum visited at the type's
//! declaration site, inherent first then trait impls in source
//! order, and standalone `impl` blocks visited at their own
//! declaration site).
//!
//! IR lowering does **not** re-walk the AST to assign ids. It
//! consumes [`phoenix_sema::ResolvedModule`]'s id-indexed tables
//! directly (`functions: Vec<FunctionInfo>` and
//! `user_methods: Vec<MethodInfo>`), so the two id spaces agree by
//! construction. Synthesized callables (closures, generic
//! specializations) are appended past the user-method range during
//! IR lowering and monomorphization.
//!
//! # Reserved id zones
//!
//! - [`EnumId(0)`] is built-in `Option`; [`EnumId(1)`] is built-in
//!   `Result`. User-declared enums start at `EnumId(2)`. The
//!   reserved zone is fixed at two — adding a third built-in enum
//!   requires updating `phoenix_sema::resolved::build_from_checker`
//!   so it places the new built-in ahead of user enums.
//! - No other id type has a reserved zone; user-declared functions,
//!   structs, and traits start at id 0.
//!
//! # Layout
//!
//! Each id type is a transparent `u32` wrapper so it round-trips
//! through `Vec` indexing without arithmetic cost and through hash
//! maps without boxing. The `Display` impls use a single-letter
//! prefix (`f0`, `s0`, `e0`, `t0`) so pretty-printed IR stays
//! compact.

use std::fmt;

/// Stable identifier for a callable function within a resolved
/// module.  Spans both free functions (`FuncId(0..N)`) and
/// user-declared methods (`FuncId(N..N+M)`), with the boundary
/// recorded as `ResolvedModule::user_method_offset`.  See the
/// module-level docs for the allocation contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FuncId(
    /// Raw u32 index. Public for transparent IR interop; prefer
    /// [`FuncId::index`] when indexing a `Vec`.
    pub u32,
);

impl FuncId {
    /// Convert to a `usize` suitable for indexing the corresponding
    /// `Vec`. Equivalent to `self.0 as usize` but spelled in a way
    /// that documents intent at the call site.
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for FuncId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "f{}", self.0)
    }
}

/// Stable identifier for a user-declared struct declaration within a
/// resolved module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StructId(
    /// Raw u32 index. Public for transparent IR interop; prefer
    /// [`StructId::index`] when indexing a `Vec`.
    pub u32,
);

impl StructId {
    /// Convert to a `usize` suitable for indexing the corresponding `Vec`.
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for StructId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "s{}", self.0)
    }
}

/// Stable identifier for a user-declared enum declaration within a
/// resolved module.  Built-in `Option` is `EnumId(0)` and built-in
/// `Result` is `EnumId(1)`; user-declared enums follow in
/// declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EnumId(
    /// Raw u32 index. Public for transparent IR interop; prefer
    /// [`EnumId::index`] when indexing a `Vec`.
    pub u32,
);

impl EnumId {
    /// Convert to a `usize` suitable for indexing the corresponding `Vec`.
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// `EnumId` reserved for the built-in `Option<T>` enum.  Pinned at
/// construction time by `phoenix_sema::resolved::build_from_checker`;
/// user-declared enums start at [`FIRST_USER_ENUM_ID`].
pub const OPTION_ENUM_ID: EnumId = EnumId(0);

/// `EnumId` reserved for the built-in `Result<T, E>` enum.  Pinned at
/// construction time by `phoenix_sema::resolved::build_from_checker`;
/// user-declared enums start at [`FIRST_USER_ENUM_ID`].
pub const RESULT_ENUM_ID: EnumId = EnumId(1);

/// First `EnumId` available for user-declared enums.  Equal to the
/// number of reserved built-in enum slots
/// ([`OPTION_ENUM_ID`] + [`RESULT_ENUM_ID`] = 2).  Adding a third
/// built-in enum requires updating this constant *and* the placement
/// loop in `phoenix_sema::resolved::build_from_checker`.
pub const FIRST_USER_ENUM_ID: EnumId = EnumId(2);

impl fmt::Display for EnumId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "e{}", self.0)
    }
}

/// Stable identifier for a user-declared trait declaration within a
/// resolved module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TraitId(
    /// Raw u32 index. Public for transparent IR interop; prefer
    /// [`TraitId::index`] when indexing a `Vec`.
    pub u32,
);

impl TraitId {
    /// Convert to a `usize` suitable for indexing the corresponding `Vec`.
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for TraitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "t{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashMap};

    #[test]
    fn index_round_trips_through_usize() {
        assert_eq!(FuncId(0).index(), 0);
        assert_eq!(FuncId(7).index(), 7);
        assert_eq!(StructId(7).index(), 7);
        assert_eq!(EnumId(7).index(), 7);
        assert_eq!(TraitId(7).index(), 7);
        // The cast is `u32 as usize`; on every platform Rust supports
        // `usize::BITS >= 32` so `u32::MAX` round-trips losslessly.
        assert_eq!(FuncId(u32::MAX).index(), u32::MAX as usize);
    }

    #[test]
    fn display_uses_single_letter_prefix() {
        assert_eq!(FuncId(0).to_string(), "f0");
        assert_eq!(FuncId(42).to_string(), "f42");
        assert_eq!(StructId(3).to_string(), "s3");
        assert_eq!(EnumId(1).to_string(), "e1");
        assert_eq!(TraitId(9).to_string(), "t9");
    }

    #[test]
    fn ids_are_hash_and_eq() {
        let mut m: HashMap<FuncId, &'static str> = HashMap::new();
        m.insert(FuncId(1), "one");
        m.insert(FuncId(2), "two");
        assert_eq!(m.get(&FuncId(1)), Some(&"one"));
        assert_eq!(m.get(&FuncId(2)), Some(&"two"));
        assert_eq!(m.get(&FuncId(3)), None);
        // PartialEq holds across all id types.
        assert_eq!(FuncId(7), FuncId(7));
        assert_ne!(FuncId(7), FuncId(8));
    }

    #[test]
    fn ids_have_total_order_consistent_with_raw_value() {
        let mut s: BTreeSet<FuncId> = BTreeSet::new();
        s.insert(FuncId(3));
        s.insert(FuncId(1));
        s.insert(FuncId(2));
        let collected: Vec<FuncId> = s.into_iter().collect();
        assert_eq!(collected, vec![FuncId(1), FuncId(2), FuncId(3)]);
    }

    #[test]
    fn reserved_enum_id_constants_match_documented_layout() {
        assert_eq!(OPTION_ENUM_ID, EnumId(0));
        assert_eq!(RESULT_ENUM_ID, EnumId(1));
        assert_eq!(FIRST_USER_ENUM_ID, EnumId(2));
        // FIRST_USER_ENUM_ID must be one past the last reserved id;
        // if a future built-in is added, update both this assertion
        // and `build_from_checker`'s placement loop together.
        assert_eq!(FIRST_USER_ENUM_ID.0, RESULT_ENUM_ID.0 + 1);
    }
}
