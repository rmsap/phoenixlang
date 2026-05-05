//! Mark-and-sweep heap.
//!
//! Uses Rust's global allocator for the underlying memory; the GC's job
//! is to *track* every allocation so that `sweep` can free unreachable
//! ones. A future revision will replace this with a size-class arena
//! (subordinate decision B) without changing the trait surface.
//!
//! Roots are precise: walked from the per-thread shadow stack (see
//! [`super::shadow_stack`]). Interior tracing is conservative: every
//! 8-byte-aligned word in a payload is checked against the registry
//! and, if it matches a known header, that object is also reachable.
//! Strings skip the interior scan since their payload is raw UTF-8 bytes.
//!
//! ## Perf note for the next revision
//!
//! `header_for_payload` runs once per 8-byte word inside every traced
//! payload, so the `HashSet<*mut ObjectHeader>` lookup dominates mark
//! cost on List/Map-heavy heaps. The Phase 2.7 perf-tuning slot
//! (segregated free lists, per-tag trace tables) is the natural place
//! to swap the registry for a sorted `Vec` + binary search (better
//! locality) or a bloom-filtered HashSet (cheaper negative lookups).
//! Don't optimize before that bench lands — the current shape is
//! correct and the workload that tells us which direction to take is
//! exactly what 2.7 will measure.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::collections::HashSet;

use super::{
    DEFAULT_COLLECTION_THRESHOLD, GcHeap, HEADER_SIZE, ObjectHeader, TypeTag, shadow_stack,
};
use crate::runtime_abort;

/// Header alignment. Every allocation is 8-byte aligned, which means
/// every payload pointer is also 8-byte aligned (header is 8 bytes).
const ALIGN: usize = 8;

pub(crate) struct MarkSweepHeap {
    /// All live header pointers. After sweep, only reachable headers remain.
    /// `HashSet` for O(1) "is this a known header?" checks during the
    /// conservative interior scan.
    headers: HashSet<*mut ObjectHeader>,

    /// Bytes allocated since the last collection. Triggers a collection
    /// when it crosses [`DEFAULT_COLLECTION_THRESHOLD`].
    bytes_since_collect: usize,

    /// Total bytes currently held alive (sum of payload sizes).
    live_bytes: usize,

    /// When true, allocations check the threshold and run a collection
    /// if exceeded. **Off by default** so a heap without shadow-stack
    /// roots wired from compiled code (Phase 2.3 mid-migration) cannot
    /// accidentally collect everything. Step 3 of Phase 2.3 flips this
    /// on once Cranelift emits shadow-stack frames.
    auto_collect: bool,

    /// Byte threshold that triggers an automatic collection. Defaults
    /// to [`DEFAULT_COLLECTION_THRESHOLD`]; tests can lower this so
    /// the auto-collect path runs without allocating a megabyte.
    threshold: usize,

    /// Reused across `mark_phase` calls: snapshot buffer for
    /// `shadow_stack::for_each_root_into`. Held on the heap so a hot
    /// collect loop doesn't churn the global allocator.
    roots_buf: Vec<*mut u8>,

    /// Reused across `mark_phase` calls: BFS work-list for the
    /// conservative interior scan. Held on the heap for the same
    /// reason as `roots_buf`.
    mark_work: Vec<*mut ObjectHeader>,

    /// Whether this heap is the process-wide singleton driven by the
    /// Cranelift-emitted shadow stack. Only the singleton has a meaningful
    /// "are other threads holding frames?" question to answer — local
    /// heaps used by unit tests are private to the test thread, share no
    /// state with the global TLS shadow stack, and would otherwise
    /// `runtime_abort` whenever a sibling test (running in parallel under
    /// `cargo test`) happens to be mid-frame. See
    /// [`Self::assert_safe_to_collect`].
    is_singleton: bool,
}

// SAFETY: `MarkSweepHeap` is only ever accessed through a `Mutex`, and
// the raw pointers stored inside are managed exclusively by the GC.
// The `Mutex` serializes concurrent accesses to the registry, but does
// *not* make cross-thread *collection* safe — the shadow stack is
// per-thread, so a collection triggered on the wrong thread sweeps
// roots it can't see. `assert_safe_to_collect` (called at the top of
// `collect`) `runtime_abort`s in that case rather than corrupt memory
// silently. Allocations alone don't sweep, so they don't need the same
// gate.
unsafe impl Send for MarkSweepHeap {}

