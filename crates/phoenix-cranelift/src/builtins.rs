//! Declarations of runtime functions imported from `phoenix-runtime`.
//!
//! Each runtime function is declared as an imported function in the
//! Cranelift module so compiled code can call it.

use cranelift_codegen::ir::types::{F64, I8, I32, I64};
use cranelift_codegen::ir::{AbiParam, Signature};
use cranelift_codegen::isa::CallConv;
use cranelift_module::{FuncId, Linkage, Module};

use crate::error::CompileError;

/// All runtime functions that can be imported.
pub struct RuntimeFunctions {
    /// `phx_print_i64(i64)`.
    pub print_i64: FuncId,
    /// `phx_print_f64(f64)`.
    pub print_f64: FuncId,
    /// `phx_print_bool(i8)`.
    pub print_bool: FuncId,
    /// `phx_print_str(ptr, len)`.
    pub print_str: FuncId,
    /// `phx_panic(ptr, len)` — abort with message.
    pub panic: FuncId,
    /// `phx_str_concat(p1, l1, p2, l2) -> (ptr, len)`.
    pub str_concat: FuncId,
    /// `phx_i64_to_str(i64) -> (ptr, len)`.
    pub i64_to_str: FuncId,
    /// `phx_f64_to_str(f64) -> (ptr, len)`.
    pub f64_to_str: FuncId,
    /// `phx_bool_to_str(i8) -> (ptr, len)`.
    pub bool_to_str: FuncId,
    /// `phx_str_eq`.
    pub str_eq: FuncId,
    /// `phx_str_ne`.
    pub str_ne: FuncId,
    /// `phx_str_lt`.
    pub str_lt: FuncId,
    /// `phx_str_gt`.
    pub str_gt: FuncId,
    /// `phx_str_le`.
    pub str_le: FuncId,
    /// `phx_str_ge`.
    pub str_ge: FuncId,
    /// `phx_gc_alloc(size, tag) -> ptr`. Typed allocation; the tag is
    /// one of the constants in [`crate::type_tag`]. Used by every
    /// codegen-emitted allocation (struct, enum, closure-env). The
    /// runtime's own typed callers (`phx_list_alloc`, `phx_map_alloc`,
    /// `phx_string_alloc`) also bottom out in `phx_gc_alloc` with their
    /// corresponding tag.
    pub gc_alloc: FuncId,
    // ── ListBuilder / MapBuilder runtime (Phase 2.7 decision F) ─────
    /// `phx_list_builder_alloc(elem_size) -> handle_ptr`.
    pub list_builder_alloc: FuncId,
    /// `phx_list_builder_push(handle, elem_ptr, elem_size)`.
    pub list_builder_push: FuncId,
    /// `phx_list_builder_freeze(handle) -> list_ptr`.
    pub list_builder_freeze: FuncId,
    /// `phx_map_builder_alloc(key_size, val_size, key_is_string) -> handle_ptr`.
    pub map_builder_alloc: FuncId,
    /// `phx_map_builder_set(handle, key_ptr, val_ptr, key_size, val_size)`.
    pub map_builder_set: FuncId,
    /// `phx_map_builder_freeze(handle) -> map_ptr`.
    pub map_builder_freeze: FuncId,
    /// `phx_str_length(ptr, len) -> i64`.
    pub str_length: FuncId,
    /// `phx_str_contains(p1, l1, p2, l2) -> i8`.
    pub str_contains: FuncId,
    /// `phx_str_starts_with(p1, l1, p2, l2) -> i8`.
    pub str_starts_with: FuncId,
    /// `phx_str_ends_with(p1, l1, p2, l2) -> i8`.
    pub str_ends_with: FuncId,
    /// `phx_str_trim(ptr, len) -> (ptr, len)`.
    pub str_trim: FuncId,
    /// `phx_str_to_lower(ptr, len) -> (ptr, len)`.
    pub str_to_lower: FuncId,
    /// `phx_str_to_upper(ptr, len) -> (ptr, len)`.
    pub str_to_upper: FuncId,
    /// `phx_json_escape_str(ptr, len) -> (ptr, len).
    pub json_escape_str: FuncId,
    /// `phx_json_parse(ptr, len) -> i64` (root handle) — Phase 4.6.
    pub json_parse: FuncId,
    /// `phx_json_free(i64)`.
    pub json_free: FuncId,
    /// `phx_json_parse_failed(i64) -> i8`.
    pub json_parse_failed: FuncId,
    /// `phx_json_parse_error(i64) -> (ptr, len)`.
    pub json_parse_error: FuncId,
    /// `phx_json_root(i64) -> i64`.
    pub json_root: FuncId,
    /// `phx_json_kind(i64) -> i64`.
    pub json_kind: FuncId,
    /// `phx_json_as_int(i64) -> i64`.
    pub json_as_int: FuncId,
    /// `phx_json_as_float(i64) -> f64`.
    pub json_as_float: FuncId,
    /// `phx_json_as_bool(i64) -> i8`.
    pub json_as_bool: FuncId,
    /// `phx_json_as_str(i64) -> (ptr, len)`.
    pub json_as_str: FuncId,
    /// `phx_json_get_field(i64, ptr, len) -> i64` — child handle, or the
    /// missing-field sentinel (`0` in this runtime). The sentinel value is
    /// backend-private (the IR interpreter uses `-1`, since its arena index
    /// `0` is a valid handle): synthesized IR must only test a handle via
    /// `json.isMissing`, never against a literal.
    pub json_get_field: FuncId,
    /// `phx_json_is_missing(i64) -> i8`.
    pub json_is_missing: FuncId,
    /// `phx_json_array_get(i64, i64) -> i64` — element handle, or the
    /// missing-field sentinel when out of range / not an array.
    pub json_array_get: FuncId,
    /// `phx_json_array_len(i64) -> i64` — array length (`0` if not an array).
    pub json_array_len: FuncId,
    /// `phx_json_object_len(i64) -> i64` — object entry count (`0` if not an
    /// object).
    pub json_object_len: FuncId,
    /// `phx_json_object_key_at(i64, i64) -> (ptr, len)` — the i-th object key.
    pub json_object_key_at: FuncId,
    /// `phx_json_object_value_at(i64, i64) -> i64` — the i-th object value
    /// node handle (or the missing sentinel when out of range).
    pub json_object_value_at: FuncId,
    /// `phx_str_index_of(p1, l1, p2, l2) -> i64`.
    pub str_index_of: FuncId,
    /// `phx_str_replace(p1, l1, p2, l2, p3, l3) -> (ptr, len)`.
    pub str_replace: FuncId,
    /// `phx_str_substring(ptr, len, start, end) -> (ptr, len)`.
    pub str_substring: FuncId,
    /// `phx_str_split(ptr, len, sep_ptr, sep_len) -> list_ptr`.
    pub str_split: FuncId,
    // ── List runtime functions ──────────────────────────────────────
    /// `phx_list_alloc(elem_size, count) -> list_ptr`.
    pub list_alloc: FuncId,
    /// `phx_list_length(list_ptr) -> i64`.
    pub list_length: FuncId,
    /// `phx_list_get_raw(list_ptr, index) -> elem_ptr`.
    pub list_get_raw: FuncId,
    /// `phx_list_push_raw(list_ptr, elem_ptr, elem_size) -> new_list_ptr`.
    pub list_push_raw: FuncId,
    /// `phx_list_contains(list_ptr, elem_ptr, elem_size, is_float, is_string) -> i8`.
    pub list_contains: FuncId,
    /// `phx_list_take(list_ptr, n) -> new_list_ptr`.
    pub list_take: FuncId,
    /// `phx_list_drop(list_ptr, n) -> new_list_ptr`.
    pub list_drop: FuncId,
    // ── Map runtime functions ───────────────────────────────────────
    /// `phx_map_from_pairs(key_size, val_size, n_pairs, pair_data, key_is_string) -> map_ptr`.
    /// Used by the map-literal lowering: codegen writes each pair into a
    /// stack buffer and the runtime hash-builds the table in one pass.
    /// `key_is_string` is recorded in the map header so later lookups
    /// compare `String` keys by content (required on wasm32).
    /// Replaces the previous `phx_map_alloc`-then-write-pairs path; the
    /// runtime keeps `phx_map_alloc` as an internal helper for `set` /
    /// `remove`, but compiled code no longer needs to call it directly.
    pub map_from_pairs: FuncId,
    /// `phx_map_length(map_ptr) -> i64`.
    pub map_length: FuncId,
    /// `phx_map_get_raw(map_ptr, key_ptr, key_size) -> val_ptr_or_null`.
    pub map_get_raw: FuncId,
    /// `phx_map_set_raw(map_ptr, key_ptr, val_ptr, key_size, val_size,
    /// key_is_string) -> new_map_ptr`. `key_is_string` is consulted only
    /// on the placeholder/shape-change recovery branch; required on
    /// wasm32 where a `StringRef` key is 8 bytes and can't be told from
    /// `Int` / `Float` by size.
    pub map_set_raw: FuncId,
    /// `phx_map_remove_raw(map_ptr, key_ptr, key_size) -> new_map_ptr`.
    pub map_remove_raw: FuncId,
    /// `phx_map_contains(map_ptr, key_ptr, key_size) -> i8`.
    pub map_contains: FuncId,
    /// `phx_map_keys(map_ptr) -> list_ptr`.
    pub map_keys: FuncId,
    /// `phx_map_values(map_ptr) -> list_ptr`.
    pub map_values: FuncId,
    // ── GC functions ────────────────────────────────────────────────
    /// `phx_gc_push_frame(n_roots: i64) -> frame_ptr`.
    pub gc_push_frame: FuncId,
    /// `phx_gc_pop_frame(frame_ptr)`.
    pub gc_pop_frame: FuncId,
    /// `phx_gc_set_root(frame_ptr, idx: i64, ptr)`.
    pub gc_set_root: FuncId,
    /// `phx_gc_enable()` — flip on threshold-based auto-collection.
    pub gc_enable: FuncId,
    /// `phx_gc_shutdown()` — free every tracked allocation; called from
    /// the generated C `main` after `phx_main` returns so compiled
    /// binaries terminate leak-clean.
    pub gc_shutdown: FuncId,
}

