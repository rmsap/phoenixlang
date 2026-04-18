//! Map runtime functions for compiled Phoenix programs.
//!
//! A Phoenix map is a heap-allocated object with the layout:
//!
//! ```text
//! offset 0:  i64  length       (number of key-value pairs)
//! offset 8:  i64  capacity     (allocated pair slots)
//! offset 16: i64  key_size     (bytes per key)
//! offset 24: i64  val_size     (bytes per value)
//! offset 32: u8[] data         (pairs: (key_size + val_size) * capacity bytes)
//! ```
//!
//! Key lookup is a linear scan with byte-wise comparison.
//! All memory is intentionally leaked (no GC yet).
//!
//! ## Effective-size pattern
//!
//! Several functions accept a `key_size` (and sometimes `val_size`)
//! parameter from the caller AND read stored sizes from the map header.
//! For non-empty maps, the stored sizes are authoritative (the data was
//! laid out using them).  For empty maps the stored sizes may be
//! placeholder values from generic type resolution, so the caller's sizes
//! are used instead.  This "effective size" pattern appears in
//! `phx_map_get_raw`, `phx_map_set_raw`, and `phx_map_remove_raw`.

use crate::phx_alloc;

/// Header size in bytes (length + capacity + key_size + val_size).
pub(crate) const HEADER_SIZE: usize = 32;

/// Read the length, key_size, and val_size from a map header.
///
/// # Safety
///
/// `map` must point to a valid map header allocated by [`phx_map_alloc`].
unsafe fn map_header(map: *const u8) -> (usize, usize, usize) {
    let length = unsafe { *(map as *const i64) } as usize;
    let key_size = unsafe { *((map as *const i64).add(2)) } as usize;
    let val_size = unsafe { *((map as *const i64).add(3)) } as usize;
    (length, key_size, val_size)
}

/// Compare two keys for equality, handling fat pointers (strings).
///
/// Delegates to [`crate::list_methods::elements_equal`] which handles
/// 16-byte string fat pointer comparison and byte-wise comparison for
/// all other sizes.
///
/// # Safety
///
/// Both `a` and `b` must point to `size` valid bytes.
unsafe fn keys_equal(a: *const u8, b: *const u8, size: usize) -> bool {
    // Maps always use byte-wise key comparison (is_float = false).
    // Float keys are unusual; byte equality is the correct behavior for
    // map key identity (NaN keys stay distinct, -0.0 and 0.0 are
    // different keys).
    unsafe { crate::list_methods::elements_equal(a, b, size, false) }
}

/// Allocate a new map with space for `count` key-value pairs.
///
/// Returns a pointer to the map header. The data region is zeroed.
///
/// # Panics
///
/// Panics if `key_size`, `val_size`, or `count` is negative, or if the
/// total allocation size overflows.
#[unsafe(no_mangle)]
pub extern "C" fn phx_map_alloc(key_size: i64, val_size: i64, count: i64) -> *mut u8 {
    assert!(
        key_size >= 0,
        "phx_map_alloc: key_size must be non-negative, got {key_size}"
    );
    assert!(
        val_size >= 0,
        "phx_map_alloc: val_size must be non-negative, got {val_size}"
    );
    assert!(
        count >= 0,
        "phx_map_alloc: count must be non-negative, got {count}"
    );
    let ks = key_size as usize;
    let vs = val_size as usize;
    let cnt = count as usize;
    let pair_size = ks
        .checked_add(vs)
        .expect("phx_map_alloc: pair size overflow");
    let data_size = pair_size
        .checked_mul(cnt)
        .expect("phx_map_alloc: allocation size overflow");
    let total = HEADER_SIZE
        .checked_add(data_size)
        .expect("phx_map_alloc: allocation size overflow");
    let ptr = phx_alloc(total);
    unsafe {
        *(ptr as *mut i64) = count;
        *((ptr as *mut i64).add(1)) = count;
        *((ptr as *mut i64).add(2)) = key_size;
        *((ptr as *mut i64).add(3)) = val_size;
    }
    ptr
}

/// Return the number of entries in a map.
///
/// # Safety
///
/// `map` must point to a valid map header allocated by [`phx_map_alloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_length(map: *const u8) -> i64 {
    let (length, _, _) = unsafe { map_header(map) };
    length as i64
}

