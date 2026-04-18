//! List runtime functions for compiled Phoenix programs.
//!
//! A Phoenix list is a heap-allocated object with the layout:
//!
//! ```text
//! offset 0:  i64  length       (number of elements)
//! offset 8:  i64  capacity     (allocated element slots)
//! offset 16: i64  elem_size    (bytes per element: 8 for scalars, 16 for StringRef)
//! offset 24: u8[] data         (element storage, length * elem_size bytes)
//! ```
//!
//! All memory is intentionally leaked (no GC yet).

use std::slice;

use crate::phx_alloc;

/// Header size in bytes (length + capacity + elem_size).
pub(crate) const HEADER_SIZE: usize = 24;

/// The element size (in bytes) that is treated as a string fat pointer by
/// [`elements_equal`].  Currently 16 (pointer + length, two 8-byte slots).
///
/// **Invariant:** The Cranelift codegen must ensure that no non-string type
/// produces elements of this size.  The `slots_for_type` function in
/// `helpers.rs` explicitly lists all IR types so that adding a new 2-slot
/// type triggers a compile error.  If a new 2-slot non-string type is added,
/// both `elements_equal` and `slots_for_type` must be updated.
pub(crate) const STRING_FAT_POINTER_SIZE: usize = 16;

/// Read the length and elem_size from a list header.
///
/// # Safety
///
/// `list` must point to a valid list header allocated by [`phx_list_alloc`].
unsafe fn list_header(list: *const u8) -> (usize, usize) {
    let length = unsafe { *(list as *const i64) } as usize;
    let elem_size = unsafe { *((list as *const i64).add(2)) } as usize;
    (length, elem_size)
}

/// Compare two elements for equality, handling 16-byte fat pointers (strings)
/// and IEEE 754 float semantics.
///
/// For 16-byte elements, assumes the element is a string fat pointer
/// (ptr + len) and compares the pointed-to content rather than the raw
/// pointer bytes.  This assumption holds because strings are currently
/// the only Phoenix type that uses 16-byte elements.
///
/// When `is_float` is true, the elements are compared using IEEE 754 equality
/// (`f64::eq`), which correctly handles `-0.0 == 0.0` and `NaN != NaN`.
///
/// # Safety
///
/// - Both `a` and `b` must point to `size` valid bytes.
/// - The caller must guarantee that 16-byte elements are string fat pointers.
///   The Cranelift codegen enforces this via `slots_for_type` in `helpers.rs`,
///   which explicitly lists every `IrType` variant so that adding a new 2-slot
///   non-string type triggers a compile error.  If a new 2-slot type is added,
///   a `kind` tag must be added to this function's signature.
pub(crate) unsafe fn elements_equal(
    a: *const u8,
    b: *const u8,
    size: usize,
    is_float: bool,
) -> bool {
    if size == STRING_FAT_POINTER_SIZE {
        // Fat pointer (string): compare by content.
        debug_assert!(
            !is_float,
            "elements_equal: 16-byte float elements are not supported — \
             16-byte elements are assumed to be string fat pointers"
        );
        let a_ptr = unsafe { *(a as *const i64) } as *const u8;
        let a_len = unsafe { *((a as *const i64).add(1)) } as usize;
        let b_ptr = unsafe { *(b as *const i64) } as *const u8;
        let b_len = unsafe { *((b as *const i64).add(1)) } as usize;
        // Guard: if length looks unreasonably large (> 1 GiB), the data
        // is almost certainly not a string fat pointer.  Fall back to
        // byte-wise comparison to avoid dereferencing a wild pointer.
        const MAX_REASONABLE_LEN: usize = 1 << 30;
        if a_len > MAX_REASONABLE_LEN || b_len > MAX_REASONABLE_LEN {
            let a_bytes = unsafe { slice::from_raw_parts(a, size) };
            let b_bytes = unsafe { slice::from_raw_parts(b, size) };
            return a_bytes == b_bytes;
        }
        if a_len != b_len {
            return false;
        }
        let a_bytes = unsafe { slice::from_raw_parts(a_ptr, a_len) };
        let b_bytes = unsafe { slice::from_raw_parts(b_ptr, b_len) };
        a_bytes == b_bytes
    } else if is_float {
        // IEEE 754 comparison: -0.0 == 0.0 and NaN != NaN.
        let a_val = unsafe { *(a as *const f64) };
        let b_val = unsafe { *(b as *const f64) };
        a_val == b_val
    } else {
        let a_bytes = unsafe { slice::from_raw_parts(a, size) };
        let b_bytes = unsafe { slice::from_raw_parts(b, size) };
        a_bytes == b_bytes
    }
}