impl MarkSweepHeap {
    /// Build a heap for *local* use (unit tests). The cross-thread collection
    /// gate is disabled — the heap is private to the constructing thread, so
    /// other threads' shadow-stack frames are irrelevant. Production code
    /// must call [`Self::new_singleton`] instead.
    pub(crate) fn new() -> Self {
        Self {
            headers: HashSet::new(),
            bytes_since_collect: 0,
            live_bytes: 0,
            auto_collect: false,
            threshold: DEFAULT_COLLECTION_THRESHOLD,
            roots_buf: Vec::new(),
            mark_work: Vec::new(),
            is_singleton: false,
        }
    }

    /// Build the process-wide singleton heap. Enables the cross-thread
    /// collection gate (see [`Self::assert_safe_to_collect`]). Used by
    /// [`super::heap`] at first allocation and by [`super::phx_gc_shutdown`]
    /// when it swaps in a fresh empty heap.
    pub(crate) fn new_singleton() -> Self {
        let mut h = Self::new();
        h.is_singleton = true;
        h
    }

    /// Best-effort fence against cross-thread collection.
    ///
    /// Called at the top of [`Self::collect`]. The mark phase walks
    /// the calling thread's TLS shadow stack only; if another thread
    /// has live frames, those frames hold roots the mark phase can't
    /// see and the sweep would silently free them. This check reads
    /// the global / per-thread frame counters and aborts when they
    /// disagree.
    ///
    /// **Not a memory-safety invariant on its own.** The two counters
    /// are atomics read outside any synchronization that covers
    /// `push_frame`, so under a multi-threaded mutator another thread
    /// can push a frame between the check and the start of `mark_phase`
    /// and the gate would not catch it. The Phase 2.3 design is
    /// single-threaded — the gate is a guardrail against accidental
    /// misuse under cargo libtest's parallel runner, not a watertight
    /// concurrency contract. Phase 4.3 will revisit with a proper
    /// safepoint protocol.
    ///
    /// Why here and not in `lock_heap`: allocations don't sweep
    /// anything, so registering a fresh allocation from any thread
    /// is harmless. Only collection turns "different thread" into
    /// "your roots are gone."
    ///
    /// **Singleton-only.** The check is meaningful only for the process-
    /// wide singleton (the one Cranelift-emitted code allocates against).
    /// Local heaps built by unit tests share no state with the global TLS
    /// shadow stack, so a sibling test on another thread holding frames
    /// has no bearing on this heap's correctness — without the gate, such
    /// sibling traffic would `runtime_abort` the whole test binary.
    fn assert_safe_to_collect(&self) {
        if !self.is_singleton {
            return;
        }
        if shadow_stack::other_threads_hold_frames() {
            runtime_abort(
                "GC collect from a thread other than the one holding \
                 live shadow-stack frames — the mark phase walks TLS, \
                 so this would sweep roots it can't see. The Phase 2.3 \
                 runtime is single-threaded; see \
                 docs/design-decisions.md#gc-implementation-subordinate-decisions",
            );
        }
    }

    /// Enable or disable threshold-triggered automatic collection.
    pub(crate) fn set_auto_collect(&mut self, enable: bool) {
        self.auto_collect = enable;
    }

    /// Set the byte threshold that triggers automatic collection.
    /// Exposed for tests; production code uses the
    /// [`DEFAULT_COLLECTION_THRESHOLD`].
    pub(crate) fn set_threshold(&mut self, threshold: usize) {
        self.threshold = threshold;
    }

