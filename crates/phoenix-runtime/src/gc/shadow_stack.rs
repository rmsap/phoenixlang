//! Per-thread shadow stack of GC root frames.
//!
//! Cranelift-generated code calls [`push_frame`] at function entry,
//! [`set_root`] whenever a ref-typed SSA value is produced, and
//! [`pop_frame`] at every function exit. The mark phase walks the
//! linked list of frames and treats each root slot as a precise root.
//!
//! Single-threaded for now (Phase 4.3 will revisit). State lives in a
//! `thread_local!` so multi-threaded test runners don't share frames.
//!
//! ## Re-entrance contract
//!
//! [`for_each_root_into`] is the only re-entrant API: it snapshots
//! every live root pointer into a caller-owned buffer *before*
//! invoking the visitor, so the visitor may freely call back through
//! [`push_frame`]/[`pop_frame`] (e.g. from a logging path that
//! allocates) without aliasing the `RefCell` borrow on `TOP`.
//! [`push_frame`], [`pop_frame`], and [`set_root`] are themselves
//! **not** re-entrant: they hold a `RefCell` borrow on `TOP` for
//! their duration, so any code path between the borrow and its
//! release that re-enters one of these would panic the `RefCell`.
//! The current call sites (`alloc_zeroed`, `dealloc`, atomic ops)
//! make this trivially safe.

use std::cell::{Cell, RefCell};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::runtime_abort;

/// Global count of pushed-but-not-popped frames across every thread.
///
/// Paired with the per-thread [`THIS_THREAD_FRAME_COUNT`] so the heap
/// can answer "are there any frames live on a thread *other than*
/// this one?" — see [`other_threads_hold_frames`] for the exact
/// predicate and `MarkSweepHeap::assert_safe_to_collect` for how it's
/// used to gate collection.
static LIVE_FRAME_COUNT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// This thread's contribution to [`LIVE_FRAME_COUNT`]. Updated in
    /// lockstep with the global counter on every push/pop so the
    /// difference (`global - this_thread`) gives us frames held by
    /// other threads, without iterating thread TLS.
    static THIS_THREAD_FRAME_COUNT: Cell<usize> = const { Cell::new(0) };
}

/// Whether any *other* thread currently holds shadow-stack frames.
///
/// Used by the heap's owner check: when a different thread tries to
/// allocate against a non-empty registry, the rebind is safe iff this
/// returns false. If only the current thread holds frames (or no one
/// does), a collection under this thread's TLS sees every live root
/// the GC needs to see — the previous owner's leftover allocations
/// are unreachable, so sweeping them is correct.
///
/// `Relaxed` is enough: this is read inside the heap mutex, and the
/// mutex provides the synchronization that matters.
pub(crate) fn other_threads_hold_frames() -> bool {
    let global = LIVE_FRAME_COUNT.load(Ordering::Relaxed);
    let mine = THIS_THREAD_FRAME_COUNT.with(|c| c.get());
    global > mine
}

/// One shadow-stack frame: a header + N root slots.
///
/// Allocated on the heap (small allocation, freed on `pop_frame`) so the
/// frame's address is stable regardless of how Cranelift lays out the
/// caller's stack.
#[repr(C)]
pub struct Frame {
    /// Link to the previous frame, or null.
    prev: *mut Frame,
    /// Number of root slots that follow.
    n_roots: usize,
    // Followed by `n_roots * 8` bytes of root storage. The first slot
    // starts at `(self as *mut u8).add(size_of::<Frame>())`.
}

impl Frame {
    /// Pointer to the start of the root storage area (mutable).
    fn roots_ptr_mut(&mut self) -> *mut *mut u8 {
        unsafe { (self as *mut Frame as *mut u8).add(std::mem::size_of::<Frame>()) as *mut *mut u8 }
    }

    /// Pointer to the start of the root storage area (read-only).
    fn roots_ptr(&self) -> *const *mut u8 {
        unsafe {
            (self as *const Frame as *const u8).add(std::mem::size_of::<Frame>()) as *const *mut u8
        }
    }

