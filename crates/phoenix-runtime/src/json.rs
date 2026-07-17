//! JSON DOM parsing + navigation for `json.decode`.
//!
//! [`phx_json_parse`] parses input bytes into a boxed `serde_json::Value`
//! tree (or a captured parse-error message) and returns an opaque handle.
//! The synthesized per-type decoders navigate that DOM through the nav
//! primitives below, then the dispatch frees the root with
//! [`phx_json_free`].
//!
//! Handles are `i64` so the ABI is identical on 64-bit native and 32-bit
//! wasm — a pointer zero-extends into the low bits and truncates back
//! losslessly. `0` is the null handle (a missing object field / the root of
//! a failed parse). Child node handles borrow into the root's tree, so only
//! the root returned by `phx_json_parse` is ever freed.

use serde_json::Value;

use crate::{PhxFatPtr, str_from, to_phx_string_from_str};

/// Node-kind tags returned by [`phx_json_kind`]. This is an ABI shared with
/// the synthesized decoders (`phoenix-ir`'s `json_synth`) and the IR
/// interpreter's `json` builtins — keep the three in lockstep.
pub const JSON_KIND_NULL: i64 = 0;
/// A JSON boolean.
pub const JSON_KIND_BOOL: i64 = 1;
/// A JSON number representable as an `i64`.
pub const JSON_KIND_INT: i64 = 2;
/// A JSON number not representable as an `i64` (has a fraction / too large).
pub const JSON_KIND_FLOAT: i64 = 3;
/// A JSON string.
pub const JSON_KIND_STRING: i64 = 4;
/// A JSON array.
pub const JSON_KIND_ARRAY: i64 = 5;
/// A JSON object.
pub const JSON_KIND_OBJECT: i64 = 6;

/// A parsed JSON DOM root: the tree, or a captured parse-error message.
enum Root {
    Parsed(Value),
    ParseError(String),
}

/// Borrow a node handle as a `serde_json::Value`.
///
/// Node handles originate only from [`phx_json_root`] (and, in later slices,
/// child-navigation primitives) — never from a *root* handle, which points at
/// a [`Root`], not a [`Value`]. The synthesized decoders always call
/// `phx_json_root` before any node primitive, upholding this.
///
/// # Safety
/// `node` must be a live node handle previously returned by [`phx_json_root`]
/// (or another nav primitive) for a still-unfreed root.
unsafe fn node<'a>(node: i64) -> &'a Value {
    unsafe { &*(node as *const Value) }
}

/// Parse JSON bytes into a boxed DOM root, returning an opaque handle.
/// Invalid UTF-8 or malformed JSON is captured as a [`Root::ParseError`], not
/// a precondition — `serde_json` validates the bytes and surfaces the failure
/// through [`phx_json_parse_failed`].
///
/// # Safety
/// `(ptr, len)` must describe a readable byte range valid for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_json_parse(ptr: *const u8, len: usize) -> i64 {
    let root = match serde_json::from_slice::<Value>(unsafe { str_from(ptr, len) }) {
        Ok(v) => Root::Parsed(v),
        Err(e) => Root::ParseError(e.to_string()),
    };
    Box::into_raw(Box::new(root)) as i64
}

/// Free a DOM root handle (returned by [`phx_json_parse`]).
///
/// # Safety
/// `root` must be a live root handle, freed at most once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_json_free(root: i64) {
    if root != 0 {
        drop(unsafe { Box::from_raw(root as *mut Root) });
    }
}

/// `1` if the root is a parse error, else `0`.
///
/// # Safety
/// `root` must be a live root handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_json_parse_failed(root: i64) -> i8 {
    matches!(unsafe { &*(root as *const Root) }, Root::ParseError(_)) as i8
}

/// The parse-error message (empty string when the root parsed cleanly).
///
/// # Safety
/// `root` must be a live root handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_json_parse_error(root: i64) -> PhxFatPtr {
    match unsafe { &*(root as *const Root) } {
        Root::ParseError(m) => to_phx_string_from_str(m),
        Root::Parsed(_) => to_phx_string_from_str(""),
    }
}

