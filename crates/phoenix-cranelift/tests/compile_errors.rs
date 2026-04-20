//! Regression tests for error paths in the Cranelift backend.

use phoenix_ir::instruction::{FuncId, Instruction, Op, ValueId};
use phoenix_ir::module::{IrFunction, IrModule};
use phoenix_ir::terminator::Terminator;
use phoenix_ir::types::IrType;

/// Shorthand for an unresolved generic-arg slot in a stdlib enum layout,
/// matching what `phoenix-ir/src/stdlib.rs` emits.
fn placeholder() -> IrType {
    IrType::StructRef(phoenix_ir::types::GENERIC_PLACEHOLDER.to_string())
}

/// Register the standard `Option` + `Result` enum layouts on `module`
/// using `GENERIC_PLACEHOLDER` payload slots, matching what the real
/// stdlib emits. Factored so payload-inference error tests can share a
/// single source of truth — if a layout detail changes (e.g. a new
/// variant), only this helper moves.
fn register_stdlib_option_result_layouts(module: &mut IrModule) {
    module.enum_layouts.insert(
        "Option".to_string(),
        vec![
            ("Some".to_string(), vec![placeholder()]),
            ("None".to_string(), vec![]),
        ],
    );
    module.enum_layouts.insert(
        "Result".to_string(),
        vec![
            ("Ok".to_string(), vec![placeholder()]),
            ("Err".to_string(), vec![placeholder()]),
        ],
    );
}

/// A `ClosureAlloc` that captures a value whose type
/// is not known must produce a compile error, not silently default to I64.
///
/// Construct a minimal IR module where `main` has a `ClosureAlloc`
/// referencing `ValueId(99)` — a value that was never defined by any
/// instruction.  Before the fix this would silently assume I64 and
/// produce corrupt code; after the fix it returns a `CompileError`.
#[test]
fn closure_with_unknown_capture_type_returns_error() {
    let mut module = IrModule::new();

    // Create a dummy closure target function.
    let closure_fid = FuncId(0);
    let mut closure_fn = IrFunction::new(
        closure_fid,
        "__closure_0".to_string(),
        vec![IrType::I64, IrType::I64], // one capture + one param
        vec!["__cap_x".to_string(), "n".to_string()],
        IrType::I64,
        None,
    );
    let bb0 = closure_fn.create_block();
    let _p0 = closure_fn.add_block_param(bb0, IrType::I64);
    let p1 = closure_fn.add_block_param(bb0, IrType::I64);
    closure_fn.set_terminator(bb0, Terminator::Return(Some(p1)));
    module.functions.push(closure_fn);
    module
        .function_index
        .insert("__closure_0".to_string(), closure_fid);

    // Create `main` with a ClosureAlloc that captures ValueId(99) — undefined.
    let main_fid = FuncId(1);
    let mut main_fn = IrFunction::new(
        main_fid,
        "main".to_string(),
        vec![],
        vec![],
        IrType::I64,
        None,
    );
    let bb0 = main_fn.create_block();
    // Emit a ClosureAlloc that captures ValueId(99), which doesn't exist.
    main_fn.block_mut(bb0).instructions.push(Instruction {
        result: Some(ValueId(50)),
        result_type: IrType::ClosureRef {
            param_types: vec![IrType::I64],
            return_type: Box::new(IrType::I64),
        },
        op: Op::ClosureAlloc(closure_fid, vec![ValueId(99)]),
        span: None,
    });
    let ret_val = main_fn.emit_value(bb0, Op::ConstI64(0), IrType::I64, None);
    main_fn.set_terminator(bb0, Terminator::Return(Some(ret_val)));
    module.functions.push(main_fn);
    module.function_index.insert("main".to_string(), main_fid);

    let result = phoenix_cranelift::compile(&module);
    assert!(
        result.is_err(),
        "compile should fail when a closure capture has unknown type"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("unknown type for closure capture"),
        "error should mention unknown capture type, got: {err_msg}"
    );
}

