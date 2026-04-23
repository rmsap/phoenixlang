//! Payload type inference for generic enums (Option, Result).
//!
//! `IrType::EnumRef` carries concrete generic type args at the use site,
//! so the primary inference path is Strategy 0 in
//! [`try_type_from_enum_args`]: read the args vector directly.  The other
//! strategies remain as load-bearing fallbacks — not defensive belt-and-
//! suspenders — because several real paths still emit empty args today:
//!
//! - `register_method` in `phoenix-ir/src/lower_decl.rs` emits
//!   `EnumRef(type_name, Vec::new())` for the `self` parameter of a user-
//!   defined method on an enum. Phoenix does not yet support methods on
//!   generic enums, so the args are genuinely empty rather than elided.
//! - Synthetic IR fragments built by unit tests without a sema source
//!   (e.g. `compile_errors.rs`) construct `EnumRef("Option", vec![])`
//!   directly to exercise the inference failure paths.
//!
//! When those fallbacks are removed (likely alongside user-defined generic
//! enum methods landing), Strategy 0 becomes the sole path.
//!
//! ## Strategy ordering
//!
//! Strategies run in **strict priority order** within
//! [`super::option_methods::option_payload_type`] and
//! [`super::result_methods::result_payload_types`]: each strategy fills
//! whichever slots are still unknown, and the first strategy to produce a
//! complete answer returns immediately. Lower-numbered strategies are
//! preferred because they read information closer to the source of truth
//! (the receiver's static type), while higher-numbered ones infer
//! indirectly from surrounding context. Never reorder strategies without
//! re-reading the slot-filling logic in both callers — Strategy 1b in
//! particular depends on Strategy 0 having already run.
//!
//! ## Strategy 2 is effectively dead for stdlib enums
//!
//! [`try_type_from_layout`] reads concrete field types from
//! `ir_module.enum_layouts`. For stdlib `Option` / `Result`, those
//! layouts use the `GENERIC_PLACEHOLDER` sentinel (see
//! `phoenix-ir/src/stdlib.rs`), so the filter in the helper (`!fields[0]
//! .is_generic_placeholder()`) rejects them and Strategy 2 returns
//! `None`. It only fires for *user-defined* enums whose layouts hold
//! concrete types — a path that cannot currently originate from Phoenix
//! source (user generic-enum methods are gated by the `debug_assert` in
//! `lower_decl.rs`) but is reachable from unit tests that build such
//! layouts directly.
//!
//! ## FIXME: collapse Strategies 1–4 once generic-enum methods land
//!
//! Once `register_method` threads generic args through (tracked in
//! `docs/phases/phase-2.md` under 2.2), every `EnumRef` reaching this
//! module will carry concrete args, Strategy 0 will succeed on every call,
//! and Strategies 1 / 1b / 2 / 3 / 4 can be deleted in a single pass.
//! Strategy 4's `expected_variant` filter and Strategy 1b's
//! result-args peel would both become dead code.

use phoenix_ir::instruction::ValueId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::{IrType, RESULT_ENUM};

use crate::error::CompileError;

use super::FuncState;

/// Try to use the instruction's `result_type` as the payload type.
///
/// Works for methods that directly return the unwrapped value (e.g.
/// `unwrap`, `unwrapOr`), where the IR result type IS the payload type.
/// Returns `None` if the result_type is `Void` or an `EnumRef` — a wrapper
/// result (e.g. `map` returns `Option<B>`) tells us nothing about the
/// receiver's payload, since the output type's args are the *mapped* type,
/// not the source payload. For those methods the caller must fall through
/// to [`try_type_from_enum_args`] (Strategy 0), which reads the receiver's
/// args directly.
pub(super) fn try_type_from_result(result_type: &IrType) -> Option<IrType> {
    if matches!(result_type, IrType::Void | IrType::EnumRef(_, _)) {
        None
    } else {
        Some(result_type.clone())
    }
}

