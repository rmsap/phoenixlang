//! `dyn Trait` verifier invariants.
//!
//! The checks here catch lowering/sema gaps that would otherwise surface
//! as silent wrong-codegen rather than as structured diagnostics:
//!
//! - [`verify_dyn_vtable_shapes`] — every vtable entry in
//!   [`IrModule::dyn_vtables`] has the same length as its trait's method
//!   count.
//! - [`verify_dyn_ops`] — per-function invariants on `DynAlloc` / `DynCall`
//!   (registered vtable, matching result type, receiver-type / slot-index
//!   bounds).
//! - [`verify_dyn_def_sites`] — any instruction whose `result_type` is
//!   `DynRef` must be an op that can actually produce a fat pointer.

use super::VerifyError;
use crate::instruction::Op;
use crate::module::{IrFunction, IrModule, IrTraitInfo};
use crate::types::IrType;

/// Every vtable in [`IrModule::dyn_vtables`] must have exactly as many slots
/// as the trait declares methods (otherwise a `DynCall` with a valid-per-
/// trait slot index would load past the end of the actual vtable).  This is
/// the cross-check the per-`DynCall` verifier cannot do on its own — from a
/// `DynCall` site alone we see the trait but not the concrete type, so we
/// can't compare the vtable length without walking the module.
pub(super) fn verify_dyn_vtable_shapes(module: &IrModule, errors: &mut Vec<VerifyError>) {
    for ((concrete, trait_name), entries) in &module.dyn_vtables {
        let Some(trait_info) = module.traits.get(trait_name) else {
            errors.push(VerifyError {
                function: format!("<vtable {concrete} as dyn {trait_name}>"),
                message: format!(
                    "vtable registered for dyn `{trait_name}` but trait is not in \
                     IrModule::traits (trait missing or non-object-safe)"
                ),
            });
            continue;
        };
        if entries.len() != trait_info.slot_count() {
            errors.push(VerifyError {
                function: format!("<vtable {concrete} as dyn {trait_name}>"),
                message: format!(
                    "vtable for `{concrete}` as dyn `{trait_name}` has {} entry/ies, \
                     but trait declares {} method(s) — any DynCall with a slot index \
                     valid-per-trait but >= vtable length would silently miscompile",
                    entries.len(),
                    trait_info.slot_count()
                ),
            });
        }
    }
}

/// Cross-check trait-object ops. Turns lowering/sema gaps into surfaced
/// verifier diagnostics rather than silent wrong-codegen.
pub(super) fn verify_dyn_ops(module: &IrModule, func: &IrFunction, errors: &mut Vec<VerifyError>) {
    for block in &func.blocks {
        for inst in &block.instructions {
            match &inst.op {
                Op::DynAlloc(trait_name, concrete_type, _value) => {
                    verify_dyn_alloc(
                        module,
                        func,
                        trait_name,
                        concrete_type,
                        &inst.result_type,
                        errors,
                    );
                }
                Op::DynCall(trait_name, method_idx, receiver, _args) => {
                    verify_dyn_call(module, func, trait_name, *method_idx, *receiver, errors);
                }
                _ => {}
            }
        }
    }
}

/// Per-`Op::DynAlloc` invariants: a vtable must already be registered for
/// the `(concrete_type, trait_name)` pair, and the result type must be
/// `DynRef(trait_name)`.
fn verify_dyn_alloc(
    module: &IrModule,
    func: &IrFunction,
    trait_name: &str,
    concrete_type: &str,
    result_type: &IrType,
    errors: &mut Vec<VerifyError>,
) {
    if !module
        .dyn_vtables
        .contains_key(&(concrete_type.to_string(), trait_name.to_string()))
    {
        errors.push(VerifyError {
            function: func.name.clone(),
            message: format!(
                "Op::DynAlloc({concrete_type}, dyn {trait_name}) has no registered vtable"
            ),
        });
    }
    if !matches!(result_type, IrType::DynRef(t) if t == trait_name) {
        errors.push(VerifyError {
            function: func.name.clone(),
            message: format!(
                "Op::DynAlloc result type must be DynRef({trait_name}), got {result_type}"
            ),
        });
    }
}

