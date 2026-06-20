//! Process-global persistent GC roots ("pins").
//!
//! The [`shadow_stack`](super::shadow_stack) roots a value only for the
//! lifetime of the Phoenix function activation that produced it. A Phoenix
//! closure handed to a JS host as a callback (`extern js`,
//! [design-decisions §Phase 2.5 decision G]) may outlive that activation: the
//! host can retain the callback and invoke it *after* the extern call that
//! handed it over has returned. Such a closure needs a root that is **not** tied
//! to any frame — a pin.
//!
//! The JS glue calls [`phx_gc_pin`](super::phx_gc_pin) when it wraps a crossing
//! closure and [`phx_gc_unpin`](super::phx_gc_unpin) when the host releases it
//! (explicitly via the wrapper's `release()`, or via a `FinalizationRegistry`
//! when the wrapper itself is collected). The mark phase scans the pin set as
//! additional precise roots, exactly like the shadow stack. A closure the host
//! never releases stays pinned for the program's life — the documented,
//! linear-only retained-callback leak.
//!
//! **Multiplicity.** Pins are a *multiset* (duplicate entries), not a set: a
//! closure pinned N times must be unpinned N times before it can be collected.
//! [`unpin`] removes a single occurrence; the mark scan coalesces duplicates so
//! the collector visits each distinct pointer once. This is a defensive runtime
//! property — the current wasm32-linear JS glue does **not** rely on it: it keeps
//! a per-env-pointer 1-or-0 refcount (its retention `Map` dedups, so it pins a
//! given closure once however many externs receive it, and unpins once). The
//! multiset support means a future caller that pins the same pointer N times is
//! still balanced correctly, without the two layers having to agree on a count.

use std::sync::Mutex;

use crate::runtime_abort;

/// The pinned payload pointers, stored as `usize` so the `static` needs no
/// `unsafe impl Send` (raw pointers are not `Send`). `Mutex::new` and
/// `Vec::new` are both `const`, so this initializes without a `OnceLock`.
///
/// Accessed only between mutator steps (the glue pins/unpins during an extern
/// call or a host-side release) and during the mark phase — never re-entrantly
/// within a single collection — so the lock is uncontended in the single-
/// threaded runtime and never aliases the heap mutex.
static PINNED: Mutex<Vec<usize>> = Mutex::new(Vec::new());

fn lock() -> std::sync::MutexGuard<'static, Vec<usize>> {
    match PINNED.lock() {
        Ok(g) => g,
        // Mirror `lock_heap`: a poisoned mutex must not unwind across the C ABI.
        Err(_) => runtime_abort("GC pin set mutex poisoned"),
    }
}

/// Add one pin for `payload`, keeping it reachable across collections until a
/// balancing [`unpin`]. A null pointer is ignored (the glue maps a null/absent
/// closure to 0 and never pins it).
pub(crate) fn pin(payload: *mut u8) {
    if payload.is_null() {
        return;
    }
    lock().push(payload as usize);
}

/// Remove one pin for `payload`. A no-op if `payload` is not pinned — the glue's
/// retention table is the source of truth for balance, and a defensive double-
/// release from a `FinalizationRegistry` racing an explicit `release()` must not
/// underflow into a negative pin count.
pub(crate) fn unpin(payload: *mut u8) {
    if payload.is_null() {
        return;
    }
    let mut v = lock();
    if let Some(i) = v.iter().rposition(|&p| p == payload as usize) {
        v.swap_remove(i);
    }
}

/// Snapshot the pinned pointers into `buf` (cleared first) and invoke `visit` on
/// each, mirroring [`shadow_stack::for_each_root_into`](super::shadow_stack::for_each_root_into).
/// The snapshot is taken under the lock and released *before* `visit` runs so a
/// visitor that allocates (and could pin/unpin) cannot deadlock on `PINNED`. The
/// sole caller today — the mark phase's root scan — only marks and never
/// allocates, so the snapshot is defensive (future-proofing against an
/// allocating visitor) rather than load-bearing. Duplicate pins are passed
/// through as-is; the mark phase's `is_marked` check makes re-marking a pointer
/// idempotent.
pub(crate) fn for_each_pinned_into<F: FnMut(*mut u8)>(buf: &mut Vec<*mut u8>, mut visit: F) {
    buf.clear();
    {
        let v = lock();
        buf.extend(v.iter().map(|&p| p as *mut u8));
    }
    for &p in buf.iter() {
        visit(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_then_unpin_balances() {
        let p = 0xABCD_0000usize as *mut u8;
        pin(p);
        let mut buf = Vec::new();
        let mut seen = Vec::new();
        for_each_pinned_into(&mut buf, |x| seen.push(x));
        assert!(
            seen.contains(&p),
            "pinned pointer should be visited: {seen:?}"
        );

        unpin(p);
        let mut seen2 = Vec::new();
        for_each_pinned_into(&mut buf, |x| seen2.push(x));
        assert!(
            !seen2.contains(&p),
            "unpinned pointer should not be visited: {seen2:?}"
        );
    }

    #[test]
    fn multiset_requires_balanced_unpins() {
        let p = 0x1234_0000usize as *mut u8;
        pin(p);
        pin(p);
        unpin(p);
        let mut buf = Vec::new();
        let mut seen = Vec::new();
        for_each_pinned_into(&mut buf, |x| seen.push(x));
        assert!(
            seen.contains(&p),
            "a doubly-pinned pointer stays rooted after one unpin: {seen:?}"
        );

        unpin(p);
        let mut seen2 = Vec::new();
        for_each_pinned_into(&mut buf, |x| seen2.push(x));
        assert!(
            !seen2.contains(&p),
            "the second unpin should drop the pin: {seen2:?}"
        );
    }

    #[test]
    fn unpin_unknown_is_a_noop() {
        // Defensive double-release: unpinning a pointer that was never pinned
        // must not panic or underflow.
        unpin(0xDEAD_0000usize as *mut u8);
    }

    #[test]
    fn null_is_ignored() {
        pin(std::ptr::null_mut());
        unpin(std::ptr::null_mut());
        let mut buf = Vec::new();
        let mut count = 0;
        for_each_pinned_into(&mut buf, |_| count += 1);
        // No assertion on the absolute count (other tests share the process-
        // global set under parallel `cargo test`); just assert null never
        // shows up.
        for_each_pinned_into(&mut buf, |p| assert!(!p.is_null()));
        let _ = count;
    }
}
