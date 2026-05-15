//! Function-signature interning for the WASM type section.
//!
//! WASM's type section is keyed by position; two functions with the
//! same `(params) -> (returns)` shape can share a single type-section
//! entry. `TypeInterner` enforces that sharing so PR 2's small module
//! emits a minimal type section, and PR 3's larger surface doesn't
//! quadratically grow it.

use wasm_encoder::{TypeSection, ValType};

/// Interned `(params, returns)` signature. The owning `Box<[ValType]>`
/// shape keeps each cache entry to a single small allocation and
/// avoids the `Vec` capacity tail.
type InternedSig = (Box<[ValType]>, Box<[ValType]>);

#[derive(Default)]
pub(super) struct TypeInterner {
    section: TypeSection,
    /// Linear-scan table indexed by insertion order. PR 2/3 modules
    /// have a handful of distinct signatures, so a `Vec` is faster
    /// than the `HashMap` allocation overhead and sidesteps the
    /// `Borrow`-tuple awkwardness that would otherwise force us to
    /// allocate the lookup key on every call. If PR 5+ grows the type
    /// section past a dozen-ish entries, switch to a hash-the-slice-
    /// pair approach (`raw_entry_mut` once stable, or a manual
    /// `Hasher` impl).
    seen: Vec<InternedSig>,
}

impl TypeInterner {
    /// Return the type-section index for `(params) -> (returns)`,
    /// inserting a fresh entry if this shape hasn't been seen.
    ///
    /// Cache hits avoid allocating: the slice pair is compared
    /// against existing entries by reference, and the owning
    /// `Box<[ValType]>` is only built when the signature is genuinely
    /// new.
    pub(super) fn intern(&mut self, params: &[ValType], returns: &[ValType]) -> u32 {
        if let Some(idx) = self
            .seen
            .iter()
            .position(|(p, r)| &p[..] == params && &r[..] == returns)
        {
            return idx as u32;
        }
        let idx = self.section.len();
        self.section
            .ty()
            .function(params.iter().copied(), returns.iter().copied());
        self.seen.push((params.into(), returns.into()));
        idx
    }

    pub(super) fn section(&self) -> &TypeSection {
        &self.section
    }
}