    fn raw_alloc(&mut self, payload_size: usize, tag: TypeTag) -> *mut u8 {
        // Threshold check lives here so the "decide whether to collect" and
        // "register the new allocation" logic share one call site. A second
        // alloc entry point that forgot to mirror the check would otherwise
        // skip auto-collect silently. `GcHeap::alloc` is now a thin wrapper.
        if self.auto_collect && self.bytes_since_collect >= self.threshold {
            self.collect();
        }
        // Compute the on-heap payload size. Two adjustments stack:
        //   1. Bump zero-size to one word so the allocator returns a
        //      valid pointer.
        //   2. For payloads the conservative interior scan will walk
        //      (everything except `String`), round up to `ALIGN` so no
        //      pointer-width trailer is silently skipped. Caller-side
        //      sizes today (`HEADER_SIZE + count*elem_size` from list/map
        //      with elem_size ∈ {8, 16}) are already multiples of 8, but
        //      this is load-bearing — a future 4-byte elem_size would
        //      otherwise cut off the trailing word. String payloads are
        //      raw UTF-8 and never scanned, so any byte length is fine.
        let actual_payload = if payload_size == 0 {
            ALIGN
        } else if tag.scans_interior() {
            payload_size.next_multiple_of(ALIGN)
        } else {
            payload_size
        };
        let total = HEADER_SIZE + actual_payload;
        let Ok(layout) = Layout::from_size_align(total, ALIGN) else {
            runtime_abort(&format!("GC alloc: invalid layout for {total} bytes"));
        };
        let raw = unsafe { alloc_zeroed(layout) };
        if raw.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        debug_assert!(
            (raw as usize).is_multiple_of(ALIGN),
            "GC allocation returned unaligned pointer"
        );

        let header_ptr = raw as *mut ObjectHeader;
        unsafe { header_ptr.write(ObjectHeader::new(actual_payload, tag)) };
        self.headers.insert(header_ptr);
        self.live_bytes += actual_payload;
        self.bytes_since_collect += actual_payload;

        // SAFETY: `raw + HEADER_SIZE` lands inside the same allocation
        // and is the start of the payload region.
        unsafe { raw.add(HEADER_SIZE) }
    }

    /// Walk the shadow stack and conservatively trace through each live
    /// object's payload. Marks every reachable header.
    ///
    /// **Call sites:** [`Self::collect`] only. The cross-thread
    /// safety check ([`Self::assert_safe_to_collect`]) gates collection
    /// at the top of `collect`; calling `mark_phase` directly would
    /// bypass that gate and let the sweep that follows free roots held
    /// on another thread. New collection entry-points must route
    /// through `collect` (or replicate the gate explicitly).
    fn mark_phase(&mut self) {
        // Move the heap-owned buffers out so the closure / loop below
        // can borrow `self` for `header_for_payload` without aliasing.
        // We restore them at the end.
        let mut roots = std::mem::take(&mut self.roots_buf);
        let mut work = std::mem::take(&mut self.mark_work);
        work.clear();

        shadow_stack::for_each_root_into(&mut roots, |root| {
            if let Some(header) = self.header_for_payload(root)
                && !unsafe { (*header).is_marked() }
            {
                unsafe { (*header).set_mark() };
                work.push(header);
            }
        });

        while let Some(header) = work.pop() {
            let tag = unsafe { (*header).tag() };
            if !tag.scans_interior() {
                continue;
            }
            let payload_size = unsafe { (*header).payload_size() };
            let payload = unsafe { (header as *mut u8).add(HEADER_SIZE) };

            // Walk every 8-byte-aligned word in the payload. Treat each
            // as a candidate pointer; if it matches a registered header's
            // payload address, mark that header.
            let mut offset = 0usize;
            while offset + std::mem::size_of::<usize>() <= payload_size {
                let word_ptr = unsafe { payload.add(offset) as *const usize };
                let candidate = unsafe { *word_ptr } as *mut u8;
                if let Some(child) = self.header_for_payload(candidate)
                    && !unsafe { (*child).is_marked() }
                {
                    unsafe { (*child).set_mark() };
                    work.push(child);
                }
                offset += std::mem::size_of::<usize>();
            }
        }

        self.roots_buf = roots;
        self.mark_work = work;
    }

    /// If `payload_ptr` is the payload of a registered allocation,
    /// return its header. Otherwise return `None`.
    fn header_for_payload(&self, payload_ptr: *mut u8) -> Option<*mut ObjectHeader> {
        // Fast-reject obviously-non-pointer values before hashing. The
        // conservative interior scan reads every 8-byte word in a
        // payload, so for `List<Int>` etc. the vast majority of
        // candidates are small integers. Skipping them avoids the
        // HashSet probe entirely. The cutoff is conservative: real heap
        // addresses on every supported target sit far above 64 KiB.
        const SMALL_PTR_CUTOFF: usize = 0x1_0000;
        let raw = payload_ptr as usize;
        if raw < SMALL_PTR_CUTOFF {
            return None;
        }
        // Header sits at payload - HEADER_SIZE.
        let candidate = raw.checked_sub(HEADER_SIZE)? as *mut ObjectHeader;
        if !(candidate as usize).is_multiple_of(ALIGN) {
            return None;
        }
        if self.headers.contains(&candidate) {
            Some(candidate)
        } else {
            None
        }
    }