/// A `BuiltinCall("String.split", ...)` must produce a compile error because
/// `split` returns a `List<String>` and List support is not yet implemented.
///
/// Construct a minimal IR module where `main` calls `String.split` on a
/// string literal.  The Cranelift backend should return an error rather
/// than panicking.
#[test]
fn unsupported_string_method_returns_error() {
    let mut module = IrModule::new();

    let main_fid = FuncId(0);
    let mut main_fn = IrFunction::new(
        main_fid,
        "main".to_string(),
        vec![],
        vec![],
        IrType::Void,
        None,
    );
    let bb0 = main_fn.create_block();

    // Emit a string constant (the receiver).
    let s = main_fn.emit_value(
        bb0,
        Op::ConstString("hello world".to_string()),
        IrType::StringRef,
        None,
    );
    // Emit a call to a non-existent string method.
    main_fn.block_mut(bb0).instructions.push(Instruction {
        result: Some(ValueId(100)),
        result_type: IrType::Void,
        op: Op::BuiltinCall("String.nonexistent".to_string(), vec![s]),
        span: None,
    });
    main_fn.set_terminator(bb0, Terminator::Return(None));
    module.functions.push(main_fn);
    module.function_index.insert("main".to_string(), main_fid);

    let result = phoenix_cranelift::compile(&module);
    assert!(
        result.is_err(),
        "compile should fail for unsupported string method 'nonexistent'"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("string method 'nonexistent' not yet supported"),
        "error should name the method and its carrier, got: {err_msg}"
    );
}

/// Calling a nonexistent method on a `List` should produce a compile error.
#[test]
fn unsupported_list_method_returns_error() {
    let mut module = IrModule::new();

    let main_fid = FuncId(0);
    let mut main_fn = IrFunction::new(
        main_fid,
        "main".to_string(),
        vec![],
        vec![],
        IrType::Void,
        None,
    );
    let bb0 = main_fn.create_block();

    // Emit a dummy list value (an i64 standing in for the list pointer).
    let list_val = main_fn.emit_value(
        bb0,
        Op::ConstI64(0),
        IrType::ListRef(Box::new(IrType::I64)),
        None,
    );
    // Call a non-existent list method.
    main_fn.block_mut(bb0).instructions.push(Instruction {
        result: Some(ValueId(100)),
        result_type: IrType::Void,
        op: Op::BuiltinCall("List.nonexistent".to_string(), vec![list_val]),
        span: None,
    });
    main_fn.set_terminator(bb0, Terminator::Return(None));
    module.functions.push(main_fn);
    module.function_index.insert("main".to_string(), main_fid);

    let result = phoenix_cranelift::compile(&module);
    assert!(
        result.is_err(),
        "compile should fail for unsupported list method 'nonexistent'"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("list method 'nonexistent' not yet supported"),
        "error should name the method and its carrier, got: {err_msg}"
    );
}

/// Calling a nonexistent method on a `Map` should produce a compile error.
#[test]
fn unsupported_map_method_returns_error() {
    let mut module = IrModule::new();

    let main_fid = FuncId(0);
    let mut main_fn = IrFunction::new(
        main_fid,
        "main".to_string(),
        vec![],
        vec![],
        IrType::Void,
        None,
    );
    let bb0 = main_fn.create_block();

    // Emit a dummy map value.
    let map_val = main_fn.emit_value(
        bb0,
        Op::ConstI64(0),
        IrType::MapRef(Box::new(IrType::StringRef), Box::new(IrType::I64)),
        None,
    );
    // Call a non-existent map method.
    main_fn.block_mut(bb0).instructions.push(Instruction {
        result: Some(ValueId(100)),
        result_type: IrType::Void,
        op: Op::BuiltinCall("Map.nonexistent".to_string(), vec![map_val]),
        span: None,
    });
    main_fn.set_terminator(bb0, Terminator::Return(None));
    module.functions.push(main_fn);
    module.function_index.insert("main".to_string(), main_fid);

    let result = phoenix_cranelift::compile(&module);
    assert!(
        result.is_err(),
        "compile should fail for unsupported map method 'nonexistent'"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("map method 'nonexistent' not yet supported"),
        "error should name the method and its carrier, got: {err_msg}"
    );
}

/// Calling a nonexistent method on an `Option` should produce a compile error.
#[test]
fn unsupported_option_method_returns_error() {
    let mut module = IrModule::new();

    let main_fid = FuncId(0);
    let mut main_fn = IrFunction::new(
        main_fid,
        "main".to_string(),
        vec![],
        vec![],
        IrType::Void,
        None,
    );
    let bb0 = main_fn.create_block();

    // Emit a dummy Option value (tag + payload = 2 slots, represented as i64).
    let opt_val = main_fn.emit_value(
        bb0,
        Op::ConstI64(0),
        IrType::EnumRef("Option".to_string(), Vec::new()),
        None,
    );
    // Call a non-existent option method.
    main_fn.block_mut(bb0).instructions.push(Instruction {
        result: Some(ValueId(100)),
        result_type: IrType::Void,
        op: Op::BuiltinCall("Option.nonexistent".to_string(), vec![opt_val]),
        span: None,
    });
    main_fn.set_terminator(bb0, Terminator::Return(None));
    module.functions.push(main_fn);
    module.function_index.insert("main".to_string(), main_fid);

    let result = phoenix_cranelift::compile(&module);
    assert!(
        result.is_err(),
        "compile should fail for unsupported option method 'nonexistent'"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("option method 'nonexistent' not yet supported"),
        "error should name the method and its carrier, got: {err_msg}"
    );
}

