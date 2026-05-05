//! String method runtime functions for compiled Phoenix programs.
//!
//! Each function implements a Phoenix `String` builtin method
//! (e.g. `length`, `contains`, `trim`).
//!
//! **Note:** The `split` method is implemented in [`crate::list_methods`]
//! as `phx_str_split` because it returns a `List<String>` and depends on
//! the list allocation infrastructure.
//!
//! All functions that return strings operate on **Unicode scalar values**
//! (Rust `char`), not grapheme clusters.  For example, `length` returns
//! the number of `char`s, and `indexOf` returns a `char`-based index.

use std::slice;

use crate::{PhxFatPtr, to_phx_string_from_str};

// Why all string-producing methods route through `to_phx_string_from_str`
// (and not a `to_phx_string(String)` variant): the helper takes `&str`
// and copies bytes into a fresh GC allocation, with an empty-string
// fast path. Methods like `to_lowercase`/`to_uppercase`/`replace` build
// their result as an owned `String` first; passing `&owned` makes the
// GC-vs-owned distinction explicit at every call site rather than
// hiding it behind two near-identical conversion fns. The owned `String`
// is dropped normally once the call returns.
//
// ## Rooting contract for the input pointer
//
// `to_phx_string_from_str` allocates, which can trigger a GC cycle.
// Each transform here falls into one of two shapes:
//
// - **Owned-result shape** (`to_lower`, `to_upper`, `replace`,
//   `substring`): the transform materializes an owned Rust `String`
//   *before* the alloc. The owned bytes live on the Rust heap, so a
//   sweep mid-alloc cannot affect the result. **No caller-side
//   rooting required** for these.
// - **Borrowed-slice shape** (`trim`): the transform produces a
//   sub-slice of the input and the alloc reads from that slice. **The
//   input pointer must be rooted by the caller's shadow-stack frame
//   for the duration of the call** — a sweep mid-alloc would free the
//   input and the subsequent copy would read freed memory.
//
// The per-function `# Safety` blocks below state which shape they
// follow; this comment is the single explanation of *why* the two
// shapes differ.

// ── Helpers ─────────────────────────────────────────────────────────

/// Reconstruct a `&str` from a raw pointer and byte length.
///
/// # Safety
///
/// `ptr` must point to `len` valid UTF-8 bytes.  The returned reference
/// has an unconstrained lifetime — callers must not hold it beyond the
/// enclosing expression.
unsafe fn utf8_str<'a>(ptr: *const u8, len: usize) -> &'a str {
    unsafe { std::str::from_utf8_unchecked(slice::from_raw_parts(ptr, len)) }
}

// ── Method implementations ──────────────────────────────────────────

/// Return the number of Unicode scalar values (Rust `char`s) in a string.
///
/// # Safety
///
/// `ptr` must point to `len` valid UTF-8 bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_str_length(ptr: *const u8, len: usize) -> i64 {
    let s = unsafe { utf8_str(ptr, len) };
    s.chars().count() as i64
}

/// Return 1 if `(p1, l1)` contains substring `(p2, l2)`, 0 otherwise.
///
/// # Safety
///
/// Both `(p1, l1)` and `(p2, l2)` must be valid UTF-8 byte slices.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_str_contains(
    p1: *const u8,
    l1: usize,
    p2: *const u8,
    l2: usize,
) -> i8 {
    let s = unsafe { utf8_str(p1, l1) };
    let sub = unsafe { utf8_str(p2, l2) };
    s.contains(sub) as i8
}

/// Return 1 if `(p1, l1)` starts with prefix `(p2, l2)`, 0 otherwise.
///
/// # Safety
///
/// Both `(p1, l1)` and `(p2, l2)` must be valid UTF-8 byte slices.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_str_starts_with(
    p1: *const u8,
    l1: usize,
    p2: *const u8,
    l2: usize,
) -> i8 {
    let s = unsafe { utf8_str(p1, l1) };
    let pre = unsafe { utf8_str(p2, l2) };
    s.starts_with(pre) as i8
}

/// Return 1 if `(p1, l1)` ends with suffix `(p2, l2)`, 0 otherwise.
///
/// # Safety
///
/// Both `(p1, l1)` and `(p2, l2)` must be valid UTF-8 byte slices.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_str_ends_with(
    p1: *const u8,
    l1: usize,
    p2: *const u8,
    l2: usize,
) -> i8 {
    let s = unsafe { utf8_str(p1, l1) };
    let suf = unsafe { utf8_str(p2, l2) };
    s.ends_with(suf) as i8
}

