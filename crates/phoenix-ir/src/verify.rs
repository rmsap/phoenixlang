//! IR verification — structural invariants after lowering.
//!
//! Checks: one terminator per block, valid branch targets, terminator
//! argument counts match target block params, no operand uses
//! [`VOID_SENTINEL`], value-ids defined in same function, `dyn`-op
//! invariants, `DynRef` def-site legitimacy, and that no
//! [`Op::UnresolvedTraitMethod`](crate::instruction::Op::UnresolvedTraitMethod)
//! or [`Op::UnresolvedDynAlloc`](crate::instruction::Op::UnresolvedDynAlloc)
//! survives into any concrete function post-monomorphization.
//!
//! The `dyn`-specific checks live in [`dyn_ops`] so the core structural
//! verifier here stays compact.
//!
//! **Not yet checked:** SSA dominance. Pass values through block
//! parameters across block boundaries (see `lower_try`).

mod dyn_ops;

use crate::block::BlockId;
use crate::instruction::{Op, VOID_SENTINEL, ValueId};
use crate::module::{IrFunction, IrModule};
use crate::terminator::Terminator;
use crate::types::IrType;
use std::collections::{HashMap, HashSet};

/// Errors found during verification.
#[derive(Debug)]
pub struct VerifyError {
    /// The function in which the error was found.
    pub function: String,
    /// A human-readable description of the error.
    pub message: String,
}

/// Verify the structural integrity of an IR module.
///
/// Returns a list of errors found.  An empty list means the module is
/// well-formed.
///
/// Generic templates ([`crate::module::FunctionSlot::Template`]) are
/// intentionally skipped: their bodies carry `IrType::TypeVar`
/// annotations that the monomorphization pass consumes to produce
/// specialized copies, and no downstream backend ever reads them.
/// Iterating via [`IrModule::concrete_functions`] is the canonical way
/// to walk verifiable functions.
pub fn verify(module: &IrModule) -> Vec<VerifyError> {
    let mut errors = Vec::new();
    dyn_ops::verify_dyn_vtable_shapes(module, &mut errors);
    for func in module.concrete_functions() {
        verify_function(func, &mut errors);
        verify_no_unresolved_placeholder_ops(func, &mut errors);
        verify_no_partial_generic_types(func, &mut errors);
        dyn_ops::verify_dyn_ops(module, func, &mut errors);
        dyn_ops::verify_dyn_def_sites(func, &mut errors);
    }
    errors
}

/// A concrete function whose inference left a *partially-generic* type —
/// a `__generic` placeholder nested in a type argument, e.g.
/// `Result<Int, __generic>` from a phantom-parameter constructor like
/// `Ok(99)`, or `Option<__generic>` from a `None` whose context didn't
/// pin it — must not reach a backend. The native and linear backends
/// tolerate it (their enums are structural) but wasm32-gc can't pin the
/// placeholder to a unique nominal instantiation when siblings exist, so
/// the *same program* would produce output on some backends and a
/// compile error on others — violating the cross-backend determinism the
/// matrix guarantees.
///
/// This is the postcondition of complete (bidirectional) inference:
/// every type in a concrete function is fully pinned from context. Sema's
/// expected-type pinning (`pin_inferred_type_to_annotation`, applied at
/// the `let` / return / lambda / call-arg / constructor-arg /
/// collection-element / `if`-`match`-branch boundaries) enforces it at
/// the source; a residual placeholder is an inference gap, caught here as
/// a hard, backend-agnostic error rather than a divergence. See §Phase
/// 2.4 K.12.
///
/// A *bare* placeholder (a dead generic closure's erased capture, an
/// unconstrained empty literal no nominal codegen consumes) is inert and
/// intentionally not flagged — see [`IrType::contains_placeholder_in_enum_arg`].
///
/// Coverage is by *value type*, not an exhaustive type walk: we check each
/// block parameter and each instruction `result_type`. A partially-generic
/// type produced anywhere materializes as some value (a constructor's
/// result, a collection alloc, a phi/merge block parameter), so a residual
/// placeholder surfaces on one of these. Operand types are not re-checked
/// (they are some other instruction's already-checked result), and the
/// function signature is covered transitively — its parameters are the
/// entry block's parameters and its return value is a checked instruction
/// result. If a future op carries a type that is neither a block param nor
/// a `result_type`, extend the walk to cover it.
fn verify_no_partial_generic_types(func: &IrFunction, errors: &mut Vec<VerifyError>) {
    // `what` is built lazily (a closure rather than a `&str`) so the
    // block-param label's `format!` only allocates on an actual violation —
    // this walk visits every block param of every concrete function, but
    // almost never flags one.
    let mut flag = |ty: &IrType, what: &dyn Fn() -> String| {
        if ty.contains_placeholder_in_enum_arg() {
            let what = what();
            errors.push(VerifyError {
                function: func.name.clone(),
                message: format!(
                    "{what} has partially-generic type `{ty:?}`: a `__generic` \
                     placeholder survived in a type argument — a generic type \
                     parameter was left unresolved. Usually a phantom-parameter \
                     constructor needs a type annotation at its binding or \
                     boundary; it can also be the known limitation on nested \
                     generic variant fields of user-defined enums (§Phase 2.4 \
                     K.4 / K.12). Reaching a backend with this would diverge \
                     across targets."
                ),
            });
        }
    };
    for block in &func.blocks {
        for (val, ty) in &block.params {
            flag(ty, &|| format!("block param {val:?}"));
        }
        for inst in &block.instructions {
            flag(&inst.result_type, &|| "instruction result".to_string());
        }
    }
}

