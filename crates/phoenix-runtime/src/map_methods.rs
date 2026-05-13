//! Map runtime functions for compiled Phoenix programs.
//!
//! Phoenix maps are **immutable**: every mutation (`set`, `remove`)
//! returns a new map. Internally each map is an open-addressing hash
//! table with linear probing **plus** a parallel insertion-order
//! array, so iteration over keys and values returns entries in the
//! order they were first inserted (matching the `Vec<(K,V)>`
//! semantics of `phoenix-ir-interp` and the user expectation set by
//! Python / JavaScript / TypeScript).
//!
//! Layout:
//!
//! ```text
//! offset 0:    i64  length        (occupied bucket count)
//! offset 8:    i64  capacity      (bucket count, power of two)
//! offset 16:   i64  key_size      (bytes per key)
//! offset 24:   i64  val_size      (bytes per value)
//! offset 32:   u8[capacity]       per-bucket tag (Empty/Occupied/Tombstone)
//! offset O:    u32[capacity]      order[i] = bucket index of the
//!                                  i-th-inserted entry (first `length`
//!                                  slots are meaningful)
//! offset Pₜ:   u8[capacity * (ks+vs)]  pairs[i] = (key_i, val_i)
//! ```
//!
//! `O = align_up(32 + capacity, 4)` and `Pₜ = align_up(O + 4 * capacity, 8)`
//! so each region stays naturally aligned regardless of capacity.
//!
//! ## Effective-size pattern
//!
//! Several functions accept caller-provided sizes and also read sizes
//! from the map header. For non-empty maps the stored sizes are
//! authoritative (the layout was built using them). For empty maps the
//! stored sizes may be placeholder values from generic type resolution,
//! so the caller's sizes are used instead.
//!
//! ## Why open addressing
//!
//! Closes the `O(n) map key lookup` Phase 2.3 perf bug (see
//! `docs/phases/phase-2.md` §2.3 "Bugs to be closed in this phase").
//! Lookups become O(1) average; copy-on-write `set` / `remove` are still
//! O(n) because each produces a fresh allocated table, but that's a
//! Phoenix immutability property, not a hashing one.

use std::slice;

use crate::gc::{TypeTag, phx_gc_alloc};
use crate::list_methods::{MAX_REASONABLE_STRING_LEN, STRING_FAT_POINTER_SIZE};
use crate::runtime_abort;

/// Header size in bytes (length + capacity + key_size + val_size).
pub(crate) const HEADER_SIZE: usize = 32;

const TAG_EMPTY: u8 = 0;
const TAG_OCCUPIED: u8 = 1;
const TAG_TOMBSTONE: u8 = 2;

/// Minimum bucket count for any non-zero map. Must be a power of two.
const MIN_BUCKETS: usize = 8;

/// Numerator of the resize threshold: a table is grown when
/// `length * MAX_LOAD_FACTOR_DEN > capacity * MAX_LOAD_FACTOR_NUM`.
/// 7/10 = 70 % keeps probe sequences short without pathological waste.
const MAX_LOAD_FACTOR_NUM: usize = 7;
const MAX_LOAD_FACTOR_DEN: usize = 10;

/// Compute the smallest power-of-two bucket count that fits `n`
/// occupied entries below the resize threshold. Caps at the smallest
/// power of two ≥ MIN_BUCKETS for empty / very-small inputs.
fn buckets_for(n: usize) -> usize {
    if n == 0 {
        return MIN_BUCKETS;
    }
    // To keep load factor < 70 %, we need capacity > n * 10/7 ≈ n * 1.43.
    // Round to the next power of two and floor at MIN_BUCKETS.
    let target = n.saturating_mul(MAX_LOAD_FACTOR_DEN) / MAX_LOAD_FACTOR_NUM + 1;
    target.max(MIN_BUCKETS).next_power_of_two()
}

/// Byte offset of the order array within an allocation of `capacity`
/// buckets. The order array is `u32[capacity]` (4-byte aligned).
fn order_offset(capacity: usize) -> usize {
    let after_tags = HEADER_SIZE + capacity;
    (after_tags + 3) & !3
}

/// Byte offset of the pairs region. 8-byte aligned so per-pair scalar
/// reads stay aligned regardless of the order-array size.
fn pairs_offset(capacity: usize) -> usize {
    let after_order = order_offset(capacity) + capacity * 4;
    (after_order + 7) & !7
}

/// Total allocation size for a map with `capacity` buckets and the
/// given key/value sizes.
///
/// Overflow is reported via [`runtime_abort`] rather than `expect`/panic:
/// every caller is reachable from an `extern "C"` entry point and the
/// workspace's panic strategy is `unwind`, so unwinding across the FFI
/// boundary would be UB.
fn total_size(capacity: usize, ks: usize, vs: usize) -> usize {
    let Some(pair_size) = ks.checked_add(vs) else {
        runtime_abort(&format!(
            "phx_map: pair size overflow (key_size={ks}, val_size={vs})"
        ));
    };
    let Some(pairs_bytes) = pair_size.checked_mul(capacity) else {
        runtime_abort(&format!(
            "phx_map: data size overflow (pair_size={pair_size}, capacity={capacity})"
        ));
    };
    let Some(total) = pairs_offset(capacity).checked_add(pairs_bytes) else {
        runtime_abort(&format!(
            "phx_map: total size overflow (pairs_offset={}, pairs_bytes={pairs_bytes})",
            pairs_offset(capacity)
        ));
    };
    total
}

/// Read a u32 from the order array at index `i`.
///
/// # Safety
///
/// `map` must point to a valid hash-table allocation with at least
/// `i + 1` slots in its order array.
unsafe fn order_read(map: *const u8, capacity: usize, i: usize) -> u32 {
    let base = unsafe { map.add(order_offset(capacity)) } as *const u32;
    unsafe { *base.add(i) }
}

/// Write a u32 to the order array at index `i`.
///
/// # Safety
///
/// `map` must point to a writable hash-table allocation with at least
/// `i + 1` slots in its order array.
unsafe fn order_write(map: *mut u8, capacity: usize, i: usize, bucket: u32) {
    let base = unsafe { map.add(order_offset(capacity)) } as *mut u32;
    unsafe { *base.add(i) = bucket };
}

/// Read header fields. Returns `(length, capacity, key_size, val_size)`.
///
/// # Safety
///
/// `map` must point to a valid map allocated by [`phx_map_alloc`] or
/// [`phx_map_from_pairs`].
unsafe fn map_header(map: *const u8) -> (usize, usize, usize, usize) {
    let length = unsafe { *(map as *const i64) } as usize;
    let capacity = unsafe { *((map as *const i64).add(1)) } as usize;
    let key_size = unsafe { *((map as *const i64).add(2)) } as usize;
    let val_size = unsafe { *((map as *const i64).add(3)) } as usize;
    (length, capacity, key_size, val_size)
}

/// Compare two keys for equality, handling fat pointers (strings).
///
/// Delegates to [`crate::list_methods::elements_equal`] with
/// `is_float = false`. Maps deliberately use byte-wise (not IEEE) key
/// comparison even for 8-byte float keys: NaN keys must stay distinct
/// from each other and `-0.0` / `0.0` must stay distinct under map
/// identity. If `elements_equal` is ever taught float semantics for
/// 8-byte elements, this helper will need a `kind`-tag parameter.
///
/// # Safety
///
/// Both `a` and `b` must point to `size` valid bytes.
unsafe fn keys_equal(a: *const u8, b: *const u8, size: usize) -> bool {
    unsafe { crate::list_methods::elements_equal(a, b, size, false) }
}