    /// Iterate over all root pointers in this frame.
    pub(crate) fn roots(&self) -> &[*mut u8] {
        unsafe { std::slice::from_raw_parts(self.roots_ptr(), self.n_roots) }
    }
}

thread_local! {
    /// Top of this thread's shadow-stack linked list.
    static TOP: RefCell<*mut Frame> = const { RefCell::new(ptr::null_mut()) };
}

/// Allocate and link a new frame with `n_roots` slots, all null.
pub(crate) fn push_frame(n_roots: usize) -> *mut Frame {
    let frame_size = std::mem::size_of::<Frame>() + n_roots * std::mem::size_of::<*mut u8>();
    let Ok(layout) = std::alloc::Layout::from_size_align(frame_size, std::mem::align_of::<Frame>())
    else {
        runtime_abort("shadow-stack push_frame: invalid layout");
    };
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) } as *mut Frame;
    if ptr.is_null() {
        std::alloc::handle_alloc_error(layout);
    }

    TOP.with(|top| {
        let prev = *top.borrow();
        unsafe {
            (*ptr).prev = prev;
            (*ptr).n_roots = n_roots;
        }
        *top.borrow_mut() = ptr;
    });
    LIVE_FRAME_COUNT.fetch_add(1, Ordering::Relaxed);
    THIS_THREAD_FRAME_COUNT.with(|c| c.set(c.get() + 1));

    ptr
}

/// Pop the top frame. `expected` must match the current top (sanity check).
///
/// # Safety
///
/// `expected` must be the pointer returned by the matching `push_frame`.
pub(crate) unsafe fn pop_frame(expected: *mut Frame) {
    TOP.with(|top| {
        let cur = *top.borrow();
        // Always-on check (not `debug_assert!`) so a release-mode mismatch
        // fails loudly instead of silently leaking `expected` or popping
        // the wrong frame. Goes through `runtime_abort` rather than
        // `panic!` because this runs from `extern "C"` callers and
        // unwinding across the FFI boundary is UB.
        if cur != expected {
            runtime_abort(&format!(
                "shadow-stack pop mismatch: expected {expected:p}, got {cur:p}"
            ));
        }
        let prev = unsafe { (*cur).prev };
        let n_roots = unsafe { (*cur).n_roots };
        *top.borrow_mut() = prev;

        let frame_size = std::mem::size_of::<Frame>() + n_roots * std::mem::size_of::<*mut u8>();
        let Ok(layout) =
            std::alloc::Layout::from_size_align(frame_size, std::mem::align_of::<Frame>())
        else {
            runtime_abort("shadow-stack pop_frame: invalid layout");
        };
        unsafe { std::alloc::dealloc(cur as *mut u8, layout) };
    });
    // Underflow on either counter would silently wrap to `usize::MAX` and
    // corrupt every subsequent `assert_safe_to_collect` answer. Single-
    // threaded design says this can't happen (the mismatch check above
    // already proved a frame was live for *this* thread). Phase 4.3
    // will revisit alongside the broader safepoint-protocol work.
    //
    // `fetch_sub` returns the previous value atomically; if it was 0,
    // the counter has just wrapped to `usize::MAX` and we abort before
    // any caller can act on the wrapped value. `Cell` has no atomic
    // RMW, so the per-thread counter keeps the explicit check pattern.
    let prev_global = LIVE_FRAME_COUNT.fetch_sub(1, Ordering::Relaxed);
    if prev_global == 0 {
        runtime_abort("shadow-stack pop: LIVE_FRAME_COUNT underflow");
    }

    THIS_THREAD_FRAME_COUNT.with(|c| {
        let prev_local = c.get();
        if prev_local == 0 {
            runtime_abort("shadow-stack pop: THIS_THREAD_FRAME_COUNT underflow");
        }
        c.set(prev_local - 1);
    });
}

