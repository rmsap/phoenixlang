//! `ListBuilder<T>` runtime: a transient mutable accumulator that
//! `.freeze()`s into an immutable [`crate::list_methods`] `List<T>`.
//!
//! Phase 2.7 decision F. The published rationale lives in
//! `docs/design-decisions.md` (`#### F. Mutable-builder API for List / Map`).
//!
//! ## Layout — two heap allocations
//!
//! A `ListBuilder<T>` is split across two GC objects so `push()` is
//! truly in-place (the handle pointer stays stable across grow). The
//! alternative — a single allocation that gets re-allocated on
//! overflow — would force the Phoenix-side variable to be reassigned
//! on every grow, which is what we're trying to avoid.
//!
//! ```text
//! Handle (GC-tracked, TypeTag::Closure tag temporarily):
//!   offset 0:   i64   length         (used element slots)
//!   offset 8:   i64   capacity       (allocated slots in `data`)
//!   offset 16:  i64   elem_size      (bytes per element)
//!   offset 24:  i64   frozen         (0 = mutable; 1 = frozen)
//!   offset 32:  *mut  data_ptr       (heap pointer into the buffer object)
//!
//! Buffer (GC-tracked, TypeTag::Unknown — payload bytes scanned conservatively):
//!   offset 0:   u8[ capacity * elem_size ]   element storage
//! ```
//!
//! The handle is what the user binds: `let b: ListBuilder<Int> = …`.
//! The buffer is reachable only through the handle; if the handle goes
//! unrooted, the GC reclaims both.
//!
//! ## Why the handle is tagged `Closure`
//!
//! No `TypeTag::ListBuilder` variant exists yet — adding one means a
//! coordinated change to the `TypeTag` enum and the `type_tag` module
//! in `phoenix-cranelift::builtins`. Until that lands, the handle uses
//! `TypeTag::Closure` (the existing tag for "small fixed-layout object
//! whose fields are pointers + scalars and gets conservatively
//! scanned"). The `data_ptr` field at offset 32 is the load-bearing
//! interior pointer the GC must find via the conservative scan; the
//! scan does walk it (every 8-byte word in the payload is checked
//! against the header registry).
//!
//! TODO(tag): replace `TypeTag::Closure` with a dedicated
//! `TypeTag::ListBuilder` so the mark phase can dispatch a precise
//! trace function (one interior pointer at OFF_DATA_PTR). Tracked in
//! [`docs/known-issues.md` — "Trace tables for typed GC mark phase"](../../../../docs/known-issues.md#trace-tables-for-typed-gc-mark-phase);
//! they reopen together.
//!
//! ## `freeze()` semantics
//!
//! `.freeze()` allocates a fresh `List<T>` of *exact* length and
//! memcpys the used portion of the buffer into it — **O(n)**, not the
//! O(1) pointer-swap that an aliased-layout design could achieve. The
//! reason: `List<T>`'s payload starts at offset 24 (3-word header),
//! while `ListBuilder<T>`'s buffer starts at offset 0 of a separate
//! allocation. They don't share a memory layout, so freeze can't just
//! cast the handle to a List. The win vs the prior immutable-only
//! path is total build cost dropping from O(n²) to O(n) (n pushes,
//! one freeze), which is what the bench numbers in
//! `docs/perf/phoenix-vs-go.md` reflect.

use crate::gc::shadow_stack;
use crate::gc::{TypeTag, phx_gc_alloc};
use crate::list_methods::HEADER_SIZE as LIST_HEADER_SIZE;
use crate::runtime_abort;

/// Builder handle size in bytes (5 × i64).
pub(crate) const BUILDER_HEADER_SIZE: usize = 40;

/// Field offsets inside the handle. Match the layout comment above
/// exactly; if you change these, also update the Cranelift codegen
/// in `crates/phoenix-cranelift/src/translate/list_builder_methods.rs`.
const OFF_LENGTH: usize = 0;
const OFF_CAPACITY: usize = 8;
const OFF_ELEM_SIZE: usize = 16;
const OFF_FROZEN: usize = 24;
const OFF_DATA_PTR: usize = 32;

/// Initial buffer capacity for a freshly-built `ListBuilder`. 8 slots
/// matches the prevailing "small initial size, double on overflow"
/// convention used by `Vec` in Rust and `MIN_BUCKETS` in
/// `map_methods.rs`. Tuned for the bench-corpus workloads where
/// hundreds of small pushes are common — too small a default would
/// double 5+ times on the way to a typical post-push size.
const INITIAL_CAPACITY: usize = 8;