/// 64-bit FNV-1a hash over the given byte slice. Stable across runs;
/// non-cryptographic; collision-tolerant given linear probing handles
/// collisions correctly.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Hash a key for bucket selection.
///
/// For 16-byte keys (string fat pointers) we hash the *string content*
/// — matching [`keys_equal`]'s by-content comparison. Wild fat-pointer
/// data (impossibly large `len`) falls back to byte-wise hashing of
/// the raw fat-pointer bytes; this keeps the hash defined for malformed
/// inputs without dereferencing a wild pointer.
///
/// # Safety
///
/// `key_ptr` must point to `key_size` valid bytes; if `key_size == 16`
/// the data is interpreted as a `(ptr, len)` fat pointer.
unsafe fn hash_key(key_ptr: *const u8, key_size: usize) -> u64 {
    if key_size == STRING_FAT_POINTER_SIZE {
        let str_ptr = unsafe { *(key_ptr as *const i64) } as *const u8;
        let str_len = unsafe { *((key_ptr as *const i64).add(1)) } as usize;
        if str_len > MAX_REASONABLE_STRING_LEN {
            // Not a sensible string fat pointer; treat as raw bytes.
            // Must agree with the threshold in `elements_equal` so a
            // malformed key hashes the same way it compares.
            let bytes = unsafe { slice::from_raw_parts(key_ptr, key_size) };
            return fnv1a_64(bytes);
        }
        let bytes = unsafe { slice::from_raw_parts(str_ptr, str_len) };
        fnv1a_64(bytes)
    } else {
        let bytes = unsafe { slice::from_raw_parts(key_ptr, key_size) };
        fnv1a_64(bytes)
    }
}

/// Outcome of probing a hash table for a given key.
enum ProbeResult {
    /// Key is present at this bucket index.
    Found(usize),
    /// Key is absent; this bucket is the right place to insert.
    /// `bucket` is the first empty/tombstone slot encountered along
    /// the probe sequence.
    Vacant { bucket: usize },
}

/// Probe an existing hash table for `key_ptr`.
///
/// Linear probing with mask `capacity - 1` (capacity is a power of two
/// so masking gives a fast modulo). On a tombstone, we remember the
/// first one we saw so an insert can reuse it; lookups skip tombstones
/// and continue probing.
///
/// # Safety
///
/// `map` must be a valid hash-table allocation; `key_ptr` must point to
/// `ks` valid bytes; `capacity` must equal the table's stored capacity.
unsafe fn probe(
    map: *const u8,
    key_ptr: *const u8,
    ks: usize,
    vs: usize,
    capacity: usize,
) -> ProbeResult {
    let mask = capacity - 1;
    let pair_size = ks + vs;
    let tags = unsafe { map.add(HEADER_SIZE) };
    let pairs = unsafe { map.add(pairs_offset(capacity)) };

    let mut idx = (unsafe { hash_key(key_ptr, ks) } as usize) & mask;
    let mut first_empty_or_tombstone: Option<usize> = None;

    for _ in 0..capacity {
        let tag = unsafe { *tags.add(idx) };
        match tag {
            TAG_EMPTY => {
                let slot = first_empty_or_tombstone.unwrap_or(idx);
                return ProbeResult::Vacant { bucket: slot };
            }
            TAG_OCCUPIED => {
                let entry = unsafe { pairs.add(idx * pair_size) };
                if unsafe { keys_equal(key_ptr, entry, ks) } {
                    return ProbeResult::Found(idx);
                }
            }
            _ => {
                // Tombstone — record first one but keep probing.
                first_empty_or_tombstone.get_or_insert(idx);
            }
        }
        idx = (idx + 1) & mask;
    }
    // Tombstone-only fallback: a fresh-table walk would have hit
    // TAG_EMPTY and returned. We can only get here if every bucket is
    // OCCUPIED or TOMBSTONE. Reuse the first tombstone we saw as the
    // insert site; a fully-occupied table violates the load-factor
    // invariant and is a bug — abort cleanly rather than panic, since
    // this helper is reached from `extern "C"` entry points and the
    // workspace's panic strategy is `unwind` (panicking across the FFI
    // boundary is UB).
    if let Some(bucket) = first_empty_or_tombstone {
        ProbeResult::Vacant { bucket }
    } else {
        runtime_abort(&format!(
            "phx_map: probe scanned a fully-occupied table (capacity {capacity}); \
             the load-factor invariant ({MAX_LOAD_FACTOR_NUM}/{MAX_LOAD_FACTOR_DEN}) was violated"
        ));
    }
}

/// Allocate a new empty hash table sized to comfortably hold `count`
/// occupied entries without rehashing.
///
/// `length` starts at 0; `capacity` is `buckets_for(count)`. All tags
/// are `EMPTY` (the underlying [`phx_gc_alloc`] zeros the whole region
/// and `TAG_EMPTY == 0`). The pairs region is also zeroed by the
/// allocator, but its bytes are *not meaningful* for unoccupied
/// buckets — readers must consult the tag array first.
///
/// # Aborts
///
/// On invalid input ([negative size, count, or allocation overflow])
/// the function calls [`runtime_abort`] rather than panicking, since
/// it is reached from `extern "C"` and the workspace's `unwind` panic
/// strategy would otherwise UB across the FFI boundary.
#[unsafe(no_mangle)]
pub extern "C" fn phx_map_alloc(key_size: i64, val_size: i64, count: i64) -> *mut u8 {
    if key_size < 0 {
        runtime_abort(&format!(
            "phx_map_alloc: key_size must be non-negative, got {key_size}"
        ));
    }
    if val_size < 0 {
        runtime_abort(&format!(
            "phx_map_alloc: val_size must be non-negative, got {val_size}"
        ));
    }
    if count < 0 {
        runtime_abort(&format!(
            "phx_map_alloc: count must be non-negative, got {count}"
        ));
    }
    let ks = key_size as usize;
    let vs = val_size as usize;
    let cnt = count as usize;
    let capacity = buckets_for(cnt);
    let total = total_size(capacity, ks, vs);
    // Tag is informational until trace tables (GC subordinate
    // decision C — see `TypeTag` in `crate::gc` for migration status)
    // replace the conservative interior scan with per-tag mark fns.
    let ptr = phx_gc_alloc(total, TypeTag::Map as u32);
    unsafe {
        *(ptr as *mut i64) = 0;
        *((ptr as *mut i64).add(1)) = capacity as i64;
        *((ptr as *mut i64).add(2)) = key_size;
        *((ptr as *mut i64).add(3)) = val_size;
    }
    // Tags are already zeroed by phx_gc_alloc, so all buckets start EMPTY.
    ptr
}