/// Per-`Op::DynCall` invariants: the receiver must be typed `DynRef` of
/// the matching trait, and the slot index must be in range for the
/// trait's method count.
///
/// Slot bounds are read from `module.traits` (the IR-level trait
/// metadata populated at lowering time), *not* from the per-`(concrete,
/// trait)` vtable map — so the check succeeds for any object-safe trait
/// in the program, including traits whose impls have not been exercised
/// by a coercion site. A missing entry means either the trait is
/// non-object-safe (sema should have rejected the `dyn` site) or the
/// trait wasn't declared at all.
fn verify_dyn_call(
    module: &IrModule,
    func: &IrFunction,
    trait_name: &str,
    method_idx: u32,
    receiver: crate::instruction::ValueId,
    errors: &mut Vec<VerifyError>,
) {
    match func.instruction_result_type(receiver) {
        Some(IrType::DynRef(t)) if t == trait_name => {}
        Some(other) => errors.push(VerifyError {
            function: func.name.clone(),
            message: format!(
                "Op::DynCall receiver must have type DynRef({trait_name}), got {other}"
            ),
        }),
        None => errors.push(VerifyError {
            function: func.name.clone(),
            message: format!("Op::DynCall receiver {receiver} has no recorded type"),
        }),
    }
    match module.traits.get(trait_name).map(IrTraitInfo::slot_count) {
        None => errors.push(VerifyError {
            function: func.name.clone(),
            message: format!(
                "Op::DynCall(dyn {trait_name}[{method_idx}]): no IR trait metadata for \
                 `{trait_name}` — trait is missing or non-object-safe"
            ),
        }),
        Some(n) if (method_idx as usize) >= n => {
            errors.push(VerifyError {
                function: func.name.clone(),
                message: format!(
                    "Op::DynCall(dyn {trait_name}[{method_idx}]): slot index out of range \
                     (trait has {n} method(s))"
                ),
            });
        }
        _ => {}
    }
}

/// Reject instructions whose op cannot produce a DynRef yet whose result
/// type is DynRef. Catches regressions where a pass manufactures a fat
/// pointer via a wrong op (e.g. StructAlloc) and bypasses the vtable ABI.
pub(super) fn verify_dyn_def_sites(func: &IrFunction, errors: &mut Vec<VerifyError>) {
    for block in &func.blocks {
        for inst in &block.instructions {
            if !matches!(inst.result_type, IrType::DynRef(_)) {
                continue;
            }
            // `Op::UnresolvedDynAlloc` is deliberately not listed: the
            // caller filters via `module.concrete_functions()`, and
            // `verify_no_unresolved_placeholder_ops` runs on the same
            // iteration to flag any residual placeholder.  Listing
            // here would be dead defensive noise — if one shows up at
            // a def site, the placeholder-op verifier already fires a
            // clearer diagnostic.
            let op_can_produce_dyn = matches!(
                inst.op,
                Op::DynAlloc(..)
                    | Op::DynCall(..)
                    | Op::Call(..)
                    | Op::CallIndirect(..)
                    | Op::BuiltinCall(..)
                    | Op::StructGetField(..)
                    | Op::EnumGetField(..)
                    | Op::Load(_)
                    | Op::Copy(_)
                    | Op::ClosureLoadCapture(..)
            );
            if !op_can_produce_dyn {
                errors.push(VerifyError {
                    function: func.name.clone(),
                    message: format!(
                        "DynRef result type on op that cannot produce a `dyn` value: {:?}",
                        inst.op
                    ),
                });
            }
        }
    }
}

#[cfg(test)]
mod dyn_verifier_tests {
    //! Negative verifier tests for `dyn Trait` ops: construct malformed
    //! IR modules by hand and assert that the verifier flags the
    //! specific invariant each new check enforces.  Positive cases are
    //! covered by the compile + roundtrip tests in the backend crates.