unsafe fn read_i64(ptr: *const u8, offset: usize) -> i64 {
    unsafe { *(ptr.add(offset) as *const i64) }
}

unsafe fn write_i64(ptr: *mut u8, offset: usize, value: i64) {
    unsafe { *(ptr.add(offset) as *mut i64) = value };
}

unsafe fn read_ptr(ptr: *const u8, offset: usize) -> *mut u8 {
    unsafe { *(ptr.add(offset) as *const *mut u8) }
}

unsafe fn write_ptr(ptr: *mut u8, offset: usize, value: *mut u8) {
    unsafe { *(ptr.add(offset) as *mut *mut u8) = value };
}

/// Allocate a fresh `ListBuilder<T>` with capacity 8. Returns a
/// pointer to the handle.
///
/// Aborts via `runtime_abort` on negative input or allocation
/// overflow — this is an `extern "C"` symbol and Phoenix's panic
/// strategy is `unwind`, so panicking would be UB across the FFI
/// boundary.
#[unsafe(no_mangle)]
pub extern "C" fn phx_list_builder_alloc(elem_size: i64) -> *mut u8 {
    // Reject `elem_size <= 0`. No Phoenix value type has size 0 at
    // runtime today (even `Void` is special-cased before this point),
    // so a 0 here would only arise from a codegen bug — but a zero-
    // sized buffer combined with the `length == capacity` grow
    // trigger would let `length` increment forever without ever
    // re-allocating, silently masking accounting bugs. Cheap
    // insurance.
    if elem_size <= 0 {
        runtime_abort(&format!(
            "phx_list_builder_alloc: elem_size must be positive, got {elem_size}"
        ));
    }
    let es = elem_size as usize;
    let capacity = INITIAL_CAPACITY;
    let Some(buffer_size) = es.checked_mul(capacity) else {
        runtime_abort("phx_list_builder_alloc: buffer size overflow");
    };

    // Allocate the buffer first, root it on a transient shadow-stack
    // frame, then allocate the handle. The second `phx_gc_alloc` can
    // trip the auto-collect threshold (see `gc::heap::raw_alloc`); without
    // an explicit root the buffer is reachable only from a Rust local,
    // and the GC scans the shadow stack — not native Rust frames — so
    // it would be swept before we write `data_ptr` into the handle.
    // `map_methods.rs` flags the same hazard around `phx_map_set_raw`'s
    // single-alloc constraint.
    let buffer = phx_gc_alloc(buffer_size, TypeTag::Unknown as u32);
    let frame = shadow_stack::push_frame(1);
    unsafe { shadow_stack::set_root(frame, 0, buffer) };

    let handle = phx_gc_alloc(BUILDER_HEADER_SIZE, TypeTag::Closure as u32);
    unsafe {
        write_i64(handle, OFF_LENGTH, 0);
        write_i64(handle, OFF_CAPACITY, capacity as i64);
        write_i64(handle, OFF_ELEM_SIZE, elem_size);
        write_i64(handle, OFF_FROZEN, 0);
        write_ptr(handle, OFF_DATA_PTR, buffer);
    }
    unsafe { shadow_stack::pop_frame(frame) };
    handle
}

/// Abort if `frozen` is set. Centralized so the message stays
/// consistent across `push` and `freeze`.
unsafe fn assert_unfrozen(handle: *const u8, method: &str) {
    let frozen = unsafe { read_i64(handle, OFF_FROZEN) };
    if frozen != 0 {
        runtime_abort(&format!(
            "ListBuilder.{method}: builder was already frozen (Phase 2.7 decision F: \
             runtime-checked use-after-freeze; static check is a future linearity story)"
        ));
    }
}

