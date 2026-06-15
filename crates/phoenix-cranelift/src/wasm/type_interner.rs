//! Function-signature interning for the WASM type section.
//!
//! WASM's type section is keyed by position; two functions with the
//! same `(params) -> (returns)` shape can share a single type-section
//! entry. `TypeInterner` enforces that sharing so PR 2's small module
//! emits a minimal type section, and PR 3's larger surface doesn't
//! quadratically grow it.
//!
//! ## Two emission modes
//!
//! - **Immediate** (default; wasm32-linear): each declaration is
//!   written to the [`TypeSection`] as it's interned. wasm32-linear
//!   interns only function signatures (no GC types), and this path is
//!   byte-identical to the pre-rec-group encoding.
//! - **Buffered rec-group** ([`TypeInterner::buffered`]; wasm32-gc):
//!   every declaration is buffered as a [`SubType`] and the whole set
//!   is emitted as one explicit `(rec …)` group by [`Self::section`].
//!   A rec group lets every type forward-reference every other, which
//!   dissolves the wasm32-gc declaration-order tangle (§Phase 2.4
//!   K.10): a `Map<K,V>` array referencing a later list type, a struct
//!   with a `dyn` field referencing a later `$dyn_T`, a `dyn` method
//!   returning a `List` — all legal inside one rec group, regardless of
//!   the order the codegen passes happen to declare them.

use wasm_encoder::{
    CompositeInnerType, CompositeType, FieldType, StructType, SubType, TypeSection, ValType,
};

/// Interned `(params, returns)` signature paired with the
/// type-section index it landed at. Storing the index alongside the
/// shape (rather than deriving it from the Vec position) keeps the
/// cache correct when *other* type-section entries precede the
/// function signatures — e.g. wasm32-gc declares all its nominal
/// `(struct …)` types via `TypeInterner::declare_struct` before any
/// signature is interned, so the first interned signature lands at a
/// non-zero index and Vec position no longer equals type-section
/// index. The owning `Box<[ValType]>` shape keeps each cache entry to a
/// single small allocation and avoids the `Vec` capacity tail.
type InternedSig = (Box<[ValType]>, Box<[ValType]>, u32);

#[derive(Default)]
pub(super) struct TypeInterner {
    section: TypeSection,
    /// Linear-scan table of function-signature shapes, with each entry
    /// carrying its real type-section index. PR 2/3 modules have a
    /// handful of distinct signatures, so a `Vec` beats a `HashMap`.
    seen: Vec<InternedSig>,
    /// `Some` in buffered rec-group mode (wasm32-gc): declarations
    /// accumulate here and [`Self::close_rec_group`] emits them as one
    /// `(rec …)` group. `None` in immediate mode (wasm32-linear), and
    /// after the group is flushed.
    buffered: Option<Vec<SubType>>,
    /// Running count of declared *types*, used as the next type-section
    /// index. Tracked explicitly rather than via [`TypeSection::len`]
    /// because emitting a `(rec …)` group does not bump that counter at
    /// all (`TypeSection::rec` writes the group's bytes directly without
    /// touching `num_added`), whereas each of the group's members
    /// consumes its own type index — so after the group is flushed,
    /// `section.len()` would undercount by the whole group and a later
    /// func type (e.g. the WASI `fd_write` import) would be handed an
    /// index pointing back into the group.
    count: u32,
}

impl TypeInterner {
    /// Construct an interner in **buffered rec-group** mode — every
    /// declaration is gathered into a single `(rec …)` group. Used by
    /// wasm32-gc so its GC types may forward-reference each other.
    pub(super) fn buffered() -> Self {
        Self {
            buffered: Some(Vec::new()),
            ..Self::default()
        }
    }

    /// The next type-section index a declaration will land at.
    fn next_index(&self) -> u32 {
        self.count
    }

    /// Buffer `build_subtype()` (buffered mode) or run `emit_immediate`
    /// (immediate mode); either way one type index is consumed. Both
    /// sides are lazy closures so each mode allocates only for the path
    /// it takes — the immediate path streams iterators straight into the
    /// encoder with no intermediate `Vec`/`SubType`, byte-identical to
    /// the pre-rec-group encoding wasm32-linear relies on, and the
    /// buffered path never materializes the immediate encoder call.
    fn push(
        &mut self,
        build_subtype: impl FnOnce() -> SubType,
        emit_immediate: impl FnOnce(&mut TypeSection),
    ) {
        match &mut self.buffered {
            Some(defs) => defs.push(build_subtype()),
            None => emit_immediate(&mut self.section),
        }
        self.count += 1;
    }

