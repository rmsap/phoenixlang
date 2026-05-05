//! Runtime library for compiled Phoenix programs.
//!
//! This crate is compiled as a static library (`libphoenix_runtime.a`) and
//! linked into every native Phoenix binary.  It provides the built-in
//! functions that compiled code calls via `extern` declarations.
//!
//! Heap allocations are managed by a tracing mark-and-sweep GC.
//! Compiled code allocates via [`phx_alloc`] (or the typed
//! variants [`gc::phx_gc_alloc`] / `phx_string_alloc`) and is responsible
//! for maintaining the shadow stack via [`gc::phx_gc_push_frame`] /
//! [`gc::phx_gc_set_root`] / [`gc::phx_gc_pop_frame`] so the collector
//! can find live roots.  See `docs/design-decisions.md#gc-implementation`
//! for the full rationale.
#![warn(missing_docs)]

/// Garbage collector: mark-and-sweep heap, shadow stack, and the C-ABI
/// hooks (`phx_gc_alloc`, `phx_gc_push_frame`, ...) called by compiled code.
pub mod gc;

mod list_methods;
mod map_methods;
mod string_methods;

/// Internal re-exports for the crate's integration tests. **Not**
/// stable Rust API: these symbols already exist on the C ABI side
/// (via `#[unsafe(no_mangle)] pub extern "C"`), and the only reason
/// to route them through Rust paths is so `tests/` can drive them
/// without declaring its own `extern "C"` block. External consumers
/// must continue to link against the C symbols.
///
/// `#[doc(hidden)]` keeps the module out of rustdoc; the `__test_support`
/// name (double underscore) keeps it out of the casual import surface
/// and signals "internal" to anyone tempted to depend on it. If a
/// symbol here ever becomes public Rust API, give it its own top-level
/// `pub use` with a real docstring.
#[doc(hidden)]
pub mod __test_support {
    pub use crate::list_methods::{
        phx_list_alloc, phx_list_drop, phx_list_get_raw, phx_list_length, phx_list_take,
        phx_str_split,
    };
    pub use crate::phx_str_concat;

    /// Test-only wrapper around the private `to_phx_string_from_str`.
    /// Kept here (rather than promoting the helper to crate-public)
    /// so the GC-managed string constructor stays out of the steady-
    /// state Rust API surface.
    pub fn to_phx_string_from_str(s: &str) -> crate::PhxFatPtr {
        crate::to_phx_string_from_str(s)
    }
}

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

/// Abort the process from inside an `extern "C"` function.
///
/// **Why:** the workspace's default panic strategy is `unwind`, and
/// unwinding across an `extern "C"` boundary is undefined behavior.
/// Any condition reachable from compiled-Phoenix code that we want to
/// fail loudly on must terminate via `process::exit` rather than
/// `panic!`/`expect`/`assert!`. Mirrors the [`phx_panic`] convention so
/// runtime aborts share a "runtime error: ..." prefix.
#[cold]
#[inline(never)]
pub(crate) fn runtime_abort(msg: &str) -> ! {
    eprintln!("runtime error: {msg}");
    process::exit(1);
}

// ── String operations ───────────────────────────────────────────────

/// Concatenate two strings, returning a new heap-allocated (ptr, len).
///
/// # Safety
///
/// - Both `(p1, l1)` and `(p2, l2)` must be valid UTF-8 byte slices.
/// - **`p1` and `p2` must be rooted by the caller's shadow-stack frame
///   for the duration of this call.** [`phx_string_alloc`] below can
///   trigger an auto-collect; the input bytes are read by the
///   subsequent `copy_nonoverlapping`, so an unrooted GC-managed input
///   would be swept between the alloc and the copy and the copy would
///   read freed memory. Cranelift-emitted callers root ref-typed
///   parameters automatically; a hand-written Rust caller passing
///   GC-managed strings must push a frame first.
///
/// # Behavior change in Phase 2.3
///
/// Pre-2.3 used `l1 + l2` and relied on `usize` wrapping: if the sum
/// wrapped to 0 (`l1 == usize::MAX`, `l2 == 1`) the empty-string fast
/// path returned a valid result on bogus input. Post-2.3 detects the
/// overflow with `checked_add` and `runtime_abort`s. No real caller can
/// trip this — `usize::MAX`-byte strings are unrepresentable in any
/// process's address space — but the input shape is no longer silently
/// rounded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_str_concat(
    p1: *const u8,
    l1: usize,
    p2: *const u8,
    l2: usize,
) -> PhxFatPtr {
    // Sum the lengths up front so an overflowing pair aborts before
    // we touch the inputs. Lengths above `isize::MAX` (but not
    // overflowing the sum) are caught one layer down by
    // `Layout::from_size_align` inside `phx_string_alloc`, which
    // routes the same way through `runtime_abort`.
    let Some(total) = l1.checked_add(l2) else {
        runtime_abort("string concat length overflow");
    };
    if total == 0 {
        return empty_phx_str();
    }
    let dest = phx_string_alloc(total);
    unsafe {
        std::ptr::copy_nonoverlapping(p1, dest, l1);
        std::ptr::copy_nonoverlapping(p2, dest.add(l1), l2);
    }
    PhxFatPtr {
        ptr: dest,
        len: total,
    }
}