/// Calling a nonexistent method on a `Result` should produce a compile error.
#[test]
fn unsupported_result_method_returns_error() {
    let mut module = IrModule::new();

    let main_fid = FuncId(0);
    let mut main_fn = IrFunction::new(
        main_fid,
        "main".to_string(),
        vec![],
        vec![],
        IrType::Void,
        None,
    );
    let bb0 = main_fn.create_block();

    // Emit a dummy Result value (tag + ok_payload + err_payload).
    let res_val = main_fn.emit_value(
        bb0,
        Op::ConstI64(0),
        IrType::EnumRef("Result".to_string(), Vec::new()),
        None,
    );
    // Call a non-existent result method.
    main_fn.block_mut(bb0).instructions.push(Instruction {
        result: Some(ValueId(100)),
        result_type: IrType::Void,
        op: Op::BuiltinCall("Result.nonexistent".to_string(), vec![res_val]),
        span: None,
    });
    main_fn.set_terminator(bb0, Terminator::Return(None));
    module.functions.push(main_fn);
    module.function_index.insert("main".to_string(), main_fid);

    let result = phoenix_cranelift::compile(&module);
    assert!(
        result.is_err(),
        "compile should fail for unsupported result method 'nonexistent'"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("result method 'nonexistent' not yet supported"),
        "error should name the method and its carrier, got: {err_msg}"
    );
}

/// `Option<String>.okOr()` on a value whose
/// payload type cannot be inferred (e.g. a function parameter) must produce
/// a compile error rather than silently falling back to `I64` and miscompiling.
#[test]
fn option_okor_unknown_payload_type_returns_error() {
    let mut module = IrModule::new();
    register_stdlib_option_result_layouts(&mut module);

    // Build a function that uses an Option value with no EnumAlloc origin.
    // No EnumAlloc in this function → all inference strategies fail.
    let main_fid = FuncId(0);
    let mut main_fn = IrFunction::new(
        main_fid,
        "main".to_string(),
        vec![],
        vec![],
        IrType::Void,
        None,
    );
    let bb0 = main_fn.create_block();

    // Simulate an Option value with no EnumAlloc origin and no generic args
    // (e.g. a synthetic IR fragment that bypasses sema's type flow). Using
    // `EnumRef("Option", vec![])` forces all inference strategies to fail:
    // Strategy 0 sees empty args, Strategies 2-4 fail for the reasons listed
    // below.
    let opt_param = main_fn.emit_value(
        bb0,
        Op::ConstI64(0),
        IrType::EnumRef("Option".to_string(), Vec::new()),
        None,
    );
    // The error argument for okOr.
    let err_val = main_fn.emit_value(bb0, Op::ConstI64(0), IrType::I64, None);
    // Call Option.okOr — should fail because T cannot be inferred.
    main_fn.block_mut(bb0).instructions.push(Instruction {
        result: Some(ValueId(100)),
        result_type: IrType::EnumRef("Result".to_string(), Vec::new()),
        op: Op::BuiltinCall("Option.okOr".to_string(), vec![opt_param, err_val]),
        span: None,
    });
    main_fn.set_terminator(bb0, Terminator::Return(None));
    module.functions.push(main_fn);
    module.function_index.insert("main".to_string(), main_fid);

    let result = phoenix_cranelift::compile(&module);
    assert!(
        result.is_err(),
        "compile should fail when Option payload type cannot be inferred for okOr"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("could not infer Option payload type"),
        "error should name the unresolved enum, got: {err_msg}"
    );
    assert!(
        err_msg.contains("'okOr'"),
        "error should name the offending method, got: {err_msg}"
    );
    // Guard against the old silent-miscompile behavior leaking back into
    // the error path as a misleading hint — the fix is for lowering to
    // propagate args, not for the user to switch methods.
    assert!(
        !err_msg.contains("try"),
        "error should not suggest a workaround, got: {err_msg}"
    );
}