/// Allocate a new list with space for `count` elements of `elem_size` bytes each.
///
/// Returns a pointer to the list header. The data region is zeroed.
///
/// # Panics
///
/// Panics if `elem_size` or `count` is negative, or if the total allocation
/// size overflows.
#[unsafe(no_mangle)]
pub extern "C" fn phx_list_alloc(elem_size: i64, count: i64) -> *mut u8 {
    assert!(
        elem_size >= 0,
        "phx_list_alloc: elem_size must be non-negative, got {elem_size}"
    );
    assert!(
        count >= 0,
        "phx_list_alloc: count must be non-negative, got {count}"
    );
    let es = elem_size as usize;
    let cnt = count as usize;
    let data_size = es
        .checked_mul(cnt)
        .expect("phx_list_alloc: allocation size overflow");
    let total = HEADER_SIZE
        .checked_add(data_size)
        .expect("phx_list_alloc: allocation size overflow");
    let ptr = phx_alloc(total);
    unsafe {
        // length = count
        *(ptr as *mut i64) = count;
        // capacity = count
        *((ptr as *mut i64).add(1)) = count;
        // elem_size
        *((ptr as *mut i64).add(2)) = elem_size;
    }
    ptr
}

/// Return the number of elements in a list.
///
/// # Safety
///
/// `list` must point to a valid list header allocated by [`phx_list_alloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_list_length(list: *const u8) -> i64 {
    let (length, _) = unsafe { list_header(list) };
    length as i64
}

/// Return a pointer to the element at `index` in the list's data region.
///
/// The caller is responsible for reading `elem_size` bytes from the returned
/// pointer.  Panics if `index` is out of bounds.
///
/// # Safety
///
/// `list` must point to a valid list header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_list_get_raw(list: *const u8, index: i64) -> *const u8 {
    let (length, elem_size) = unsafe { list_header(list) };
    if index < 0 || index >= length as i64 {
        eprintln!(
            "runtime error: list index {} out of bounds (length {})",
            index, length
        );
        std::process::exit(1);
    }
    unsafe { list.add(HEADER_SIZE + index as usize * elem_size) }
}

/// Create a new list with the given element appended.
///
/// Lists are immutable in Phoenix, so this allocates a new list with
/// `length + 1` elements, copies the old data, then copies the new element.
///
/// # Safety
///
/// - `list` must point to a valid list header.
/// - `elem_ptr` must point to `elem_size` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_list_push_raw(
    list: *const u8,
    elem_ptr: *const u8,
    elem_size: i64,
) -> *mut u8 {
    let (old_len, stored_es) = unsafe { list_header(list) };
    // For empty lists the stored elem_size may be a placeholder (e.g., 0).
    // Use the caller's concrete elem_size in that case.
    let es = if old_len == 0 {
        elem_size as usize
    } else {
        debug_assert_eq!(
            elem_size as usize, stored_es,
            "phx_list_push_raw: caller elem_size ({elem_size}) != stored elem_size ({stored_es})"
        );
        stored_es
    };
    let new_len = old_len + 1;
    let new_list = phx_list_alloc(es as i64, new_len as i64);
    // Copy old data.
    let old_data = unsafe { list.add(HEADER_SIZE) };
    let new_data = unsafe { new_list.add(HEADER_SIZE) };
    unsafe {
        std::ptr::copy_nonoverlapping(old_data, new_data, old_len * es);
    }
    // Copy new element.
    let dest = unsafe { new_data.add(old_len * es) };
    unsafe {
        std::ptr::copy_nonoverlapping(elem_ptr, dest, es);
    }
    new_list
}