/// Build a hash table from a flat array of `n_pairs` `(key, value)`
/// records. Used by Cranelift's map-literal lowering: codegen writes
/// each pair into a stack buffer and hands the whole buffer over to
/// the runtime, which hashes everything in one pass.
///
/// Requires `pair_data` to be `n_pairs * (ks + vs)` bytes laid out as
/// `(key_0, val_0, key_1, val_1, ...)`. Duplicate keys are resolved
/// last-wins (later pairs overwrite earlier values; the order array
/// records the *first* insertion position so iteration order matches
/// the user's source order).
///
/// Builds into a single allocation: no copy-on-write, no intermediate
/// maps. Critical because the only allocation here is the `phx_map_alloc`
/// at the top — looping through `phx_map_set_raw` would allocate n+1
/// maps and leave each previous `current` unrooted across the next
/// `phx_gc_alloc` (auto-collect would reclaim it as garbage).
///
/// # Safety
///
/// - When `n_pairs > 0`: `pair_data` must point to
///   `n_pairs * (key_size + val_size)` valid bytes.
/// - When `n_pairs == 0`: `pair_data` is not dereferenced; passing a
///   null or dangling pointer is permitted. Cranelift's empty-literal
///   lowering relies on this carve-out (it elides the stack buffer
///   and passes a null pointer).
/// - Key and value sizes must be non-negative.
/// - **If any key or value bytes are 16-byte string fat pointers (or
///   any other reference into a GC-managed allocation), the
///   *pointed-to* heap objects must be rooted by the caller for the
///   duration of this call.** The `phx_map_alloc` at the top of the
///   function can trigger an auto-collect, after which the per-pair
///   `copy_nonoverlapping` reads each key/value's bytes — an unrooted
///   string would have been swept by then. Cranelift-emitted callers
///   satisfy this automatically because the keys/values are values in
///   the surrounding function frame, which already roots ref-typed
///   locals.
///
/// Negative `key_size`, `val_size`, or `n_pairs` triggers
/// [`runtime_abort`] rather than a panic, for the same FFI-safety
/// reason as [`phx_map_alloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_from_pairs(
    key_size: i64,
    val_size: i64,
    n_pairs: i64,
    pair_data: *const u8,
) -> *mut u8 {
    if key_size < 0 || val_size < 0 || n_pairs < 0 {
        runtime_abort(&format!(
            "phx_map_from_pairs: sizes must be non-negative \
             (key_size={key_size}, val_size={val_size}, n_pairs={n_pairs})"
        ));
    }
    let ks = key_size as usize;
    let vs = val_size as usize;
    let n = n_pairs as usize;
    let map = phx_map_alloc(key_size, val_size, n_pairs);
    if n == 0 {
        return map;
    }
    let pair_size = ks + vs;
    // Use the capacity that phx_map_alloc actually picked rather than
    // recomputing — keeps the two in lock-step if buckets_for changes.
    let capacity = unsafe { *((map as *const i64).add(1)) } as usize;
    let mask = capacity - 1;
    let tags = unsafe { map.add(HEADER_SIZE) };
    let pairs = unsafe { map.add(pairs_offset(capacity)) };
    let mut length: usize = 0;

    for i in 0..n {
        let kp = unsafe { pair_data.add(i * pair_size) };
        let vp = unsafe { pair_data.add(i * pair_size + ks) };
        let mut idx = (unsafe { hash_key(kp, ks) } as usize) & mask;
        loop {
            let tag = unsafe { *tags.add(idx) };
            if tag == TAG_EMPTY {
                // New key — copy key + value, append to order array.
                unsafe {
                    *tags.add(idx) = TAG_OCCUPIED;
                    std::ptr::copy_nonoverlapping(kp, pairs.add(idx * pair_size), ks);
                    std::ptr::copy_nonoverlapping(vp, pairs.add(idx * pair_size + ks), vs);
                    order_write(map, capacity, length, idx as u32);
                }
                length += 1;
                break;
            }
            // No tombstones in a fresh build, so tag must be OCCUPIED.
            let entry = unsafe { pairs.add(idx * pair_size) };
            if unsafe { keys_equal(kp, entry, ks) } {
                // Duplicate key — last-wins on value, do not touch the
                // order array (the first insertion's position is kept).
                unsafe {
                    std::ptr::copy_nonoverlapping(vp, pairs.add(idx * pair_size + ks), vs);
                }
                break;
            }
            idx = (idx + 1) & mask;
        }
    }
    unsafe {
        *(map as *mut i64) = length as i64;
    }
    map
}

/// Return the number of entries in a map.
///
/// # Safety
///
/// `map` must point to a valid map header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_length(map: *const u8) -> i64 {
    let (length, _, _, _) = unsafe { map_header(map) };
    length as i64
}

/// Look up a key. Returns a pointer to the value, or null if absent.
///
/// # Safety
///
/// - `map` must point to a valid map.
/// - `key_ptr` must point to `key_size` valid bytes. `key_size` is
///   retained only for ABI compatibility with codegen — the lookup uses
///   the map's stored key-size, since for non-empty maps the stored
///   value is authoritative and for empty maps the early-return below
///   skips the size entirely.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_get_raw(
    map: *const u8,
    key_ptr: *const u8,
    _key_size: i64,
) -> *const u8 {
    let (length, capacity, stored_ks, vs) = unsafe { map_header(map) };
    if length == 0 {
        return std::ptr::null();
    }
    let ks = stored_ks;
    let pair_size = ks + vs;
    let pairs = unsafe { map.add(pairs_offset(capacity)) };
    match unsafe { probe(map, key_ptr, ks, vs, capacity) } {
        ProbeResult::Found(idx) => unsafe { pairs.add(idx * pair_size + ks) },
        ProbeResult::Vacant { .. } => std::ptr::null(),
    }
}

/// Copy `src_map` into `dst_map`. Both must have identical capacity,
/// key_size, and val_size; this is the hot path for copy-on-write
/// `set` and `remove`.
///
/// Copies the entire body (tags + order array + pairs) in one
/// memcpy. The header is not copied — the caller writes a fresh one
/// (potentially with an updated `length`).
///
/// # Safety
///
/// Both pointers must reference valid maps with matching layout
/// parameters.
unsafe fn copy_map_same_layout(
    dst_map: *mut u8,
    src_map: *const u8,
    capacity: usize,
    ks: usize,
    vs: usize,
) {
    let body_len = total_size(capacity, ks, vs) - HEADER_SIZE;
    let dst = unsafe { dst_map.add(HEADER_SIZE) };
    let src = unsafe { src_map.add(HEADER_SIZE) };
    unsafe { std::ptr::copy_nonoverlapping(src, dst, body_len) };
}

/// Insert `(key, val)` into a hash table that contains no tombstones
/// or matching keys for `key_ptr` — i.e., a freshly allocated or
/// freshly rehashed table where the first non-OCCUPIED bucket along
/// the probe chain is always the right insertion site. Writes the
/// bucket index to the order array at `order_pos`.
///
/// Used by `rehash_into` and `phx_map_set_raw`'s grow path; both are
/// situations where caller logic guarantees the key cannot collide
/// (rehash visits each src entry exactly once; the grow path only
/// runs after `probe` returned `Vacant`). `phx_map_from_pairs` does
/// *not* use this helper because it has to dedupe duplicate keys
/// in-line.
///
/// # Safety
///
/// `map` must be a valid hash-table allocation with `capacity`
/// buckets (power of two); `key_ptr` / `val_ptr` must point to
/// `ks` / `vs` valid bytes; `order_pos` must be a valid order-array
/// slot. The load-factor invariant must hold so the probe terminates.
unsafe fn insert_into_fresh(
    map: *mut u8,
    capacity: usize,
    ks: usize,
    vs: usize,
    key_ptr: *const u8,
    val_ptr: *const u8,
    order_pos: usize,
) {
    let pair_size = ks + vs;
    let mask = capacity - 1;
    let tags = unsafe { map.add(HEADER_SIZE) };
    let pairs = unsafe { map.add(pairs_offset(capacity)) };
    let mut idx = (unsafe { hash_key(key_ptr, ks) } as usize) & mask;
    while unsafe { *tags.add(idx) } == TAG_OCCUPIED {
        idx = (idx + 1) & mask;
    }
    unsafe {
        *tags.add(idx) = TAG_OCCUPIED;
        std::ptr::copy_nonoverlapping(key_ptr, pairs.add(idx * pair_size), ks);
        std::ptr::copy_nonoverlapping(val_ptr, pairs.add(idx * pair_size + ks), vs);
        order_write(map, capacity, order_pos, idx as u32);
    }
}