    use crate::instruction::{FuncId, Op};
    use crate::module::{FunctionSlot, IrFunction, IrModule, IrTraitInfo, IrTraitMethod};
    use crate::terminator::Terminator;
    use crate::types::IrType;
    use crate::verify::verify;

    #[test]
    fn dyn_alloc_without_registered_vtable_is_flagged() {
        let mut module = IrModule::new();
        module.traits.insert(
            "Trait".into(),
            IrTraitInfo {
                methods: vec![IrTraitMethod {
                    name: "m0".into(),
                    param_types: Vec::new(),
                    return_type: IrType::Void,
                }],
            },
        );
        let mut func = IrFunction::new(
            FuncId(0),
            "test".into(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        let entry = func.create_block();
        let val = func.add_block_param(entry, IrType::StructRef("Concrete".into(), Vec::new()));
        func.emit(
            entry,
            Op::DynAlloc("Trait".into(), "Concrete".into(), val),
            IrType::DynRef("Trait".into()),
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("has no") && e.message.contains("registered vtable")),
            "expected 'no registered vtable' error, got: {errors:?}"
        );
    }

    #[test]
    fn dyn_call_slot_out_of_range_is_flagged() {
        let mut func = IrFunction::new(
            FuncId(0),
            "test".to_string(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        let entry = func.create_block();
        // Block param of type DynRef as receiver — legal def-site for DynRef.
        let recv = func.add_block_param(entry, IrType::DynRef("Trait".into()));
        func.emit(
            entry,
            Op::DynCall("Trait".into(), 99, recv, Vec::new()),
            IrType::Void,
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));

        let mut module = IrModule::new();
        module.traits.insert(
            "Trait".into(),
            IrTraitInfo {
                methods: vec![IrTraitMethod {
                    name: "only".into(),
                    param_types: Vec::new(),
                    return_type: IrType::Void,
                }],
            },
        );
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors.iter().any(|e| e.message.contains("out of range")),
            "expected slot-out-of-range error, got: {errors:?}"
        );
    }

    #[test]
    fn dyn_call_on_unknown_trait_is_flagged() {
        let mut func = IrFunction::new(
            FuncId(0),
            "test".to_string(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        let entry = func.create_block();
        let recv = func.add_block_param(entry, IrType::DynRef("Ghost".into()));
        func.emit(
            entry,
            Op::DynCall("Ghost".into(), 0, recv, Vec::new()),
            IrType::Void,
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));

        let mut module = IrModule::new();
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("no IR trait metadata")),
            "expected 'no IR trait metadata' error, got: {errors:?}"
        );
    }

