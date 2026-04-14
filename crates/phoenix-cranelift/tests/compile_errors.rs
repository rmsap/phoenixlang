//! Regression tests for error paths in the Cranelift backend.

use phoenix_ir::instruction::{FuncId, Instruction, Op, ValueId};
use phoenix_ir::module::{IrFunction, IrModule};
use phoenix_ir::terminator::Terminator;
use phoenix_ir::types::IrType;

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