    /// Return the type-section index for `(params) -> (returns)`,
    /// inserting a fresh entry if this shape hasn't been seen.
    pub(super) fn intern(&mut self, params: &[ValType], returns: &[ValType]) -> u32 {
        if let Some((_, _, idx)) = self
            .seen
            .iter()
            .find(|(p, r, _)| &p[..] == params && &r[..] == returns)
        {
            return *idx;
        }
        let idx = self.next_index();
        self.push(
            || {
                let func =
                    wasm_encoder::FuncType::new(params.iter().copied(), returns.iter().copied());
                final_subtype(CompositeInnerType::Func(func))
            },
            |sec| {
                sec.ty()
                    .function(params.iter().copied(), returns.iter().copied());
            },
        );
        self.seen.push((params.into(), returns.into(), idx));
        idx
    }

    /// Close the buffered rec group, flushing it into the section. In
    /// buffered mode this must be called once, after every type that
    /// participates in the GC type graph (structs / arrays / enum &
    /// closure & dyn subtypes, plus the `$fn_SIG` / `$dynfn` func types
    /// they reference) has been declared, and before any **import- or
    /// export-facing** func type is interned. A func type that lands in
    /// the big rec group is canonicalized as a *member of that group*,
    /// so it is not type-compatible with the host's standalone WASI
    /// `fd_write` — closing the group first gives `fd_write` (and the
    /// user/helper signatures interned afterwards) a singleton-rec-group
    /// identity that matches. After close, `intern` / `declare_*` emit
    /// immediately, exactly like the default immediate mode. Idempotent.
    ///
    /// The `seen` dedup cache is cleared here so a post-close [`intern`]
    /// can never alias an *in-group* func type's index: a standalone
    /// `fd_write` / user / helper signature must get its own standalone
    /// type even if it happens to share a shape with a grouped
    /// `$fn_SIG` / `$dynfn` (otherwise it would inherit that member's
    /// rec-group identity and break host import compatibility).
    /// Post-close signatures still dedup among themselves — the cache
    /// simply restarts empty, holding only standalone entries.
    ///
    /// [`intern`]: Self::intern
    pub(super) fn close_rec_group(&mut self) {
        if let Some(defs) = self.buffered.take() {
            if !defs.is_empty() {
                self.section.ty().rec(defs);
            }
            self.seen.clear();
        }
    }

    /// The materialized type section. In buffered mode, a fallback flush
    /// of any still-open rec group runs here (the wasm32-gc pipeline
    /// normally closes it earlier via [`Self::close_rec_group`]); later
    /// calls return the already-built section.
    pub(super) fn section(&mut self) -> &TypeSection {
        self.close_rec_group();
        &self.section
    }

    /// Declare a nominal WASM-GC struct type with the given field
    /// layout and return its type-section index. No dedup — WASM-GC's
    /// type system is nominal (two Phoenix structs with identical field
    /// shapes must be distinct WASM types). wasm32-gc only.
    pub(super) fn declare_struct(&mut self, fields: &[FieldType]) -> u32 {
        let idx = self.next_index();
        self.push(
            || struct_subtype(fields, true, None),
            |sec| {
                sec.ty().struct_(fields.iter().cloned());
            },
        );
        idx
    }

    /// Declare a nominal WASM-GC array type with the given element
    /// `FieldType` and return its type-section index. No dedup (nominal).
    /// wasm32-gc only.
    pub(super) fn declare_array(&mut self, element: FieldType) -> u32 {
        let idx = self.next_index();
        self.push(
            || final_subtype(CompositeInnerType::Array(wasm_encoder::ArrayType(element))),
            |sec| {
                sec.ty().array(&element.element_type, element.mutable);
            },
        );
        idx
    }

    /// Declare a non-final, no-parent WASM-GC struct type that subtypes
    /// can later extend (the K.4 enum parent / per-trait pieces).
    pub(super) fn declare_open_struct(&mut self, fields: &[FieldType]) -> u32 {
        let idx = self.next_index();
        self.push(
            || struct_subtype(fields, false, None),
            |sec| {
                sec.ty().subtype(&struct_subtype(fields, false, None));
            },
        );
        idx
    }

