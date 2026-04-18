//! Payload type inference for generic enums (Option, Result).
//!
//! When the IR's `EnumRef("Option")` type doesn't carry generic arguments,
//! methods like `map`, `unwrap`, and `okOr` need to infer the concrete
//! payload type from available context.  The helpers here implement
//! multiple inference strategies, used by `option_methods` and
//! `result_methods` in a priority-based fallback chain.

use phoenix_ir::instruction::ValueId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use super::FuncState;

/// Try to use the instruction's `result_type` as the payload type.
///
/// Works for methods that directly return the unwrapped value (e.g.
/// `unwrap`, `unwrapOr`), where the IR result type IS the payload type.
/// Returns `None` if the result_type is `Void` or an `EnumRef` (which
/// doesn't carry inner type information).
pub(super) fn try_type_from_result(result_type: &IrType) -> Option<IrType> {
    if matches!(result_type, IrType::Void | IrType::EnumRef(_)) {
        None
    } else {
        Some(result_type.clone())
    }
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
pub(super) fn try_type_from_enum_alloc(
    state: &FuncState,
    receiver: ValueId,
    enum_name: &str,
) -> Option<IrType> {
    // Direct: check the receiver's recorded payload types.
    if let Some(field_types) = state.enum_payload_types.get(&receiver)
        && let Some(ty) = field_types.first()
    {
        return Some(ty.clone());
    }
    // Scan: find a consistent payload type from all allocations of this enum.
    let payloads: Vec<&IrType> = state
        .enum_payload_types
        .iter()
        .filter_map(|(vid, types)| {
            if let Some(IrType::EnumRef(name)) = state.type_map.get(vid)
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