/// Strategy 0: read the payload type directly from an `EnumRef`'s generic
/// args.
///
/// Given the receiver `ValueId` and the expected enum name, look up the
/// value's IR type in the function's `type_map`.  If it is
/// `EnumRef(enum_name, args)` with a non-empty `args` whose first element
/// is a concrete type (i.e. not the `GENERIC_PLACEHOLDER` sentinel),
/// return `args[index]`.  Otherwise `None`, and the caller falls through
/// to the legacy inference strategies.
pub(super) fn try_type_from_enum_args(
    state: &FuncState,
    receiver: ValueId,
    enum_name: &str,
    index: usize,
) -> Option<IrType> {
    let IrType::EnumRef(name, args) = state.type_map.get(&receiver)? else {
        return None;
    };
    if name != enum_name {
        return None;
    }
    let arg = args.get(index)?;
    if arg.is_generic_placeholder() {
        return None;
    }
    Some(arg.clone())
}

/// Strategy 0 for `Result<T, E>`: read both payload types from the
/// receiver's `EnumRef("Result", [ok_ty, err_ty])` generic args in a
/// single lookup.  Each slot is independently `None` if the type is
/// missing or the `GENERIC_PLACEHOLDER` sentinel — callers use the
/// per-slot results to seed later fallback strategies.
pub(super) fn try_result_payload_types_from_args(
    state: &FuncState,
    receiver: ValueId,
) -> (Option<IrType>, Option<IrType>) {
    let Some(IrType::EnumRef(name, args)) = state.type_map.get(&receiver) else {
        return (None, None);
    };
    if name != RESULT_ENUM {
        return (None, None);
    }
    let pick = |i: usize| args.get(i).filter(|t| !t.is_generic_placeholder()).cloned();
    (pick(0), pick(1))
}

/// Strategy 1b: read a payload type from the method's `result_type`
/// when the result is itself an enum whose args carry the payload we
/// want.
///
/// The receiver-args path ([`try_type_from_enum_args`]) can miss slots
/// when sema did not resolve a TypeVar in the receiver's position. The
/// method call's own return type is computed from the fully-resolved
/// binding type, so peeling an arg off the result type recovers what
/// Strategy 0 could not.
///
/// # Worked example
///
/// ```phoenix
/// let r: Result<Int, String> = Err("boom")
/// let o = r.ok()
/// ```
///
/// Sema types the RHS `Err("boom")` independently of the `let`
/// annotation, giving it `Result<T, String>` with a placeholder `T`
/// (nothing in the RHS constrains the Ok slot). So when IR lowering
/// asks sema for the type of the value flowing into `r.ok()`, it sees
/// `EnumRef("Result", [PLACEHOLDER, String])` — Strategy 0 returns
/// `None` for the Ok slot. But sema types the method call itself from
/// `r`'s binding type, so `r.ok()` has
/// `result_type = EnumRef("Option", [I64])`. Peeling `args[0]` from
/// that result type recovers the Ok payload (`Int`) that Strategy 0
/// missed.
///
/// Methods where a result-arg slot carries the receiver's payload:
/// - `Result::ok() -> Option<T>` → args[0] = Ok payload
/// - `Result::err() -> Option<E>` → args[0] = Err payload
/// - `Option::okOr(err) -> Result<T, E>` → args[0] = Option payload
///
/// Methods like `map` / `andThen` / `mapErr` remap the payload type, so
/// their result args describe the *new* payload — not the source
/// receiver's payload. Callers must only consult this helper for the
/// methods listed above.
pub(super) fn try_type_from_result_args(result_type: &IrType, index: usize) -> Option<IrType> {
    if let IrType::EnumRef(_, args) = result_type
        && let Some(arg) = args.get(index)
        && !arg.is_generic_placeholder()
    {
        return Some(arg.clone());
    }
    None
}