/// Trait-bounded method calls on type-variable receivers are emitted
/// as [`Op::UnresolvedTraitMethod`] by IR lowering and must be
/// rewritten to a direct `Op::Call` by the monomorphization pass.
/// Similarly, `dyn Trait` coercions on type-variable sources are
/// emitted as [`Op::UnresolvedDynAlloc`] and rewritten to
/// [`Op::DynAlloc`] by the same pass.  Any residual placeholder in a
/// concrete function indicates a mono bug.
fn verify_no_unresolved_placeholder_ops(func: &IrFunction, errors: &mut Vec<VerifyError>) {
    for block in &func.blocks {
        for instr in &block.instructions {
            match &instr.op {
                Op::UnresolvedTraitMethod(method, _, _) => errors.push(VerifyError {
                    function: func.name.clone(),
                    message: format!(
                        "Op::UnresolvedTraitMethod `.{method}` survived \
                         monomorphization — this is an internal compiler bug",
                    ),
                }),
                Op::UnresolvedDynAlloc(trait_name, _) => errors.push(VerifyError {
                    function: func.name.clone(),
                    message: format!(
                        "Op::UnresolvedDynAlloc `@{trait_name}` survived \
                         monomorphization — this is an internal compiler bug",
                    ),
                }),
                _ => {}
            }
        }
    }
}

