//! Function-signature interning for the WASM type section.
//!
//! WASM's type section is keyed by position; two functions with the
//! same `(params) -> (returns)` shape can share a single type-section
//! entry. `TypeInterner` enforces that sharing so PR 2's small module
//! emits a minimal type section, and PR 3's larger surface doesn't
//! quadratically grow it.

use wasm_encoder::{FieldType, TypeSection, ValType};

/// Interned `(params, returns)` signature paired with the
/// type-section index it landed at. Storing the index alongside the
/// shape (rather than deriving it from the Vec position) keeps the
/// cache correct when *other* type-section entries precede the
/// function signatures — e.g. wasm32-gc declares all its nominal
/// `(struct …)` types via `TypeInterner::declare_struct` before any
/// signature is interned, so the first interned signature lands at a
/// non-zero index and Vec position no longer equals type-section
/// index. (Nothing currently declares struct types *between* two
/// signatures, but carrying the real index stays correct if that ever
/// changes.) The owning `Box<[ValType]>` shape keeps each cache entry
/// to a single small allocation and avoids the `Vec` capacity tail.
type InternedSig = (Box<[ValType]>, Box<[ValType]>, u32);

#[derive(Default)]
pub(super) struct TypeInterner {
    section: TypeSection,
    /// Linear-scan table of function-signature shapes, with each
    /// entry carrying its real type-section index (since struct
    /// declarations precede the signatures). PR 2/3 modules have a handful of
    /// distinct signatures, so a `Vec` is faster than the `HashMap`
    /// allocation overhead and sidesteps the `Borrow`-tuple awkwardness
    /// that would otherwise force us to allocate the lookup key on
    /// every call. If PR 5+ grows the type section past a dozen-ish
    /// entries, switch to a hash-the-slice-pair approach (`raw_entry_mut`
    /// once stable, or a manual `Hasher` impl).
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
        if let Some((_, _, idx)) = self
            .seen
            .iter()
            .find(|(p, r, _)| &p[..] == params && &r[..] == returns)
        {
            return *idx;
        }
        let idx = self.section.len();
        self.section
            .ty()
            .function(params.iter().copied(), returns.iter().copied());
        self.seen.push((params.into(), returns.into(), idx));
        idx
    }

    pub(super) fn section(&self) -> &TypeSection {
        &self.section
    }

    /// Declare a nominal WASM-GC struct type with the given field
    /// layout and return its type-section index.
    ///
    /// Unlike [`Self::intern`], no dedup is performed — WASM-GC's type
    /// system is nominal, so two Phoenix structs with identical field
    /// shapes (`Point { Int, Int }` and `Pixel { Int, Int }`) must
    /// declare *separate* WASM struct types. Sharing the WASM type
    /// would erase the Phoenix-level distinction and break any future
    /// `dyn Trait` / pattern-match dispatch that relies on
    /// per-Phoenix-struct identity.
    ///
    /// Only used by the wasm32-gc backend. The wasm32-linear backend
    /// never calls this — its struct-typed values are tagged offsets
    /// into linear memory, not WASM-GC managed refs.
    pub(super) fn declare_struct(&mut self, fields: &[FieldType]) -> u32 {
        let idx = self.section.len();
        self.section.ty().struct_(fields.iter().cloned());
        idx
    }
}
