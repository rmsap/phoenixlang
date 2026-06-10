//! Ryū d2s power-of-5 tables, computed from first principles.
//!
//! The Ryū algorithm (Adams, "Ryū: Fast Float-to-String Conversion",
//! PLDI 2018) needs two precomputed tables of 125-bit fixed-point
//! constants, one entry per reachable power of 5:
//!
//! - `DOUBLE_POW5_SPLIT[i]` — the top 125 bits of `5^i`, i.e.
//!   `floor(5^i · 2^(125 − bitlen(5^i)))`, consulted for binary
//!   exponents < 0.
//! - `DOUBLE_POW5_INV_SPLIT[q]` — a rounded-up reciprocal,
//!   `floor(2^(bitlen(5^q) − 1 + 125) / 5^q) + 1`, consulted for
//!   binary exponents ≥ 0.
//!
//! Each 125-bit value is split into `(lo, hi)` u64 words —
//! `value = hi · 2^64 + lo` — matching the layout `phx_ryu_d2d`'s
//! `mul_shift` loads expect (see `float_helpers.rs`).
//!
//! The entries are **computed here from the definitions above**, not
//! copied from a reference implementation, so this file carries no
//! third-party license — the repo stays MIT-only (the constants are
//! mathematical facts; only their verbatim source form would have
//! carried the `ryu` crate's Apache-2.0/BSL-1.0 terms). Equality with
//! the `ryu` crate — and therefore with native
//! `phoenix_runtime::format_f64` — is pinned end-to-end by the
//! differential tests in `compile_wasm_gc.rs`, in particular
//! `float_print_every_binary_exponent_matches_native`, which exercises
//! every reachable index of both tables against the `ryu` oracle.
//!
//! Both tables are trimmed to the index ranges f64 inputs can reach
//! (see the `*_TABLE_SIZE` / `*_FIRST_IDX` constants) — the reference
//! implementation's 342- and 326-entry tables include entries no f64
//! ever indexes, which would be dead weight in every Float-printing
//! module. Where reachable, the entries are bit-identical.
//!
//! The computation runs once per process (`OnceLock`) and only when a
//! module actually prints a Float; it is a few hundred microseconds of
//! schoolbook bignum arithmetic on ≤ 12-limb numbers (`5^325` is 755
//! bits), which is noise next to the rest of codegen.

use std::sync::OnceLock;

/// Entry count of `DOUBLE_POW5_INV_SPLIT` — exactly the f64-reachable
/// indices 0..=290 (q = log10_pow2(e2) − 1 for e2 ≤ 969). The
/// reference implementation's table carries 342 entries, sized for
/// the algorithm family rather than the f64 input range; the trailing
/// 51 entries are unreachable from any f64 and are trimmed here so
/// they don't ship in every Float-printing module.
pub(super) const DOUBLE_POW5_INV_TABLE_SIZE: usize = 291;
/// First f64-reachable index of `DOUBLE_POW5_SPLIT`: i = −e2 − q ≥ 1
/// for every e2 < 0, so the reference table's entry 0 (= 5^0) is
/// unreachable and trimmed. Entry for index i lives at array (and
/// data-segment) position `i − DOUBLE_POW5_SPLIT_FIRST_IDX`;
/// `phx_ryu_d2d` folds the bias into its load offset
/// (`RYU_POW5_SPLIT_INDEX_BASE` in `module_builder.rs`).
pub(super) const DOUBLE_POW5_SPLIT_FIRST_IDX: usize = 1;
/// Entry count of `DOUBLE_POW5_SPLIT` — the f64-reachable indices
/// 1..=325 (i = −e2 − q for e2 ≥ −1076).
pub(super) const DOUBLE_POW5_TABLE_SIZE: usize = 325;

/// Fixed-point precision of every table entry, in bits — ryu's
/// `DOUBLE_POW5_BITCOUNT` / `DOUBLE_POW5_INV_BITCOUNT` (both 125).
/// `phx_ryu_d2d`'s `j` computation hard-codes the same constant (the
/// `125` in its `k` formulas); they must move together.
const POW5_BITCOUNT: u32 = 125;