/// Look up a key in the map, returning a pointer to the value or null.
///
/// # Safety
///
/// - `map` must point to a valid map header.
/// - `key_ptr` must point to `key_size` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_get_raw(
    map: *const u8,
    key_ptr: *const u8,
    key_size: i64,
) -> *const u8 {
    let (length, stored_ks, vs) = unsafe { map_header(map) };
    // For non-empty maps, use the stored key_size (from the header) so we
    // stride correctly even if the caller passes a stale/placeholder value.
    // For empty maps the stored header may contain placeholder sizes from
    // generic type resolution, so fall back to the caller's key_size.
    let ks = if length == 0 {
        key_size as usize
    } else {
        stored_ks
    };
    let pair_size = ks + vs;
    let data = unsafe { map.add(HEADER_SIZE) };
    for i in 0..length {
        let entry = unsafe { data.add(i * pair_size) };
        if unsafe { keys_equal(key_ptr, entry, ks) } {
            return unsafe { entry.add(ks) };
        }
    }
    std::ptr::null()
}

/// Set a key-value pair in the map, returning a new map.
///
/// If the key already exists, its value is updated in the new copy.
/// Otherwise the pair is appended.
///
/// # Safety
///
/// - `map` must point to a valid map header.
/// - `key_ptr` must point to `key_size` valid bytes.
/// - `val_ptr` must point to `val_size` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_set_raw(
    map: *const u8,
    key_ptr: *const u8,
    val_ptr: *const u8,
    key_size: i64,
    val_size: i64,
) -> *mut u8 {
    let (length, stored_ks, stored_vs) = unsafe { map_header(map) };
    // For non-empty maps, use stored sizes so we stride through the
    // existing data correctly.  For empty maps the stored header may
    // contain placeholder sizes, so use the caller's concrete sizes.
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
    let data = unsafe { map.add(HEADER_SIZE) };
    // Check if key exists.
    for i in 0..length {
        let entry = unsafe { data.add(i * pair_size) };
        if unsafe { keys_equal(key_ptr, entry, ks) } {
            // Key found — copy entire map, update value at this index.
            // Use the effective ks/vs (not the caller sizes) so the new
            // map's header matches the data layout being copied.
            let new_map = phx_map_alloc(ks as i64, vs as i64, length as i64);
            let new_data = unsafe { new_map.add(HEADER_SIZE) };
            unsafe {
                std::ptr::copy_nonoverlapping(data, new_data, length * pair_size);
                let dest_val = new_data.add(i * pair_size + ks);
                std::ptr::copy_nonoverlapping(val_ptr, dest_val, vs);
            }
            return new_map;
        }
    }

    // Key not found — copy and append.
    // Use effective sizes (ks/vs) — not caller sizes — so the new map's
    // header is consistent with the data layout being copied.
    let new_map = phx_map_alloc(ks as i64, vs as i64, (length + 1) as i64);
    let new_data = unsafe { new_map.add(HEADER_SIZE) };
    unsafe {
        std::ptr::copy_nonoverlapping(data, new_data, length * pair_size);
        let dest = new_data.add(length * pair_size);
        std::ptr::copy_nonoverlapping(key_ptr, dest, ks);
        std::ptr::copy_nonoverlapping(val_ptr, dest.add(ks), vs);
    }
    new_map
}

/// Remove a key from the map, returning a new map without it.
///
/// If the key is not found, returns a copy of the original map.
///
/// # Safety
///
/// - `map` must point to a valid map header.
/// - `key_ptr` must point to `key_size` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_remove_raw(
    map: *const u8,
    key_ptr: *const u8,
    key_size: i64,
) -> *mut u8 {
    let (length, stored_ks, vs) = unsafe { map_header(map) };
    // Use stored key_size for non-empty maps so we stride correctly.
    let ks = if length == 0 {
        key_size as usize
    } else {
        stored_ks
    };
    let val_size = vs as i64;
    let pair_size = ks + vs;
    let data = unsafe { map.add(HEADER_SIZE) };
    // Find the key's index.
    let mut found_idx: Option<usize> = None;
    for i in 0..length {
        let entry = unsafe { data.add(i * pair_size) };
        if unsafe { keys_equal(key_ptr, entry, ks) } {
            found_idx = Some(i);
            break;
        }
    }

    let Some(idx) = found_idx else {
        // Not found — return a copy.
        // Use effective sizes (ks/vs) so the new map's header is consistent
        // with the data layout being copied, matching phx_map_set_raw.
        let new_map = phx_map_alloc(ks as i64, vs as i64, length as i64);
        let new_data = unsafe { new_map.add(HEADER_SIZE) };
        unsafe {
            std::ptr::copy_nonoverlapping(data, new_data, length * pair_size);
        }
        return new_map;
    };

    let new_len = length - 1;
    let new_map = phx_map_alloc(key_size, val_size, new_len as i64);
    let new_data = unsafe { new_map.add(HEADER_SIZE) };
    // Copy entries before the removed one.
    if idx > 0 {
        unsafe {
            std::ptr::copy_nonoverlapping(data, new_data, idx * pair_size);
        }
    }
    // Copy entries after the removed one.
    if idx < new_len {
        unsafe {
            let src = data.add((idx + 1) * pair_size);
            let dst = new_data.add(idx * pair_size);
            std::ptr::copy_nonoverlapping(src, dst, (new_len - idx) * pair_size);
        }
    }
    new_map
}

