//! Runtime library for compiled Phoenix programs.
//!
//! This crate is compiled as a static library (`libphoenix_runtime.a`) and
//! linked into every native Phoenix binary.  It provides the built-in
//! functions that compiled code calls via `extern` declarations.
//!
//! All memory allocated by runtime functions is currently leaked.
//! A garbage collector or explicit free mechanism will be added in
//! Phase 2.3.  Until then, compiled binaries are not suitable for
//! long-running processes.
#![warn(missing_docs)]

mod list_methods;
mod map_methods;
mod string_methods;

/// Return the list header size in bytes.
///
/// Exposed so the compiler crate can assert at test time that its
/// `LIST_HEADER` constant matches the runtime's layout.
pub fn list_header_size() -> usize {
    list_methods::HEADER_SIZE
}

/// Return the map header size in bytes.
///
/// Exposed so the compiler crate can assert at test time that its
/// `MAP_HEADER` constant matches the runtime's layout.
pub fn map_header_size() -> usize {
    map_methods::HEADER_SIZE
}

use std::io::Write;
use std::process;
use std::slice;

// ── Print functions ─────────────────────────────────────────────────

/// Print an integer followed by a newline.
#[unsafe(no_mangle)]
pub extern "C" fn phx_print_i64(val: i64) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "{val}");
}

/// Print a float followed by a newline, matching interpreter formatting.
#[unsafe(no_mangle)]
pub extern "C" fn phx_print_f64(val: f64) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "{}", format_f64(val));
}

/// Print a boolean (`true`/`false`) followed by a newline.
#[unsafe(no_mangle)]
pub extern "C" fn phx_print_bool(val: i8) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if val != 0 {
        let _ = writeln!(out, "true");
    } else {
        let _ = writeln!(out, "false");
    }
}

/// Print a string given as a (ptr, len) fat pointer.
///
/// # Safety
///
/// `ptr` must point to `len` valid UTF-8 bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_print_str(ptr: *const u8, len: usize) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let bytes = unsafe { slice::from_raw_parts(ptr, len) };
    let _ = out.write_all(bytes);
    let _ = writeln!(out);
}

// ── Panic / abort ───────────────────────────────────────────────────

/// Abort execution with an error message.
///
/// # Safety
///
/// `msg_ptr` must point to `msg_len` valid UTF-8 bytes.
#[cold]
#[inline(never)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_panic(msg_ptr: *const u8, msg_len: usize) {
    let msg = format_panic_message(unsafe { slice::from_raw_parts(msg_ptr, msg_len) });
    eprintln!("{msg}");
    process::exit(1);
}

/// Format a panic message from raw bytes, prefixed with `"runtime error: "`.
///
/// Invalid UTF-8 is replaced with `"<invalid UTF-8>"`.  This is
/// extracted as a separate function so the formatting logic can be
/// unit-tested without calling `process::exit`.
fn format_panic_message(bytes: &[u8]) -> String {
    let msg = std::str::from_utf8(bytes).unwrap_or("<invalid UTF-8>");
    format!("runtime error: {msg}")
}

// ── String operations ───────────────────────────────────────────────

/// Concatenate two strings, returning a new heap-allocated (ptr, len).
///
/// # Safety
///
/// Both `(p1, l1)` and `(p2, l2)` must be valid UTF-8 byte slices.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_str_concat(
    p1: *const u8,
    l1: usize,
    p2: *const u8,
    l2: usize,
) -> PhxFatPtr {
    let s1 = unsafe { slice::from_raw_parts(p1, l1) };
    let s2 = unsafe { slice::from_raw_parts(p2, l2) };
    if l1 + l2 == 0 {
        // Return a valid (non-dangling) empty string pointer.
        // Vec::with_capacity(0) doesn't allocate, so as_ptr() would dangle.
        //
        // NOTE: This returns a pointer into the binary's .rodata section.
        // When a GC is added (Phase 2.3), this pointer must NOT be freed
        // or reallocated. The GC will need to distinguish static pointers
        // from heap pointers (e.g., via a heap range check or tag bit).
        return PhxFatPtr {
            ptr: b"".as_ptr(),
            len: 0,
        };
    }
    let mut buf = Vec::with_capacity(l1 + l2);
    buf.extend_from_slice(s1);
    buf.extend_from_slice(s2);
    let len = buf.len();
    let ptr = buf.as_ptr();
    std::mem::forget(buf);
    PhxFatPtr { ptr, len }
}

