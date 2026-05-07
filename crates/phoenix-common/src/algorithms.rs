//! Small algorithmic helpers shared between the AST interpreter
//! (`phoenix-interp`) and the IR interpreter (`phoenix-ir-interp`).
//!
//! These crates each implement `List.sortBy` over their own value
//! enum (`Value` vs `IrValue`) and call back into a Phoenix closure
//! through their respective `call_closure(...)` entry points, both of
//! which require `&mut self` on the interpreter. That `&mut self`
//! disqualifies `slice::sort_by` (whose comparator is
//! `Fn(&T, &T) -> Ordering`), so both crates need their own
//! comparator-based sort. Rather than carry two copies of the
//! algorithm, the common pieces live here.

/// Bottom-up iterative merge sort with a fallible, `FnMut` comparator.
///
/// **O(n log n)** worst case. Stable: when the comparator returns
/// zero or negative, the left-hand element keeps its position
/// (matching the contract that `phoenix-cranelift`'s
/// `translate_list_sortby` enforces in compiled code).
///
/// `cmp(a, b)` should return:
/// * negative — `a` sorts before `b`
/// * zero — equal; left-hand element preserved (stability)
/// * positive — `a` sorts after `b`
///
/// Errors from the comparator short-circuit the sort and propagate
/// out unchanged. The partial state of the buffers is dropped.
///
/// Implementation note: a fresh aux buffer of `len` placeholders is
/// allocated up front so the merge body can index-write
/// (`aux[k] = ...`) without first checking length. The placeholders
/// are clones of `items[0]` and are overwritten on the first pass
/// before being read; their content is irrelevant. After each width
/// pass `src` and `aux` are swapped, so the freshly merged data
/// becomes the next pass's source — no per-pass copyback.
pub fn merge_sort_by<V, E, F>(items: Vec<V>, mut cmp: F) -> Result<Vec<V>, E>
where
    V: Clone,
    F: FnMut(&V, &V) -> Result<i64, E>,
{
    let len = items.len();
    if len < 2 {
        return Ok(items);
    }
    let mut src = items;
    // `src[0]` is safe to index here because the `len < 2` early
    // return above guarantees `len >= 2`. If that early return is
    // ever weakened (e.g. to `len == 0`) this line panics.
    let mut aux: Vec<V> = vec![src[0].clone(); len];

    let mut width = 1usize;
    while width < len {
        let mut start = 0usize;
        while start < len {
            let mid = (start + width).min(len);
            let end = (start + 2 * width).min(len);
            let mut i = start;
            let mut j = mid;
            let mut k = start;
            while i < mid && j < end {
                let c = cmp(&src[i], &src[j])?;
                if c <= 0 {
                    aux[k] = src[i].clone();
                    i += 1;
                } else {
                    aux[k] = src[j].clone();
                    j += 1;
                }
                k += 1;
            }
            while i < mid {
                aux[k] = src[i].clone();
                i += 1;
                k += 1;
            }
            while j < end {
                aux[k] = src[j].clone();
                j += 1;
                k += 1;
            }
            start += 2 * width;
        }
        std::mem::swap(&mut src, &mut aux);
        width *= 2;
    }
    Ok(src)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_cmp<V: Ord + Clone>(a: &V, b: &V) -> Result<i64, ()> {
        Ok(match a.cmp(b) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        })
    }

    #[test]
    fn empty() {
        let r = merge_sort_by(Vec::<i32>::new(), ok_cmp).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn single() {
        let r = merge_sort_by(vec![42i32], ok_cmp).unwrap();
        assert_eq!(r, vec![42]);
    }

    #[test]
    fn reverse_sorted() {
        let r = merge_sort_by((1..=50i32).rev().collect(), ok_cmp).unwrap();
        assert_eq!(r, (1..=50).collect::<Vec<_>>());
    }

    #[test]
    fn already_sorted() {
        let r = merge_sort_by((1..=50i32).collect(), ok_cmp).unwrap();
        assert_eq!(r, (1..=50).collect::<Vec<_>>());
    }

    #[test]
    fn stable_on_ties() {
        // Pair of (key, original_index) sorted by key only — equal
        // keys must preserve original order.
        let input: Vec<(i32, usize)> = vec![(1, 0), (0, 1), (1, 2), (0, 3), (1, 4)];
        let r = merge_sort_by(input, |a, b| Ok::<i64, ()>((a.0 - b.0) as i64)).unwrap();
        assert_eq!(r, vec![(0, 1), (0, 3), (1, 0), (1, 2), (1, 4)]);
    }

    #[test]
    fn comparator_error_propagates() {
        let r = merge_sort_by(vec![3, 1, 2], |_a, _b| Err::<i64, _>("boom"));
        assert_eq!(r, Err("boom"));
    }

    #[test]
    fn lengths_two_and_three() {
        assert_eq!(merge_sort_by(vec![7, 3], ok_cmp).unwrap(), vec![3, 7]);
        assert_eq!(merge_sort_by(vec![3, 1, 2], ok_cmp).unwrap(), vec![1, 2, 3]);
    }
}