/// `DOUBLE_POW5_SPLIT[i] = floor(5^i · 2^(125 − bitlen(5^i)))` as
/// `(lo, hi)` u64 pairs, for `i` in `1..=325` (array position `i − 1`).
pub(super) fn double_pow5_split() -> &'static [(u64, u64); DOUBLE_POW5_TABLE_SIZE] {
    static TABLE: OnceLock<[(u64, u64); DOUBLE_POW5_TABLE_SIZE]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [(0u64, 0u64); DOUBLE_POW5_TABLE_SIZE];
        let mut pow5 = Big::one();
        for entry in table.iter_mut() {
            // First iteration: 5^DOUBLE_POW5_SPLIT_FIRST_IDX.
            pow5.mul_small(5);
            let bits = pow5.bit_len();
            let v = if bits <= POW5_BITCOUNT {
                pow5.shl_bits(POW5_BITCOUNT - bits)
            } else {
                pow5.shr_bits(bits - POW5_BITCOUNT)
            };
            debug_assert_eq!(v.bit_len(), POW5_BITCOUNT);
            *entry = (v.word(0), v.word(1));
        }
        table
    })
}

/// `DOUBLE_POW5_INV_SPLIT[q] = floor(2^(bitlen(5^q) − 1 + 125) / 5^q) + 1`
/// as `(lo, hi)` u64 pairs, for `q` in `0..=290` (array position `q`).
pub(super) fn double_pow5_inv_split() -> &'static [(u64, u64); DOUBLE_POW5_INV_TABLE_SIZE] {
    static TABLE: OnceLock<[(u64, u64); DOUBLE_POW5_INV_TABLE_SIZE]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [(0u64, 0u64); DOUBLE_POW5_INV_TABLE_SIZE];
        let mut pow5 = Big::one();
        for (q, entry) in table.iter_mut().enumerate() {
            if q > 0 {
                pow5.mul_small(5);
            }
            let mut v = pow2_div(pow5.bit_len() - 1 + POW5_BITCOUNT, &pow5);
            v.add_one();
            // v ∈ [2^124 + 1, 2^125 + 1] — two words.
            debug_assert!(v.word(2) == 0);
            *entry = (v.word(0), v.word(1));
        }
        table
    })
}

/// `floor(2^n / d)` by restoring long division over the dividend's
/// `n + 1` bits (only the top bit is set). `d` must be non-zero.
fn pow2_div(n: u32, d: &Big) -> Big {
    let mut rem = Big::zero();
    let mut quot = Big::zero();
    for bit in (0..=n).rev() {
        rem.shl1();
        if bit == n {
            rem.add_one();
        }
        quot.shl1();
        if rem.ge(d) {
            rem.sub_assign(d);
            quot.add_one();
        }
    }
    quot
}

/// Minimal arbitrary-precision unsigned integer: little-endian base-2^64
/// limbs with no trailing zero limb (zero is the empty vec). Just the
/// operations the two table computations need — for anything more,
/// reach for a real bignum crate instead of growing this.
#[derive(Clone)]
struct Big(Vec<u64>);

impl Big {
    fn zero() -> Self {
        Big(Vec::new())
    }

    fn one() -> Self {
        Big(vec![1])
    }

    /// Limb `i`, zero-extended past the top.
    fn word(&self, i: usize) -> u64 {
        self.0.get(i).copied().unwrap_or(0)
    }

    fn bit_len(&self) -> u32 {
        match self.0.last() {
            None => 0,
            Some(top) => (self.0.len() as u32 - 1) * 64 + (64 - top.leading_zeros()),
        }
    }

    fn mul_small(&mut self, m: u64) {
        let mut carry = 0u128;
        for limb in &mut self.0 {
            let prod = u128::from(*limb) * u128::from(m) + carry;
            *limb = prod as u64;
            carry = prod >> 64;
        }
        if carry != 0 {
            self.0.push(carry as u64);
        }
    }