/// Trim leading and trailing whitespace, returning a new heap-allocated string.
///
/// # Safety
///
/// - `ptr` must point to `len` valid UTF-8 bytes.
/// - **Borrowed-slice shape — input must be rooted.** `s.trim()`
///   returns a sub-slice of the input that is read by the subsequent
///   alloc-and-copy; an unrooted GC-managed input would be swept
///   between alloc and copy. See the *Rooting contract* in the module
///   header for the rationale. Cranelift-emitted callers root
///   ref-typed parameters automatically.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_str_trim(ptr: *const u8, len: usize) -> PhxFatPtr {
    let s = unsafe { utf8_str(ptr, len) };
    to_phx_string_from_str(s.trim())
}

/// Convert a string to lowercase, returning a new heap-allocated string.
///
/// # Safety
///
/// - `ptr` must point to `len` valid UTF-8 bytes.
/// - Owned-result shape — no caller-side rooting required (see the
///   *Rooting contract* in the module header).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_str_to_lower(ptr: *const u8, len: usize) -> PhxFatPtr {
    let s = unsafe { utf8_str(ptr, len) };
    to_phx_string_from_str(&s.to_lowercase())
}

/// Convert a string to uppercase, returning a new heap-allocated string.
///
/// # Safety
///
/// - `ptr` must point to `len` valid UTF-8 bytes.
/// - Owned-result shape — no caller-side rooting required (see the
///   *Rooting contract* in the module header).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_str_to_upper(ptr: *const u8, len: usize) -> PhxFatPtr {
    let s = unsafe { utf8_str(ptr, len) };
    to_phx_string_from_str(&s.to_uppercase())
}

/// Return the Unicode scalar value index of the first occurrence of
/// `(p2, l2)` in `(p1, l1)`, or -1 if not found.
///
/// The returned index counts `char`s (Unicode scalar values), not bytes
/// or grapheme clusters.  This matches the interpreter semantics.
///
/// # Safety
///
/// Both `(p1, l1)` and `(p2, l2)` must be valid UTF-8 byte slices.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_str_index_of(
    p1: *const u8,
    l1: usize,
    p2: *const u8,
    l2: usize,
) -> i64 {
    let s = unsafe { utf8_str(p1, l1) };
    let sub = unsafe { utf8_str(p2, l2) };
    s.find(sub)
        .map(|byte_offset| s[..byte_offset].chars().count() as i64)
        .unwrap_or(-1)
}

/// Replace all occurrences of `(p2, l2)` with `(p3, l3)` in `(p1, l1)`,
/// returning a new heap-allocated string.
///
/// # Safety
///
/// - All three `(ptr, len)` pairs must be valid UTF-8 byte slices.
/// - Owned-result shape — no caller-side rooting required (see the
///   *Rooting contract* in the module header).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_str_replace(
    p1: *const u8,
    l1: usize,
    p2: *const u8,
    l2: usize,
    p3: *const u8,
    l3: usize,
) -> PhxFatPtr {
    let s = unsafe { utf8_str(p1, l1) };
    let old = unsafe { utf8_str(p2, l2) };
    let replacement = unsafe { utf8_str(p3, l3) };
    to_phx_string_from_str(&s.replace(old, replacement))
}