/// The root's `Value` node handle for navigation (`0` if the parse failed).
///
/// # Safety
/// `root` must be a live root handle, unfreed for the returned handle's use.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_json_root(root: i64) -> i64 {
    match unsafe { &*(root as *const Root) } {
        Root::Parsed(v) => v as *const Value as i64,
        Root::ParseError(_) => 0,
    }
}

/// The node's kind tag (see the `JSON_KIND_*` constants).
///
/// # Safety
/// `handle` must be a live node handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_json_kind(handle: i64) -> i64 {
    match unsafe { node(handle) } {
        Value::Null => JSON_KIND_NULL,
        Value::Bool(_) => JSON_KIND_BOOL,
        Value::Number(n) if n.is_i64() => JSON_KIND_INT,
        Value::Number(_) => JSON_KIND_FLOAT,
        Value::String(_) => JSON_KIND_STRING,
        Value::Array(_) => JSON_KIND_ARRAY,
        Value::Object(_) => JSON_KIND_OBJECT,
    }
}

/// Look up an object field by key, returning the child node handle (or `0`
/// when `handle` is not an object or has no such key — the "missing" handle,
/// tested by [`phx_json_is_missing`]).
///
/// # Safety
/// `handle` must be a live node handle and `(key_ptr, key_len)` a valid UTF-8
/// slice.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_json_get_field(
    handle: i64,
    key_ptr: *const u8,
    key_len: usize,
) -> i64 {
    let key = unsafe { std::str::from_utf8_unchecked(str_from(key_ptr, key_len)) };
    match unsafe { node(handle) }.get(key) {
        Some(child) => child as *const Value as i64,
        None => 0,
    }
}

/// `1` if `handle` is the missing-field sentinel (`0`), else `0`.
///
/// # Safety
/// Pure integer comparison — always safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_json_is_missing(handle: i64) -> i8 {
    (handle == 0) as i8
}

/// Index an array node, returning the element handle (or `0`/"missing" when
/// `handle` is not an array or `index` is out of bounds — tested by
/// [`phx_json_is_missing`]).
///
/// # Safety
/// `handle` must be a live node handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_json_array_get(handle: i64, index: i64) -> i64 {
    match unsafe { node(handle) }.get(index as usize) {
        Some(elem) => elem as *const Value as i64,
        None => 0,
    }
}

/// The length of an array node (`0` when `handle` is not an array). The
/// caller confirms kind `ARRAY` before iterating with [`phx_json_array_get`].
///
/// # Safety
/// `handle` must be a live node handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_json_array_len(handle: i64) -> i64 {
    match unsafe { node(handle) } {
        Value::Array(a) => a.len() as i64,
        _ => 0,
    }
}

/// Extract a node as an `i64` (caller has confirmed kind `INT`).
///
/// # Safety
/// `handle` must be a live node handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_json_as_int(handle: i64) -> i64 {
    unsafe { node(handle) }.as_i64().unwrap_or(0)
}

/// Extract a number node as an `f64` (kind `INT` or `FLOAT`).
///
/// # Safety
/// `handle` must be a live node handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_json_as_float(handle: i64) -> f64 {
    unsafe { node(handle) }.as_f64().unwrap_or(0.0)
}

/// Extract a boolean node as an `i8` (`0`/`1`).
///
/// # Safety
/// `handle` must be a live node handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_json_as_bool(handle: i64) -> i8 {
    unsafe { node(handle) }.as_bool().unwrap_or(false) as i8
}

/// Extract a string node as a GC-managed string (copies the bytes out, so
/// the result outlives the DOM).
///
/// # Safety
/// `handle` must be a live node handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_json_as_str(handle: i64) -> PhxFatPtr {
    to_phx_string_from_str(unsafe { node(handle) }.as_str().unwrap_or(""))
}