/// Push one element into the builder. Grows the buffer 2× on
/// overflow. Aborts via `runtime_abort` if the builder is frozen.
///
/// # Safety
///
/// - `handle` must be a builder allocated by [`phx_list_builder_alloc`].
/// - `elem_ptr` must point to `elem_size` valid bytes; `elem_size`
///   must match the size the builder was constructed with.
/// - **`handle` must be rooted by the caller's shadow-stack frame**
///   for the duration of this call. A grow re-allocates the buffer
///   via `phx_gc_alloc`, which can trigger an auto-collect; without
///   the root, the handle (and the new buffer it now points at) would
///   be reclaimed before this function returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_list_builder_push(
    handle: *mut u8,
    elem_ptr: *const u8,
    elem_size: i64,
) {
    unsafe { assert_unfrozen(handle, "push") };
    let stored_es = unsafe { read_i64(handle, OFF_ELEM_SIZE) };
    if stored_es != elem_size {
        runtime_abort(&format!(
            "phx_list_builder_push: caller elem_size ({elem_size}) does not match \
             builder's stored elem_size ({stored_es})"
        ));
    }
    let es = elem_size as usize;
    let length = unsafe { read_i64(handle, OFF_LENGTH) } as usize;
    let capacity = unsafe { read_i64(handle, OFF_CAPACITY) } as usize;

    let mut data_ptr = unsafe { read_ptr(handle, OFF_DATA_PTR) };
    if length == capacity {
        // Grow 2×; overflow-checked. Cap minimum grow at 1 so a
        // hypothetical zero-capacity builder still makes progress
        // (today INITIAL_CAPACITY is 8, but the runtime contract
        // should not depend on that).
        let new_capacity = capacity.saturating_mul(2).max(1);
        let Some(new_buffer_size) = es.checked_mul(new_capacity) else {
            runtime_abort("phx_list_builder_push: grow size overflow");
        };
        // Pre-alloc `data_ptr` (Rust local read before `phx_gc_alloc`
        // below) stays valid across the alloc for two reasons: (1)
        // mark-sweep doesn't move objects, so the heap address itself
        // doesn't change; (2) the old buffer is reachable via
        // `handle → OFF_DATA_PTR`, and the caller's shadow-stack
        // root on `handle` keeps the chain alive across any
        // auto-collect the alloc triggers. So `data_ptr` is read
        // once and used after the alloc for the memcpy below.
        //
        // Root the freshly-allocated `new_buffer` on the shadow stack
        // before further work. Today nothing between this alloc and
        // the `write_ptr(handle, OFF_DATA_PTR, new_buffer)` below
        // triggers a GC, so the local-only reference would survive
        // — but the safety contract should not depend on that. The
        // alloc functions (`phx_list_builder_alloc`,
        // `phx_map_builder_alloc`) use the same pattern around their
        // second `phx_gc_alloc`.
        let new_buffer = phx_gc_alloc(new_buffer_size, TypeTag::Unknown as u32);
        let frame = shadow_stack::push_frame(1);
        unsafe { shadow_stack::set_root(frame, 0, new_buffer) };
        if length > 0 {
            let copy_bytes = length * es;
            unsafe { std::ptr::copy_nonoverlapping(data_ptr, new_buffer, copy_bytes) };
        }
        unsafe { write_ptr(handle, OFF_DATA_PTR, new_buffer) };
        unsafe { write_i64(handle, OFF_CAPACITY, new_capacity as i64) };
        unsafe { shadow_stack::pop_frame(frame) };
        data_ptr = new_buffer;
    }

    let dst = unsafe { data_ptr.add(length * es) };
    unsafe { std::ptr::copy_nonoverlapping(elem_ptr, dst, es) };
    unsafe { write_i64(handle, OFF_LENGTH, (length + 1) as i64) };
}