    /// Free every unmarked allocation; clear marks on survivors.
    ///
    /// **Call sites:** [`Self::collect`] only. Calling `sweep_phase`
    /// without a preceding `mark_phase` (or without
    /// [`Self::assert_safe_to_collect`] gating cross-thread misuse)
    /// would free every live allocation, since no headers carry the
    /// mark bit. New collection entry-points must route through
    /// `collect`.
    fn sweep_phase(&mut self) {
        let mut freed_bytes = 0usize;
        self.headers.retain(|&header_ptr| {
            let header = unsafe { &mut *header_ptr };
            if header.is_marked() {
                header.clear_mark();
                true
            } else {
                let payload_size = header.payload_size();
                freed_bytes += payload_size;
                let total = HEADER_SIZE + payload_size;
                let Ok(layout) = Layout::from_size_align(total, ALIGN) else {
                    runtime_abort(&format!("GC sweep: invalid layout for {total} bytes"));
                };
                unsafe { dealloc(header_ptr as *mut u8, layout) };
                false
            }
        });
        debug_assert!(
            self.live_bytes >= freed_bytes,
            "GC accounting underflow: live={} freed={}",
            self.live_bytes,
            freed_bytes,
        );
        self.live_bytes -= freed_bytes;
        self.bytes_since_collect = 0;
    }
}

impl Drop for MarkSweepHeap {
    fn drop(&mut self) {
        // On process exit, free everything still tracked. This is what
        // makes "no leaks under valgrind" possible. `drain()` empties the
        // set as it yields each pointer, so we never iterate over a
        // structure whose elements we just freed.
        for header_ptr in self.headers.drain() {
            let payload_size = unsafe { (*header_ptr).payload_size() };
            let total = HEADER_SIZE + payload_size;
            let Ok(layout) = Layout::from_size_align(total, ALIGN) else {
                runtime_abort(&format!("GC drop: invalid layout for {total} bytes"));
            };
            unsafe { dealloc(header_ptr as *mut u8, layout) };
        }
        self.live_bytes = 0;
    }
}

impl GcHeap for MarkSweepHeap {
    fn alloc(&mut self, size: usize, tag: TypeTag) -> *mut u8 {
        self.raw_alloc(size, tag)
    }

    fn collect(&mut self) {
        self.assert_safe_to_collect();
        self.mark_phase();
        self.sweep_phase();
    }

    fn live_bytes(&self) -> usize {
        self.live_bytes
    }

    fn live_objects(&self) -> usize {
        self.headers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a heap with auto-collection disabled so tests can drive
    /// mark/sweep deterministically.
    fn manual_heap() -> MarkSweepHeap {
        let mut h = MarkSweepHeap::new();
        h.auto_collect = false;
        h
    }

    #[test]
    fn alloc_returns_aligned_payload() {
        let mut h = manual_heap();
        let ptr = h.alloc(32, TypeTag::Unknown);
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % ALIGN, 0, "payload must be 8-aligned");
        assert_eq!(h.live_objects(), 1);
        assert_eq!(h.live_bytes(), 32);
    }

    #[test]
    fn alloc_returns_zeroed_payload() {
        // Compiled code (struct/enum/closure stores) writes only the
        // fields it knows about and assumes anything it doesn't touch
        // is zero. If raw_alloc ever stopped zeroing, generic-payload
        // enums (whose unused trailing slots stay at the alloc default)
        // would suddenly carry garbage interior words that the
        // conservative scan would chase.
        let mut h = manual_heap();
        let payload_size = 64;
        let ptr = h.alloc(payload_size, TypeTag::Unknown);
        let payload = unsafe { std::slice::from_raw_parts(ptr, payload_size) };
        assert!(
            payload.iter().all(|&b| b == 0),
            "raw_alloc must hand back zeroed payload bytes"
        );
    }

    #[test]
    fn unrooted_alloc_swept() {
        let mut h = manual_heap();
        let _ptr = h.alloc(64, TypeTag::Unknown);
        // No shadow-stack roots, so collect frees it.
        h.collect();
        assert_eq!(h.live_objects(), 0);
        assert_eq!(h.live_bytes(), 0);
    }

