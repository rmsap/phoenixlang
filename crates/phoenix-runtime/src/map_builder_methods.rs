//! `MapBuilder<K, V>` runtime: a transient mutable accumulator that
//! `.freeze()`s into an immutable [`crate::map_methods`] `Map<K, V>`.
//!
//! Phase 2.7 decision F.
//!
//! ## Layout — append-only pairs, dedup at freeze
//!
//! Rather than maintain an in-place hash table (which would mean
//! re-implementing `map_methods.rs`'s probe / rehash / load-factor
//! logic in a mutable flavor), the builder stores `(key, value)` pairs
//! in append order. Duplicates are allowed during the build phase;
//! `.freeze()` calls into the existing `phx_map_from_pairs` which
//! already implements last-wins dedup in one O(n) hash-build pass.
//!
//! Trade-off: `MapBuilder.length()` is intentionally **not** exposed —
//! the only way to observe the entry count is to freeze and call
//! `Map.length()`. Without this restriction the builder would have to
//! either (a) maintain dedup state inline (the work we're trying to
//! avoid) or (b) lie about its current count. (a) regresses to the
//! immutable-build problem; (b) is a footgun. Dropping the method is
//! the honest answer.
//!
//! ```text
//! Handle (GC-tracked, TypeTag::Closure):
//!   offset 0:   i64   length         (used pair slots)
//!   offset 8:   i64   capacity       (allocated pair slots in `data`)
//!   offset 16:  i64   key_size       (bytes per key)
//!   offset 24:  i64   val_size       (bytes per value)
//!   offset 32:  i64   frozen         (0 = mutable; 1 = frozen)
//!   offset 40:  *mut  data_ptr       (heap pointer into the buffer object)
//!
//! Buffer (GC-tracked, TypeTag::Unknown):
//!   offset 0:   u8[ capacity * (key_size + val_size) ]
//!                                    pairs laid out (k_0, v_0, k_1, v_1, ...)
//! ```
//!
//! The two-allocation handle/buffer split mirrors `list_builder_methods.rs`'s
//! rationale: the handle pointer must stay stable across grow so
//! `b.set(k, v)` is true in-place mutation.
//!
//! TODO(tag): handle is `TypeTag::Closure` as a temporary placeholder
//! until a dedicated `TypeTag::MapBuilder` lands; see the matching
//! TODO in `list_builder_methods.rs` for the full rationale.

use crate::gc::shadow_stack;
use crate::gc::{TypeTag, phx_gc_alloc};
use crate::map_methods::phx_map_from_pairs;
use crate::runtime_abort;

/// Builder handle size in bytes (6 × i64).
pub(crate) const BUILDER_HEADER_SIZE: usize = 48;

const OFF_LENGTH: usize = 0;
const OFF_CAPACITY: usize = 8;
const OFF_KEY_SIZE: usize = 16;
const OFF_VAL_SIZE: usize = 24;
const OFF_FROZEN: usize = 32;
const OFF_DATA_PTR: usize = 40;

/// Initial pair-slot capacity. 8 matches `phx_list_builder_alloc`'s
/// choice — the existing per-doubling reasoning applies.
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

/// Allocate a fresh `MapBuilder<K, V>` with capacity 8 pairs.
#[unsafe(no_mangle)]
pub extern "C" fn phx_map_builder_alloc(key_size: i64, val_size: i64) -> *mut u8 {
    // Reject zero / negative sizes. See the same check in
    // `phx_list_builder_alloc` for the rationale — a zero-sized pair
    // would let `length` increment forever without ever growing the
    // buffer, masking accounting bugs in compiled callers.
    if key_size <= 0 || val_size <= 0 {
        runtime_abort(&format!(
            "phx_map_builder_alloc: sizes must be positive \
             (key_size={key_size}, val_size={val_size})"
        ));
    }
    let ks = key_size as usize;
    let vs = val_size as usize;
    let capacity = INITIAL_CAPACITY;
    let Some(pair_size) = ks.checked_add(vs) else {
        runtime_abort("phx_map_builder_alloc: pair size overflow");
    };
    let Some(buffer_size) = pair_size.checked_mul(capacity) else {
        runtime_abort("phx_map_builder_alloc: buffer size overflow");
    };

    // Root the buffer on a transient shadow-stack frame before the
    // second `phx_gc_alloc`. See `phx_list_builder_alloc` for the full
    // rationale — auto-collect can fire during the handle alloc and
    // sweep the buffer (the GC scans the shadow stack, not Rust
    // frames).
    let buffer = phx_gc_alloc(buffer_size, TypeTag::Unknown as u32);
    let frame = shadow_stack::push_frame(1);
    unsafe { shadow_stack::set_root(frame, 0, buffer) };

    let handle = phx_gc_alloc(BUILDER_HEADER_SIZE, TypeTag::Closure as u32);
    unsafe {
        write_i64(handle, OFF_LENGTH, 0);
        write_i64(handle, OFF_CAPACITY, capacity as i64);
        write_i64(handle, OFF_KEY_SIZE, key_size);
        write_i64(handle, OFF_VAL_SIZE, val_size);
        write_i64(handle, OFF_FROZEN, 0);
        write_ptr(handle, OFF_DATA_PTR, buffer);
    }
    unsafe { shadow_stack::pop_frame(frame) };
    handle
}