/// Allocate a new map with `new_capacity` buckets and rehash every
/// occupied entry from `src` into it. Used when an insert would push
/// the load factor past the threshold.
///
/// # Safety
///
/// `src` must be a valid map; `new_capacity` must be a power of two
/// large enough to hold every occupied entry below the load-factor
/// threshold.
unsafe fn rehash_into(src: *const u8, new_capacity: usize) -> *mut u8 {
    let (length, src_capacity, ks, vs) = unsafe { map_header(src) };
    let dst = phx_map_alloc_internal(new_capacity, ks, vs, length);
    let pair_size = ks + vs;
    let src_tags = unsafe { src.add(HEADER_SIZE) };
    let src_pairs = unsafe { src.add(pairs_offset(src_capacity)) };

    // Walk the source's *insertion order* array so the destination
    // preserves user-visible iteration order across the rehash. Each
    // src entry's bucket may map to a different dst bucket because the
    // capacity changed, but the order in which we visit them — and
    // thus the order in which they're appended to the dst order array
    // — stays insertion-order.
    for ord_idx in 0..length {
        let src_bucket = unsafe { order_read(src, src_capacity, ord_idx) } as usize;
        debug_assert_eq!(
            unsafe { *src_tags.add(src_bucket) },
            TAG_OCCUPIED,
            "order entry {ord_idx} → bucket {src_bucket} is not occupied"
        );
        let entry = unsafe { src_pairs.add(src_bucket * pair_size) };
        let val = unsafe { entry.add(ks) };
        unsafe { insert_into_fresh(dst, new_capacity, ks, vs, entry, val, ord_idx) };
    }
    dst
}

/// Set a key-value pair, returning a new map (Phoenix maps are
/// immutable).
///
/// If the key already exists the new map has the same capacity and
/// length, with the value at the matched bucket overwritten. If the
/// key is new we either copy-and-insert at the found vacant bucket
/// (if load factor stays below 70 %) or grow the table to twice the
/// current capacity, rehashing in the process.
///
/// # Safety
///
/// - `map` must point to a valid map.
/// - `map` must be rooted by the caller's shadow-stack frame for the
///   duration of this call. The internal `phx_gc_alloc` (via
///   `phx_map_alloc_internal` / `rehash_into`) can trigger an
///   auto-collect that would sweep an unrooted input before
///   `copy_map_same_layout` reads from it. See the *Rooting contract*
///   in the `list_methods` module header.
/// - `key_ptr` and `val_ptr` must point to `key_size` / `val_size`
///   valid bytes respectively. **If the key or value bytes are a
///   16-byte string fat pointer (or any other reference into a
///   GC-managed allocation), the *pointed-to* heap object must also be
///   rooted by the caller for the duration of this call** — the same
///   internal `phx_gc_alloc` that endangers `map` would sweep an unrooted
///   string before its bytes are read by `copy_nonoverlapping`. The
///   fat-pointer bytes themselves are captured by value (no rooting
///   needed for `key_ptr`/`val_ptr` as locations).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_set_raw(
    map: *const u8,
    key_ptr: *const u8,
    val_ptr: *const u8,
    key_size: i64,
    val_size: i64,
) -> *mut u8 {
    let (length, capacity, stored_ks, stored_vs) = unsafe { map_header(map) };
    // Empty maps may carry placeholder stored sizes from generic-type
    // resolution; the caller's sizes are authoritative in that case.
    let ks = if length == 0 {
        key_size as usize
    } else {
        stored_ks
    };
    let vs = if length == 0 {
        val_size as usize
    } else {
        stored_vs
    };
    let pair_size = ks + vs;

    // Empty map whose stored sizes differ from the caller's: allocate
    // a fresh table at the caller's sizes and insert the pair directly.
    //
    // The motivating case is generic-type-resolution placeholders
    // (stored ks/vs = 0 followed by a concrete-sized insert). The same
    // branch also catches concrete-to-concrete shape changes — an empty
    // `Map<Int, Int>` followed by a `Map<String, Int>` insert — which
    // `phoenix-ir` does not currently produce, but if a future pass
    // emits one this branch silently rewrites the shape rather than
    // wedging at the old stride. That's intentional: the input is
    // empty and discardable, and the caller's sizes are by definition
    // authoritative.
    //
    // The single allocation here is critical — any second `phx_gc_alloc`
    // between `phx_map_alloc_internal` returning and the body fully
    // written would put `new_map` at risk of being swept (it's held
    // only in a Rust local, not on the shadow stack), so we never
    // recurse back into `phx_map_set_raw`.
    if length == 0 && (ks != stored_ks || vs != stored_vs) {
        let new_map = phx_map_alloc_internal(MIN_BUCKETS, ks, vs, 1);
        unsafe { insert_into_fresh(new_map, MIN_BUCKETS, ks, vs, key_ptr, val_ptr, 0) };
        return new_map;
    }

    match unsafe { probe(map, key_ptr, ks, vs, capacity) } {
        ProbeResult::Found(idx) => {
            // Same-shape copy + value overwrite at `idx`. Insertion
            // order is unchanged because the key was already present.
            let new_map = phx_map_alloc_internal(capacity, ks, vs, length);
            unsafe { copy_map_same_layout(new_map, map, capacity, ks, vs) };
            let pairs = unsafe { new_map.add(pairs_offset(capacity)) };
            let dst_val = unsafe { pairs.add(idx * pair_size + ks) };
            unsafe { std::ptr::copy_nonoverlapping(val_ptr, dst_val, vs) };
            new_map
        }
        ProbeResult::Vacant { bucket } => {
            // Decide whether the new length crosses the load threshold.
            let new_length = length + 1;
            let needs_grow = new_length.saturating_mul(MAX_LOAD_FACTOR_DEN)
                > capacity.saturating_mul(MAX_LOAD_FACTOR_NUM);
            if needs_grow {
                let new_capacity = (capacity * 2).max(MIN_BUCKETS);
                let grown = unsafe { rehash_into(map, new_capacity) };
                // Insert the new key into the freshly rehashed table
                // (no tombstones, no possible duplicate) and bump length.
                unsafe {
                    insert_into_fresh(grown, new_capacity, ks, vs, key_ptr, val_ptr, length);
                    *(grown as *mut i64) = new_length as i64;
                }
                grown
            } else {
                // Same-capacity copy + insert at `bucket`, then append
                // `bucket` to the order array at slot `length`.
                let new_map = phx_map_alloc_internal(capacity, ks, vs, new_length);
                unsafe { copy_map_same_layout(new_map, map, capacity, ks, vs) };
                let tags = unsafe { new_map.add(HEADER_SIZE) };
                let pairs = unsafe { new_map.add(pairs_offset(capacity)) };
                unsafe {
                    *tags.add(bucket) = TAG_OCCUPIED;
                    std::ptr::copy_nonoverlapping(key_ptr, pairs.add(bucket * pair_size), ks);
                    std::ptr::copy_nonoverlapping(val_ptr, pairs.add(bucket * pair_size + ks), vs);
                    order_write(new_map, capacity, length, bucket as u32);
                }
                new_map
            }
        }
    }
}