fn verify_function(func: &IrFunction, errors: &mut Vec<VerifyError>) {
    if func.blocks.is_empty() {
        // Functions with no blocks are stubs (body not yet lowered).
        return;
    }

    let valid_blocks: HashSet<BlockId> = func.blocks.iter().map(|b| b.id).collect();

    // Map from BlockId to its parameter count for argument-count checks.
    let block_param_counts: HashMap<BlockId, usize> =
        func.blocks.iter().map(|b| (b.id, b.params.len())).collect();

    // Collect all defined ValueIds: block parameters + instruction results.
    let mut defined: HashSet<ValueId> = HashSet::new();
    for block in &func.blocks {
        for (val, _) in &block.params {
            defined.insert(*val);
        }
        for inst in &block.instructions {
            if let Some(val) = inst.result {
                defined.insert(val);
            }
        }
    }

    for block in &func.blocks {
        // Check that the terminator is not `None`.
        if matches!(block.terminator, Terminator::None) {
            errors.push(VerifyError {
                function: func.name.clone(),
                message: format!("block {} has no terminator", block.id),
            });
        }

        // Check that all branch targets are valid.
        for target in terminator_targets(&block.terminator) {
            if !valid_blocks.contains(&target) {
                errors.push(VerifyError {
                    function: func.name.clone(),
                    message: format!("block {} references invalid target {}", block.id, target),
                });
            }
        }

        // Check terminator argument counts match target block parameter counts.
        for (target, arg_count) in terminator_target_args(&block.terminator) {
            if let Some(&param_count) = block_param_counts.get(&target)
                && arg_count != param_count
            {
                errors.push(VerifyError {
                    function: func.name.clone(),
                    message: format!(
                        "block {} jumps to {} with {} args, but {} expects {} params",
                        block.id, target, arg_count, target, param_count
                    ),
                });
            }
        }

        // Check instruction operands: no VOID_SENTINEL, all defined.
        for inst in &block.instructions {
            for val in inst.op.operands() {
                if val == VOID_SENTINEL {
                    errors.push(VerifyError {
                        function: func.name.clone(),
                        message: format!(
                            "block {} instruction uses VOID_SENTINEL as operand",
                            block.id
                        ),
                    });
                } else if !defined.contains(&val) {
                    errors.push(VerifyError {
                        function: func.name.clone(),
                        message: format!(
                            "block {} instruction uses undefined value {}",
                            block.id, val
                        ),
                    });
                }
            }
            // `Op::ClosureLoadCapture` carries an immediate ordinal
            // index (not a `ValueId`), so the generic operand walk
            // above doesn't see it. Check the bound here instead — the
            // Cranelift backend's bound check fires only at codegen
            // time, by which point we've already lost the IR-level
            // diagnostic context.
            if let Op::ClosureLoadCapture(_, capture_idx) = inst.op
                && (capture_idx as usize) >= func.capture_types.len()
            {
                errors.push(VerifyError {
                    function: func.name.clone(),
                    message: format!(
                        "block {} Op::ClosureLoadCapture: capture index {} out of \
                         range (function has {} captures)",
                        block.id,
                        capture_idx,
                        func.capture_types.len(),
                    ),
                });
            }
        }

        // Check terminator operands: no VOID_SENTINEL, all defined.
        for val in terminator_operands(&block.terminator) {
            if val == VOID_SENTINEL {
                errors.push(VerifyError {
                    function: func.name.clone(),
                    message: format!(
                        "block {} terminator uses VOID_SENTINEL as operand",
                        block.id
                    ),
                });
            } else if !defined.contains(&val) {
                errors.push(VerifyError {
                    function: func.name.clone(),
                    message: format!("block {} terminator uses undefined value {}", block.id, val),
                });
            }
        }
    }
}

/// Extract all block targets from a terminator.
fn terminator_targets(term: &Terminator) -> Vec<BlockId> {
    match term {
        Terminator::Jump { target, .. } => vec![*target],
        Terminator::Branch {
            true_block,
            false_block,
            ..
        } => vec![*true_block, *false_block],
        Terminator::Switch { cases, default, .. } => {
            let mut targets: Vec<BlockId> = cases.iter().map(|(_, b, _)| *b).collect();
            targets.push(*default);
            targets
        }
        Terminator::Return(_) | Terminator::Unreachable | Terminator::None => Vec::new(),
    }
}

/// Extract `(target_block, argument_count)` pairs from a terminator.
fn terminator_target_args(term: &Terminator) -> Vec<(BlockId, usize)> {
    match term {
        Terminator::Jump { target, args } => vec![(*target, args.len())],
        Terminator::Branch {
            true_block,
            true_args,
            false_block,
            false_args,
            ..
        } => vec![
            (*true_block, true_args.len()),
            (*false_block, false_args.len()),
        ],
        Terminator::Switch {
            cases,
            default,
            default_args,
            ..
        } => {
            let mut pairs: Vec<(BlockId, usize)> =
                cases.iter().map(|(_, b, a)| (*b, a.len())).collect();
            pairs.push((*default, default_args.len()));
            pairs
        }
        Terminator::Return(_) | Terminator::Unreachable | Terminator::None => Vec::new(),
    }
}

/// Extract all [`ValueId`] operands from a terminator (conditions, args,
/// return values).
fn terminator_operands(term: &Terminator) -> Vec<ValueId> {
    match term {
        Terminator::Jump { args, .. } => args.clone(),
        Terminator::Branch {
            condition,
            true_args,
            false_args,
            ..
        } => {
            let mut ops = vec![*condition];
            ops.extend(true_args);
            ops.extend(false_args);
            ops
        }
        Terminator::Switch {
            value,
            cases,
            default_args,
            ..
        } => {
            let mut ops = vec![*value];
            for (_, _, args) in cases {
                ops.extend(args);
            }
            ops.extend(default_args);
            ops
        }
        Terminator::Return(Some(v)) => vec![*v],
        Terminator::Return(None) | Terminator::Unreachable | Terminator::None => Vec::new(),
    }
}