/// Extract a substring from char index `start` to `end` (exclusive),
/// returning a new heap-allocated string. Indices are clamped to bounds.
///
/// # Safety
///
/// - `ptr` must point to `len` valid UTF-8 bytes.
/// - Owned-result shape — no caller-side rooting required (see the
///   *Rooting contract* in the module header).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_str_substring(
    ptr: *const u8,
    len: usize,
    start: i64,
    end: i64,
) -> PhxFatPtr {
    let s = unsafe { utf8_str(ptr, len) };
    let chars: Vec<char> = s.chars().collect();
    let char_len = chars.len();
    let start_u = (start.max(0) as usize).min(char_len);
    let end_u = (end.max(0) as usize).min(char_len);
    let end_u = end_u.max(start_u);
    let collected: String = chars[start_u..end_u].iter().collect();
    to_phx_string_from_str(&collected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::slice;

    /// Helper: call a string-producing runtime function and return the output as a `String`.
    fn fat_ptr_to_string(fp: PhxFatPtr) -> String {
        let bytes = unsafe { slice::from_raw_parts(fp.ptr, fp.len) };
        std::str::from_utf8(bytes).unwrap().to_string()
    }

    // ── length ────────────────────────────────────────────────────

    #[test]
    fn str_length_normal() {
        let s = "hello";
        assert_eq!(unsafe { phx_str_length(s.as_ptr(), s.len()) }, 5);
    }

    #[test]
    fn str_length_empty() {
        let s = "";
        assert_eq!(unsafe { phx_str_length(s.as_ptr(), s.len()) }, 0);
    }

    #[test]
    fn str_length_unicode() {
        let s = "h\u{00e9}llo"; // é is one char, two bytes
        assert_eq!(unsafe { phx_str_length(s.as_ptr(), s.len()) }, 5);
    }

    #[test]
    fn str_length_emoji() {
        let s = "\u{1F600}"; // 😀 — 1 char, 4 bytes
        assert_eq!(unsafe { phx_str_length(s.as_ptr(), s.len()) }, 1);
    }

    // ── contains ──────────────────────────────────────────────────

    #[test]
    fn str_contains_found() {
        let s = "hello world";
        let sub = "world";
        assert_eq!(
            unsafe { phx_str_contains(s.as_ptr(), s.len(), sub.as_ptr(), sub.len()) },
            1
        );
    }

    #[test]
    fn str_contains_not_found() {
        let s = "hello world";
        let sub = "xyz";
        assert_eq!(
            unsafe { phx_str_contains(s.as_ptr(), s.len(), sub.as_ptr(), sub.len()) },
            0
        );
    }

    #[test]
    fn str_contains_empty_needle() {
        let s = "hello";
        let sub = "";
        assert_eq!(
            unsafe { phx_str_contains(s.as_ptr(), s.len(), sub.as_ptr(), sub.len()) },
            1
        );
    }

    #[test]
    fn str_contains_empty_haystack() {
        let s = "";
        let sub = "a";
        assert_eq!(
            unsafe { phx_str_contains(s.as_ptr(), s.len(), sub.as_ptr(), sub.len()) },
            0
        );
    }

    #[test]
    fn str_contains_identity() {
        let s = "hello";
        let sub = "hello";
        assert_eq!(
            unsafe { phx_str_contains(s.as_ptr(), s.len(), sub.as_ptr(), sub.len()) },
            1
        );
    }

    // ── startsWith ────────────────────────────────────────────────

    #[test]
    fn str_starts_with_true() {
        let s = "hello world";
        let pre = "hello";
        assert_eq!(
            unsafe { phx_str_starts_with(s.as_ptr(), s.len(), pre.as_ptr(), pre.len()) },
            1
        );
    }

    #[test]
    fn str_starts_with_false() {
        let s = "hello world";
        let pre = "world";
        assert_eq!(
            unsafe { phx_str_starts_with(s.as_ptr(), s.len(), pre.as_ptr(), pre.len()) },
            0
        );
    }

    #[test]
    fn str_starts_with_empty_prefix() {
        let s = "hello";
        let pre = "";
        assert_eq!(
            unsafe { phx_str_starts_with(s.as_ptr(), s.len(), pre.as_ptr(), pre.len()) },
            1
        );
    }

    #[test]
    fn str_starts_with_empty_receiver() {
        let s = "";
        let pre = "a";
        assert_eq!(
            unsafe { phx_str_starts_with(s.as_ptr(), s.len(), pre.as_ptr(), pre.len()) },
            0
        );
    }

    // ── endsWith ──────────────────────────────────────────────────

    #[test]
    fn str_ends_with_true() {
        let s = "hello world";
        let suf = "world";
        assert_eq!(
            unsafe { phx_str_ends_with(s.as_ptr(), s.len(), suf.as_ptr(), suf.len()) },
            1
        );
    }

    #[test]
    fn str_ends_with_false() {
        let s = "hello world";
        let suf = "hello";
        assert_eq!(
            unsafe { phx_str_ends_with(s.as_ptr(), s.len(), suf.as_ptr(), suf.len()) },
            0
        );
    }

    #[test]
    fn str_ends_with_empty_suffix() {
        let s = "hello";
        let suf = "";
        assert_eq!(
            unsafe { phx_str_ends_with(s.as_ptr(), s.len(), suf.as_ptr(), suf.len()) },
            1
        );
    }

    #[test]
    fn str_ends_with_empty_receiver() {
        let s = "";
        let suf = "a";
        assert_eq!(
            unsafe { phx_str_ends_with(s.as_ptr(), s.len(), suf.as_ptr(), suf.len()) },
            0
        );
    }

    // ── trim ──────────────────────────────────────────────────────

    #[test]
    fn str_trim_whitespace() {
        let s = "  hello  ";
        assert_eq!(
            fat_ptr_to_string(unsafe { phx_str_trim(s.as_ptr(), s.len()) }),
            "hello"
        );
    }

    #[test]
    fn str_trim_no_whitespace() {
        let s = "hello";
        assert_eq!(
            fat_ptr_to_string(unsafe { phx_str_trim(s.as_ptr(), s.len()) }),
            "hello"
        );
    }

    #[test]
    fn str_trim_all_whitespace() {
        let s = "   ";
        let result = unsafe { phx_str_trim(s.as_ptr(), s.len()) };
        assert_eq!(result.len, 0);
    }

    #[test]
    fn str_trim_empty() {
        let s = "";
        let result = unsafe { phx_str_trim(s.as_ptr(), s.len()) };
        assert_eq!(result.len, 0);
    }

    #[test]
    fn str_trim_tabs_and_newlines() {
        let s = "\t\nhello\r\n";
        assert_eq!(
            fat_ptr_to_string(unsafe { phx_str_trim(s.as_ptr(), s.len()) }),
            "hello"
        );
    }

    #[test]
    fn str_trim_inner_whitespace_preserved() {
        let s = "a b c";
        assert_eq!(
            fat_ptr_to_string(unsafe { phx_str_trim(s.as_ptr(), s.len()) }),
            "a b c"
        );
    }

    // ── toLowerCase ───────────────────────────────────────────────

    #[test]
    fn str_to_lower_mixed() {
        let s = "Hello World";
        assert_eq!(
            fat_ptr_to_string(unsafe { phx_str_to_lower(s.as_ptr(), s.len()) }),
            "hello world"
        );
    }

    #[test]
    fn str_to_lower_already_lower() {
        let s = "hello";
        assert_eq!(
            fat_ptr_to_string(unsafe { phx_str_to_lower(s.as_ptr(), s.len()) }),
            "hello"
        );
    }

    #[test]
    fn str_to_lower_unicode() {
        let s = "\u{00dc}BER"; // ÜBER
        assert_eq!(
            fat_ptr_to_string(unsafe { phx_str_to_lower(s.as_ptr(), s.len()) }),
            "\u{00fc}ber" // über
        );
    }

    #[test]
    fn str_to_lower_empty() {
        let s = "";
        let result = unsafe { phx_str_to_lower(s.as_ptr(), s.len()) };
        assert_eq!(result.len, 0);
    }

    // ── toUpperCase ───────────────────────────────────────────────

    #[test]
    fn str_to_upper_mixed() {
        let s = "Hello World";
        assert_eq!(
            fat_ptr_to_string(unsafe { phx_str_to_upper(s.as_ptr(), s.len()) }),
            "HELLO WORLD"
        );
    }

    #[test]
    fn str_to_upper_already_upper() {
        let s = "HELLO";
        assert_eq!(
            fat_ptr_to_string(unsafe { phx_str_to_upper(s.as_ptr(), s.len()) }),
            "HELLO"
        );
    }

    #[test]
    fn str_to_upper_unicode() {
        let s = "\u{00fc}ber"; // über
        assert_eq!(
            fat_ptr_to_string(unsafe { phx_str_to_upper(s.as_ptr(), s.len()) }),
            "\u{00dc}BER" // ÜBER
        );
    }

    #[test]
    fn str_to_upper_empty() {
        let s = "";
        let result = unsafe { phx_str_to_upper(s.as_ptr(), s.len()) };
        assert_eq!(result.len, 0);
    }

    // ── indexOf ───────────────────────────────────────────────────

    #[test]
    fn str_index_of_found() {
        let s = "hello world";
        let sub = "world";
        assert_eq!(
            unsafe { phx_str_index_of(s.as_ptr(), s.len(), sub.as_ptr(), sub.len()) },
            6
        );
    }

    #[test]
    fn str_index_of_not_found() {
        let s = "hello world";
        let sub = "xyz";
        assert_eq!(
            unsafe { phx_str_index_of(s.as_ptr(), s.len(), sub.as_ptr(), sub.len()) },
            -1
        );
    }

    #[test]
    fn str_index_of_at_start() {
        let s = "hello";
        let sub = "hel";
        assert_eq!(
            unsafe { phx_str_index_of(s.as_ptr(), s.len(), sub.as_ptr(), sub.len()) },
            0
        );
    }

    #[test]
    fn str_index_of_empty_needle() {
        let s = "hello";
        let sub = "";
        assert_eq!(
            unsafe { phx_str_index_of(s.as_ptr(), s.len(), sub.as_ptr(), sub.len()) },
            0
        );
    }

    #[test]
    fn str_index_of_unicode() {
        let s = "h\u{00e9}llo"; // héllo — é is at byte 1..3, char index 1
        let sub = "llo";
        // "llo" starts at byte offset 3 but character index 2.
        assert_eq!(
            unsafe { phx_str_index_of(s.as_ptr(), s.len(), sub.as_ptr(), sub.len()) },
            2
        );
    }

    #[test]
    fn str_index_of_at_end() {
        let s = "hello";
        let sub = "lo";
        assert_eq!(
            unsafe { phx_str_index_of(s.as_ptr(), s.len(), sub.as_ptr(), sub.len()) },
            3
        );
    }

    #[test]
    fn str_index_of_empty_haystack_empty_needle() {
        let s = "";
        let sub = "";
        assert_eq!(
            unsafe { phx_str_index_of(s.as_ptr(), s.len(), sub.as_ptr(), sub.len()) },
            0
        );
    }

    #[test]
    fn str_index_of_full_match() {
        let s = "hello";
        let sub = "hello";
        assert_eq!(
            unsafe { phx_str_index_of(s.as_ptr(), s.len(), sub.as_ptr(), sub.len()) },
            0
        );
    }

    // ── replace ───────────────────────────────────────────────────

    fn call_replace(s: &str, from: &str, to: &str) -> String {
        fat_ptr_to_string(unsafe {
            phx_str_replace(
                s.as_ptr(),
                s.len(),
                from.as_ptr(),
                from.len(),
                to.as_ptr(),
                to.len(),
            )
        })
    }

    #[test]
    fn str_replace_single() {
        assert_eq!(
            call_replace("hello world", "world", "phoenix"),
            "hello phoenix"
        );
    }

    #[test]
    fn str_replace_multiple() {
        assert_eq!(call_replace("aaa", "a", "bb"), "bbbbbb");
    }

    #[test]
    fn str_replace_not_found() {
        assert_eq!(call_replace("hello", "xyz", "abc"), "hello");
    }

    #[test]
    fn str_replace_empty_from() {
        // Rust inserts between every character: "xaxbx".
        assert_eq!(call_replace("ab", "", "x"), "xaxbx");
    }

    #[test]
    fn str_replace_empty_to() {
        assert_eq!(call_replace("hello", "l", ""), "heo");
    }

    #[test]
    fn str_replace_unicode() {
        assert_eq!(call_replace("h\u{00e9}llo", "\u{00e9}", "e"), "hello");
    }

    #[test]
    fn str_replace_identity() {
        assert_eq!(call_replace("hello", "l", "l"), "hello");
    }

    // ── substring ─────────────────────────────────────────────────

    fn call_substring(s: &str, start: i64, end: i64) -> String {
        fat_ptr_to_string(unsafe { phx_str_substring(s.as_ptr(), s.len(), start, end) })
    }

    #[test]
    fn str_substring_normal() {
        assert_eq!(call_substring("hello world", 0, 5), "hello");
    }

    #[test]
    fn str_substring_middle() {
        assert_eq!(call_substring("hello world", 6, 11), "world");
    }

    #[test]
    fn str_substring_empty_range() {
        assert_eq!(call_substring("hello", 2, 2), "");
    }

    #[test]
    fn str_substring_full_range() {
        assert_eq!(call_substring("hello", 0, 5), "hello");
    }

    #[test]
    fn str_substring_negative_start_clamped() {
        // -3 is clamped to 0.
        assert_eq!(call_substring("hello", -3, 3), "hel");
    }

    #[test]
    fn str_substring_end_beyond_length_clamped() {
        // 100 is clamped to 5.
        assert_eq!(call_substring("hello", 3, 100), "lo");
    }

    #[test]
    fn str_substring_start_greater_than_end_clamped() {
        // end (2) is clamped up to start (4), giving an empty range.
        assert_eq!(call_substring("hello", 4, 2), "");
    }

    #[test]
    fn str_substring_unicode() {
        // héllo — é is 2 bytes but 1 char
        assert_eq!(call_substring("h\u{00e9}llo", 1, 3), "\u{00e9}l");
    }

    #[test]
    fn str_substring_both_negative() {
        // Both clamped to 0, end clamped up to start (both 0) → empty.
        assert_eq!(call_substring("hello", -5, -1), "");
    }
}