/// Convert an integer to a heap-allocated string.
#[unsafe(no_mangle)]
pub extern "C" fn phx_i64_to_str(val: i64) -> PhxFatPtr {
    leak_string(val.to_string())
}

/// Convert a float to a heap-allocated string.
#[unsafe(no_mangle)]
pub extern "C" fn phx_f64_to_str(val: f64) -> PhxFatPtr {
    leak_string(format_f64(val))
}

/// Convert a bool to a heap-allocated string.
#[unsafe(no_mangle)]
pub extern "C" fn phx_bool_to_str(val: i8) -> PhxFatPtr {
    leak_string(if val != 0 { "true" } else { "false" }.to_string())
}

// ── String comparison ───────────────────────────────────────────────

/// Generate a string comparison function exported as `extern "C"`.
macro_rules! str_cmp_fn {
    ($name:ident, $op:tt) => {
        /// Compare two strings by byte slice.
        ///
        /// # Safety
        ///
        /// Both `(p1, l1)` and `(p2, l2)` must be valid byte slices.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            p1: *const u8, l1: usize,
            p2: *const u8, l2: usize,
        ) -> i8 {
            (unsafe { str_from(p1, l1) } $op unsafe { str_from(p2, l2) }) as i8
        }
    };
}

str_cmp_fn!(phx_str_eq, ==);
str_cmp_fn!(phx_str_ne, !=);
str_cmp_fn!(phx_str_lt, <);
str_cmp_fn!(phx_str_gt, >);
str_cmp_fn!(phx_str_le, <=);
str_cmp_fn!(phx_str_ge, >=);

// ── Heap allocation ─────────────────────────────────────────────────

