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
    /// `phx_list_contains(list_ptr, elem_ptr, elem_size, is_float) -> i8`.
    pub list_contains: FuncId,
    /// `phx_list_take(list_ptr, n) -> new_list_ptr`.
    pub list_take: FuncId,
    /// `phx_list_drop(list_ptr, n) -> new_list_ptr`.
    pub list_drop: FuncId,
    // ── Map runtime functions ───────────────────────────────────────
    /// `phx_map_from_pairs(key_size, val_size, n_pairs, pair_data) -> map_ptr`.
    /// Used by the map-literal lowering: codegen writes each pair into a
    /// stack buffer and the runtime hash-builds the table in one pass.
    /// Replaces the previous `phx_map_alloc`-then-write-pairs path; the
    /// runtime keeps `phx_map_alloc` as an internal helper for `set` /
    /// `remove`, but compiled code no longer needs to call it directly.
    pub map_from_pairs: FuncId,
    /// `phx_map_length(map_ptr) -> i64`.
    pub map_length: FuncId,
    /// `phx_map_get_raw(map_ptr, key_ptr, key_size) -> val_ptr_or_null`.
    pub map_get_raw: FuncId,
    /// `phx_map_set_raw(map_ptr, key_ptr, val_ptr, key_size, val_size) -> new_map_ptr`.
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
            str_length: declare_func(module, "phx_str_length", &[I64, I64], &[I64], call_conv)?,
            // contains/startsWith/endsWith share the same signature as str_cmp.
            str_contains: declare_str_cmp(module, "phx_str_contains", call_conv)?,
            str_starts_with: declare_str_cmp(module, "phx_str_starts_with", call_conv)?,
            str_ends_with: declare_str_cmp(module, "phx_str_ends_with", call_conv)?,
            // trim/toLowerCase/toUpperCase: (ptr, len) -> (ptr, len).
            str_trim: declare_str_transform(module, "phx_str_trim", call_conv)?,
            str_to_lower: declare_str_transform(module, "phx_str_to_lower", call_conv)?,
            str_to_upper: declare_str_transform(module, "phx_str_to_upper", call_conv)?,
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
                &[I64, I64, I64, I8],
                &[I8],
                call_conv,
            )?,
            list_take: declare_func(module, "phx_list_take", &[I64, I64], &[I64], call_conv)?,
            list_drop: declare_func(module, "phx_list_drop", &[I64, I64], &[I64], call_conv)?,
            // Map functions.
            map_from_pairs: declare_func(
                module,
                "phx_map_from_pairs",
                &[I64, I64, I64, I64],
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
                &[I64, I64, I64, I64, I64],
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