/// Check if a list contains an element.
///
/// For 16-byte elements, assumes the element is a string fat pointer
/// (ptr + len) and compares the pointed-to content rather than the raw
/// pointer bytes.  This assumption holds because strings are currently
/// the only Phoenix type that uses 16-byte elements.  If a future type
/// also occupies 16 bytes, this heuristic will need a type tag.
///
/// When `is_float` is non-zero, elements are compared using IEEE 754
/// equality (`f64::eq`), which correctly handles `-0.0 == 0.0` and
/// `NaN != NaN`.
///
/// For all other sizes, uses byte-wise comparison.
///
/// Returns 1 if found, 0 otherwise.
///
/// # Safety
///
/// - `list` must point to a valid list header.
/// - `elem_ptr` must point to `elem_size` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_list_contains(
    list: *const u8,
    elem_ptr: *const u8,
    elem_size: i64,
    is_float: i8,
) -> i8 {
    let (length, es) = unsafe { list_header(list) };
    debug_assert!(
        length == 0 || elem_size as usize == es,
        "phx_list_contains: caller elem_size ({elem_size}) != stored elem_size ({es})"
    );
    let data = unsafe { list.add(HEADER_SIZE) };
    let float_cmp = is_float != 0;

    for i in 0..length {
        let item = unsafe { data.add(i * es) };
        if unsafe { elements_equal(elem_ptr, item, es, float_cmp) } {
            return 1;
        }
    }
    0
}

/// Create a new list containing the first `n` elements of the source list.
///
/// If `n >= length`, returns a copy of the entire list.
///
/// # Safety
///
/// `list` must point to a valid list header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_list_take(list: *const u8, n: i64) -> *mut u8 {
    let (length, es) = unsafe { list_header(list) };
    let elem_size = es as i64;
    let take_count = (n.max(0) as usize).min(length);
    let new_list = phx_list_alloc(elem_size, take_count as i64);
    let old_data = unsafe { list.add(HEADER_SIZE) };
    let new_data = unsafe { new_list.add(HEADER_SIZE) };
    unsafe {
        std::ptr::copy_nonoverlapping(old_data, new_data, take_count * es);
    }
    new_list
}

/// Create a new list with the first `n` elements removed.
///
/// If `n >= length`, returns an empty list.
///
/// # Safety
///
/// `list` must point to a valid list header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_list_drop(list: *const u8, n: i64) -> *mut u8 {
    let (length, es) = unsafe { list_header(list) };
    let elem_size = es as i64;
    let skip = (n.max(0) as usize).min(length);
    let new_len = length - skip;
    let new_list = phx_list_alloc(elem_size, new_len as i64);
    let old_data = unsafe { list.add(HEADER_SIZE + skip * es) };
    let new_data = unsafe { new_list.add(HEADER_SIZE) };
    unsafe {
        std::ptr::copy_nonoverlapping(old_data, new_data, new_len * es);
    }
    new_list
}