/// Try to extract a concrete (non-generic) field type from an enum
/// layout variant.
pub(super) fn try_type_from_layout(
    ir_module: &IrModule,
    enum_name: &str,
    variant_name: &str,
) -> Option<IrType> {
    ir_module.enum_layouts.get(enum_name).and_then(|layout| {
        layout.iter().find_map(|(name, fields)| {
            if name == variant_name && !fields.is_empty() && !fields[0].is_generic_placeholder() {
                Some(fields[0].clone())
            } else {
                None
            }
        })
    })
}

/// Try to infer the payload type from a closure argument's first parameter type.
///
/// For single-parameter closures like `(T) -> U` (used by `map`, `filter`,
/// `andThen`, etc.), the first parameter is the payload type `T`.
pub(super) fn try_type_from_closure_arg(state: &FuncState, args: &[ValueId]) -> Option<IrType> {
    if args.len() > 1
        && let Some(IrType::ClosureRef { param_types, .. }) = state.type_map.get(&args[1])
    {
        return param_types.first().cloned();
    }
    None
}

/// Try to infer the type from a direct value argument.
pub(super) fn try_type_from_value_arg(
    state: &FuncState,
    args: &[ValueId],
    index: usize,
) -> Option<IrType> {
    if args.len() > index {
        state.type_map.get(&args[index]).cloned()
    } else {
        None
    }
}

/// Try to infer the payload type from recorded `EnumAlloc` info for the
/// receiver, then from a scan of all same-enum allocations in the function.
///
/// `expected_variant` selects which variant's payload the caller wants:
/// for `Option` the payload lives in `Some` (variant 0); for `Result` the
/// Ok slot is variant 0 and the Err slot is variant 1. This distinction
/// matters because `enum_payload_types` records `field_types[0]` for
/// whatever variant was allocated — without filtering by variant we would
/// mix Ok and Err payloads in the scan, and for the receiver-direct path
/// we would return an Err payload when the caller asked for Ok (or vice
/// versa).
///
/// Returns `None` when the recorded variant doesn't match, the scan finds
/// no matching allocations, or the scan finds allocations that disagree.
pub(super) fn try_type_from_enum_alloc(
    state: &FuncState,
    receiver: ValueId,
    enum_name: &str,
    expected_variant: u32,
) -> Option<IrType> {
    // Direct: the receiver is an EnumAlloc in this function, allocated as
    // the variant the caller asked about.
    if let Some((variant, field_types)) = state.enum_payload_types.get(&receiver)
        && *variant == expected_variant
        && let Some(ty) = field_types.first()
    {
        // Consistency check: if the receiver's `EnumRef` carries a concrete
        // arg at the slot corresponding to this variant, it must agree with
        // the `EnumAlloc`-recorded payload type. A mismatch would mean IR
        // lowering and payload-tracking disagreed about the same value, and
        // Strategy 0 would silently win over Strategy 4 — masking a lowering
        // bug. Debug-only to keep release builds cheap.
        //
        // The slot index equals the variant index because both Option and
        // Result place variant `v`'s payload at `EnumRef.args[v]`: Option
        // has Some=0, and Result has Ok=0, Err=1.
        if let Some(IrType::EnumRef(_, args)) = state.type_map.get(&receiver)
            && let Some(arg) = args.get(*variant as usize)
            && !arg.is_generic_placeholder()
        {
            debug_assert_eq!(
                arg, ty,
                "EnumRef arg disagrees with EnumAlloc-recorded payload for \
                 receiver {receiver:?} (variant {variant}): EnumRef says \
                 {arg:?}, EnumAlloc recorded {ty:?}",
            );
        }
        return Some(ty.clone());
    }
    // Scan: find a consistent payload type from all same-enum, same-variant
    // allocations in this function.
    let payloads: Vec<&IrType> = state
        .enum_payload_types
        .iter()
        .filter_map(|(vid, (variant, types))| {
            if *variant == expected_variant
                && let Some(IrType::EnumRef(name, _)) = state.type_map.get(vid)
                && name == enum_name
                && !types.is_empty()
            {
                return Some(&types[0]);
            }
            None
        })
        .collect();
    if !payloads.is_empty() && payloads.iter().all(|t| *t == payloads[0]) {
        return Some(payloads[0].clone());
    }
    None
}