unsafe fn assert_unfrozen(handle: *const u8, method: &str) {
    let frozen = unsafe { read_i64(handle, OFF_FROZEN) };
    if frozen != 0 {
        runtime_abort(&format!(
            "MapBuilder.{method}: builder was already frozen (Phase 2.7 decision F)"
        ));
    }
}

/// Append a `(key, value)` pair to the builder. Duplicate keys are
/// stored verbatim; the final dedup runs in `.freeze()` (last-wins,
/// inherited from `phx_map_from_pairs`).
///
/// # Safety
///
/// - `handle` must be a builder allocated by [`phx_map_builder_alloc`].
/// - `key_ptr` / `val_ptr` must point to `key_size` / `val_size`
///   valid bytes respectively; sizes must match the builder's stored
///   sizes.
/// - **`handle` must be rooted by the caller's shadow-stack frame.**
///   A grow allocates via `phx_gc_alloc`, which can trigger
///   auto-collect.
/// - If a key or value is a 16-byte string fat pointer, the
///   *pointed-to* heap string must also be rooted by the caller —
///   same rule as `phx_map_from_pairs` itself.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_builder_set(
    handle: *mut u8,
    key_ptr: *const u8,
    val_ptr: *const u8,
    key_size: i64,
    val_size: i64,
) {
    unsafe { assert_unfrozen(handle, "set") };
    let stored_ks = unsafe { read_i64(handle, OFF_KEY_SIZE) };
    let stored_vs = unsafe { read_i64(handle, OFF_VAL_SIZE) };
    if stored_ks != key_size || stored_vs != val_size {
        runtime_abort(&format!(
            "phx_map_builder_set: caller sizes ({key_size}, {val_size}) do not match \
             builder's stored sizes ({stored_ks}, {stored_vs})"
        ));
    }
    let ks = key_size as usize;
    let vs = val_size as usize;
    let Some(pair_size) = ks.checked_add(vs) else {
        runtime_abort("phx_map_builder_set: pair size overflow");
    };
    let length = unsafe { read_i64(handle, OFF_LENGTH) } as usize;
    let capacity = unsafe { read_i64(handle, OFF_CAPACITY) } as usize;

    let mut data_ptr = unsafe { read_ptr(handle, OFF_DATA_PTR) };
    if length == capacity {
        let new_capacity = capacity.saturating_mul(2).max(1);
        let Some(new_buffer_size) = pair_size.checked_mul(new_capacity) else {
            runtime_abort("phx_map_builder_set: grow size overflow");
        };
        // Pre-alloc `data_ptr` (Rust local read before `phx_gc_alloc`
        // below) stays valid across the alloc: mark-sweep doesn't
        // move objects, and the old buffer is reachable via
        // `handle → OFF_DATA_PTR` (caller's root on `handle` keeps
        // the chain alive across any auto-collect). See the parallel
        // comment in `phx_list_builder_push` for the same reasoning.
        //
        // Root the new buffer on the shadow stack before further
        // work so a future change that interposes a GC-triggering
        // call between alloc-return and write_ptr stays sound.
        let new_buffer = phx_gc_alloc(new_buffer_size, TypeTag::Unknown as u32);
        let frame = shadow_stack::push_frame(1);
        unsafe { shadow_stack::set_root(frame, 0, new_buffer) };
        if length > 0 {
            let copy_bytes = length * pair_size;
            unsafe { std::ptr::copy_nonoverlapping(data_ptr, new_buffer, copy_bytes) };
        }
        unsafe { write_ptr(handle, OFF_DATA_PTR, new_buffer) };
        unsafe { write_i64(handle, OFF_CAPACITY, new_capacity as i64) };
        unsafe { shadow_stack::pop_frame(frame) };
        data_ptr = new_buffer;
    }

    let dst = unsafe { data_ptr.add(length * pair_size) };
    unsafe { std::ptr::copy_nonoverlapping(key_ptr, dst, ks) };
    unsafe { std::ptr::copy_nonoverlapping(val_ptr, dst.add(ks), vs) };
    unsafe { write_i64(handle, OFF_LENGTH, (length + 1) as i64) };
}