/// Set root slot `idx` of `frame` to `ptr`.
///
/// # Safety
///
/// `frame` must be a valid handle returned by `push_frame` and `idx` must
/// be `< n_roots` for that frame.
pub(crate) unsafe fn set_root(frame: *mut Frame, idx: usize, ptr: *mut u8) {
    // Always-on checks (not `debug_assert!`): a stale slot index in a
    // release build would silently corrupt memory past the frame (or
    // another frame's `prev`/`n_roots` if frames are heap-adjacent).
    // Routes through `runtime_abort` rather than `panic!` so the
    // termination path is FFI-safe under the workspace's default
    // `panic = "unwind"`.
    if frame.is_null() {
        runtime_abort("shadow-stack set_root on null frame");
    }
    let frame_ref = unsafe { &mut *frame };
    if idx >= frame_ref.n_roots {
        runtime_abort(&format!(
            "shadow-stack set_root: idx {} >= n_roots {}",
            idx, frame_ref.n_roots
        ));
    }
    let slot = unsafe { frame_ref.roots_ptr_mut().add(idx) };
    unsafe { *slot = ptr };
}

/// Visit every root pointer across all live frames on this thread.
///
/// Snapshots all non-null roots into `buf` *before* invoking `visit` so
/// the visitor can safely re-enter the shadow stack (e.g. via a nested
/// `push_frame` from a logging path) without aliasing the `RefCell`
/// borrow on `TOP`. `buf` is cleared first and reused across calls so a
/// hot collect loop doesn't churn the global allocator.
pub(crate) fn for_each_root_into<F: FnMut(*mut u8)>(buf: &mut Vec<*mut u8>, mut visit: F) {
    buf.clear();
    TOP.with(|top| {
        let mut cur = *top.borrow();
        while !cur.is_null() {
            let frame = unsafe { &*cur };
            for &root in frame.roots() {
                if !root.is_null() {
                    buf.push(root);
                }
            }
            cur = frame.prev;
        }
    });
    for &root in buf.iter() {
        visit(root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_balance() {
        let f1 = push_frame(2);
        let f2 = push_frame(3);
        unsafe {
            pop_frame(f2);
            pop_frame(f1);
        }
    }

    #[test]
    fn set_root_visible_via_for_each() {
        let f = push_frame(2);
        let target = 0xDEADBEEFusize as *mut u8;
        unsafe { set_root(f, 0, target) };

        let mut found = Vec::new();
        let mut buf = Vec::new();
        for_each_root_into(&mut buf, |p| found.push(p));
        assert!(
            found.contains(&target),
            "root not visible to for_each_root: {:?}",
            found
        );

        unsafe { pop_frame(f) };
    }

    #[test]
    fn pop_clears_visibility() {
        let f = push_frame(1);
        let target = 0xCAFEBABEusize as *mut u8;
        unsafe { set_root(f, 0, target) };
        unsafe { pop_frame(f) };

        let mut found = Vec::new();
        let mut buf = Vec::new();
        for_each_root_into(&mut buf, |p| found.push(p));
        assert!(
            !found.contains(&target),
            "popped root should not be visible: {:?}",
            found
        );
    }

    #[test]
    fn null_roots_are_skipped() {
        let f = push_frame(3);
        unsafe { set_root(f, 1, 0x1234usize as *mut u8) };
        // slots 0 and 2 stay null

        let mut visited = 0usize;
        let mut buf = Vec::new();
        for_each_root_into(&mut buf, |_| visited += 1);
        assert_eq!(visited, 1, "only the non-null root should be visited");

        unsafe { pop_frame(f) };
    }

    #[test]
    fn nested_frames_walked_in_order() {
        let f1 = push_frame(1);
        let f2 = push_frame(1);
        unsafe {
            set_root(f1, 0, 0x1111usize as *mut u8);
            set_root(f2, 0, 0x2222usize as *mut u8);
        }

        let mut found = Vec::new();
        let mut buf = Vec::new();
        for_each_root_into(&mut buf, |p| found.push(p as usize));
        assert_eq!(found.len(), 2);
        // Top-of-stack visited first
        assert_eq!(found[0], 0x2222);
        assert_eq!(found[1], 0x1111);

        unsafe {
            pop_frame(f2);
            pop_frame(f1);
        }
    }
}