/// Allocate `size` bytes on the heap.  Returns a pointer.
/// No GC — memory is leaked until Phase 2.3.
///
/// # Safety
///
/// The caller must ensure `size` is a valid allocation size.
#[unsafe(no_mangle)]
pub extern "C" fn phx_alloc(size: usize) -> *mut u8 {
    // Zero-size allocations are UB for the global allocator.
    // Bump to 8 so we always get a valid pointer.
    let actual_size = if size == 0 { 8 } else { size };
    let layout =
        std::alloc::Layout::from_size_align(actual_size, 8).expect("invalid allocation size");
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    if ptr.is_null() {
        eprintln!("runtime error: out of memory");
        process::exit(1);
    }
    debug_assert!(
        (ptr as usize).is_multiple_of(8),
        "phx_alloc returned unaligned pointer"
    );
    ptr
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Fat pointer returned by string-producing runtime functions.
///
/// Must match the C ABI: two machine words returned in registers (rax, rdx).
///
/// # Ownership
///
/// The pointed-to byte data is heap-allocated and **intentionally leaked**
/// (see [`leak_string`]).  There is currently no mechanism to free it.
/// A garbage collector will reclaim these allocations in Phase 2.3.
#[repr(C)]
pub struct PhxFatPtr {
    /// Pointer to the UTF-8 byte data (heap-allocated, leaked).
    pub ptr: *const u8,
    /// Length in bytes.
    pub len: usize,
}

/// Format a float matching interpreter semantics.
///
/// Whole-number floats that are finite and fit in an `i64` are printed
/// without a decimal point (e.g. `3.0` → `"3"`).  All other values use
/// Rust's default `f64` formatting (e.g. `3.14` → `"3.14"`,
/// `f64::NAN` → `"NaN"`).
fn format_f64(val: f64) -> String {
    if val.fract() == 0.0 && val.is_finite() && val >= i64::MIN as f64 && val < i64::MAX as f64 {
        (val as i64).to_string()
    } else {
        val.to_string()
    }
}

/// Convert a `String` into a [`PhxFatPtr`] by leaking its allocation.
///
/// The string's backing memory is deliberately not freed — a GC will
/// reclaim it in Phase 2.3.  Until then, every call to this function
/// leaks `s.len()` bytes.
pub(crate) fn leak_string(s: String) -> PhxFatPtr {
    let len = s.len();
    let ptr = s.as_ptr();
    std::mem::forget(s);
    PhxFatPtr { ptr, len }
}

/// Reconstruct a byte slice from a raw pointer and length.
///
/// # Safety
///
/// - `ptr` must point to at least `len` valid bytes.
/// - The caller must not hold the returned reference beyond the immediate
///   expression — the lifetime `'a` is unconstrained and the compiler
///   cannot verify it.  All current call sites use the reference
///   transiently within a single comparison expression.
unsafe fn str_from<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    unsafe { slice::from_raw_parts(ptr, len) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── String concat ──────────────────────────────────────────────

    #[test]
    fn str_concat_normal() {
        let a = b"hello ";
        let b = b"world";
        let result = unsafe { phx_str_concat(a.as_ptr(), a.len(), b.as_ptr(), b.len()) };
        let s = unsafe { slice::from_raw_parts(result.ptr, result.len) };
        assert_eq!(s, b"hello world");
    }

    #[test]
    fn str_concat_empty_left() {
        let a = b"";
        let b = b"world";
        let result = unsafe { phx_str_concat(a.as_ptr(), a.len(), b.as_ptr(), b.len()) };
        let s = unsafe { slice::from_raw_parts(result.ptr, result.len) };
        assert_eq!(s, b"world");
    }

    #[test]
    fn str_concat_empty_right() {
        let a = b"hello";
        let b = b"";
        let result = unsafe { phx_str_concat(a.as_ptr(), a.len(), b.as_ptr(), b.len()) };
        let s = unsafe { slice::from_raw_parts(result.ptr, result.len) };
        assert_eq!(s, b"hello");
    }

    #[test]
    fn str_concat_both_empty() {
        let a = b"";
        let b = b"";
        let result = unsafe { phx_str_concat(a.as_ptr(), a.len(), b.as_ptr(), b.len()) };
        assert_eq!(result.len, 0);
    }

    /// Concatenating two empty strings must return a
    /// valid (non-null) pointer, not a dangling or null one.
    #[test]
    fn str_concat_both_empty_returns_valid_ptr() {
        let a = b"";
        let b_str = b"";
        let result = unsafe { phx_str_concat(a.as_ptr(), a.len(), b_str.as_ptr(), b_str.len()) };
        assert_eq!(result.len, 0);
        assert!(!result.ptr.is_null(), "empty concat should not return null");
    }

    // ── toString functions ─────────────────────────────────────────

    #[test]
    fn i64_to_str_positive() {
        let result = phx_i64_to_str(42);
        let s = unsafe { slice::from_raw_parts(result.ptr, result.len) };
        assert_eq!(s, b"42");
    }

    #[test]
    fn i64_to_str_negative() {
        let result = phx_i64_to_str(-7);
        let s = unsafe { slice::from_raw_parts(result.ptr, result.len) };
        assert_eq!(s, b"-7");
    }

    #[test]
    fn i64_to_str_zero() {
        let result = phx_i64_to_str(0);
        let s = unsafe { slice::from_raw_parts(result.ptr, result.len) };
        assert_eq!(s, b"0");
    }

    #[test]
    fn f64_to_str_whole() {
        let result = phx_f64_to_str(42.0);
        let s = unsafe { slice::from_raw_parts(result.ptr, result.len) };
        assert_eq!(s, b"42");
    }

    #[test]
    fn f64_to_str_fractional() {
        let result = phx_f64_to_str(3.125);
        let s = unsafe { slice::from_raw_parts(result.ptr, result.len) };
        assert_eq!(s, b"3.125");
    }

    #[test]
    fn bool_to_str_true() {
        let result = phx_bool_to_str(1);
        let s = unsafe { slice::from_raw_parts(result.ptr, result.len) };
        assert_eq!(s, b"true");
    }

    #[test]
    fn bool_to_str_false() {
        let result = phx_bool_to_str(0);
        let s = unsafe { slice::from_raw_parts(result.ptr, result.len) };
        assert_eq!(s, b"false");
    }

    // ── String comparison ──────────────────────────────────────────

    #[test]
    fn str_cmp_eq() {
        let a = b"hello";
        let b = b"hello";
        assert_eq!(
            unsafe { phx_str_eq(a.as_ptr(), a.len(), b.as_ptr(), b.len()) },
            1
        );
        let c = b"world";
        assert_eq!(
            unsafe { phx_str_eq(a.as_ptr(), a.len(), c.as_ptr(), c.len()) },
            0
        );
    }

    #[test]
    fn str_cmp_ne() {
        let a = b"hello";
        let b = b"world";
        assert_eq!(
            unsafe { phx_str_ne(a.as_ptr(), a.len(), b.as_ptr(), b.len()) },
            1
        );
        assert_eq!(
            unsafe { phx_str_ne(a.as_ptr(), a.len(), a.as_ptr(), a.len()) },
            0
        );
    }

    #[test]
    fn str_cmp_lt() {
        let a = b"abc";
        let b = b"def";
        assert_eq!(
            unsafe { phx_str_lt(a.as_ptr(), a.len(), b.as_ptr(), b.len()) },
            1
        );
        assert_eq!(
            unsafe { phx_str_lt(b.as_ptr(), b.len(), a.as_ptr(), a.len()) },
            0
        );
    }

    #[test]
    fn str_cmp_gt() {
        let a = b"xyz";
        let b = b"abc";
        assert_eq!(
            unsafe { phx_str_gt(a.as_ptr(), a.len(), b.as_ptr(), b.len()) },
            1
        );
        assert_eq!(
            unsafe { phx_str_gt(b.as_ptr(), b.len(), a.as_ptr(), a.len()) },
            0
        );
    }

    #[test]
    fn str_cmp_le() {
        let a = b"abc";
        assert_eq!(
            unsafe { phx_str_le(a.as_ptr(), a.len(), a.as_ptr(), a.len()) },
            1
        );
        let b = b"def";
        assert_eq!(
            unsafe { phx_str_le(a.as_ptr(), a.len(), b.as_ptr(), b.len()) },
            1
        );
        assert_eq!(
            unsafe { phx_str_le(b.as_ptr(), b.len(), a.as_ptr(), a.len()) },
            0
        );
    }

    #[test]
    fn str_cmp_ge() {
        let a = b"abc";
        assert_eq!(
            unsafe { phx_str_ge(a.as_ptr(), a.len(), a.as_ptr(), a.len()) },
            1
        );
        let b = b"def";
        assert_eq!(
            unsafe { phx_str_ge(b.as_ptr(), b.len(), a.as_ptr(), a.len()) },
            1
        );
        assert_eq!(
            unsafe { phx_str_ge(a.as_ptr(), a.len(), b.as_ptr(), b.len()) },
            0
        );
    }

    #[test]
    fn str_cmp_different_lengths() {
        let a = b"abc";
        let b = b"abcd";
        assert_eq!(
            unsafe { phx_str_lt(a.as_ptr(), a.len(), b.as_ptr(), b.len()) },
            1
        );
        assert_eq!(
            unsafe { phx_str_eq(a.as_ptr(), a.len(), b.as_ptr(), b.len()) },
            0
        );
    }

    // ── f64 edge cases ─────────────────────────────────────────────

    #[test]
    fn f64_to_str_nan() {
        let result = phx_f64_to_str(f64::NAN);
        let s = unsafe { slice::from_raw_parts(result.ptr, result.len) };
        assert_eq!(s, b"NaN");
    }

    #[test]
    fn f64_to_str_infinity() {
        let result = phx_f64_to_str(f64::INFINITY);
        let s = unsafe { slice::from_raw_parts(result.ptr, result.len) };
        assert_eq!(s, b"inf");
    }

    #[test]
    fn f64_to_str_neg_infinity() {
        let result = phx_f64_to_str(f64::NEG_INFINITY);
        let s = unsafe { slice::from_raw_parts(result.ptr, result.len) };
        assert_eq!(s, b"-inf");
    }

    #[test]
    fn f64_to_str_large_value() {
        // Value that is finite with no fractional part but exceeds i64 range.
        let result = phx_f64_to_str(1e19);
        let s = unsafe { slice::from_raw_parts(result.ptr, result.len) };
        // Should use float formatting, not i64 cast.
        let s_str = std::str::from_utf8(s).unwrap();
        assert!(s_str.contains("e") || s_str.contains("10000000000000000000"));
    }

    /// `i64::MAX as f64` rounds up to 2^63 which
    /// overflows when cast back to i64.  `format_f64` must use float
    /// formatting for this value, not integer formatting.
    #[test]
    fn f64_to_str_i64_max_boundary() {
        // i64::MAX as f64 rounds up to 9223372036854775808.0 (2^63),
        // which is i64::MAX + 1.  Casting this to i64 is UB / overflow.
        let val = i64::MAX as f64;
        let result = format_f64(val);
        // Must use float formatting (contains 'e' or the full digit string),
        // NOT integer formatting (which would require a val-as-i64 cast).
        let parsed: f64 = result.parse().expect("must be a valid float string");
        assert_eq!(parsed, val);
        // Verify we didn't take the integer path by checking the string
        // is NOT what (val as i64).to_string() would produce (which wraps
        // to i64::MIN = -9223372036854775808).
        assert!(
            !result.starts_with('-'),
            "format_f64({val}) = \"{result}\" — should not be negative"
        );
    }

    /// Values just below i64::MAX should still use integer
    /// formatting (the fix must not break the normal path).
    #[test]
    fn f64_to_str_just_below_i64_max() {
        // The largest f64 that is strictly less than i64::MAX as f64.
        // This is 9223372036854774784.0, which fits in i64.
        let val = 9223372036854774784.0_f64;
        assert!(val < i64::MAX as f64);
        let result = format_f64(val);
        // Should use integer formatting.
        let expected = (val as i64).to_string();
        assert_eq!(result, expected);
    }

    // ── Allocation ─────────────────────────────────────────────────

    /// phx_alloc(0) must not trigger UB.
    #[test]
    fn alloc_zero_size() {
        let ptr = phx_alloc(0);
        assert!(!ptr.is_null());
    }

    #[test]
    fn alloc_normal() {
        let ptr = phx_alloc(64);
        assert!(!ptr.is_null());
        // Verify memory is zeroed.
        let slice = unsafe { slice::from_raw_parts(ptr, 64) };
        assert!(slice.iter().all(|&b| b == 0));
    }

    // ── Print functions ───────────────────────────────────────────

    #[test]
    fn print_i64_does_not_panic() {
        // We cannot easily capture stdout in a unit test, but we can
        // verify the function does not panic for representative inputs.
        phx_print_i64(0);
        phx_print_i64(i64::MAX);
        phx_print_i64(i64::MIN);
    }

    #[test]
    fn print_f64_does_not_panic() {
        phx_print_f64(0.0);
        phx_print_f64(f64::NAN);
        phx_print_f64(f64::INFINITY);
        phx_print_f64(f64::NEG_INFINITY);
        phx_print_f64(1.23);
    }

    #[test]
    fn print_bool_does_not_panic() {
        phx_print_bool(0);
        phx_print_bool(1);
        phx_print_bool(42); // non-zero treated as true
    }

    #[test]
    fn print_str_does_not_panic() {
        let s = b"hello";
        unsafe { phx_print_str(s.as_ptr(), s.len()) };
        // Empty string.
        unsafe { phx_print_str(b"".as_ptr(), 0) };
    }

    // ── Panic formatting ──────────────────────────────────────────

    #[test]
    fn panic_message_valid_utf8() {
        let msg = format_panic_message(b"division by zero");
        assert_eq!(msg, "runtime error: division by zero");
    }

    #[test]
    fn panic_message_empty() {
        let msg = format_panic_message(b"");
        assert_eq!(msg, "runtime error: ");
    }

    #[test]
    fn panic_message_invalid_utf8() {
        let msg = format_panic_message(&[0xFF, 0xFE]);
        assert_eq!(msg, "runtime error: <invalid UTF-8>");
    }
}