/// Internal allocator: takes a pre-computed capacity (in buckets) and
/// a final length and writes them straight into the header.
///
/// Differs from [`phx_map_alloc`] in two ways: (1) capacity is supplied
/// directly rather than derived from a target count via `buckets_for`,
/// so callers that already know the destination shape (rehash, copy,
/// or grow) skip the recomputation; (2) the header's `length` field is
/// written to the supplied value, letting the caller commit the final
/// length up-front rather than fixing it up after the body is
/// populated. Tags region is still zeroed by `phx_gc_alloc`, so all
/// buckets start `EMPTY` regardless of the `length` value.
///
/// Used by `set_raw`, `remove_raw`, and `rehash_into` after they've
/// decided the new layout. Not exposed as `extern "C"` because the
/// pre-computed capacity must agree with the table's `buckets_for`
/// invariants, and the header `length` write trusts the caller.
fn phx_map_alloc_internal(capacity: usize, ks: usize, vs: usize, length: usize) -> *mut u8 {
    let total = total_size(capacity, ks, vs);
    let ptr = phx_gc_alloc(total, TypeTag::Map as u32);
    unsafe {
        *(ptr as *mut i64) = length as i64;
        *((ptr as *mut i64).add(1)) = capacity as i64;
        *((ptr as *mut i64).add(2)) = ks as i64;
        *((ptr as *mut i64).add(3)) = vs as i64;
    }
    ptr
}

/// Remove a key, returning a new map. If the key is absent the
/// returned map is a same-layout copy of the input.
///
/// Removed buckets are marked `TOMBSTONE`. Tombstones are *carried
/// forward* into the new map (because `copy_map_same_layout` blits the
/// whole body) and are only cleared when an insert pushes length past
/// the load-factor threshold and triggers `rehash_into`. Workloads that
/// alternate `set` and `remove` without ever crossing that threshold
/// will see probe sequences lengthen as tombstones accumulate; this is
/// acceptable today because Phoenix's immutable-map model means most
/// removed maps are quickly reclaimed by the GC, but a tombstone-aware
/// rebuild would be the fix if churn workloads regress.
///
/// ## Empty-map fast path and placeholder sizes
///
/// When `length == 0`, the function returns a fresh empty map carrying
/// the *stored* sizes — including any placeholder values left over from
/// generic-type resolution. The returned map is observationally
/// equivalent to the input: lookups short-circuit on `length == 0` so
/// stored sizes are never used to stride, and the next non-trivial
/// operation (`set`) heals any placeholder via the recovery branch in
/// [`phx_map_set_raw`]. Mixing the caller's `key_size` with stored
/// `val_size` here would create an inconsistent shape with no upside.
///
/// # Safety
///
/// - `map` must point to a valid map.
/// - `map` must be rooted by the caller's shadow-stack frame for the
///   duration of this call. The internal `phx_gc_alloc` (via
///   `phx_map_alloc_internal`) can trigger an auto-collect that would
///   sweep an unrooted input before `copy_map_same_layout` reads from
///   it. See the *Rooting contract* in the `list_methods` module
///   header.
/// - `key_ptr` must point to `key_size` valid bytes. **If the key
///   bytes are a 16-byte string fat pointer (or any other reference
///   into a GC-managed allocation), the *pointed-to* heap object must
///   also be rooted by the caller for the duration of this call** —
///   `probe` reads the key content via `keys_equal` after the internal
///   alloc, and an unrooted string would have been swept by then.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_remove_raw(
    map: *const u8,
    key_ptr: *const u8,
    _key_size: i64,
) -> *mut u8 {
    let (length, capacity, stored_ks, stored_vs) = unsafe { map_header(map) };
    if length == 0 {
        // Nothing to remove; return a fresh empty map carrying the
        // input's stored shape verbatim. See the doc above for why we
        // don't try to splice in the caller's `key_size`.
        return phx_map_alloc(stored_ks as i64, stored_vs as i64, 0);
    }
    let ks = stored_ks;
    let vs = stored_vs;
    match unsafe { probe(map, key_ptr, ks, vs, capacity) } {
        ProbeResult::Vacant { .. } => {
            // Not found — same-shape copy.
            let new_map = phx_map_alloc_internal(capacity, ks, vs, length);
            unsafe { copy_map_same_layout(new_map, map, capacity, ks, vs) };
            new_map
        }
        ProbeResult::Found(idx) => {
            // Same-shape copy with the bucket flipped to TOMBSTONE,
            // and the order array compacted to drop the entry whose
            // bucket index was `idx`.
            let new_map = phx_map_alloc_internal(capacity, ks, vs, length - 1);
            unsafe { copy_map_same_layout(new_map, map, capacity, ks, vs) };
            let tags = unsafe { new_map.add(HEADER_SIZE) };
            unsafe { *tags.add(idx) = TAG_TOMBSTONE };
            // Single-pass compaction over the order array: scan until
            // we hit the entry whose bucket index is `idx`, then shift
            // every subsequent entry one slot left. The bucket is
            // guaranteed to appear because `probe` returned Found(idx).
            // The trailing slot's stale data is harmless — `new_map`'s
            // length is `length - 1`, so it's never read.
            let mut shifted = false;
            for ord_idx in 0..length {
                let bucket = unsafe { order_read(new_map, capacity, ord_idx) };
                if shifted {
                    unsafe { order_write(new_map, capacity, ord_idx - 1, bucket) };
                } else if bucket as usize == idx {
                    shifted = true;
                }
            }
            debug_assert!(
                shifted,
                "remove_raw: bucket {idx} absent from order array — probe returned a stale index"
            );
            new_map
        }
    }
}

/// Return 1 if `key_ptr` is present in the map, 0 otherwise.
///
/// # Safety
///
/// - `map` must point to a valid map.
/// - `key_ptr` must point to `key_size` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_contains(map: *const u8, key_ptr: *const u8, key_size: i64) -> i8 {
    let result = unsafe { phx_map_get_raw(map, key_ptr, key_size) };
    if result.is_null() { 0 } else { 1 }
}

/// Extract every key from a map (in **insertion order**) into a new list.
///
/// # Safety
///
/// - `map` must point to a valid map.
/// - `map` must be rooted by the caller's shadow-stack frame for the
///   duration of this call. The internal `phx_list_alloc` can trigger
///   an auto-collect that would sweep an unrooted input — see the
///   *Rooting contract* in the `list_methods` module header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_keys(map: *const u8) -> *mut u8 {
    let (length, capacity, ks, vs) = unsafe { map_header(map) };
    let key_size = ks as i64;
    let pair_size = ks + vs;
    let pairs = unsafe { map.add(pairs_offset(capacity)) };

    let list = crate::list_methods::phx_list_alloc(key_size, length as i64);
    let list_data = unsafe { list.add(crate::list_methods::HEADER_SIZE) };
    for ord_idx in 0..length {
        let bucket = unsafe { order_read(map, capacity, ord_idx) } as usize;
        let key = unsafe { pairs.add(bucket * pair_size) };
        unsafe {
            std::ptr::copy_nonoverlapping(key, list_data.add(ord_idx * ks), ks);
        }
    }
    list
}