    /// Reserve a type-section index now and fill its definition later
    /// via [`Self::define_struct`]. Buffered mode only (it relies on the
    /// rec group making the resulting forward reference legal): a type
    /// declared *before* this one in the group may reference the
    /// reserved index even though its real shape isn't known yet — the
    /// K.10 use is a `$dyn_T` whose index a `dyn` struct field or
    /// `List<dyn T>` element must embed before the trait's methods (and
    /// thus the `$vtable_T` the `$dyn_T` points at) can be built. The
    /// placeholder is an empty final struct, overwritten by
    /// `define_struct`; leaving it unfilled is a compiler bug (it would
    /// emit a bogus empty struct, not silently corrupt indices).
    pub(super) fn reserve(&mut self) -> u32 {
        let idx = self.next_index();
        self.push(
            || struct_subtype(&[], true, None),
            |_| unreachable!("reserve() is buffered-mode only; wasm32-gc uses buffered"),
        );
        idx
    }

    /// Fill the definition of a slot previously [`Self::reserve`]d, as a
    /// plain final struct. Buffered mode only.
    pub(super) fn define_struct(&mut self, idx: u32, fields: &[FieldType]) {
        let defs = self
            .buffered
            .as_mut()
            .expect("define_struct is buffered-mode only (wasm32-gc)");
        defs[idx as usize] = struct_subtype(fields, true, None);
    }

    /// Declare a final WASM-GC struct subtype extending `super_idx`
    /// (the K.4 enum variant subtypes). `fields` must start with the
    /// parent's fields in order (a WASM-GC subtype requirement).
    pub(super) fn declare_subtype_struct(&mut self, fields: &[FieldType], super_idx: u32) -> u32 {
        let idx = self.next_index();
        self.push(
            || struct_subtype(fields, true, Some(super_idx)),
            |sec| {
                sec.ty()
                    .subtype(&struct_subtype(fields, true, Some(super_idx)));
            },
        );
        idx
    }
}

/// Build a `SubType` wrapping a composite inner type, final with no
/// supertype (the common case for func / array / plain struct types).
fn final_subtype(inner: CompositeInnerType) -> SubType {
    SubType {
        is_final: true,
        supertype_idx: None,
        composite_type: CompositeType {
            inner,
            shared: false,
            descriptor: None,
            describes: None,
        },
    }
}