/// Freeze the builder and return a fresh `Map<K, V>`. Routes through
/// `phx_map_from_pairs` for the actual hash-table build; that helper
/// already handles last-wins dedup so multiple `set(k, v)` calls with
/// the same key in the build phase collapse to one entry in the
/// frozen map, with the *latest* value winning.
///
/// # Safety
///
/// - `handle` must be a builder allocated by [`phx_map_builder_alloc`].
/// - **`handle` must be rooted by the caller's shadow-stack frame.**
/// - The string-fat-pointer rooting rule from
///   [`phx_map_builder_set`] applies transitively here: any heap
///   string referenced by a key or value pair stored in the builder
///   must remain rooted (typically through the builder's own
///   reachability chain) until freeze returns, because
///   `phx_map_from_pairs` re-reads each pair's fat-pointer payload
///   while building the destination hash table.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_builder_freeze(handle: *mut u8) -> *mut u8 {
    unsafe { assert_unfrozen(handle, "freeze") };
    let key_size = unsafe { read_i64(handle, OFF_KEY_SIZE) };
    let val_size = unsafe { read_i64(handle, OFF_VAL_SIZE) };
    let length = unsafe { read_i64(handle, OFF_LENGTH) };
    let data_ptr = unsafe { read_ptr(handle, OFF_DATA_PTR) };

    // Mark frozen *before* calling `phx_map_from_pairs` — that call
    // can trigger an auto-collect (it allocates), and a sweep that
    // ran between marking and the next user method would be racing
    // against an "I'm still mutable" view. Marking first makes the
    // ordering unambiguous.
    unsafe { write_i64(handle, OFF_FROZEN, 1) };

    unsafe { phx_map_from_pairs(key_size, val_size, length, data_ptr) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_alloc_initializes_header() {
        let b = phx_map_builder_alloc(8, 8);
        unsafe {
            assert_eq!(read_i64(b, OFF_LENGTH), 0);
            assert_eq!(read_i64(b, OFF_CAPACITY), INITIAL_CAPACITY as i64);
            assert_eq!(read_i64(b, OFF_KEY_SIZE), 8);
            assert_eq!(read_i64(b, OFF_VAL_SIZE), 8);
            assert_eq!(read_i64(b, OFF_FROZEN), 0);
            assert!(!read_ptr(b, OFF_DATA_PTR).is_null());
        }
    }

    /// Use-after-freeze triggers `assert_unfrozen`'s `runtime_abort`
    /// path. Calls `assert_unfrozen` directly to avoid panicking
    /// across an `extern "C"` boundary (which Rust treats as
    /// cannot-unwind and aborts the test process). The cranelift
    /// integration fixture `map_builder_use_after_freeze_aborts`
    /// covers the full path including the extern boundary; see the
    /// matching test in `list_builder_methods` for the framing.
    #[test]
    #[should_panic(expected = "MapBuilder.set: builder was already frozen")]
    fn assert_unfrozen_panics_when_frozen() {
        let b = phx_map_builder_alloc(8, 8);
        unsafe { write_i64(b, OFF_FROZEN, 1) };
        unsafe { assert_unfrozen(b, "set") };
    }

    /// Mirrors the `freeze`-method-name shape, so the cranelift
    /// fixture `map_builder_double_freeze_aborts` can rely on the
    /// same needles.
    #[test]
    #[should_panic(expected = "MapBuilder.freeze: builder was already frozen")]
    fn assert_unfrozen_panics_with_freeze_method_name() {
        let b = phx_map_builder_alloc(8, 8);
        unsafe { write_i64(b, OFF_FROZEN, 1) };
        unsafe { assert_unfrozen(b, "freeze") };
    }

    /// Sanity: still-mutable builder does not panic on the check.
    #[test]
    fn assert_unfrozen_does_not_panic_when_mutable() {
        let b = phx_map_builder_alloc(8, 8);
        unsafe { assert_unfrozen(b, "set") };
        unsafe { assert_unfrozen(b, "freeze") };
    }

    #[test]
    fn builder_set_and_freeze_round_trip() {
        let b = phx_map_builder_alloc(8, 8);
        for k in 0i64..20 {
            let v: i64 = k * 7;
            let kb = k.to_le_bytes();
            let vb = v.to_le_bytes();
            unsafe { phx_map_builder_set(b, kb.as_ptr(), vb.as_ptr(), 8, 8) };
        }
        unsafe {
            assert_eq!(read_i64(b, OFF_LENGTH), 20);
            assert!(read_i64(b, OFF_CAPACITY) >= 20);
        }
        let m = unsafe { phx_map_builder_freeze(b) };
        unsafe {
            // Map header: length at offset 0.
            assert_eq!(*(m as *const i64), 20);
            // Frozen flag flipped.
            assert_eq!(read_i64(b, OFF_FROZEN), 1);
        }
    }

    #[test]
    fn builder_set_last_wins_after_freeze() {
        let b = phx_map_builder_alloc(8, 8);
        let k = 42i64.to_le_bytes();
        let v1 = 1i64.to_le_bytes();
        let v2 = 2i64.to_le_bytes();
        unsafe {
            phx_map_builder_set(b, k.as_ptr(), v1.as_ptr(), 8, 8);
            phx_map_builder_set(b, k.as_ptr(), v2.as_ptr(), 8, 8);
        }
        let m = unsafe { phx_map_builder_freeze(b) };
        unsafe {
            // Last-wins: m has 1 entry (k=42 → v=2).
            assert_eq!(*(m as *const i64), 1);
            let v_ptr = crate::__test_support::phx_map_get_raw(m, k.as_ptr(), 8);
            assert!(!v_ptr.is_null());
            let v = *(v_ptr as *const i64);
            assert_eq!(v, 2);
        }
    }
}