#[cfg(test)]
mod unresolved_placeholder_op_tests {
    //! Negative verifier tests for [`Op::UnresolvedTraitMethod`] and
    //! [`Op::UnresolvedDynAlloc`]: both placeholders must not survive
    //! the monomorphization pass inside a concrete (non-template)
    //! function.  Positive cases — where the placeholders legitimately
    //! appear in a template body — are covered by the backend-crate
    //! integration suite.

    use crate::instruction::{FuncId, Op};
    use crate::module::{FunctionSlot, IrFunction, IrModule};
    use crate::terminator::Terminator;
    use crate::types::IrType;
    use crate::verify::verify;

    #[test]
    fn unresolved_trait_method_in_concrete_function_is_flagged() {
        let mut module = IrModule::new();
        let mut func = IrFunction::new(
            FuncId(0),
            "concrete".into(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        let entry = func.create_block();
        let recv = func.add_block_param(entry, IrType::StructRef("Concrete".into(), Vec::new()));
        func.emit(
            entry,
            Op::UnresolvedTraitMethod("greet".into(), Vec::new(), vec![recv]),
            IrType::Void,
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors.iter().any(
                |e| e.message.contains("UnresolvedTraitMethod") && e.message.contains(".greet")
            ),
            "expected unresolved-trait-method error, got: {errors:?}"
        );
    }

    #[test]
    fn unresolved_dyn_alloc_in_concrete_function_is_flagged() {
        let mut module = IrModule::new();
        let mut func = IrFunction::new(
            FuncId(0),
            "concrete".into(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        let entry = func.create_block();
        let src = func.add_block_param(entry, IrType::StructRef("Circle".into(), Vec::new()));
        func.emit(
            entry,
            Op::UnresolvedDynAlloc("Drawable".into(), src),
            IrType::DynRef("Drawable".into()),
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors.iter().any(
                |e| e.message.contains("UnresolvedDynAlloc") && e.message.contains("@Drawable")
            ),
            "expected unresolved-dyn-alloc error, got: {errors:?}"
        );
    }

    #[test]
    fn unresolved_trait_method_in_template_is_not_flagged() {
        let mut module = IrModule::new();
        let mut func = IrFunction::new(
            FuncId(0),
            "tmpl".into(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        // Templates are skipped by `concrete_functions()`, so the
        // placeholder is allowed here.
        let entry = func.create_block();
        let recv = func.add_block_param(entry, IrType::TypeVar("T".into()));
        func.emit(
            entry,
            Op::UnresolvedTraitMethod("greet".into(), Vec::new(), vec![recv]),
            IrType::Void,
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Template(func));

        let errors = verify(&module);
        assert!(
            !errors
                .iter()
                .any(|e| e.message.contains("UnresolvedTraitMethod")),
            "placeholder in a template must not be flagged, got: {errors:?}"
        );
    }

    #[test]
    fn unresolved_dyn_alloc_in_template_is_not_flagged() {
        let mut module = IrModule::new();
        let mut func = IrFunction::new(
            FuncId(0),
            "tmpl".into(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        );
        let entry = func.create_block();
        let src = func.add_block_param(entry, IrType::TypeVar("T".into()));
        func.emit(
            entry,
            Op::UnresolvedDynAlloc("Drawable".into(), src),
            IrType::DynRef("Drawable".into()),
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Template(func));

        let errors = verify(&module);
        assert!(
            !errors
                .iter()
                .any(|e| e.message.contains("UnresolvedDynAlloc")),
            "placeholder in a template must not be flagged, got: {errors:?}"
        );
    }
}

#[cfg(test)]
mod partial_generic_type_tests {
    //! Tests for [`super::verify_no_partial_generic_types`]: a `__generic`
    //! placeholder surviving as an *enum type argument* in a concrete
    //! function must be rejected, whether it surfaces on a block parameter
    //! or an instruction result. A bare placeholder, or one in a list
    //! *element*, is inert and must pass, and a template (skipped by
    //! [`crate::module::IrModule::concrete_functions`]) is never flagged.
    //!
    //! The underlying type predicate
    //! ([`crate::types::IrType::contains_placeholder_in_enum_arg`]) is
    //! exhaustively unit-tested in [`crate::types`]; these lock the verifier
    //! *walk* — which value types it inspects (block params + instruction
    //! `result_type`s) and its template skip — so a future change to the IR
    //! shape can't silently narrow coverage.

    use crate::instruction::{FuncId, Op};
    use crate::module::{FunctionSlot, IrFunction, IrModule};
    use crate::terminator::Terminator;
    use crate::types::{GENERIC_PLACEHOLDER, IrType};
    use crate::verify::{VerifyError, verify};

    /// The `__generic` sentinel as a value type.
    fn placeholder() -> IrType {
        IrType::StructRef(GENERIC_PLACEHOLDER.to_string(), Vec::new())
    }

    fn has_partial_generic_error(errors: &[VerifyError]) -> bool {
        errors
            .iter()
            .any(|e| e.message.contains("partially-generic type"))
    }

    fn concrete_func(name: &str) -> IrFunction {
        IrFunction::new(
            FuncId(0),
            name.into(),
            Vec::new(),
            Vec::new(),
            IrType::Void,
            None,
        )
    }

    #[test]
    fn partial_generic_on_block_param_is_flagged() {
        // `Result<Int, __generic>` on a block parameter — the shape a
        // phantom-error `Ok(_)` produces at a merge/phi join.
        let mut module = IrModule::new();
        let mut func = concrete_func("concrete");
        let entry = func.create_block();
        func.add_block_param(
            entry,
            IrType::EnumRef("Result".into(), vec![IrType::I64, placeholder()]),
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        assert!(
            has_partial_generic_error(&verify(&module)),
            "a partially-generic block param must be flagged"
        );
    }

    #[test]
    fn partial_generic_on_instruction_result_is_flagged() {
        // `Option<__generic>` as an instruction result — the value a `None`
        // whose context never pinned it materializes as. Operands need no
        // separate check: every operand is some other instruction's result,
        // so a placeholder used downstream is already caught at the
        // instruction that defines it (covered by this `result_type` walk).
        let mut module = IrModule::new();
        let mut func = concrete_func("concrete");
        let entry = func.create_block();
        func.emit(
            entry,
            Op::EnumAlloc("Option".into(), 1, Vec::new()),
            IrType::EnumRef("Option".into(), vec![placeholder()]),
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        assert!(
            has_partial_generic_error(&verify(&module)),
            "a partially-generic instruction result must be flagged"
        );
    }

    #[test]
    fn inert_placeholder_in_list_element_is_not_flagged() {
        // `List<__generic>` (an unconstrained empty literal's element) is
        // inert — it runs identically on every backend — so the verifier
        // must accept it. Guards against the over-broad invariant that
        // rejected working programs (`list_of_options`, et al.).
        let mut module = IrModule::new();
        let mut func = concrete_func("concrete");
        let entry = func.create_block();
        func.emit(
            entry,
            Op::ListAlloc(Vec::new()),
            IrType::ListRef(Box::new(placeholder())),
            None,
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        assert!(
            !has_partial_generic_error(&verify(&module)),
            "an inert placeholder (list element) must not be flagged"
        );
    }

    #[test]
    fn partial_generic_in_template_is_not_flagged() {
        // Templates carry unresolved types by design and are skipped by
        // `concrete_functions()`, so the same enum-arg placeholder rejected
        // in a concrete function is allowed here.
        let mut module = IrModule::new();
        let mut func = concrete_func("tmpl");
        let entry = func.create_block();
        func.add_block_param(
            entry,
            IrType::EnumRef("Result".into(), vec![IrType::I64, placeholder()]),
        );
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Template(func));

        assert!(
            !has_partial_generic_error(&verify(&module)),
            "a placeholder in a template must not be flagged"
        );
    }

    #[test]
    fn partial_generic_in_signature_only_is_covered_transitively_not_directly() {
        // Documents the walk's scope: it inspects *materialized value types*
        // (block params + instruction `result_type`s), not the function's
        // declared `return_type`/`params` fields directly. A function whose
        // declared `return_type` carries a placeholder but whose body never
        // materializes it (no block param, no instruction result of that
        // type) is therefore *not* flagged here. That is sound because a
        // value of the return type can't arise without surfacing as a checked
        // instruction result — and source can't spell `__generic`, so a
        // signature can't independently carry one. This test pins that
        // contract: if a future IR shape lets a partially-generic signature
        // exist without a corresponding materialized value, extend the walk.
        let mut module = IrModule::new();
        let mut func = IrFunction::new(
            FuncId(0),
            "concrete".into(),
            Vec::new(),
            Vec::new(),
            IrType::EnumRef("Result".into(), vec![IrType::I64, placeholder()]),
            None,
        );
        let entry = func.create_block();
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        assert!(
            !has_partial_generic_error(&verify(&module)),
            "the declared return_type is not walked directly — only \
             materialized values are"
        );
    }

    #[test]
    fn partial_generic_function_param_is_flagged_via_entry_block() {
        // The companion to the `signature_only` test above: a partially-
        // generic *parameter* type IS caught, because IR lowering binds each
        // declared parameter as a parameter of the entry block (see
        // `IrFunction::entry_block`'s invariant: "Function parameters are
        // bound as the parameters of this block"). The walk's coverage of
        // function parameters rests entirely on that binding — so this pins
        // it: a function whose declared `param_types` carry `Result<Int,
        // __generic>`, materialized as the entry block's param the way
        // lowering does, must be flagged. If a future lowering change stopped
        // binding params as entry-block params, the walk would silently miss
        // them; this test fails loudly instead.
        let mut module = IrModule::new();
        let param_ty = IrType::EnumRef("Result".into(), vec![IrType::I64, placeholder()]);
        let mut func = IrFunction::new(
            FuncId(0),
            "concrete".into(),
            vec![param_ty.clone()],
            vec!["p".into()],
            IrType::Void,
            None,
        );
        let entry = func.create_block();
        func.add_block_param(entry, param_ty);
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        assert!(
            has_partial_generic_error(&verify(&module)),
            "a partially-generic function parameter (bound as an entry-block \
             param) must be flagged"
        );
    }
}

#[cfg(test)]
mod structural_verifier_tests {
    //! Negative tests for the core structural invariants in
    //! [`verify_function`]. Companion to the four
    //! `unresolved_placeholder_op_tests` above and the `dyn`-op tests in
    //! [`super::dyn_ops`]. Each test constructs a minimal IR module that
    //! violates one specific invariant and asserts the verifier rejects
    //! it with a recognisable message.
    //!
    //! The historical `value_types`-desync invariant has no test here:
    //! [`crate::value_alloc::ValueIdAllocator`] makes
    //! [`crate::instruction::ValueId`] allocation and type-recording the
    //! same operation, so desync is structurally impossible.

    use crate::block::BlockId;
    use crate::instruction::{FuncId, Op, VOID_SENTINEL, ValueId};
    use crate::module::{ENV_PARAM_NAME, FunctionSlot, IrFunction, IrModule};
    use crate::terminator::Terminator;
    use crate::types::IrType;
    use crate::verify::verify;

    fn func_returning(ret: IrType) -> IrFunction {
        IrFunction::new(FuncId(0), "f".into(), Vec::new(), Vec::new(), ret, None)
    }

    fn empty_func() -> IrFunction {
        func_returning(IrType::Void)
    }

    #[test]
    fn terminator_arg_count_mismatch_is_flagged() {
        let mut module = IrModule::new();
        let mut func = empty_func();
        let entry = func.create_block();
        let target = func.create_block();
        // Target expects one Int param.
        func.add_block_param(target, IrType::I64);
        func.set_terminator(target, Terminator::Return(None));
        // Entry jumps to target with zero args.
        func.set_terminator(
            entry,
            Terminator::Jump {
                target,
                args: Vec::new(),
            },
        );
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("0 args") && e.message.contains("expects 1 params")),
            "expected arg-count mismatch, got: {errors:?}"
        );
    }

    #[test]
    fn instruction_operand_void_sentinel_is_flagged() {
        let mut module = IrModule::new();
        let mut func = empty_func();
        let entry = func.create_block();
        let lhs = func.add_block_param(entry, IrType::I64);
        // RHS is VOID_SENTINEL — the verifier must catch this.
        func.emit(entry, Op::IAdd(lhs, VOID_SENTINEL), IrType::I64, None);
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("instruction uses VOID_SENTINEL")),
            "expected VOID_SENTINEL operand error, got: {errors:?}"
        );
    }

    #[test]
    fn instruction_operand_undefined_is_flagged() {
        let mut module = IrModule::new();
        let mut func = empty_func();
        let entry = func.create_block();
        let lhs = func.add_block_param(entry, IrType::I64);
        // RHS is ValueId(99) — never allocated.
        func.emit(entry, Op::IAdd(lhs, ValueId(99)), IrType::I64, None);
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("instruction uses undefined value")),
            "expected undefined-operand error, got: {errors:?}"
        );
    }

    #[test]
    fn terminator_operand_void_sentinel_is_flagged() {
        let mut module = IrModule::new();
        let mut func = func_returning(IrType::I64);
        let entry = func.create_block();
        // Return VOID_SENTINEL from a function declared to return I64.
        func.set_terminator(entry, Terminator::Return(Some(VOID_SENTINEL)));
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("terminator uses VOID_SENTINEL")),
            "expected VOID_SENTINEL terminator-operand error, got: {errors:?}"
        );
    }

    #[test]
    fn terminator_operand_undefined_is_flagged() {
        let mut module = IrModule::new();
        let mut func = func_returning(IrType::I64);
        let entry = func.create_block();
        // Branch on a value that was never allocated.
        let other = func.create_block();
        func.set_terminator(other, Terminator::Return(None));
        func.set_terminator(
            entry,
            Terminator::Branch {
                condition: ValueId(99),
                true_block: other,
                true_args: Vec::new(),
                false_block: other,
                false_args: Vec::new(),
            },
        );
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("terminator uses undefined value")),
            "expected undefined terminator-operand error, got: {errors:?}"
        );
    }

    #[test]
    fn block_with_no_terminator_is_flagged() {
        let mut module = IrModule::new();
        let mut func = empty_func();
        // create_block leaves Terminator::None in place; never call
        // set_terminator on it.
        func.create_block();
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("has no terminator")),
            "expected missing-terminator error, got: {errors:?}"
        );
    }

    #[test]
    fn terminator_references_invalid_target_is_flagged() {
        let mut module = IrModule::new();
        let mut func = empty_func();
        let entry = func.create_block();
        // Jump to BlockId(99), which was never created.
        func.set_terminator(
            entry,
            Terminator::Jump {
                target: BlockId(99),
                args: Vec::new(),
            },
        );
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("references invalid target")),
            "expected invalid-target error, got: {errors:?}"
        );
    }

    #[test]
    fn well_formed_function_passes_verification() {
        // Sanity check: a minimal well-formed function verifies clean.
        // Catches regressions where a verifier change starts flagging
        // legitimate IR.
        let mut module = IrModule::new();
        let mut func = empty_func();
        let entry = func.create_block();
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(errors.is_empty(), "expected clean verify, got: {errors:?}");
    }

    /// `Op::ClosureLoadCapture(env, idx)` with `idx >= capture_types.len()`
    /// must be flagged. The op carries an immediate ordinal index (not a
    /// `ValueId`), so the generic undefined-operand walk doesn't catch
    /// it; this test pins the dedicated bounds check.
    #[test]
    fn closure_load_capture_out_of_range_is_flagged() {
        let mut module = IrModule::new();
        // Closure-shaped function: env is param 0, capture_types has
        // length 1 (a single Int capture). Emitting
        // `ClosureLoadCapture(env, 5)` is out of range.
        let mut func = IrFunction::new(
            FuncId(0),
            "__closure_oob".into(),
            vec![IrType::I64], // env-ptr; type detail doesn't matter for this check
            vec![ENV_PARAM_NAME.into()],
            IrType::Void,
            None,
        );
        func.capture_types = vec![IrType::I64];
        let entry = func.create_block();
        let env = func.add_block_param(entry, IrType::I64);
        func.emit(entry, Op::ClosureLoadCapture(env, 5), IrType::I64, None);
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("ClosureLoadCapture")
                    && e.message.contains("out of range")),
            "expected out-of-range capture-index error, got: {errors:?}"
        );
    }

    /// In-range `Op::ClosureLoadCapture` must not be flagged.
    #[test]
    fn closure_load_capture_in_range_passes_verification() {
        let mut module = IrModule::new();
        let mut func = IrFunction::new(
            FuncId(0),
            "__closure_ok".into(),
            vec![IrType::I64],
            vec![ENV_PARAM_NAME.into()],
            IrType::Void,
            None,
        );
        func.capture_types = vec![IrType::I64, IrType::I64];
        let entry = func.create_block();
        let env = func.add_block_param(entry, IrType::I64);
        func.emit(entry, Op::ClosureLoadCapture(env, 1), IrType::I64, None);
        func.set_terminator(entry, Terminator::Return(None));
        module.functions.push(FunctionSlot::Concrete(func));

        let errors = verify(&module);
        assert!(
            !errors
                .iter()
                .any(|e| e.message.contains("ClosureLoadCapture")),
            "expected clean verify on in-range ClosureLoadCapture, got: {errors:?}"
        );
    }
}