impl RuntimeFunctions {
    /// Declare all runtime functions in the given module.
    pub fn declare(module: &mut impl Module, call_conv: CallConv) -> Result<Self, CompileError> {
        Ok(Self {
            print_i64: declare_func(module, "phx_print_i64", &[I64], &[], call_conv)?,
            print_f64: declare_func(module, "phx_print_f64", &[F64], &[], call_conv)?,
            print_bool: declare_func(module, "phx_print_bool", &[I8], &[], call_conv)?,
            print_str: declare_func(module, "phx_print_str", &[I64, I64], &[], call_conv)?,
            panic: declare_func(module, "phx_panic", &[I64, I64], &[], call_conv)?,
            str_concat: declare_func(
                module,
                "phx_str_concat",
                &[I64, I64, I64, I64],
                &[I64, I64],
                call_conv,
            )?,
            i64_to_str: declare_func(module, "phx_i64_to_str", &[I64], &[I64, I64], call_conv)?,
            f64_to_str: declare_func(module, "phx_f64_to_str", &[F64], &[I64, I64], call_conv)?,
            bool_to_str: declare_func(module, "phx_bool_to_str", &[I8], &[I64, I64], call_conv)?,
            str_eq: declare_str_cmp(module, "phx_str_eq", call_conv)?,
            str_ne: declare_str_cmp(module, "phx_str_ne", call_conv)?,
            str_lt: declare_str_cmp(module, "phx_str_lt", call_conv)?,
            str_gt: declare_str_cmp(module, "phx_str_gt", call_conv)?,
            str_le: declare_str_cmp(module, "phx_str_le", call_conv)?,
            str_ge: declare_str_cmp(module, "phx_str_ge", call_conv)?,
            gc_alloc: declare_func(module, "phx_gc_alloc", &[I64, I32], &[I64], call_conv)?,
            list_builder_alloc: declare_func(
                module,
                "phx_list_builder_alloc",
                &[I64],
                &[I64],
                call_conv,
            )?,
            list_builder_push: declare_func(
                module,
                "phx_list_builder_push",
                &[I64, I64, I64],
                &[],
                call_conv,
            )?,
            list_builder_freeze: declare_func(
                module,
                "phx_list_builder_freeze",
                &[I64],
                &[I64],
                call_conv,
            )?,
            map_builder_alloc: declare_func(
                module,
                "phx_map_builder_alloc",
                &[I64, I64, I64],
                &[I64],
                call_conv,
            )?,
            map_builder_set: declare_func(
                module,
                "phx_map_builder_set",
                &[I64, I64, I64, I64, I64],
                &[],
                call_conv,
            )?,
            map_builder_freeze: declare_func(
                module,
                "phx_map_builder_freeze",
                &[I64],
                &[I64],
                call_conv,
            )?,
            str_length: declare_func(module, "phx_str_length", &[I64, I64], &[I64], call_conv)?,
            // contains/startsWith/endsWith share the same signature as str_cmp.
            str_contains: declare_str_cmp(module, "phx_str_contains", call_conv)?,
            str_starts_with: declare_str_cmp(module, "phx_str_starts_with", call_conv)?,
            str_ends_with: declare_str_cmp(module, "phx_str_ends_with", call_conv)?,
            // trim/toLowerCase/toUpperCase: (ptr, len) -> (ptr, len).
            str_trim: declare_str_transform(module, "phx_str_trim", call_conv)?,
            str_to_lower: declare_str_transform(module, "phx_str_to_lower", call_conv)?,
            str_to_upper: declare_str_transform(module, "phx_str_to_upper", call_conv)?,
            json_escape_str: declare_str_transform(module, "phx_json_escape_str", call_conv)?,
            json_parse: declare_func(module, "phx_json_parse", &[I64, I64], &[I64], call_conv)?,
            json_free: declare_func(module, "phx_json_free", &[I64], &[], call_conv)?,
            json_parse_failed: declare_func(
                module,
                "phx_json_parse_failed",
                &[I64],
                &[I8],
                call_conv,
            )?,
            json_parse_error: declare_func(
                module,
                "phx_json_parse_error",
                &[I64],
                &[I64, I64],
                call_conv,
            )?,
            json_root: declare_func(module, "phx_json_root", &[I64], &[I64], call_conv)?,
            json_kind: declare_func(module, "phx_json_kind", &[I64], &[I64], call_conv)?,
            json_as_int: declare_func(module, "phx_json_as_int", &[I64], &[I64], call_conv)?,
            json_as_float: declare_func(module, "phx_json_as_float", &[I64], &[F64], call_conv)?,
            json_as_bool: declare_func(module, "phx_json_as_bool", &[I64], &[I8], call_conv)?,
            json_as_str: declare_func(module, "phx_json_as_str", &[I64], &[I64, I64], call_conv)?,
            json_get_field: declare_func(
                module,
                "phx_json_get_field",
                &[I64, I64, I64],
                &[I64],
                call_conv,
            )?,
            json_is_missing: declare_func(module, "phx_json_is_missing", &[I64], &[I8], call_conv)?,
            json_array_get: declare_func(
                module,
                "phx_json_array_get",
                &[I64, I64],
                &[I64],
                call_conv,
            )?,
            json_array_len: declare_func(module, "phx_json_array_len", &[I64], &[I64], call_conv)?,
            json_object_len: declare_func(
                module,
                "phx_json_object_len",
                &[I64],
                &[I64],
                call_conv,
            )?,
            json_object_key_at: declare_func(
                module,
                "phx_json_object_key_at",
                &[I64, I64],
                &[I64, I64],
                call_conv,
            )?,
            json_object_value_at: declare_func(
                module,
                "phx_json_object_value_at",
                &[I64, I64],
                &[I64],
                call_conv,
            )?,
            str_index_of: declare_func(
                module,
                "phx_str_index_of",
                &[I64, I64, I64, I64],
                &[I64],
                call_conv,
            )?,
            str_replace: declare_func(
                module,
                "phx_str_replace",
                &[I64, I64, I64, I64, I64, I64],
                &[I64, I64],
                call_conv,
            )?,
            str_substring: declare_func(
                module,
                "phx_str_substring",
                &[I64, I64, I64, I64],
                &[I64, I64],
                call_conv,
            )?,
            str_split: declare_func(
                module,
                "phx_str_split",
                &[I64, I64, I64, I64],
                &[I64],
                call_conv,
            )?,
            // List functions.
            list_alloc: declare_func(module, "phx_list_alloc", &[I64, I64], &[I64], call_conv)?,
            list_length: declare_func(module, "phx_list_length", &[I64], &[I64], call_conv)?,
            list_get_raw: declare_func(module, "phx_list_get_raw", &[I64, I64], &[I64], call_conv)?,
            list_push_raw: declare_func(
                module,
                "phx_list_push_raw",
                &[I64, I64, I64],
                &[I64],
                call_conv,
            )?,
            list_contains: declare_func(
                module,
                "phx_list_contains",
                &[I64, I64, I64, I8, I8],
                &[I8],
                call_conv,
            )?,
            list_take: declare_func(module, "phx_list_take", &[I64, I64], &[I64], call_conv)?,
            list_drop: declare_func(module, "phx_list_drop", &[I64, I64], &[I64], call_conv)?,
            // Map functions.
            map_from_pairs: declare_func(
                module,
                "phx_map_from_pairs",
                &[I64, I64, I64, I64, I64],
                &[I64],
                call_conv,
            )?,
            map_length: declare_func(module, "phx_map_length", &[I64], &[I64], call_conv)?,
            map_get_raw: declare_func(
                module,
                "phx_map_get_raw",
                &[I64, I64, I64],
                &[I64],
                call_conv,
            )?,
            map_set_raw: declare_func(
                module,
                "phx_map_set_raw",
                &[I64, I64, I64, I64, I64, I64],
                &[I64],
                call_conv,
            )?,
            map_remove_raw: declare_func(
                module,
                "phx_map_remove_raw",
                &[I64, I64, I64],
                &[I64],
                call_conv,
            )?,
            map_contains: declare_func(
                module,
                "phx_map_contains",
                &[I64, I64, I64],
                &[I8],
                call_conv,
            )?,
            map_keys: declare_func(module, "phx_map_keys", &[I64], &[I64], call_conv)?,
            map_values: declare_func(module, "phx_map_values", &[I64], &[I64], call_conv)?,
            // GC functions.
            gc_push_frame: declare_func(module, "phx_gc_push_frame", &[I64], &[I64], call_conv)?,
            gc_pop_frame: declare_func(module, "phx_gc_pop_frame", &[I64], &[], call_conv)?,
            gc_set_root: declare_func(module, "phx_gc_set_root", &[I64, I64, I64], &[], call_conv)?,
            gc_enable: declare_func(module, "phx_gc_enable", &[], &[], call_conv)?,
            gc_shutdown: declare_func(module, "phx_gc_shutdown", &[], &[], call_conv)?,
        })
    }
}

/// Declare a single runtime function with the given parameter and return types.
fn declare_func(
    module: &mut impl Module,
    name: &str,
    params: &[cranelift_codegen::ir::Type],
    returns: &[cranelift_codegen::ir::Type],
    call_conv: CallConv,
) -> Result<FuncId, CompileError> {
    let mut sig = Signature::new(call_conv);
    for &ty in params {
        sig.params.push(AbiParam::new(ty));
    }
    for &ty in returns {
        sig.returns.push(AbiParam::new(ty));
    }
    Ok(module.declare_function(name, Linkage::Import, &sig)?)
}

/// Declare a string comparison/predicate function: `(ptr, len, ptr, len) -> i8`.
fn declare_str_cmp(
    module: &mut impl Module,
    name: &str,
    call_conv: CallConv,
) -> Result<FuncId, CompileError> {
    declare_func(module, name, &[I64, I64, I64, I64], &[I8], call_conv)
}

/// Declare a unary string transform: `(ptr, len) -> (ptr, len)`.
fn declare_str_transform(
    module: &mut impl Module,
    name: &str,
    call_conv: CallConv,
) -> Result<FuncId, CompileError> {
    declare_func(module, name, &[I64, I64], &[I64, I64], call_conv)
}