    #[test]
    fn rooted_alloc_survives() {
        let mut h = manual_heap();
        let ptr = h.alloc(64, TypeTag::Unknown);

        let frame = shadow_stack::push_frame(1);
        unsafe { shadow_stack::set_root(frame, 0, ptr) };

        h.collect();
        assert_eq!(h.live_objects(), 1, "rooted object must survive");

        unsafe { shadow_stack::pop_frame(frame) };

        h.collect();
        assert_eq!(h.live_objects(), 0, "after pop, object is unreachable");
    }

    #[test]
    fn interior_pointer_traced() {
        let mut h = manual_heap();
        // Allocate a child first.
        let child = h.alloc(16, TypeTag::Unknown);
        // Now allocate a parent and write the child pointer into it.
        let parent = h.alloc(16, TypeTag::Unknown);
        unsafe {
            *(parent as *mut *mut u8) = child;
        }

        // Root only the parent.
        let frame = shadow_stack::push_frame(1);
        unsafe { shadow_stack::set_root(frame, 0, parent) };

        h.collect();
        assert_eq!(
            h.live_objects(),
            2,
            "child reachable through parent's interior pointer"
        );

        unsafe { shadow_stack::pop_frame(frame) };
    }

    #[test]
    fn string_payload_not_scanned() {
        let mut h = manual_heap();
        let target = h.alloc(16, TypeTag::Unknown);
        let s = h.alloc(16, TypeTag::String);
        // Write the target pointer into the string's bytes — but since
        // strings skip the interior scan, this should NOT keep `target` alive.
        unsafe {
            *(s as *mut *mut u8) = target;
        }

        // Root only the string.
        let frame = shadow_stack::push_frame(1);
        unsafe { shadow_stack::set_root(frame, 0, s) };

        h.collect();
        assert_eq!(
            h.live_objects(),
            1,
            "string survives but `target` is collected"
        );

        unsafe { shadow_stack::pop_frame(frame) };
    }

    #[test]
    fn integer_payload_words_do_not_falsely_retain() {
        // Conservative interior scan reads each 8-byte word in a non-
        // string payload as a candidate pointer. Small integers (the
        // common case for `List<Int>`) must be rejected before they
        // can spuriously keep an allocation alive.
        let mut h = manual_heap();
        let target = h.alloc(16, TypeTag::Unknown);
        let target_addr = target as usize;

        // Parent payload is 32 bytes (4 i64 slots) full of small
        // integers. None of these match `target` or any other live
        // header, so `target` must still be collected.
        let parent = h.alloc(32, TypeTag::Unknown);
        unsafe {
            let words = parent as *mut i64;
            *words = 0;
            *words.add(1) = 1;
            *words.add(2) = 42;
            *words.add(3) = 12345;
        }
        // Sanity: the small-integer cutoff in `header_for_payload`
        // must be well below any real heap address — guard against a
        // future cutoff bump that would shadow this assertion.
        assert!(
            target_addr > 0x1_0000,
            "real heap addresses must exceed the small-pointer cutoff"
        );

        let frame = shadow_stack::push_frame(1);
        unsafe { shadow_stack::set_root(frame, 0, parent) };

        h.collect();
        assert_eq!(
            h.live_objects(),
            1,
            "parent survives via root; integer payload words must not \
             retain `target`",
        );

        unsafe { shadow_stack::pop_frame(frame) };
    }

    #[test]
    fn cycle_collected_when_unrooted() {
        let mut h = manual_heap();
        let a = h.alloc(16, TypeTag::Unknown);
        let b = h.alloc(16, TypeTag::Unknown);
        unsafe {
            *(a as *mut *mut u8) = b;
            *(b as *mut *mut u8) = a;
        }
        // No roots — both should die despite the cycle.
        h.collect();
        assert_eq!(h.live_objects(), 0);
    }