/// Build a struct `SubType` with the given finality / supertype.
fn struct_subtype(fields: &[FieldType], is_final: bool, supertype_idx: Option<u32>) -> SubType {
    SubType {
        is_final,
        supertype_idx,
        composite_type: CompositeType {
            inner: CompositeInnerType::Struct(StructType {
                fields: fields.to_vec().into_boxed_slice(),
            }),
            shared: false,
            descriptor: None,
            describes: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_encoder::StorageType;

    fn i32_field() -> FieldType {
        FieldType {
            element_type: StorageType::Val(ValType::I32),
            mutable: false,
        }
    }

    /// Number of fields in the buffered slot `idx`, asserting it's a
    /// struct subtype (the only shape these tests buffer).
    fn struct_field_count(ti: &TypeInterner, idx: u32) -> usize {
        let defs = ti.buffered.as_ref().expect("buffered mode");
        match &defs[idx as usize].composite_type.inner {
            CompositeInnerType::Struct(st) => st.fields.len(),
            other => panic!("slot {idx} is not a struct: {other:?}"),
        }
    }

    /// Immediate mode: sequential indices, structural dedup, and
    /// `close_rec_group` is an inert no-op (the dedup cache is *not*
    /// cleared, since wasm32-linear never opens a group). Guards that the
    /// rec-group machinery left the linear backend's behavior untouched.
    #[test]
    fn immediate_mode_dedups_and_close_is_inert() {
        let mut ti = TypeInterner::default();
        let a = ti.intern(&[ValType::I32], &[ValType::I32]);
        let b = ti.intern(&[ValType::I32], &[ValType::I32]);
        let c = ti.intern(&[], &[]);
        assert_eq!((a, b, c), (0, 0, 1), "dedup hit reuses index; miss bumps");

        // No group was ever opened, so close clears nothing.
        ti.close_rec_group();
        let d = ti.intern(&[ValType::I32], &[ValType::I32]);
        assert_eq!(d, a, "immediate-mode close must not clear the dedup cache");
        let e = ti.intern(&[ValType::F64], &[]);
        assert_eq!(e, 2, "next miss still lands at the running count");
    }

    /// Immediate mode: the non-`intern` declarations (`declare_struct` /
    /// `declare_array` / `declare_open_struct`) each consume one type
    /// index in declaration order and stream straight into the encoder.
    /// wasm32-linear never reaches these paths (it declares no GC types),
    /// so this guards that their `emit_immediate` arms stay index-correct
    /// alongside the buffered arms the other tests cover.
    #[test]
    fn immediate_mode_declares_gc_types_in_order() {
        let mut ti = TypeInterner::default();
        let s = ti.declare_struct(&[i32_field()]);
        let a = ti.declare_array(i32_field());
        let o = ti.declare_open_struct(&[i32_field()]);
        let f = ti.intern(&[ValType::I32], &[]);
        assert_eq!(
            (s, a, o, f),
            (0, 1, 2, 3),
            "each declaration takes the next index; no dedup, none skipped"
        );
    }

    /// Buffered mode: `reserve` consumes one type-section slot in
    /// declaration order, and `define_struct` *fills that exact slot*
    /// without shifting any neighbor — the invariant `define_struct`'s
    /// unchecked `defs[idx]` index relies on.
    #[test]
    fn reserve_then_define_fills_the_reserved_slot() {
        let mut ti = TypeInterner::buffered();
        let a = ti.declare_struct(&[]);
        let r = ti.reserve();
        let b = ti.declare_struct(&[]);
        assert_eq!((a, r, b), (0, 1, 2), "reserve takes a slot in order");

        ti.define_struct(r, &[i32_field(), i32_field()]);
        assert_eq!(
            ti.buffered.as_ref().unwrap().len(),
            3,
            "define fills in place; it must not append a 4th slot"
        );
        assert_eq!(struct_field_count(&ti, r), 2, "reserved slot got defined");
        assert_eq!(struct_field_count(&ti, a), 0, "neighbor slot untouched");
        assert_eq!(struct_field_count(&ti, b), 0, "neighbor slot untouched");
    }

    /// Buffered mode: every grouped type consumes its own type index, so
    /// the first post-close type lands at the group size `N` — not at
    /// `TypeSection::len()` (which doesn't count `rec` members at all).
    /// And `close_rec_group` clears the dedup cache, so a post-close
    /// signature sharing a shape with an in-group func type gets a fresh
    /// *standalone* index (host-import compatibility) while post-close
    /// signatures still dedup among themselves.
    #[test]
    fn count_survives_close_and_dedup_restarts() {
        let mut ti = TypeInterner::buffered();
        let grouped_fn = ti.intern(&[ValType::I32], &[]);
        let _s = ti.declare_struct(&[]);
        assert_eq!((grouped_fn, _s), (0, 1), "two members buffered at 0,1");

        ti.close_rec_group();

        // First standalone type must land at index 2 (the group's two
        // members each consumed an index), proving `count` — not
        // `section.len()` — drives the next index.
        let post = ti.intern(&[ValType::I32], &[]);
        assert_eq!(post, 2, "post-close index = group size, not section.len()");
        assert_ne!(
            post, grouped_fn,
            "identical shape must NOT alias the in-group member"
        );

        let post_dup = ti.intern(&[ValType::I32], &[]);
        assert_eq!(post_dup, post, "post-close signatures still dedup");
        let post_new = ti.intern(&[], &[ValType::F64]);
        assert_eq!(post_new, 3, "fresh post-close shape bumps the count");
    }

    /// `close_rec_group` is idempotent: a second call (e.g. the fallback
    /// flush in [`TypeInterner::section`] after an explicit close) is a
    /// no-op and does not disturb the post-close dedup cache or count.
    #[test]
    fn close_rec_group_is_idempotent() {
        let mut ti = TypeInterner::buffered();
        ti.declare_struct(&[]);
        ti.close_rec_group();
        let a = ti.intern(&[ValType::I32], &[]);
        ti.close_rec_group(); // second close — must be inert
        let b = ti.intern(&[ValType::I32], &[]);
        assert_eq!(b, a, "redundant close must not clear the post-close cache");
        let _ = ti.section(); // fallback flush path — also inert here
        let c = ti.intern(&[ValType::F64], &[]);
        assert_eq!(c, a + 1, "count still advances after redundant closes");
    }
}