    fn add_one(&mut self) {
        for limb in &mut self.0 {
            let (sum, overflow) = limb.overflowing_add(1);
            *limb = sum;
            if !overflow {
                return;
            }
        }
        self.0.push(1);
    }

    fn shl1(&mut self) {
        let mut carry = 0u64;
        for limb in &mut self.0 {
            let next_carry = *limb >> 63;
            *limb = (*limb << 1) | carry;
            carry = next_carry;
        }
        if carry != 0 {
            self.0.push(carry);
        }
    }

    fn shl_bits(&self, n: u32) -> Big {
        let limb_shift = (n / 64) as usize;
        let bit_shift = n % 64;
        let mut limbs = vec![0u64; self.0.len() + limb_shift + 1];
        for (i, &limb) in self.0.iter().enumerate() {
            limbs[i + limb_shift] |= limb << bit_shift;
            if bit_shift != 0 {
                limbs[i + limb_shift + 1] |= limb >> (64 - bit_shift);
            }
        }
        let mut out = Big(limbs);
        out.normalize();
        out
    }

    /// Floor shift right.
    fn shr_bits(&self, n: u32) -> Big {
        let limb_shift = (n / 64) as usize;
        let bit_shift = n % 64;
        if limb_shift >= self.0.len() {
            return Big::zero();
        }
        let mut limbs = vec![0u64; self.0.len() - limb_shift];
        for (i, limb) in limbs.iter_mut().enumerate() {
            *limb = self.0[i + limb_shift] >> bit_shift;
            if bit_shift != 0 && i + limb_shift + 1 < self.0.len() {
                *limb |= self.0[i + limb_shift + 1] << (64 - bit_shift);
            }
        }
        let mut out = Big(limbs);
        out.normalize();
        out
    }

    fn ge(&self, other: &Big) -> bool {
        if self.0.len() != other.0.len() {
            return self.0.len() > other.0.len();
        }
        for i in (0..self.0.len()).rev() {
            if self.0[i] != other.0[i] {
                return self.0[i] > other.0[i];
            }
        }
        true
    }

    /// `self -= other`; requires `self >= other`.
    fn sub_assign(&mut self, other: &Big) {
        let mut borrow = false;
        for i in 0..self.0.len() {
            let (diff, b1) = self.0[i].overflowing_sub(other.word(i));
            let (diff, b2) = diff.overflowing_sub(u64::from(borrow));
            self.0[i] = diff;
            borrow = b1 || b2;
        }
        debug_assert!(!borrow, "Big::sub_assign underflow");
        self.normalize();
    }