/// Split a string by a separator, returning a new list of string fat pointers.
///
/// Each element in the returned list is 16 bytes: `(ptr: i64, len: i64)`.
///
/// # Safety
///
/// Both `(ptr, len)` and `(sep_ptr, sep_len)` must be valid UTF-8 byte slices.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_str_split(
    ptr: *const u8,
    len: usize,
    sep_ptr: *const u8,
    sep_len: usize,
) -> *mut u8 {
    let s = unsafe { std::str::from_utf8_unchecked(slice::from_raw_parts(ptr, len)) };
    let sep = unsafe { std::str::from_utf8_unchecked(slice::from_raw_parts(sep_ptr, sep_len)) };
    let parts: Vec<&str> = s.split(sep).collect();
    let count = parts.len();
    // elem_size = 16 (two i64s: ptr + len)
    let list = phx_list_alloc(16, count as i64);
    let data = unsafe { list.add(HEADER_SIZE) };
    for (i, part) in parts.iter().enumerate() {
        let leaked = crate::leak_string(part.to_string());
        let dest = unsafe { data.add(i * 16) };
        unsafe {
            *(dest as *mut i64) = leaked.ptr as i64;
            *((dest as *mut i64).add(1)) = leaked.len as i64;
        }
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_alloc_and_length() {
        let list = phx_list_alloc(8, 3);
        assert_eq!(unsafe { phx_list_length(list) }, 3);
    }

    #[test]
    fn list_alloc_empty() {
        let list = phx_list_alloc(8, 0);
        assert_eq!(unsafe { phx_list_length(list) }, 0);
    }

    #[test]
    fn list_get_and_push() {
        // Create a list with one i64 element.
        let list = phx_list_alloc(8, 1);
        let data = unsafe { list.add(HEADER_SIZE) };
        unsafe { *(data as *mut i64) = 42 };

        // Verify get.
        let elem_ptr = unsafe { phx_list_get_raw(list, 0) };
        assert_eq!(unsafe { *(elem_ptr as *const i64) }, 42);

        // Push a new element.
        let val: i64 = 99;
        let new_list = unsafe { phx_list_push_raw(list, &val as *const i64 as *const u8, 8) };
        assert_eq!(unsafe { phx_list_length(new_list) }, 2);
        let e0 = unsafe { phx_list_get_raw(new_list, 0) };
        assert_eq!(unsafe { *(e0 as *const i64) }, 42);
        let e1 = unsafe { phx_list_get_raw(new_list, 1) };
        assert_eq!(unsafe { *(e1 as *const i64) }, 99);
    }

    #[test]
    fn list_contains_found() {
        let list = phx_list_alloc(8, 2);
        let data = unsafe { list.add(HEADER_SIZE) };
        unsafe {
            *(data as *mut i64) = 10;
            *((data as *mut i64).add(1)) = 20;
        }
        let val: i64 = 20;
        assert_eq!(
            unsafe { phx_list_contains(list, &val as *const i64 as *const u8, 8, 0) },
            1
        );
    }

    #[test]
    fn list_contains_not_found() {
        let list = phx_list_alloc(8, 2);
        let data = unsafe { list.add(HEADER_SIZE) };
        unsafe {
            *(data as *mut i64) = 10;
            *((data as *mut i64).add(1)) = 20;
        }
        let val: i64 = 30;
        assert_eq!(
            unsafe { phx_list_contains(list, &val as *const i64 as *const u8, 8, 0) },
            0
        );
    }

    #[test]
    fn list_contains_float_neg_zero() {
        // IEEE 754: -0.0 == 0.0 must hold with float comparison.
        let list = phx_list_alloc(8, 2);
        let data = unsafe { list.add(HEADER_SIZE) };
        unsafe {
            *(data as *mut f64) = 1.0;
            *((data as *mut f64).add(1)) = -0.0_f64;
        }
        let val: f64 = 0.0;
        assert_eq!(
            unsafe { phx_list_contains(list, &val as *const f64 as *const u8, 8, 1) },
            1,
            "-0.0 should equal 0.0 with float comparison"
        );
        // Without float flag, byte comparison would fail.
        assert_eq!(
            unsafe { phx_list_contains(list, &val as *const f64 as *const u8, 8, 0) },
            0,
            "-0.0 and 0.0 have different byte representations"
        );
    }

    #[test]
    fn list_contains_float_nan() {
        // IEEE 754: NaN != NaN must hold with float comparison.
        let list = phx_list_alloc(8, 1);
        let data = unsafe { list.add(HEADER_SIZE) };
        unsafe {
            *(data as *mut f64) = f64::NAN;
        }
        let val: f64 = f64::NAN;
        assert_eq!(
            unsafe { phx_list_contains(list, &val as *const f64 as *const u8, 8, 1) },
            0,
            "NaN should not equal NaN with float comparison"
        );
    }

    #[test]
    fn list_take_drop() {
        let list = phx_list_alloc(8, 3);
        let data = unsafe { list.add(HEADER_SIZE) };
        unsafe {
            *(data as *mut i64) = 1;
            *((data as *mut i64).add(1)) = 2;
            *((data as *mut i64).add(2)) = 3;
        }

        let taken = unsafe { phx_list_take(list, 2) };
        assert_eq!(unsafe { phx_list_length(taken) }, 2);
        assert_eq!(unsafe { *(phx_list_get_raw(taken, 0) as *const i64) }, 1);
        assert_eq!(unsafe { *(phx_list_get_raw(taken, 1) as *const i64) }, 2);

        let dropped = unsafe { phx_list_drop(list, 1) };
        assert_eq!(unsafe { phx_list_length(dropped) }, 2);
        assert_eq!(unsafe { *(phx_list_get_raw(dropped, 0) as *const i64) }, 2);
        assert_eq!(unsafe { *(phx_list_get_raw(dropped, 1) as *const i64) }, 3);
    }

    /// Push used stored elem_size from empty list.
    /// An empty list allocated with elem_size 8 may store a placeholder elem_size.
    /// `phx_list_push_raw` must use the caller's concrete elem_size instead.
    #[test]
    fn push_to_empty_list() {
        let list = phx_list_alloc(8, 0);
        assert_eq!(unsafe { phx_list_length(list) }, 0);

        let val: i64 = 42;
        let new_list = unsafe { phx_list_push_raw(list, &val as *const i64 as *const u8, 8) };
        assert_eq!(unsafe { phx_list_length(new_list) }, 1);
        let elem_ptr = unsafe { phx_list_get_raw(new_list, 0) };
        assert_eq!(unsafe { *(elem_ptr as *const i64) }, 42);
    }

    #[test]
    fn list_contains_empty() {
        let list = phx_list_alloc(8, 0);
        let val: i64 = 1;
        assert_eq!(
            unsafe { phx_list_contains(list, &val as *const i64 as *const u8, 8, 0) },
            0
        );
    }

    #[test]
    fn list_take_zero() {
        let list = phx_list_alloc(8, 3);
        let data = unsafe { list.add(HEADER_SIZE) };
        unsafe {
            *(data as *mut i64) = 1;
            *((data as *mut i64).add(1)) = 2;
            *((data as *mut i64).add(2)) = 3;
        }
        let taken = unsafe { phx_list_take(list, 0) };
        assert_eq!(unsafe { phx_list_length(taken) }, 0);
    }

    #[test]
    fn list_drop_beyond_length() {
        let list = phx_list_alloc(8, 3);
        let data = unsafe { list.add(HEADER_SIZE) };
        unsafe {
            *(data as *mut i64) = 1;
            *((data as *mut i64).add(1)) = 2;
            *((data as *mut i64).add(2)) = 3;
        }
        let dropped = unsafe { phx_list_drop(list, 100) };
        assert_eq!(unsafe { phx_list_length(dropped) }, 0);
    }

    #[test]
    fn str_split_basic() {
        let s = "a,b,c";
        let sep = ",";
        let list = unsafe { phx_str_split(s.as_ptr(), s.len(), sep.as_ptr(), sep.len()) };
        assert_eq!(unsafe { phx_list_length(list) }, 3);
        // Check first element.
        let elem = unsafe { phx_list_get_raw(list, 0) };
        let ptr = unsafe { *(elem as *const i64) } as *const u8;
        let len = unsafe { *((elem as *const i64).add(1)) } as usize;
        let result = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len)) };
        assert_eq!(result, "a");
    }

    /// Helper: read the i-th string element from a split result list.
    unsafe fn read_split_elem(list: *const u8, index: i64) -> String {
        let elem = unsafe { phx_list_get_raw(list, index) };
        let ptr = unsafe { *(elem as *const i64) } as *const u8;
        let len = unsafe { *((elem as *const i64).add(1)) } as usize;
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len)) }.to_string()
    }

    /// Split empty string by "," should produce [""].
    #[test]
    fn str_split_empty_string() {
        let s = "";
        let sep = ",";
        let list = unsafe { phx_str_split(s.as_ptr(), s.len(), sep.as_ptr(), sep.len()) };
        assert_eq!(unsafe { phx_list_length(list) }, 1);
        assert_eq!(unsafe { read_split_elem(list, 0) }, "");
    }

    /// Split "abc" by "xyz" (no match) should produce ["abc"].
    #[test]
    fn str_split_no_match() {
        let s = "abc";
        let sep = "xyz";
        let list = unsafe { phx_str_split(s.as_ptr(), s.len(), sep.as_ptr(), sep.len()) };
        assert_eq!(unsafe { phx_list_length(list) }, 1);
        assert_eq!(unsafe { read_split_elem(list, 0) }, "abc");
    }

    /// Split ",a,b," by "," (separator at start and end) should produce ["", "a", "b", ""].
    #[test]
    fn str_split_separator_at_boundaries() {
        let s = ",a,b,";
        let sep = ",";
        let list = unsafe { phx_str_split(s.as_ptr(), s.len(), sep.as_ptr(), sep.len()) };
        assert_eq!(unsafe { phx_list_length(list) }, 4);
        assert_eq!(unsafe { read_split_elem(list, 0) }, "");
        assert_eq!(unsafe { read_split_elem(list, 1) }, "a");
        assert_eq!(unsafe { read_split_elem(list, 2) }, "b");
        assert_eq!(unsafe { read_split_elem(list, 3) }, "");
    }
}