/// Build the error returned when all inference strategies fail for a
/// method that requires a concrete payload type.
///
/// Deduplicates the same five-line message previously duplicated across
/// `option_payload_type` (one slot) and `result_payload_types` (Ok and
/// Err slots). `enum_label` is `"Option"` or `"Result"`, `slot_label` is
/// the slot whose type is missing (e.g. `"payload"`, `"Ok"`, `"Err"`),
/// and `type_syntax` is the human-readable form callers should use to
/// make the args explicit (e.g. `"Option<T>"`, `"Result<T, E>"`).
pub(super) fn payload_inference_error(
    enum_label: &str,
    slot_label: &str,
    method: &str,
    type_syntax: &str,
) -> CompileError {
    CompileError::new(format!(
        "could not infer {enum_label} {slot_label} type for method \
         '{method}'. All inference strategies failed — the receiver's \
         `EnumRef` carried no concrete type args for this slot, and no \
         `EnumAlloc` in the current function constrained it either. \
         Reaching this error from a valid Phoenix program indicates a \
         compiler bug in type propagation or lowering, not a user error: \
         sema should always supply concrete `{type_syntax}` args to IR \
         lowering, and lowering should thread them through to the \
         backend. Please file a report with the program that reproduced \
         it."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_ir::types::GENERIC_PLACEHOLDER;
    use std::collections::HashMap;

    /// Empty `FuncState` seeded only with a `type_map`. The other fields
    /// aren't read by the helpers under test, but we can't derive
    /// `Default` because `value_map` stores Cranelift `Value`s.
    fn state_with_types(types: Vec<(ValueId, IrType)>) -> FuncState {
        FuncState {
            value_map: HashMap::new(),
            alloca_map: HashMap::new(),
            type_map: types.into_iter().collect(),
            closure_func_map: HashMap::new(),
            enum_payload_types: HashMap::new(),
            block_map: HashMap::new(),
        }
    }

    fn placeholder() -> IrType {
        IrType::StructRef(GENERIC_PLACEHOLDER.to_string(), Vec::new())
    }

    // ── try_type_from_enum_args ──────────────────────────────────────

    #[test]
    fn enum_args_reads_concrete_slot() {
        let v = ValueId(1);
        let state = state_with_types(vec![(
            v,
            IrType::EnumRef("Option".into(), vec![IrType::I64]),
        )]);
        assert_eq!(
            try_type_from_enum_args(&state, v, "Option", 0),
            Some(IrType::I64)
        );
    }

    #[test]
    fn enum_args_rejects_placeholder() {
        let v = ValueId(1);
        let state = state_with_types(vec![(
            v,
            IrType::EnumRef("Option".into(), vec![placeholder()]),
        )]);
        assert_eq!(try_type_from_enum_args(&state, v, "Option", 0), None);
    }

    #[test]
    fn enum_args_rejects_wrong_enum_name() {
        let v = ValueId(1);
        let state = state_with_types(vec![(
            v,
            IrType::EnumRef("Result".into(), vec![IrType::I64]),
        )]);
        assert_eq!(try_type_from_enum_args(&state, v, "Option", 0), None);
    }

    #[test]
    fn enum_args_rejects_missing_receiver() {
        let state = state_with_types(vec![]);
        assert_eq!(
            try_type_from_enum_args(&state, ValueId(99), "Option", 0),
            None
        );
    }

    #[test]
    fn enum_args_rejects_non_enum_receiver() {
        let v = ValueId(1);
        let state = state_with_types(vec![(v, IrType::I64)]);
        assert_eq!(try_type_from_enum_args(&state, v, "Option", 0), None);
    }

    #[test]
    fn enum_args_rejects_out_of_range_index() {
        let v = ValueId(1);
        let state = state_with_types(vec![(
            v,
            IrType::EnumRef("Option".into(), vec![IrType::I64]),
        )]);
        assert_eq!(try_type_from_enum_args(&state, v, "Option", 5), None);
    }

    // ── try_result_payload_types_from_args ───────────────────────────

    #[test]
    fn result_args_both_concrete() {
        let v = ValueId(1);
        let state = state_with_types(vec![(
            v,
            IrType::EnumRef("Result".into(), vec![IrType::I64, IrType::StringRef]),
        )]);
        assert_eq!(
            try_result_payload_types_from_args(&state, v),
            (Some(IrType::I64), Some(IrType::StringRef))
        );
    }

    #[test]
    fn result_args_partial_ok_placeholder_err_concrete() {
        let v = ValueId(1);
        let state = state_with_types(vec![(
            v,
            IrType::EnumRef("Result".into(), vec![placeholder(), IrType::StringRef]),
        )]);
        assert_eq!(
            try_result_payload_types_from_args(&state, v),
            (None, Some(IrType::StringRef))
        );
    }

    #[test]
    fn result_args_partial_ok_concrete_err_placeholder() {
        let v = ValueId(1);
        let state = state_with_types(vec![(
            v,
            IrType::EnumRef("Result".into(), vec![IrType::I64, placeholder()]),
        )]);
        assert_eq!(
            try_result_payload_types_from_args(&state, v),
            (Some(IrType::I64), None)
        );
    }

    #[test]
    fn result_args_rejects_wrong_enum_name() {
        let v = ValueId(1);
        let state = state_with_types(vec![(
            v,
            IrType::EnumRef("Option".into(), vec![IrType::I64]),
        )]);
        assert_eq!(try_result_payload_types_from_args(&state, v), (None, None));
    }

    #[test]
    fn result_args_missing_receiver_is_none_none() {
        let state = state_with_types(vec![]);
        assert_eq!(
            try_result_payload_types_from_args(&state, ValueId(99)),
            (None, None)
        );
    }

    // ── try_type_from_result_args ────────────────────────────────────

    #[test]
    fn result_args_peel_reads_concrete_arg() {
        let ty = IrType::EnumRef("Option".into(), vec![IrType::StringRef]);
        assert_eq!(try_type_from_result_args(&ty, 0), Some(IrType::StringRef));
    }

    #[test]
    fn result_args_peel_rejects_placeholder() {
        let ty = IrType::EnumRef("Option".into(), vec![placeholder()]);
        assert_eq!(try_type_from_result_args(&ty, 0), None);
    }

    #[test]
    fn result_args_peel_rejects_non_enum() {
        assert_eq!(try_type_from_result_args(&IrType::I64, 0), None);
    }

    #[test]
    fn result_args_peel_rejects_out_of_range() {
        let ty = IrType::EnumRef("Option".into(), vec![IrType::I64]);
        assert_eq!(try_type_from_result_args(&ty, 5), None);
    }

    // ── try_type_from_enum_alloc ──────────────────────────────────────
    //
    // Exercises Strategy 4. Specifically guards the variant-filtering
    // that replaced the old "take field_types[0] unconditionally" bug:
    // the direct and scan paths must both reject an Err(_) receiver when
    // the caller asked for the Ok slot (and vice versa).

    fn state_with_alloc(
        type_map: Vec<(ValueId, IrType)>,
        enum_allocs: Vec<(ValueId, u32, Vec<IrType>)>,
    ) -> FuncState {
        FuncState {
            value_map: HashMap::new(),
            alloca_map: HashMap::new(),
            type_map: type_map.into_iter().collect(),
            closure_func_map: HashMap::new(),
            enum_payload_types: enum_allocs
                .into_iter()
                .map(|(v, variant, fields)| (v, (variant, fields)))
                .collect(),
            block_map: HashMap::new(),
        }
    }

    #[test]
    fn enum_alloc_direct_path_returns_payload_for_matching_variant() {
        let v = ValueId(1);
        let state = state_with_alloc(
            vec![(v, IrType::EnumRef("Option".into(), Vec::new()))],
            vec![(v, 0, vec![IrType::StringRef])],
        );
        assert_eq!(
            try_type_from_enum_alloc(&state, v, "Option", 0),
            Some(IrType::StringRef)
        );
    }

    #[test]
    fn enum_alloc_direct_path_rejects_wrong_variant() {
        // Receiver is an `Err(string)` allocation (variant 1 of Result),
        // but caller asked for the Ok slot (variant 0). Pre-fix this
        // returned the Err payload as the Ok type — now None.
        let v = ValueId(1);
        let state = state_with_alloc(
            vec![(v, IrType::EnumRef("Result".into(), Vec::new()))],
            vec![(v, 1, vec![IrType::StringRef])],
        );
        assert_eq!(try_type_from_enum_alloc(&state, v, "Result", 0), None);
    }

    #[test]
    fn enum_alloc_scan_path_recovers_across_same_variant_allocations() {
        // Receiver (ValueId 99) has no EnumAlloc record, but two other
        // Ok(_) allocations in the same function agree on `I64` — the
        // scan path recovers that.
        let ok1 = ValueId(1);
        let ok2 = ValueId(2);
        let receiver = ValueId(99);
        let state = state_with_alloc(
            vec![
                (ok1, IrType::EnumRef("Result".into(), Vec::new())),
                (ok2, IrType::EnumRef("Result".into(), Vec::new())),
            ],
            vec![(ok1, 0, vec![IrType::I64]), (ok2, 0, vec![IrType::I64])],
        );
        assert_eq!(
            try_type_from_enum_alloc(&state, receiver, "Result", 0),
            Some(IrType::I64)
        );
    }

    #[test]
    fn enum_alloc_scan_path_ignores_other_variant_allocations() {
        // Function contains an `Ok(I64)` and an `Err(String)` allocation.
        // Asking for the Ok slot must return I64 (ignoring Err), not
        // None-because-mixed. Pre-fix: scan saw `[I64, StringRef]`, the
        // `.all(== payloads[0])` check failed, scan returned None.
        let ok = ValueId(1);
        let err = ValueId(2);
        let receiver = ValueId(99);
        let state = state_with_alloc(
            vec![
                (ok, IrType::EnumRef("Result".into(), Vec::new())),
                (err, IrType::EnumRef("Result".into(), Vec::new())),
            ],
            vec![
                (ok, 0, vec![IrType::I64]),
                (err, 1, vec![IrType::StringRef]),
            ],
        );
        assert_eq!(
            try_type_from_enum_alloc(&state, receiver, "Result", 0),
            Some(IrType::I64)
        );
        assert_eq!(
            try_type_from_enum_alloc(&state, receiver, "Result", 1),
            Some(IrType::StringRef)
        );
    }

    #[test]
    fn enum_alloc_scan_path_rejects_inconsistent_payloads() {
        // Two Ok allocations with different payload types — scan returns
        // None rather than picking one arbitrarily.
        let ok1 = ValueId(1);
        let ok2 = ValueId(2);
        let receiver = ValueId(99);
        let state = state_with_alloc(
            vec![
                (ok1, IrType::EnumRef("Result".into(), Vec::new())),
                (ok2, IrType::EnumRef("Result".into(), Vec::new())),
            ],
            vec![
                (ok1, 0, vec![IrType::I64]),
                (ok2, 0, vec![IrType::StringRef]),
            ],
        );
        assert_eq!(
            try_type_from_enum_alloc(&state, receiver, "Result", 0),
            None
        );
    }
}