    fn normalize(&mut self) {
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big_from_u128(v: u128) -> Big {
        let mut b = Big(vec![v as u64, (v >> 64) as u64]);
        b.normalize();
        b
    }

    fn big_to_u128(b: &Big) -> u128 {
        assert!(b.word(2) == 0, "value exceeds u128");
        u128::from(b.word(1)) << 64 | u128::from(b.word(0))
    }

    /// The bignum primitives against u128 ground truth, across limb
    /// boundaries and shift edge cases (bit_shift == 0 in particular).
    #[test]
    fn big_primitives_match_u128() {
        let samples: &[u128] = &[
            0,
            1,
            5,
            u128::from(u64::MAX),
            u128::from(u64::MAX) + 1,
            0x0123_4567_89AB_CDEF_FEDC_BA98_7654_3210,
            1 << 127,
        ];
        for &v in samples {
            let b = big_from_u128(v);
            assert_eq!(128 - v.leading_zeros(), b.bit_len());
            for shift in [0u32, 1, 63, 64, 65, 127] {
                if v.checked_shl(shift).is_some_and(|s| s >> shift == v) {
                    assert_eq!(
                        big_to_u128(&b.shl_bits(shift)),
                        v << shift,
                        "{v} << {shift}"
                    );
                }
                assert_eq!(
                    big_to_u128(&b.shr_bits(shift)),
                    v >> shift,
                    "{v} >> {shift}"
                );
            }
            if v < u128::MAX / 5 {
                let mut m = b.clone();
                m.mul_small(5);
                assert_eq!(big_to_u128(&m), v * 5);
            }
            let mut inc = b.clone();
            inc.add_one();
            if v < u128::MAX {
                assert_eq!(big_to_u128(&inc), v + 1);
            }
        }
        // Subtraction with borrow propagation across a limb boundary.
        let mut a = big_from_u128(u128::from(u64::MAX) + 1);
        a.sub_assign(&Big::one());
        assert_eq!(big_to_u128(&a), u128::from(u64::MAX));
    }

    /// `pow2_div` against u128 ground truth over a sweep of exponents
    /// and divisors, including multi-limb divisors.
    #[test]
    fn pow2_div_matches_u128() {
        for n in [0u32, 1, 7, 63, 64, 65, 100, 127] {
            for d in [
                1u128,
                2,
                3,
                5,
                7,
                125,
                u128::from(u64::MAX),
                (1 << 80) + 12345,
            ] {
                let expect = (1u128 << n) / d;
                let got = pow2_div(n, &big_from_u128(d));
                assert_eq!(big_to_u128(&got), expect, "2^{n} / {d}");
            }
        }
    }

    /// Both tables against direct u128 evaluation of the defining
    /// formulas, for every index where the intermediate values fit in
    /// u128 — a cross-check of the bignum composition that shares no
    /// code with the `Big`-based computation. (`5^i` fits u128 through
    /// i = 55; the inverse table's `2^n` dividend only fits through
    /// q = 1, so its deep entries are covered by the end-to-end
    /// differential sweep in `compile_wasm_gc.rs` instead.)
    #[test]
    fn table_heads_match_direct_u128_evaluation() {
        let mut pow5: u128 = 1;
        for i in 0..=55usize {
            if i > 0 {
                pow5 *= 5;
            }
            let bits = 128 - pow5.leading_zeros();
            if i >= DOUBLE_POW5_SPLIT_FIRST_IDX {
                let expect = if bits <= POW5_BITCOUNT {
                    pow5 << (POW5_BITCOUNT - bits)
                } else {
                    pow5 >> (bits - POW5_BITCOUNT)
                };
                let (lo, hi) = double_pow5_split()[i - DOUBLE_POW5_SPLIT_FIRST_IDX];
                assert_eq!(
                    u128::from(hi) << 64 | u128::from(lo),
                    expect,
                    "POW5_SPLIT[{i}]"
                );
            }

            if bits - 1 + POW5_BITCOUNT <= 127 {
                let expect_inv = (1u128 << (bits - 1 + POW5_BITCOUNT)) / pow5 + 1;
                let (lo, hi) = double_pow5_inv_split()[i];
                assert_eq!(
                    u128::from(hi) << 64 | u128::from(lo),
                    expect_inv,
                    "POW5_INV_SPLIT[{i}]"
                );
            }
        }
    }

    /// Structural invariants over every entry: `POW5_SPLIT` values
    /// have exactly 125 bits (hi word in [2^60, 2^61)), and
    /// `POW5_INV_SPLIT` values are `floor + 1` of a quotient in
    /// (2^124, 2^125], so their hi word is in [2^60, 2^61].
    #[test]
    fn table_entries_have_expected_magnitude() {
        for (pos, &(_, hi)) in double_pow5_split().iter().enumerate() {
            let i = pos + DOUBLE_POW5_SPLIT_FIRST_IDX;
            assert!(
                (1 << 60..1 << 61).contains(&hi),
                "POW5_SPLIT[{i}] hi = {hi}"
            );
        }
        for (q, &(_, hi)) in double_pow5_inv_split().iter().enumerate() {
            assert!(
                (1 << 60..=1 << 61).contains(&hi),
                "POW5_INV_SPLIT[{q}] hi = {hi}"
            );
        }
    }
}