/// Convert an integer to a heap-allocated string.
#[unsafe(no_mangle)]
pub extern "C" fn phx_i64_to_str(val: i64) -> PhxFatPtr {
    to_phx_string_from_str(&val.to_string())
}

/// Convert a float to a heap-allocated string.
#[unsafe(no_mangle)]
pub extern "C" fn phx_f64_to_str(val: f64) -> PhxFatPtr {
    to_phx_string_from_str(&format_f64(val))
}

/// Convert a bool to a heap-allocated string.
#[unsafe(no_mangle)]
pub extern "C" fn phx_bool_to_str(val: i8) -> PhxFatPtr {
    to_phx_string_from_str(if val != 0 { "true" } else { "false" })
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

/// Allocate `size` bytes on the heap, registering the result with the GC.
///
/// Returns a pointer to the **payload**; the GC's 8-byte object header
/// sits immediately before it.  Payload memory is zeroed.  The returned
/// pointer is owned by the GC; do not call `free` on it.  It stays
/// valid until a collection cycle in which no shadow-stack frame holds
/// a reference to it.
///
/// The allocation is registered with the [`TypeTag::Unknown`](gc::TypeTag)
/// tag, meaning the GC will conservatively scan its payload for interior
/// pointers during the mark phase.  Use [`phx_string_alloc`] for string
/// data (whose payload should not be scanned), or [`gc::phx_gc_alloc`]
/// directly to specify a different tag.
///
/// # Semantic change in Phase 2.3
///
/// Pre-2.3, `phx_alloc` returned `malloc`-backed memory that lived
/// until the process exited.  Post-2.3, the returned pointer is GC-
/// managed: once `phx_gc_enable` runs (the generated C `main` calls
/// it before `phx_main`), an allocation that no shadow-stack frame
/// roots will be reclaimed at the next collection.  Any non-codegen
/// caller (an FFI consumer, a Rust runtime helper, an external test
/// linker) that previously relied on the leak-everything semantics
/// must now either push a shadow-stack frame for the lifetime of the
/// returned pointer, or call `gc::phx_gc_disable()` to opt back into
/// leak-only behavior.
#[unsafe(no_mangle)]
pub extern "C" fn phx_alloc(size: usize) -> *mut u8 {
    gc::phx_gc_alloc(size, gc::TypeTag::Unknown as u32)
}

/// Allocate `size` bytes for raw string data.
///
/// Like [`phx_alloc`], but tagged so the GC skips the interior scan
/// (UTF-8 bytes never look like valid heap pointers — scanning them
/// would just waste cycles and risk false retention from coincidental
/// byte patterns).
#[unsafe(no_mangle)]
pub extern "C" fn phx_string_alloc(size: usize) -> *mut u8 {
    gc::phx_gc_alloc(size, gc::TypeTag::String as u32)
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Fat pointer returned by string-producing runtime functions.
///
/// Must match the C ABI: two machine words returned in registers (rax, rdx).
///
/// # Ownership
///
/// The pointed-to byte data is allocated on the GC heap (see
/// [`to_phx_string_from_str`]).  The collector reclaims it once no
/// shadow-stack frame holds a reference.
#[repr(C)]
pub struct PhxFatPtr {
    /// Pointer to the UTF-8 byte data on the GC heap.
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

/// Convert a `&str` into a [`PhxFatPtr`] backed by GC memory.
///
/// Allocates a GC-managed buffer (tagged [`gc::TypeTag::String`] so its
/// contents are never scanned for interior pointers), copies the bytes
/// into it, and returns a fat pointer to the GC payload.
///
/// Empty inputs short-circuit to a process-static pointer (`b""`) — every
/// hot path that produces an empty string (concat with `""`, `trim` on
/// whitespace-only, `replace` with full overlap, `substring` with
/// `start == end`) thus avoids an allocator round-trip and a sweep slot.
pub(crate) fn to_phx_string_from_str(s: &str) -> PhxFatPtr {
    let len = s.len();
    if len == 0 {
        return empty_phx_str();
    }
    let dest = phx_string_alloc(len);
    unsafe {
        std::ptr::copy_nonoverlapping(s.as_ptr(), dest, len);
    }
    PhxFatPtr { ptr: dest, len }
}

/// Cheap zero-length [`PhxFatPtr`] backed by a `.rodata` byte.
///
/// The GC's `header_for_payload` doesn't recognize this pointer (it isn't
/// in the heap registry) so it's silently skipped during mark — exactly
/// what we want for a process-lifetime constant.  Read-only consumers
/// don't dereference past `len = 0` so the static is never observed.
pub(crate) fn empty_phx_str() -> PhxFatPtr {
    // Why this is safe to leave un-rooted: `b"".as_ptr()` returns a
    // pointer into the binary's `.rodata` section, which is never
    // registered in the heap header set — the conservative scan asks
    // `header_for_payload`, which returns None, so no false-positive
    // retention. With `len == 0`, no consumer ever dereferences the
    // pointer either.
    PhxFatPtr {
        ptr: b"".as_ptr(),
        len: 0,
    }
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