/// Check if a map contains a given key.
///
/// Returns 1 if found, 0 otherwise.
///
/// # Safety
///
/// - `map` must point to a valid map header.
/// - `key_ptr` must point to `key_size` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_contains(map: *const u8, key_ptr: *const u8, key_size: i64) -> i8 {
    let result = unsafe { phx_map_get_raw(map, key_ptr, key_size) };
    if result.is_null() { 0 } else { 1 }
}

/// Extract all keys from a map into a new list.
///
/// # Safety
///
/// `map` must point to a valid map header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_keys(map: *const u8) -> *mut u8 {
    let (length, ks, vs) = unsafe { map_header(map) };
    let key_size = ks as i64;
    let pair_size = ks + vs;
    let data = unsafe { map.add(HEADER_SIZE) };

    let list = crate::list_methods::phx_list_alloc(key_size, length as i64);
    let list_data = unsafe { list.add(crate::list_methods::HEADER_SIZE) };
    for i in 0..length {
        let key = unsafe { data.add(i * pair_size) };
        unsafe {
            std::ptr::copy_nonoverlapping(key, list_data.add(i * ks), ks);
        }
    }
    list
}

/// Extract all values from a map into a new list.
///
/// # Safety
///
/// `map` must point to a valid map header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_map_values(map: *const u8) -> *mut u8 {
    let (length, ks, vs) = unsafe { map_header(map) };
    let val_size = vs as i64;
    let pair_size = ks + vs;
    let data = unsafe { map.add(HEADER_SIZE) };

    let list = crate::list_methods::phx_list_alloc(val_size, length as i64);
    let list_data = unsafe { list.add(crate::list_methods::HEADER_SIZE) };
    for i in 0..length {
        let val = unsafe { data.add(i * pair_size + ks) };
        unsafe {
            std::ptr::copy_nonoverlapping(val, list_data.add(i * vs), vs);
        }
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_alloc_and_length() {
        let map = phx_map_alloc(8, 8, 3);
        assert_eq!(unsafe { phx_map_length(map) }, 3);
    }

    #[test]
    fn map_set_and_get() {
        let map = phx_map_alloc(8, 8, 0);
        let key: i64 = 10;
        let val: i64 = 100;
        let map2 = unsafe {
            phx_map_set_raw(
                map,
                &key as *const i64 as *const u8,
                &val as *const i64 as *const u8,
                8,
                8,
            )
        };
        assert_eq!(unsafe { phx_map_length(map2) }, 1);
        let result = unsafe { phx_map_get_raw(map2, &key as *const i64 as *const u8, 8) };
        assert!(!result.is_null());
        assert_eq!(unsafe { *(result as *const i64) }, 100);
    }

    #[test]
    fn map_contains_and_remove() {
        let map = phx_map_alloc(8, 8, 0);
        let k1: i64 = 1;
        let v1: i64 = 10;
        let map = unsafe {
            phx_map_set_raw(
                map,
                &k1 as *const i64 as *const u8,
                &v1 as *const i64 as *const u8,
                8,
                8,
            )
        };
        assert_eq!(
            unsafe { phx_map_contains(map, &k1 as *const i64 as *const u8, 8) },
            1
        );
        let map = unsafe { phx_map_remove_raw(map, &k1 as *const i64 as *const u8, 8) };
        assert_eq!(
            unsafe { phx_map_contains(map, &k1 as *const i64 as *const u8, 8) },
            0
        );
    }

    #[test]
    fn map_keys_and_values() {
        let map = phx_map_alloc(8, 8, 0);
        let k1: i64 = 10;
        let v1: i64 = 100;
        let k2: i64 = 20;
        let v2: i64 = 200;
        let map = unsafe {
            phx_map_set_raw(
                map,
                &k1 as *const i64 as *const u8,
                &v1 as *const i64 as *const u8,
                8,
                8,
            )
        };
        let map = unsafe {
            phx_map_set_raw(
                map,
                &k2 as *const i64 as *const u8,
                &v2 as *const i64 as *const u8,
                8,
                8,
            )
        };
        let keys = unsafe { phx_map_keys(map) };
        assert_eq!(unsafe { crate::list_methods::phx_list_length(keys) }, 2);
        let vals = unsafe { phx_map_values(map) };
        assert_eq!(unsafe { crate::list_methods::phx_list_length(vals) }, 2);
    }

    #[test]
    fn map_remove_nonexistent() {
        let map = phx_map_alloc(8, 8, 0);
        let k1: i64 = 1;
        let v1: i64 = 10;
        let map = unsafe {
            phx_map_set_raw(
                map,
                &k1 as *const i64 as *const u8,
                &v1 as *const i64 as *const u8,
                8,
                8,
            )
        };
        let k2: i64 = 99;
        let map2 = unsafe { phx_map_remove_raw(map, &k2 as *const i64 as *const u8, 8) };
        // Should still have the original entry.
        assert_eq!(unsafe { phx_map_length(map2) }, 1);
        assert_eq!(
            unsafe { phx_map_contains(map2, &k1 as *const i64 as *const u8, 8) },
            1
        );
    }

    /// Overwriting a key in a multi-entry map must preserve
    /// all other entries (exercises stored header sizes in `phx_map_set_raw`).
    #[test]
    fn map_set_overwrite_preserves_other_entries() {
        let map = phx_map_alloc(8, 8, 0);
        let k1: i64 = 1;
        let v1: i64 = 10;
        let k2: i64 = 2;
        let v2: i64 = 20;
        let k3: i64 = 3;
        let v3: i64 = 30;
        let map = unsafe {
            phx_map_set_raw(
                map,
                &k1 as *const i64 as *const u8,
                &v1 as *const i64 as *const u8,
                8,
                8,
            )
        };
        let map = unsafe {
            phx_map_set_raw(
                map,
                &k2 as *const i64 as *const u8,
                &v2 as *const i64 as *const u8,
                8,
                8,
            )
        };
        let map = unsafe {
            phx_map_set_raw(
                map,
                &k3 as *const i64 as *const u8,
                &v3 as *const i64 as *const u8,
                8,
                8,
            )
        };
        // Overwrite k2.
        let v2_new: i64 = 99;
        let map = unsafe {
            phx_map_set_raw(
                map,
                &k2 as *const i64 as *const u8,
                &v2_new as *const i64 as *const u8,
                8,
                8,
            )
        };
        assert_eq!(unsafe { phx_map_length(map) }, 3);
        // k1 should still be 10.
        let r1 = unsafe { phx_map_get_raw(map, &k1 as *const i64 as *const u8, 8) };
        assert_eq!(unsafe { *(r1 as *const i64) }, 10);
        // k2 should be 99.
        let r2 = unsafe { phx_map_get_raw(map, &k2 as *const i64 as *const u8, 8) };
        assert_eq!(unsafe { *(r2 as *const i64) }, 99);
        // k3 should still be 30.
        let r3 = unsafe { phx_map_get_raw(map, &k3 as *const i64 as *const u8, 8) };
        assert_eq!(unsafe { *(r3 as *const i64) }, 30);
    }

    /// `phx_map_keys` and `phx_map_values` must return lists
    /// with the correct content (not just the correct length).
    #[test]
    fn map_keys_content() {
        let map = phx_map_alloc(8, 8, 0);
        let k1: i64 = 10;
        let v1: i64 = 100;
        let k2: i64 = 20;
        let v2: i64 = 200;
        let map = unsafe {
            phx_map_set_raw(
                map,
                &k1 as *const i64 as *const u8,
                &v1 as *const i64 as *const u8,
                8,
                8,
            )
        };
        let map = unsafe {
            phx_map_set_raw(
                map,
                &k2 as *const i64 as *const u8,
                &v2 as *const i64 as *const u8,
                8,
                8,
            )
        };
        let keys = unsafe { phx_map_keys(map) };
        assert_eq!(unsafe { crate::list_methods::phx_list_length(keys) }, 2);
        let k0 = unsafe { crate::list_methods::phx_list_get_raw(keys, 0) };
        let k1_out = unsafe { crate::list_methods::phx_list_get_raw(keys, 1) };
        assert_eq!(unsafe { *(k0 as *const i64) }, 10);
        assert_eq!(unsafe { *(k1_out as *const i64) }, 20);
        let vals = unsafe { phx_map_values(map) };
        let v0 = unsafe { crate::list_methods::phx_list_get_raw(vals, 0) };
        let v1_out = unsafe { crate::list_methods::phx_list_get_raw(vals, 1) };
        assert_eq!(unsafe { *(v0 as *const i64) }, 100);
        assert_eq!(unsafe { *(v1_out as *const i64) }, 200);
    }

    #[test]
    fn map_get_empty() {
        let map = phx_map_alloc(8, 8, 0);
        let key: i64 = 42;
        let result = unsafe { phx_map_get_raw(map, &key as *const i64 as *const u8, 8) };
        assert!(result.is_null());
    }

    #[test]
    fn map_keys_empty() {
        let map = phx_map_alloc(8, 8, 0);
        let keys = unsafe { phx_map_keys(map) };
        assert_eq!(unsafe { crate::list_methods::phx_list_length(keys) }, 0);
    }

    #[test]
    fn map_values_empty() {
        let map = phx_map_alloc(8, 8, 0);
        let vals = unsafe { phx_map_values(map) };
        assert_eq!(unsafe { crate::list_methods::phx_list_length(vals) }, 0);
    }

    /// map_set_raw "key found" path must use
    /// consistent sizes when overwriting an existing key.
    #[test]
    fn map_set_overwrite_preserves_sizes() {
        let map = phx_map_alloc(8, 8, 0);
        let key: i64 = 1;
        let val1: i64 = 100;
        let map = unsafe {
            phx_map_set_raw(
                map,
                &key as *const i64 as *const u8,
                &val1 as *const i64 as *const u8,
                8,
                8,
            )
        };
        assert_eq!(unsafe { phx_map_length(map) }, 1);

        // Overwrite the same key with a new value.
        let val2: i64 = 200;
        let map = unsafe {
            phx_map_set_raw(
                map,
                &key as *const i64 as *const u8,
                &val2 as *const i64 as *const u8,
                8,
                8,
            )
        };
        // Length should still be 1 (overwrite, not append).
        assert_eq!(unsafe { phx_map_length(map) }, 1);
        // Value should be the new one.
        let result = unsafe { phx_map_get_raw(map, &key as *const i64 as *const u8, 8) };
        assert!(!result.is_null());
        assert_eq!(unsafe { *(result as *const i64) }, 200);
    }

    #[test]
    fn map_string_keys() {
        // Test 16-byte (string fat pointer) key comparison.
        let s1 = "hello";
        let s2 = "world";
        let s3 = "hello"; // same content as s1, different pointer
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
        // Look up using s3 (same content as s1, different pointer).
        let k3 = [s3.as_ptr() as i64, s3.len() as i64];
        let result = unsafe { phx_map_get_raw(map, k3.as_ptr() as *const u8, 16) };
        assert!(!result.is_null());
        assert_eq!(unsafe { *(result as *const i64) }, 1);
    }

    /// `phx_map_set_raw` "key not found" path must
    /// use the effective sizes (from the stored header) rather than the
    /// caller's raw parameters, so the new map's header is consistent with
    /// the copied data layout.
    #[test]
    fn map_set_append_uses_stored_sizes() {
        // Build a map with one entry using key_size=8, val_size=8.
        let map = phx_map_alloc(8, 8, 0);
        let k1: i64 = 1;
        let v1: i64 = 10;
        let map = unsafe {
            phx_map_set_raw(
                map,
                &k1 as *const i64 as *const u8,
                &v1 as *const i64 as *const u8,
                8,
                8,
            )
        };
        // Append a second key.  The stored header already knows
        // key_size=8, val_size=8.
        let k2: i64 = 2;
        let v2: i64 = 20;
        let map = unsafe {
            phx_map_set_raw(
                map,
                &k2 as *const i64 as *const u8,
                &v2 as *const i64 as *const u8,
                8,
                8,
            )
        };
        assert_eq!(unsafe { phx_map_length(map) }, 2);
        // Verify the new map's header reports correct sizes by reading
        // both entries — if the header were wrong, get_raw would stride
        // incorrectly and return garbage.
        let r1 = unsafe { phx_map_get_raw(map, &k1 as *const i64 as *const u8, 8) };
        assert!(!r1.is_null());
        assert_eq!(unsafe { *(r1 as *const i64) }, 10);
        let r2 = unsafe { phx_map_get_raw(map, &k2 as *const i64 as *const u8, 8) };
        assert!(!r2.is_null());
        assert_eq!(unsafe { *(r2 as *const i64) }, 20);
    }
}
