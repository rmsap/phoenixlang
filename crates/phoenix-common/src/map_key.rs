//! Canonical map-key projection and last-wins dedup, shared by every
//! interpreter backend's `MapBuilder.freeze` and map-literal lowering.
//!
//! Phoenix `Map` keys compare **byte-wise** on floats (§Phase 2.4 K.9):
//! `-0.0` and `0.0` are distinct, and two `NaN`s are equal iff their bit
//! patterns match — deliberately *not* IEEE `==`. Each interpreter
//! carries its own runtime value enum, so the projection from a value to
//! a [`CanonicalMapKey`] stays a small per-crate match; but the hashable
//! key type and the dedup algorithm live here so the float-bits rule and
//! the last-wins / first-insertion-position semantics are defined exactly
//! once, byte-for-byte with native's `phx_map_from_pairs`.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

/// A hashable, `Eq` projection of a Phoenix map key whose `Hash`/`Eq`
/// honor the byte-wise float contract: `-0.0` and `0.0` land in distinct
/// buckets, two `NaN`s are equal iff their bits match. Non-float scalars
/// and strings use their natural `Eq`/`Hash`.
///
/// Phoenix map keys are always scalars or strings (sema rejects
/// non-hashable key types), so [`CanonicalMapKey::Other`] is a
/// defensive fallthrough for shapes the type checker forbids as keys —
/// rendered to a stable string by the caller so dedup still terminates
/// rather than misbehaving. Only the scalar/string arms are a
/// cross-backend contract: the `Other` rendering is per-caller (each
/// interpreter's own `Display`) and is *not* guaranteed to agree across
/// backends or with native — it exists only so an unreachable shape
/// can't make dedup loop, not to define dedup semantics for it. If a
/// non-scalar key type ever becomes reachable, give it a real arm here
/// rather than leaning on `Other`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CanonicalMapKey {
    /// An `Int` key.
    Int(i64),
    /// A `Float` key, stored as its raw bits so the hash/eq are
    /// byte-wise (mirroring `map_key_eq`).
    FloatBits(u64),
    /// A `Bool` key.
    Bool(bool),
    /// A `String` key.
    String(String),
    /// Any other (type-checker-forbidden) key shape, rendered to a
    /// stable string so distinct values stay distinct.
    Other(String),
}

/// Dedup `pairs` **last-wins** while keeping each key's
/// **first-insertion** position, projecting each key to its
/// [`CanonicalMapKey`] via `canon`.
///
/// O(n)-amortized via a hashed side-index from canonical key to the
/// output slot: a repeat key overwrites that slot's value in place
/// (preserving its original position), a fresh key appends. A naive
/// per-pair linear scan would be O(n²) and make large builds (e.g. the
/// `hash_map_churn` bench) pathological. Matches native
/// `phx_map_from_pairs` byte-for-byte.
pub fn dedup_last_wins<K, V>(
    pairs: impl IntoIterator<Item = (K, V)>,
    canon: impl Fn(&K) -> CanonicalMapKey,
) -> Vec<(K, V)> {
    let iter = pairs.into_iter();
    let cap = iter.size_hint().0;
    let mut out: Vec<(K, V)> = Vec::with_capacity(cap);
    let mut index: HashMap<CanonicalMapKey, usize> = HashMap::with_capacity(cap);
    for (k, v) in iter {
        match index.entry(canon(&k)) {
            Entry::Occupied(slot) => out[*slot.get()].1 = v,
            Entry::Vacant(slot) => {
                slot.insert(out.len());
                out.push((k, v));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The float-bits contract: `-0.0` and `0.0` are distinct keys (they
    /// must not collapse), and equal-bit `NaN`s are the same key (they
    /// must collapse) — the two cases where byte-wise and IEEE disagree.
    #[test]
    fn float_bits_key_is_byte_wise() {
        assert_ne!(
            CanonicalMapKey::FloatBits(0.0_f64.to_bits()),
            CanonicalMapKey::FloatBits((-0.0_f64).to_bits()),
            "±0.0 must be distinct map keys"
        );
        assert_eq!(
            CanonicalMapKey::FloatBits(f64::NAN.to_bits()),
            CanonicalMapKey::FloatBits(f64::NAN.to_bits()),
            "equal-bit NaNs must be the same map key"
        );
    }

    /// `dedup_last_wins` keeps each key's first-insertion position but
    /// takes the last value written for it. Key `1` keeps slot 0 but
    /// ends with value `99`; `2` and `3` follow in insertion order.
    #[test]
    fn dedup_keeps_first_position_last_value() {
        let pairs = vec![(1, 10), (2, 20), (1, 99), (3, 30)];
        let out = dedup_last_wins(pairs, |k| CanonicalMapKey::Int(*k as i64));
        assert_eq!(out, vec![(1, 99), (2, 20), (3, 30)]);
    }

    /// Dedup honors the byte-wise float rule end-to-end: `-0.0` and
    /// `0.0` survive as two entries, while two `NaN` keys collapse to one
    /// (last value wins).
    #[test]
    fn dedup_respects_float_bits() {
        let pairs = vec![(0.0_f64, 1), (-0.0_f64, 2), (f64::NAN, 3), (f64::NAN, 4)];
        let out = dedup_last_wins(pairs, |k| CanonicalMapKey::FloatBits(k.to_bits()));
        assert_eq!(out.len(), 3, "±0.0 distinct, NaNs collapsed");
        assert_eq!(out[0].1, 1); // 0.0 kept
        assert_eq!(out[1].1, 2); // -0.0 kept, separate
        assert_eq!(out[2].1, 4); // NaN last-wins
    }
}