/// `okOr` on an `Option` whose generic arg is the `GENERIC_PLACEHOLDER`
/// sentinel must still fail — Strategy 0 in `try_type_from_enum_args`
/// explicitly rejects placeholder args so inference falls through to the
/// fallback strategies, which in this synthetic setup have nothing to
/// work with.  Locks in the `is_generic_placeholder` branch.
#[test]
fn option_okor_placeholder_arg_returns_error() {
    let mut module = IrModule::new();
    register_stdlib_option_result_layouts(&mut module);

    let main_fid = FuncId(0);
    let mut main_fn = IrFunction::new(
        main_fid,
        "main".to_string(),
        vec![],
        vec![],
        IrType::Void,
        None,
    );
    let bb0 = main_fn.create_block();

    // Args contain the GENERIC_PLACEHOLDER sentinel — Strategy 0 must
    // skip this (same outcome as empty args) rather than returning the
    // sentinel as the payload type.
    let opt_param = main_fn.emit_value(
        bb0,
        Op::ConstI64(0),
        IrType::EnumRef("Option".to_string(), vec![placeholder()]),
        None,
    );
    let err_val = main_fn.emit_value(bb0, Op::ConstI64(0), IrType::I64, None);
    main_fn.block_mut(bb0).instructions.push(Instruction {
        result: Some(ValueId(100)),
        result_type: IrType::EnumRef("Result".to_string(), Vec::new()),
        op: Op::BuiltinCall("Option.okOr".to_string(), vec![opt_param, err_val]),
        span: None,
    });
    main_fn.set_terminator(bb0, Terminator::Return(None));
    module.functions.push(main_fn);
    module.function_index.insert("main".to_string(), main_fid);

    let result = phoenix_cranelift::compile(&module);
    assert!(
        result.is_err(),
        "placeholder-arg Option must still error — Strategy 0 should reject the sentinel"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("could not infer Option payload type"),
        "error should name the unresolved enum, got: {err_msg}"
    );
    assert!(
        err_msg.contains("'okOr'"),
        "error should name the offending method, got: {err_msg}"
    );
}

/// Result mirror of `option_okor_placeholder_arg_returns_error`:
/// `try_result_payload_types_from_args` must reject `GENERIC_PLACEHOLDER`
/// args in the Ok/Err slots rather than returning the sentinel as a real
/// payload type. With a single-element closure-arg method like
/// `Result.mapErr`, filtering Err lets Strategy 3 fill Err from the closure
/// arg, but Ok remains unresolvable and the compile must fail.
#[test]
fn result_map_err_placeholder_args_returns_error() {
    let mut module = IrModule::new();
    register_stdlib_option_result_layouts(&mut module);

    let main_fid = FuncId(0);
    let mut main_fn = IrFunction::new(
        main_fid,
        "main".to_string(),
        vec![],
        vec![],
        IrType::Void,
        None,
    );
    let bb0 = main_fn.create_block();

    // Both Result args are placeholders — Strategy 0 must reject both.
    let res_param = main_fn.emit_value(
        bb0,
        Op::ConstI64(0),
        IrType::EnumRef("Result".to_string(), vec![placeholder(), placeholder()]),
        None,
    );
    // Non-closure second arg so Strategy 3 (`try_type_from_closure_arg`)
    // cannot seed either slot from the method argument.
    let non_closure = main_fn.emit_value(bb0, Op::ConstI64(0), IrType::I64, None);
    main_fn.block_mut(bb0).instructions.push(Instruction {
        result: Some(ValueId(100)),
        result_type: IrType::EnumRef("Result".to_string(), Vec::new()),
        op: Op::BuiltinCall("Result.mapErr".to_string(), vec![res_param, non_closure]),
        span: None,
    });
    main_fn.set_terminator(bb0, Terminator::Return(None));
    module.functions.push(main_fn);
    module.function_index.insert("main".to_string(), main_fid);

    let result = phoenix_cranelift::compile(&module);
    assert!(
        result.is_err(),
        "placeholder-arg Result must still error — Strategy 0 should reject the sentinels"
    );
    let err_msg = result.unwrap_err().to_string();
    // `mapErr` only needs the Err slot (Ok isn't read by the method), and
    // the second arg isn't a closure here — so Strategy 3 can't fill Err
    // from `try_type_from_closure_arg`. The error must name the Err slot
    // specifically, not just "Result" generically.
    assert!(
        err_msg.contains("could not infer Result Err type"),
        "error should name the specific unresolved slot (Err), got: {err_msg}"
    );
    assert!(
        err_msg.contains("'mapErr'"),
        "error should name the offending method, got: {err_msg}"
    );
}

/// The `find_closure_capture_types` ambiguity error path in
/// `ir_analysis.rs` is documented and handled but cannot be tested at the
/// integration level without constructing a multi-function IR module with
/// valid closure bodies — Cranelift's verifier rejects synthetic closure
/// stubs.  The error path is covered by code review and documentation
/// (see `ir_analysis.rs` "Known limitation" comment).
#[test]
#[ignore = "requires synthetic multi-closure IR module; see ir_analysis.rs known limitation"]
fn closure_capture_ambiguity_error() {
    // This test is a placeholder to track the untested error path.
    // When the closure representation is enriched to carry capture metadata,
    // this test should be implemented to verify the ambiguity error.
}