/// Freeze the builder and return a fresh `List<T>` of exact length.
/// O(n) memcpy; total build cost across `n` pushes + one freeze is
/// O(n) (vs the immutable-only path's O(n²)). After this call the
/// builder is unusable — every subsequent method aborts.
///
/// # Safety
///
/// - `handle` must be a builder allocated by [`phx_list_builder_alloc`].
/// - **`handle` must be rooted by the caller's shadow-stack frame**
///   for the duration of this call (`phx_list_alloc` inside can
///   trigger an auto-collect).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_list_builder_freeze(handle: *mut u8) -> *mut u8 {
    unsafe { assert_unfrozen(handle, "freeze") };
    let elem_size = unsafe { read_i64(handle, OFF_ELEM_SIZE) };
    let length = unsafe { read_i64(handle, OFF_LENGTH) };
    let es = elem_size as usize;
    let len = length as usize;

    // Mark frozen *before* allocating the destination list. Matches the
    // ordering in `phx_map_builder_freeze` so a future change that
    // re-enters the builder mid-freeze (today nothing does) observes a
    // consistent "no longer mutable" view.
    unsafe { write_i64(handle, OFF_FROZEN, 1) };

    // Allocate the destination List<T> through the same path used by
    // `phx_list_alloc` so the resulting object is indistinguishable
    // from a list built any other way (header layout, TypeTag::List,
    // payload zero-initialized).
    let list = crate::list_methods::phx_list_alloc(elem_size, length);

    if len > 0 && es > 0 {
        let copy_bytes = len * es;
        let src = unsafe { read_ptr(handle, OFF_DATA_PTR) };
        let dst = unsafe { list.add(LIST_HEADER_SIZE) };
        unsafe { std::ptr::copy_nonoverlapping(src, dst, copy_bytes) };
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_alloc_initializes_header() {
        let b = phx_list_builder_alloc(8);
        unsafe {
            assert_eq!(read_i64(b, OFF_LENGTH), 0);
            assert_eq!(read_i64(b, OFF_CAPACITY), INITIAL_CAPACITY as i64);
            assert_eq!(read_i64(b, OFF_ELEM_SIZE), 8);
            assert_eq!(read_i64(b, OFF_FROZEN), 0);
            assert!(!read_ptr(b, OFF_DATA_PTR).is_null());
        }
    }

    /// Use-after-freeze triggers `assert_unfrozen`'s `runtime_abort`
    /// path. Calls `assert_unfrozen` *directly* rather than going
    /// through `phx_list_builder_push` because the latter is
    /// `extern "C"` (cannot unwind) — a panic out of it aborts the
    /// test process before `#[should_panic]` can catch it. The
    /// cranelift integration fixture `list_builder_use_after_freeze_aborts`
    /// covers the full path including the extern boundary; this unit
    /// test isolates the unfrozen-check logic from compiler / linker /
    /// runtime-lib dependencies.
    ///
    /// Pinned to the "ListBuilder.push" + "already frozen" needles so
    /// a future message reformat can't silently satisfy a substring
    /// match against unrelated panic output.
    #[test]
    #[should_panic(expected = "ListBuilder.push: builder was already frozen")]
    fn assert_unfrozen_panics_when_frozen() {
        let b = phx_list_builder_alloc(8);
        // Bypass `phx_list_builder_freeze` (also `extern "C"`) to
        // toggle the frozen flag without crossing the FFI boundary.
        unsafe { write_i64(b, OFF_FROZEN, 1) };
        unsafe { assert_unfrozen(b, "push") };
    }

    /// Same shape, `freeze` arm of the message. Verifies the method
    /// name flows through `assert_unfrozen`'s formatter correctly so
    /// the cranelift-side `list_builder_double_freeze_aborts` fixture
    /// can rely on the same needles.
    #[test]
    #[should_panic(expected = "ListBuilder.freeze: builder was already frozen")]
    fn assert_unfrozen_panics_with_freeze_method_name() {
        let b = phx_list_builder_alloc(8);
        unsafe { write_i64(b, OFF_FROZEN, 1) };
        unsafe { assert_unfrozen(b, "freeze") };
    }

    /// Sanity: `assert_unfrozen` does not panic when the builder is
    /// still mutable (frozen == 0). Defends against a future regression
    /// where the check inverts the predicate.
    #[test]
    fn assert_unfrozen_does_not_panic_when_mutable() {
        let b = phx_list_builder_alloc(8);
        unsafe { assert_unfrozen(b, "push") };
        unsafe { assert_unfrozen(b, "freeze") };
    }

    #[test]
    fn builder_push_and_freeze_round_trip() {
        let b = phx_list_builder_alloc(8);
        for v in 0i64..20 {
            let bytes = v.to_le_bytes();
            unsafe { phx_list_builder_push(b, bytes.as_ptr(), 8) };
        }
        unsafe {
            assert_eq!(read_i64(b, OFF_LENGTH), 20);
            // Capacity must have grown past the initial 8.
            assert!(read_i64(b, OFF_CAPACITY) >= 20);
        }
        let list = unsafe { phx_list_builder_freeze(b) };
        unsafe {
            // List header: length at offset 0.
            assert_eq!(*(list as *const i64), 20);
            // Verify last element round-trips.
            let data = (list as *const u8).add(LIST_HEADER_SIZE);
            let last = *((data as *const i64).add(19));
            assert_eq!(last, 19);
            // Builder is now frozen.
            assert_eq!(read_i64(b, OFF_FROZEN), 1);
        }
    }
}