    #[test]
    fn auto_collect_fires_at_threshold() {
        let mut h = MarkSweepHeap::new();
        h.set_auto_collect(true);
        // Tiny threshold so auto-collect fires after only a few allocations.
        h.set_threshold(256);

        // Allocate well past the threshold with no shadow-stack roots.
        // The auto-collect path must run at least once; without it the
        // count would equal `n_allocs`.
        let n_allocs = 32;
        for _ in 0..n_allocs {
            let _ = h.alloc(64, TypeTag::Unknown);
        }

        // After auto-collect fired one or more times during the loop,
        // the live count is strictly less than `n_allocs` (each sweep
        // frees every unrooted allocation seen so far).
        assert!(
            h.live_objects() < n_allocs,
            "auto-collect should have run at least once and freed \
             unrooted allocations; saw {} live (expected < {})",
            h.live_objects(),
            n_allocs,
        );
    }

    #[test]
    fn header_for_payload_rejects_small_addresses() {
        // The conservative scan calls `header_for_payload` on every
        // 8-byte word inside a payload. Small integers (which dominate
        // typical `List<Int>` content) must be rejected by the
        // `SMALL_PTR_CUTOFF` gate before they hit the registry probe;
        // the `checked_sub(HEADER_SIZE)` backstop catches the same case
        // if the cutoff is ever lowered. This test exercises both:
        // values across the whole `0..SMALL_PTR_CUTOFF` range — and a
        // value at the bottom of the address space — must return None
        // even when the heap has live allocations to compete with.
        let mut h = manual_heap();
        let _live = h.alloc(32, TypeTag::Unknown);

        for raw in [0usize, 1, 4, 7, 8, 0xFFFF] {
            assert!(
                h.header_for_payload(raw as *mut u8).is_none(),
                "small candidate {raw:#x} must be rejected"
            );
        }

        // Boundary: exactly at `SMALL_PTR_CUTOFF` (0x10000) is the first
        // value that *passes* the small-pointer gate. It must still be
        // rejected — by the registry miss in this case (no real allocation
        // sits at this address). Just-above-and-misaligned (0x10001) is
        // rejected by the alignment check before the registry probe.
        for raw in [0x1_0000usize, 0x1_0001] {
            assert!(
                h.header_for_payload(raw as *mut u8).is_none(),
                "boundary candidate {raw:#x} must be rejected"
            );
        }

        // A misaligned address above the cutoff is rejected by the
        // alignment check rather than the cutoff. Pick an address in
        // the same range as a real heap pointer (use one of our own
        // allocations + 1 so it can't collide with anything else).
        let live_ptr = _live as usize;
        assert!(
            h.header_for_payload((live_ptr + 1) as *mut u8).is_none(),
            "misaligned candidate must be rejected"
        );
    }

    #[test]
    fn header_for_payload_rejects_high_non_pointer_words() {
        // The conservative interior scan reads every 8-byte word in a
        // non-string payload. Large non-pointer values (e.g. a string's
        // length stored alongside its pointer in a fat-pointer element)
        // can sit well above `SMALL_PTR_CUTOFF` — they pass the cheap
        // gate and must be rejected by the registry miss instead. This
        // test exercises that path with values that look pointer-shaped
        // (8-aligned, far above the cutoff) but are not in the headers
        // set.
        let mut h = manual_heap();
        let _live = h.alloc(32, TypeTag::Unknown);

        for raw in [
            0x0000_FFFF_0000_0000usize,
            0xFFFF_0000_0000_0000,
            0x7FFF_FFFF_FFFF_FFF8,
            usize::MAX & !7,
        ] {
            assert!(
                h.header_for_payload(raw as *mut u8).is_none(),
                "high non-pointer word {raw:#x} must be rejected by registry miss",
            );
        }
    }

    #[test]
    fn drop_frees_every_tracked_allocation() {
        // Build a heap with several allocations, drop it, and confirm
        // (via Miri / valgrind in CI) that no allocation leaks. We
        // can't directly observe deallocation here, but we *can* prove
        // that Drop completes cleanly without aborting and that the
        // headers set is emptied.
        let mut h = MarkSweepHeap::new();
        for _ in 0..16 {
            let _ = h.alloc(48, TypeTag::Unknown);
        }
        for _ in 0..4 {
            let _ = h.alloc(16, TypeTag::String);
        }
        assert_eq!(h.live_objects(), 20);
        // Manually invoke Drop so we can inspect post-drop state. After
        // Drop, the headers set is empty and live_bytes is zero.
        drop(h);
        // (No further state to inspect — but if this test passes under
        // Miri, every Layout/dealloc pair was matched.)
    }
}