    #[test]
    fn dyn_alloc_result_type_must_match_trait_name() {
        let mut module = IrModule::new();
        module.traits.insert(
            "Trait".into(),
            IrTraitInfo {
                methods: Vec::new(),
            },
        );
        module
            .dyn_vtables
            .insert(("Concrete".into(), "Trait".into()), Vec::new());

        let mut func = IrFunction::new(
            FuncId(0),
            "test".to_string(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        let entry = func.create_block();
        let val = func.add_block_param(entry, IrType::StructRef("Concrete".into(), Vec::new()));
        // DynAlloc with result type DynRef("OtherTrait") — mismatch.
        func.emit(
            entry,
            Op::DynAlloc("Trait".into(), "Concrete".into(), val),
            IrType::DynRef("OtherTrait".into()),
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("result type must be DynRef")),
            "expected result-type-mismatch error, got: {errors:?}"
        );
    }

    #[test]
    fn dyn_ref_result_on_illegal_op_is_flagged() {
        let mut func = IrFunction::new(
            FuncId(0),
            "test".to_string(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        let entry = func.create_block();
        // ConstI64 claiming to produce a DynRef — verify_dyn_def_sites
        // should flag this.
        func.emit(entry, Op::ConstI64(0), IrType::DynRef("Trait".into()), None);
        func.set_terminator(entry, Terminator::Return(None));

        let mut module = IrModule::new();
        module.functions.push(FunctionSlot::Concrete(func));
        // No trait metadata needed — the def-site check is independent.

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("DynRef result type on op")),
            "expected illegal-dyn-def-site error, got: {errors:?}"
        );
    }

    /// Receiver's `DynRef` trait must match the `DynCall`'s trait name —
    /// a mismatch is silent wrong-codegen otherwise (a vtable from one
    /// trait would be indexed by another trait's slot conventions).
    #[test]
    fn dyn_call_receiver_trait_mismatch_is_flagged() {
        let mut module = IrModule::new();
        module.traits.insert(
            "Trait".into(),
            IrTraitInfo {
                methods: vec![IrTraitMethod {
                    name: "m0".into(),
                    param_types: Vec::new(),
                    return_type: IrType::Void,
                }],
            },
        );

        let mut func = IrFunction::new(
            FuncId(0),
            "test".into(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        let entry = func.create_block();
        // Receiver typed `DynRef("Other")` but the DynCall targets "Trait".
        let recv = func.add_block_param(entry, IrType::DynRef("Other".into()));
        func.emit(
            entry,
            Op::DynCall("Trait".into(), 0, recv, Vec::new()),
            IrType::Void,
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("receiver must have type DynRef")),
            "expected receiver-trait-mismatch error, got: {errors:?}"
        );
    }

    /// A `DynAlloc` whose `concrete_type` is a type-parameter name (e.g.
    /// `"T"`) is a lowering bug: either sema admitted `dyn GenericTrait`
    /// (currently rejected at check-types) or lowering emitted the op
    /// before monomorphization reified the concrete type. The verifier
    /// surfaces it as a missing-vtable error — the `(TypeVar, trait)`
    /// pair is never registered by `register_dyn_vtable`, which only
    /// runs for coercion sites with concrete source types.
    ///
    /// Pinning today's behavior so if the rejection moves (e.g. to a
    /// dedicated "TypeVar as dyn concrete" diagnostic), this test is
    /// the tripwire rather than silent acceptance.
    #[test]
    fn dyn_alloc_with_type_var_concrete_is_flagged() {
        let mut module = IrModule::new();
        module.traits.insert(
            "Trait".into(),
            IrTraitInfo {
                methods: vec![IrTraitMethod {
                    name: "m0".into(),
                    param_types: Vec::new(),
                    return_type: IrType::Void,
                }],
            },
        );
        let mut func = IrFunction::new(
            FuncId(0),
            "test".into(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        let entry = func.create_block();
        // A TypeVar-typed source value paired with a TypeVar-named
        // concrete in the DynAlloc — no vtable for ("T", "Trait") will
        // ever be registered, so the shape check fires.
        let val = func.add_block_param(entry, IrType::TypeVar("T".into()));
        func.emit(
            entry,
            Op::DynAlloc("Trait".into(), "T".into(), val),
            IrType::DynRef("Trait".into()),
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("has no") && e.message.contains("registered vtable")),
            "expected 'no registered vtable' error for TypeVar concrete, got: {errors:?}"
        );
    }

    #[test]
    fn well_formed_dyn_module_passes_verification() {
        // Sanity: the constructors above don't spuriously trigger
        // verifier errors for a well-formed module.
        let mut module = IrModule::new();
        module.traits.insert(
            "Trait".into(),
            IrTraitInfo {
                methods: vec![IrTraitMethod {
                    name: "m0".into(),
                    param_types: Vec::new(),
                    return_type: IrType::Void,
                }],
            },
        );
        module.dyn_vtables.insert(
            ("Concrete".into(), "Trait".into()),
            vec![("m0".into(), FuncId(1))],
        );

        let mut func = IrFunction::new(
            FuncId(0),
            "test".into(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        let entry = func.create_block();
        let concrete =
            func.add_block_param(entry, IrType::StructRef("Concrete".into(), Vec::new()));
        let wrapped = func
            .emit(
                entry,
                Op::DynAlloc("Trait".into(), "Concrete".into(), concrete),
                IrType::DynRef("Trait".into()),
                None,
            )
            .unwrap();
        func.emit(
            entry,
            Op::DynCall("Trait".into(), 0, wrapped, Vec::new()),
            IrType::Void,
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }
}