/// Extract every value from a map (in **insertion order**) into a new list.
///
/// # Safety
///
/// - `map` must point to a valid map.
/// - `map` must be rooted by the caller's shadow-stack frame for the
///   duration of this call. See [`phx_map_keys`] for the rationale.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_values(map: *const u8) -> *mut u8 {
    let (length, capacity, ks, vs) = unsafe { map_header(map) };
    let val_size = vs as i64;
    let pair_size = ks + vs;
    let pairs = unsafe { map.add(pairs_offset(capacity)) };

    let list = crate::list_methods::phx_list_alloc(val_size, length as i64);
    let list_data = unsafe { list.add(crate::list_methods::HEADER_SIZE) };
    for ord_idx in 0..length {
        let bucket = unsafe { order_read(map, capacity, ord_idx) } as usize;
        let val = unsafe { pairs.add(bucket * pair_size + ks) };
        unsafe {
            std::ptr::copy_nonoverlapping(val, list_data.add(ord_idx * vs), vs);
        }
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alloc_i64_i64(count: i64) -> *mut u8 {
        phx_map_alloc(8, 8, count)
    }

    fn set_i64(map: *mut u8, k: i64, v: i64) -> *mut u8 {
        unsafe {
            phx_map_set_raw(
                map,
                &k as *const i64 as *const u8,
                &v as *const i64 as *const u8,
                8,
                8,
            )
        }
    }

    fn get_i64(map: *const u8, k: i64) -> Option<i64> {
        let key_ptr = &k as *const i64 as *const u8;
        let p = unsafe { phx_map_get_raw(map, key_ptr, 8) };
        if p.is_null() {
            None
        } else {
            Some(unsafe { *(p as *const i64) })
        }
    }

    #[test]
    fn alloc_and_length() {
        let map = alloc_i64_i64(0);
        assert_eq!(unsafe { phx_map_length(map) }, 0);
    }

    #[test]
    fn set_and_get_single() {
        let map = alloc_i64_i64(0);
        let map = set_i64(map, 42, 100);
        assert_eq!(unsafe { phx_map_length(map) }, 1);
        assert_eq!(get_i64(map, 42), Some(100));
        assert_eq!(get_i64(map, 41), None);
    }

    #[test]
    fn set_overwrites_value_preserves_others() {
        let mut map = alloc_i64_i64(0);
        for i in 0..5i64 {
            map = set_i64(map, i, i * 10);
        }
        // Overwrite key 2.
        map = set_i64(map, 2, 999);
        assert_eq!(get_i64(map, 0), Some(0));
        assert_eq!(get_i64(map, 1), Some(10));
        assert_eq!(get_i64(map, 2), Some(999));
        assert_eq!(get_i64(map, 3), Some(30));
        assert_eq!(get_i64(map, 4), Some(40));
        assert_eq!(unsafe { phx_map_length(map) }, 5);
    }

    #[test]
    fn growth_preserves_all_entries() {
        // Insert enough keys to force at least one resize from the
        // initial 8-bucket table.
        let mut map = alloc_i64_i64(0);
        for i in 0..50i64 {
            map = set_i64(map, i, i + 1000);
        }
        assert_eq!(unsafe { phx_map_length(map) }, 50);
        for i in 0..50i64 {
            assert_eq!(get_i64(map, i), Some(i + 1000));
        }
    }

    #[test]
    fn contains_and_remove() {
        let mut map = alloc_i64_i64(0);
        map = set_i64(map, 1, 10);
        let key_ptr = &1i64 as *const i64 as *const u8;
        assert_eq!(unsafe { phx_map_contains(map, key_ptr, 8) }, 1);
        let map = unsafe { phx_map_remove_raw(map, key_ptr, 8) };
        assert_eq!(unsafe { phx_map_contains(map, key_ptr, 8) }, 0);
        assert_eq!(get_i64(map, 1), None);
    }

    #[test]
    fn remove_nonexistent_returns_copy() {
        let mut map = alloc_i64_i64(0);
        map = set_i64(map, 1, 10);
        let key_ptr = &99i64 as *const i64 as *const u8;
        let map2 = unsafe { phx_map_remove_raw(map, key_ptr, 8) };
        assert_eq!(unsafe { phx_map_length(map2) }, 1);
        assert_eq!(get_i64(map2, 1), Some(10));
    }

    #[test]
    fn keys_and_values() {
        let mut map = alloc_i64_i64(0);
        for i in 0..5i64 {
            map = set_i64(map, i, i * 100);
        }
        let keys = unsafe { phx_map_keys(map) };
        let vals = unsafe { phx_map_values(map) };
        assert_eq!(unsafe { crate::list_methods::phx_list_length(keys) }, 5);
        assert_eq!(unsafe { crate::list_methods::phx_list_length(vals) }, 5);

        // Sum should match — bucket-order is not guaranteed, but content is.
        let mut key_sum = 0i64;
        let mut val_sum = 0i64;
        for i in 0..5 {
            let kp = unsafe { crate::list_methods::phx_list_get_raw(keys, i) };
            let vp = unsafe { crate::list_methods::phx_list_get_raw(vals, i) };
            key_sum += unsafe { *(kp as *const i64) };
            val_sum += unsafe { *(vp as *const i64) };
        }
        assert_eq!(key_sum, 1 + 2 + 3 + 4);
        assert_eq!(val_sum, 100 + 200 + 300 + 400);
    }

    #[test]
    fn string_keys_by_content() {
        // Force distinct heap allocations so `s1.as_ptr() != s3.as_ptr()`.
        // Plain `"hello"` literals would deduplicate via the rodata
        // pool, leaving the test trivially passing on pointer equality.
        let s1: String = ['h', 'e', 'l', 'l', 'o'].iter().collect();
        let s2: String = ['w', 'o', 'r', 'l', 'd'].iter().collect();
        let s3: String = ['h', 'e', 'l', 'l', 'o'].iter().collect();
        assert_ne!(
            s1.as_ptr(),
            s3.as_ptr(),
            "test setup failed: s1 and s3 share a backing buffer, so this \
             test would not actually exercise content-based lookup",
        );
        let map = phx_map_alloc(16, 8, 0);
        let k1 = [s1.as_ptr() as i64, s1.len() as i64];
        let v1: i64 = 1;
        let map = unsafe {
            phx_map_set_raw(
                map,
                k1.as_ptr() as *const u8,
                &v1 as *const i64 as *const u8,
                16,
                8,
            )
        };
        let k2 = [s2.as_ptr() as i64, s2.len() as i64];
        let v2: i64 = 2;
        let map = unsafe {
            phx_map_set_raw(
                map,
                k2.as_ptr() as *const u8,
                &v2 as *const i64 as *const u8,
                16,
                8,
            )
        };
        // Look up using s3 (same content as s1, distinct pointer).
        let k3 = [s3.as_ptr() as i64, s3.len() as i64];
        let result = unsafe { phx_map_get_raw(map, k3.as_ptr() as *const u8, 16) };
        assert!(!result.is_null());
        assert_eq!(unsafe { *(result as *const i64) }, 1);
    }

    #[test]
    fn from_pairs_builder_matches_set_raw() {
        // Build via the literal-path builder and via repeated set_raw;
        // the two maps should agree on every key's value.
        let pair_data: Vec<i64> = vec![1, 100, 2, 200, 3, 300];
        let from_pairs = unsafe { phx_map_from_pairs(8, 8, 3, pair_data.as_ptr() as *const u8) };
        let mut by_set = alloc_i64_i64(0);
        for (k, v) in [(1, 100), (2, 200), (3, 300)] {
            by_set = set_i64(by_set, k as i64, v as i64);
        }
        for k in 1..=3i64 {
            assert_eq!(get_i64(from_pairs, k), get_i64(by_set, k));
        }
    }

    #[test]
    fn from_pairs_last_write_wins_on_duplicate_keys() {
        let pair_data: Vec<i64> = vec![5, 100, 5, 200, 5, 300];
        let m = unsafe { phx_map_from_pairs(8, 8, 3, pair_data.as_ptr() as *const u8) };
        assert_eq!(unsafe { phx_map_length(m) }, 1);
        assert_eq!(get_i64(m, 5), Some(300));
    }

    /// Documented carve-out: when `n_pairs == 0`, `pair_data` is not
    /// dereferenced and a null pointer is permitted. Cranelift's
    /// empty-literal lowering depends on this — it elides the stack
    /// buffer entirely and passes `0`. Pinned here so a future
    /// "let's just always read pair_data" simplification fails loudly.
    #[test]
    fn from_pairs_with_null_pair_data_when_n_pairs_zero() {
        let m = unsafe { phx_map_from_pairs(8, 8, 0, std::ptr::null()) };
        assert_eq!(unsafe { phx_map_length(m) }, 0);
        assert!(get_i64(m, 0).is_none());
        assert!(get_i64(m, 42).is_none());
    }

    #[test]
    fn empty_map_get_returns_null() {
        let map = alloc_i64_i64(0);
        assert!(get_i64(map, 99).is_none());
    }

    #[test]
    fn lookup_after_many_removes_still_finds_existing() {
        // Insert 20 keys, remove 10, verify the remaining 10 are
        // still findable through the tombstone trail.
        let mut map = alloc_i64_i64(0);
        for i in 0..20i64 {
            map = set_i64(map, i, i * 7);
        }
        for i in 0..10i64 {
            let kp = &i as *const i64 as *const u8;
            map = unsafe { phx_map_remove_raw(map, kp, 8) };
        }
        for i in 0..10i64 {
            assert_eq!(get_i64(map, i), None);
        }
        for i in 10..20i64 {
            assert_eq!(get_i64(map, i), Some(i * 7));
        }
        assert_eq!(unsafe { phx_map_length(map) }, 10);
    }

    /// Read every key from a map (in insertion order) into a Vec<i64>.
    /// Helper used by the order-preservation tests below.
    fn keys_i64_vec(map: *const u8) -> Vec<i64> {
        let keys = unsafe { phx_map_keys(map) };
        let n = unsafe { crate::list_methods::phx_list_length(keys) } as usize;
        (0..n)
            .map(|i| {
                let kp = unsafe { crate::list_methods::phx_list_get_raw(keys, i as i64) };
                unsafe { *(kp as *const i64) }
            })
            .collect()
    }

    /// Read every value from a map (in insertion order) into a Vec<i64>.
    fn values_i64_vec(map: *const u8) -> Vec<i64> {
        let vals = unsafe { phx_map_values(map) };
        let n = unsafe { crate::list_methods::phx_list_length(vals) } as usize;
        (0..n)
            .map(|i| {
                let vp = unsafe { crate::list_methods::phx_list_get_raw(vals, i as i64) };
                unsafe { *(vp as *const i64) }
            })
            .collect()
    }

    #[test]
    fn keys_and_values_preserve_insertion_order() {
        // Insert in non-sorted order and check both keys() and values()
        // return in the original insertion order.
        let mut map = alloc_i64_i64(0);
        let order = [42i64, 7, 99, 1, 88];
        for (k, v) in order.iter().zip([100i64, 200, 300, 400, 500].iter()) {
            map = set_i64(map, *k, *v);
        }
        assert_eq!(keys_i64_vec(map), order);
        assert_eq!(values_i64_vec(map), [100, 200, 300, 400, 500]);
    }

    #[test]
    fn keys_preserve_insertion_order_across_rehash() {
        // Insert 30 keys in non-sorted order, crossing at least the
        // 8 → 16 → 32 → 64 grow boundaries. The order array must be
        // rewritten by `rehash_into` such that iteration still returns
        // the original insertion sequence.
        let order: Vec<i64> = (0..30i64).map(|i| (i * 17) % 97).collect();
        let mut map = alloc_i64_i64(0);
        for (i, k) in order.iter().enumerate() {
            map = set_i64(map, *k, i as i64);
        }
        assert_eq!(keys_i64_vec(map), order);
        // Values should match positions in `order`.
        assert_eq!(values_i64_vec(map), (0..30i64).collect::<Vec<_>>());
    }

    #[test]
    fn remove_preserves_remaining_insertion_order() {
        // Insert 5 keys, remove a middle one, verify the remaining four
        // come back in their original insertion order.
        let mut map = alloc_i64_i64(0);
        let inserted = [10i64, 20, 30, 40, 50];
        for k in inserted {
            map = set_i64(map, k, k * 2);
        }
        let k = 30i64;
        let kp = &k as *const i64 as *const u8;
        map = unsafe { phx_map_remove_raw(map, kp, 8) };
        assert_eq!(keys_i64_vec(map), [10, 20, 40, 50]);
        assert_eq!(values_i64_vec(map), [20, 40, 80, 100]);
    }

    #[test]
    fn from_pairs_large_payload_no_corruption() {
        // 50 pairs forces phx_map_alloc to pick a 128-bucket table and
        // exercises the in-place build over a non-trivial probe space.
        // If the build path ever regresses to per-pair COW, this is the
        // size at which a GC interaction would be visible.
        const N: i64 = 50;
        let mut pair_data = Vec::<i64>::with_capacity((N * 2) as usize);
        for i in 0..N {
            pair_data.push(i * 13 + 1);
            pair_data.push(i * 100);
        }
        let m = unsafe { phx_map_from_pairs(8, 8, N, pair_data.as_ptr() as *const u8) };
        assert_eq!(unsafe { phx_map_length(m) }, N);
        for i in 0..N {
            assert_eq!(get_i64(m, i * 13 + 1), Some(i * 100));
        }
    }

    #[test]
    fn from_pairs_preserves_insertion_order() {
        // Source order is non-sorted; the resulting map's keys() should
        // come back in exactly that order.
        let pair_data: Vec<i64> = vec![42, 1, 7, 2, 99, 3, 1, 4, 88, 5];
        let m = unsafe { phx_map_from_pairs(8, 8, 5, pair_data.as_ptr() as *const u8) };
        assert_eq!(keys_i64_vec(m), [42, 7, 99, 1, 88]);
        assert_eq!(values_i64_vec(m), [1, 2, 3, 4, 5]);
    }

    #[test]
    fn from_pairs_duplicate_keys_keep_first_position_last_value() {
        // Duplicates: key 5 appears at positions 0, 2, 4 with values
        // 100, 200, 300. Expected behaviour: one entry, position is the
        // *first* sighting (index 0), value is the *last* (300).
        let pair_data: Vec<i64> = vec![5, 100, 9, 11, 5, 200, 7, 22, 5, 300];
        let m = unsafe { phx_map_from_pairs(8, 8, 5, pair_data.as_ptr() as *const u8) };
        assert_eq!(unsafe { phx_map_length(m) }, 3);
        assert_eq!(keys_i64_vec(m), [5, 9, 7]);
        assert_eq!(values_i64_vec(m), [300, 11, 22]);
    }

    /// Concrete-to-concrete shape change: an empty `Map<Int, Int>`
    /// (stored ks=8, vs=8, length=0) accepting a string-keyed insert
    /// (caller ks=16, vs=8). `phoenix-ir` does not currently emit this
    /// pattern, but the recovery branch in `phx_map_set_raw` is
    /// deliberately written to absorb it: the input is empty and
    /// discardable, and the caller's sizes are by definition
    /// authoritative. This test pins the documented contract so a
    /// future simplification of the recovery condition (e.g.
    /// "only run for placeholder 0/0 sizes") is caught.
    #[test]
    fn set_on_empty_concrete_sized_map_accepts_shape_change() {
        // Empty Map<Int, Int>: stored ks/vs = 8/8.
        let initial = phx_map_alloc(8, 8, 0);
        // Insert a 16-byte fat-pointer-shaped key with an i64 value —
        // caller's sizes (16, 8) differ from stored (8, 8). The
        // recovery branch must allocate a fresh table at the caller's
        // sizes rather than striding at the old shape.
        let s: String = ['x', 'y', 'z'].iter().collect();
        let key_bytes = [s.as_ptr() as i64, s.len() as i64];
        let val: i64 = 777;
        let map = unsafe {
            phx_map_set_raw(
                initial,
                key_bytes.as_ptr() as *const u8,
                &val as *const i64 as *const u8,
                16,
                8,
            )
        };
        assert_eq!(unsafe { phx_map_length(map) }, 1);
        // Header should reflect the caller's sizes after recovery.
        let stored_ks = unsafe { *((map as *const i64).add(2)) };
        let stored_vs = unsafe { *((map as *const i64).add(3)) };
        assert_eq!(stored_ks, 16, "recovery should rewrite ks to caller's 16");
        assert_eq!(stored_vs, 8, "recovery should keep vs at 8");
        // Look up the same string content via a distinct buffer to
        // confirm the key is actually findable through the new shape.
        let s2: String = ['x', 'y', 'z'].iter().collect();
        assert_ne!(s.as_ptr(), s2.as_ptr());
        let lookup = [s2.as_ptr() as i64, s2.len() as i64];
        let result = unsafe { phx_map_get_raw(map, lookup.as_ptr() as *const u8, 16) };
        assert!(!result.is_null());
        assert_eq!(unsafe { *(result as *const i64) }, 777);
    }

    /// Empty map allocated with placeholder sizes (0/0 — what
    /// generic-type resolution emits before a concrete type is known)
    /// must accept a `set` against the caller's concrete sizes by
    /// re-allocating. Without the recovery branch in `phx_map_set_raw`
    /// the table would wedge at a 0-byte pair stride.
    #[test]
    fn set_on_empty_placeholder_sized_map_recovers_with_caller_sizes() {
        // Allocate as if from a generic placeholder: ks = vs = 0.
        let placeholder = phx_map_alloc(0, 0, 0);
        // Insert with concrete (8, 8) sizes — the same shape codegen
        // would emit once the generic was resolved to Map<Int, Int>.
        let key: i64 = 7;
        let val: i64 = 42;
        let map = unsafe {
            phx_map_set_raw(
                placeholder,
                &key as *const i64 as *const u8,
                &val as *const i64 as *const u8,
                8,
                8,
            )
        };
        assert_eq!(unsafe { phx_map_length(map) }, 1);
        assert_eq!(get_i64(map, 7), Some(42));
        // Header should reflect the caller's sizes, not the placeholder.
        let stored_ks = unsafe { *((map as *const i64).add(2)) };
        let stored_vs = unsafe { *((map as *const i64).add(3)) };
        assert_eq!(stored_ks, 8);
        assert_eq!(stored_vs, 8);
    }

    /// Float-key invariant — pinned because `keys_equal`/`hash_key`
    /// deliberately diverge from IEEE float semantics:
    ///
    /// - `0.0` and `-0.0` have different bit patterns and must map to
    ///   distinct keys (IEEE would treat them as equal).
    /// - Two NaN values with different bit patterns must map to
    ///   distinct keys (IEEE would treat them as unequal — and so would
    ///   never find them again — but byte equality lets us at least
    ///   round-trip a stored NaN).
    /// - The same NaN bit pattern must round-trip: insert NaN(bits X),
    ///   look up NaN(bits X), get the value back. IEEE would say
    ///   `NaN != NaN` and the lookup would always fail.
    ///
    /// If anyone teaches `elements_equal` IEEE semantics for 8-byte
    /// elements without giving `keys_equal` a `kind`-tag bypass, this
    /// test fires.
    fn set_f64(map: *mut u8, k: f64, v: i64) -> *mut u8 {
        unsafe {
            phx_map_set_raw(
                map,
                &k as *const f64 as *const u8,
                &v as *const i64 as *const u8,
                8,
                8,
            )
        }
    }

    fn get_f64(map: *const u8, k: f64) -> Option<i64> {
        let p = unsafe { phx_map_get_raw(map, &k as *const f64 as *const u8, 8) };
        if p.is_null() {
            None
        } else {
            Some(unsafe { *(p as *const i64) })
        }
    }

    #[test]
    fn float_keys_pos_zero_and_neg_zero_are_distinct() {
        let map = phx_map_alloc(8, 8, 0);
        let map = set_f64(map, 0.0, 1);
        let map = set_f64(map, -0.0, 2);
        assert_eq!(unsafe { phx_map_length(map) }, 2);
        assert_eq!(get_f64(map, 0.0), Some(1));
        assert_eq!(get_f64(map, -0.0), Some(2));
    }

    #[test]
    fn float_keys_distinct_nan_bit_patterns_are_distinct_keys() {
        // Two NaN values with different payload bits. Both satisfy
        // `is_nan()`, but their byte representations differ — so map
        // identity (byte equality) keeps them separate.
        let nan_a = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan_b = f64::from_bits(0x7ff8_0000_0000_0002);
        assert!(nan_a.is_nan() && nan_b.is_nan());
        let map = phx_map_alloc(8, 8, 0);
        let map = set_f64(map, nan_a, 100);
        let map = set_f64(map, nan_b, 200);
        assert_eq!(unsafe { phx_map_length(map) }, 2);
        assert_eq!(get_f64(map, nan_a), Some(100));
        assert_eq!(get_f64(map, nan_b), Some(200));
    }

    #[test]
    fn float_keys_same_nan_bit_pattern_round_trips() {
        // Same bits → same key, despite IEEE NaN != NaN. If
        // `keys_equal` ever switches to IEEE compare for 8-byte
        // elements, this fails (`get_f64` would return None).
        let nan = f64::from_bits(0x7ff8_dead_beef_cafe);
        assert!(nan.is_nan());
        let map = phx_map_alloc(8, 8, 0);
        let map = set_f64(map, nan, 999);
        assert_eq!(get_f64(map, nan), Some(999));
        assert_eq!(unsafe { phx_map_length(map) }, 1);
    }

    /// Interleave grow-triggering inserts with removes so the resulting
    /// table has *both* tombstones and rehash-relocated entries in the
    /// probe space. This is the path the existing `growth_*` and
    /// `lookup_after_many_removes_*` tests don't cover individually.
    ///
    /// The sequence:
    ///   1. Insert keys 0..30 (crosses 8 → 16 → 32 → 64 grow boundaries,
    ///      so every entry has been rehashed at least once).
    ///   2. Remove keys 5..15 (leaves a tombstone trail in the post-grow
    ///      bucket layout).
    ///   3. Insert keys 30..40 — most of these stay below the next grow
    ///      threshold, so they go through the same-capacity-copy path
    ///      and may reuse tombstone buckets along their probe chain.
    ///   4. Verify length and every expected key/value.
    #[test]
    fn rehash_then_tombstone_then_reinsert_preserves_all_entries() {
        let mut map = alloc_i64_i64(0);
        for i in 0..30i64 {
            map = set_i64(map, i, i * 11);
        }
        for i in 5..15i64 {
            let kp = &i as *const i64 as *const u8;
            map = unsafe { phx_map_remove_raw(map, kp, 8) };
        }
        for i in 30..40i64 {
            map = set_i64(map, i, i * 11);
        }

        // Final shape: 0..5, 15..40 → 5 + 25 = 30 entries.
        assert_eq!(unsafe { phx_map_length(map) }, 30);
        for i in 0..5i64 {
            assert_eq!(get_i64(map, i), Some(i * 11));
        }
        for i in 5..15i64 {
            assert_eq!(get_i64(map, i), None);
        }
        for i in 15..40i64 {
            assert_eq!(get_i64(map, i), Some(i * 11));
        }
    }
}
