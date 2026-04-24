//! Core IR interpreter: execution loop, call frames, instruction dispatch.

use crate::builtins;
use crate::error::{IrRuntimeError, Result, error};
use crate::value::{EnumData, IrValue, StructData};
use phoenix_ir::block::BlockId;
use phoenix_ir::instruction::{FuncId, Op, ValueId};
use phoenix_ir::module::{IrFunction, IrModule};
use phoenix_ir::terminator::Terminator;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::rc::Rc;

/// Maximum call depth before reporting a stack overflow.
const MAX_CALL_DEPTH: usize = 256;

/// A call frame on the interpreter stack.
struct CallFrame {
    /// SSA value store indexed by `ValueId.0`. `None` means the slot has not
    /// been written yet (accessing it is an IR bug).
    values: Vec<Option<IrValue>>,
    /// Mutable slots from `Alloca`: the ValueId of the alloca -> current value.
    alloca_slots: HashMap<ValueId, IrValue>,
}

impl CallFrame {
    fn new(value_count: u32) -> Self {
        Self {
            values: vec![None; value_count as usize],
            alloca_slots: HashMap::new(),
        }
    }

    fn get(&self, vid: ValueId) -> Result<&IrValue> {
        self.values
            .get(vid.0 as usize)
            .and_then(|slot| slot.as_ref())
            .ok_or_else(|| IrRuntimeError {
                message: format!("undefined value {vid}"),
            })
    }

    fn set(&mut self, vid: ValueId, val: IrValue) {
        let idx = vid.0 as usize;
        if idx >= self.values.len() {
            self.values.resize(idx + 1, None);
        }
        self.values[idx] = Some(val);
    }
}

/// Bind block arguments: resolve source `ValueId`s in the current frame and
/// assign them to the target block's parameters.
fn bind_block_args(
    frame: &mut CallFrame,
    func: &IrFunction,
    target: BlockId,
    source_args: &[ValueId],
) -> Result<()> {
    let arg_vals: Vec<IrValue> = source_args
        .iter()
        .map(|vid| frame.get(*vid).cloned())
        .collect::<Result<_>>()?;
    let target_block = &func.blocks[target.0 as usize];
    for (i, (param_vid, _)) in target_block.params.iter().enumerate() {
        if let Some(v) = arg_vals.get(i) {
            frame.set(*param_vid, v.clone());
        }
    }
    Ok(())
}

/// The IR interpreter.
pub struct IrInterpreter<'m> {
    /// The IR module being executed.
    module: &'m IrModule,
    /// Output sink for `print()`.
    output: Box<dyn Write>,
    /// Current call depth (for stack overflow detection).
    depth: usize,
}

impl<'m> IrInterpreter<'m> {
    /// Create a new interpreter for the given module.
    pub fn new(module: &'m IrModule, output: Box<dyn Write>) -> Self {
        Self {
            module,
            output,
            depth: 0,
        }
    }

    /// Run the `main` function.
    pub fn run(&mut self) -> Result<()> {
        let main_id = self
            .module
            .function_index
            .get("main")
            .copied()
            .ok_or_else(|| IrRuntimeError {
                message: "no main function found".to_string(),
            })?;
        self.call_function(main_id, vec![])?;
        Ok(())
    }

    /// Call a function by ID with arguments. Returns the return value.
    fn call_function(&mut self, func_id: FuncId, args: Vec<IrValue>) -> Result<IrValue> {
        if self.depth >= MAX_CALL_DEPTH {
            return error("stack overflow: maximum call depth exceeded");
        }

        let func = &self.module.functions[func_id.0 as usize];
        let mut frame = CallFrame::new(func.value_count());

        // Bind arguments to entry block parameters.
        let entry = &func.blocks[0];
        let expected = entry.params.len();
        let actual = args.len();
        if actual != expected {
            return error(format!(
                "function {} expects {} argument(s), got {}",
                func.name, expected, actual,
            ));
        }
        for (i, (param_vid, _)) in entry.params.iter().enumerate() {
            frame.set(*param_vid, args[i].clone());
        }

        self.depth += 1;
        let result = self.execute_function(func, &mut frame);
        self.depth -= 1;
        result
    }

    /// Execute a function from its entry block.
    fn execute_function(&mut self, func: &IrFunction, frame: &mut CallFrame) -> Result<IrValue> {
        let mut block_id = BlockId(0);

        loop {
            let block = &func.blocks[block_id.0 as usize];

            // Execute instructions.
            for inst in &block.instructions {
                let val = self.execute_op(&inst.op, frame)?;
                if let Some(result_vid) = inst.result {
                    frame.set(result_vid, val);
                }
            }

            // Execute terminator.
            match &block.terminator {
                Terminator::Return(val) => {
                    return if let Some(vid) = val {
                        Ok(frame.get(*vid)?.clone())
                    } else {
                        Ok(IrValue::Void)
                    };
                }
                Terminator::Jump { target, args } => {
                    bind_block_args(frame, func, *target, args)?;
                    block_id = *target;
                }
                Terminator::Branch {
                    condition,
                    true_block,
                    true_args,
                    false_block,
                    false_args,
                } => {
                    let cond = frame.get(*condition)?;
                    let (target, args) = match cond {
                        IrValue::Bool(true) => (*true_block, true_args),
                        IrValue::Bool(false) => (*false_block, false_args),
                        other => {
                            return error(format!("branch condition must be Bool, got {other}"));
                        }
                    };
                    bind_block_args(frame, func, target, args)?;
                    block_id = target;
                }
                Terminator::Switch {
                    value,
                    cases,
                    default,
                    default_args,
                } => {
                    let disc = frame.get(*value)?;
                    let disc_val = match disc {
                        IrValue::Int(n) => *n as u32,
                        _ => return error("switch on non-integer value"),
                    };
                    let mut matched = false;
                    for (case_val, target, case_args) in cases {
                        if disc_val == *case_val {
                            bind_block_args(frame, func, *target, case_args)?;
                            block_id = *target;
                            matched = true;
                            break;
                        }
                    }
                    if !matched {
                        bind_block_args(frame, func, *default, default_args)?;
                        block_id = *default;
                    }
                }
                Terminator::Unreachable => {
                    return error("reached unreachable code");
                }
                Terminator::None => {
                    return error("reached block with no terminator");
                }
            }
        }
    }

    /// Dispatch a trait-object method call: look up
    /// `(concrete_type, trait_name)` in `dyn_vtables`, select slot
    /// `method_idx`, and invoke the resolved `FuncId` with the concrete
    /// receiver prepended to `args`. Matches the Cranelift-backend path
    /// (`translate/dyn_trait.rs::translate_dyn_call`) semantically so
    /// IR-interp and compiled output stay in lockstep.
    fn interpret_dyn_call(
        &mut self,
        trait_name: &str,
        method_idx: u32,
        receiver: IrValue,
        args: Vec<IrValue>,
    ) -> Result<IrValue> {
        let IrValue::Dyn {
            concrete,
            concrete_type,
            trait_name: recv_trait,
        } = receiver
        else {
            return error(format!(
                "DynCall receiver is not a `dyn` value: {:?}",
                receiver
            ));
        };
        // Hard check (not `debug_assert`): vtable-wiring bugs that make a
        // release build silently load the wrong method would be invisible
        // under an asserts-stripped build.
        if recv_trait != trait_name {
            return error(format!(
                "DynCall trait mismatch: receiver carries `dyn {recv_trait}` \
                 but the call site is `dyn {trait_name}`"
            ));
        }
        let key = (concrete_type.clone(), trait_name.to_string());
        let vtable = self
            .module
            .dyn_vtables
            .get(&key)
            .ok_or_else(|| IrRuntimeError {
                message: format!("DynCall: no vtable for ({concrete_type}, dyn {trait_name})"),
            })?;
        let (_name, func_id) = vtable
            .get(method_idx as usize)
            .ok_or_else(|| IrRuntimeError {
                message: format!("DynCall: slot {method_idx} out of range for dyn {trait_name}"),
            })?;
        let func_id = *func_id;
        // Cross-backend ABI contract: prepend the concrete receiver as
        // the first argument, matching `self: StructRef/EnumRef(...)`
        // at index 0 of the trait method's `FuncId`. The Cranelift
        // backend does the same in
        // `phoenix-cranelift/src/translate/dyn_trait.rs::build_dyn_call_signature`.
        // Keep the two sites in lockstep — divergence here is silent
        // wrong-dispatch with no verifier signal.
        let mut full_args: Vec<IrValue> = vec![*concrete];
        full_args.extend(args);
        self.call_function(func_id, full_args)
    }

    /// Execute a single IR operation.
    fn execute_op(&mut self, op: &Op, frame: &mut CallFrame) -> Result<IrValue> {
        match op {
            // --- Constants ---
            Op::ConstI64(n) => Ok(IrValue::Int(*n)),
            Op::ConstF64(n) => Ok(IrValue::Float(*n)),
            Op::ConstBool(b) => Ok(IrValue::Bool(*b)),
            Op::ConstString(s) => Ok(IrValue::String(s.clone())),

            // --- Arithmetic ---
            Op::IAdd(..)
            | Op::ISub(..)
            | Op::IMul(..)
            | Op::IDiv(..)
            | Op::IMod(..)
            | Op::INeg(..)
            | Op::FAdd(..)
            | Op::FSub(..)
            | Op::FMul(..)
            | Op::FDiv(..)
            | Op::FMod(..)
            | Op::FNeg(..) => self.execute_arithmetic(op, frame),

            // --- Comparisons, logic, and string ops ---
            Op::IEq(..)
            | Op::INe(..)
            | Op::ILt(..)
            | Op::IGt(..)
            | Op::ILe(..)
            | Op::IGe(..)
            | Op::FEq(..)
            | Op::FNe(..)
            | Op::FLt(..)
            | Op::FGt(..)
            | Op::FLe(..)
            | Op::FGe(..)
            | Op::StringEq(..)
            | Op::StringNe(..)
            | Op::StringLt(..)
            | Op::StringGt(..)
            | Op::StringLe(..)
            | Op::StringGe(..)
            | Op::BoolEq(..)
            | Op::BoolNe(..)
            | Op::BoolNot(..)
            | Op::StringConcat(..) => self.execute_comparison(op, frame),

            // --- Struct operations ---
            Op::StructAlloc(name, field_vids) => {
                let fields: Vec<IrValue> = field_vids
                    .iter()
                    .map(|vid| frame.get(*vid).cloned())
                    .collect::<Result<_>>()?;
                Ok(IrValue::Struct(Rc::new(RefCell::new(StructData {
                    name: name.clone(),
                    fields,
                }))))
            }
            Op::StructGetField(obj_vid, idx) => {
                let obj = frame.get(*obj_vid)?;
                if let IrValue::Struct(data) = obj {
                    let data = data.borrow();
                    data.fields
                        .get(*idx as usize)
                        .cloned()
                        .ok_or_else(|| IrRuntimeError {
                            message: format!("struct field index {} out of bounds", idx),
                        })
                } else {
                    error(format!("StructGetField on non-struct value: {obj}"))
                }
            }
            Op::StructSetField(obj_vid, idx, val_vid) => {
                let val = frame.get(*val_vid)?.clone();
                let obj = frame.get(*obj_vid)?;
                if let IrValue::Struct(data) = obj {
                    let mut data = data.borrow_mut();
                    if (*idx as usize) >= data.fields.len() {
                        return error(format!(
                            "struct field index {} out of bounds ({})",
                            idx,
                            data.fields.len(),
                        ));
                    }
                    data.fields[*idx as usize] = val;
                } else {
                    return error(format!("StructSetField on non-struct value: {obj}"));
                }
                Ok(IrValue::Void)
            }

            // --- Enum operations ---
            Op::EnumAlloc(enum_name, variant_idx, field_vids) => {
                let fields: Vec<IrValue> = field_vids
                    .iter()
                    .map(|vid| frame.get(*vid).cloned())
                    .collect::<Result<_>>()?;
                Ok(IrValue::EnumVariant(Rc::new(RefCell::new(EnumData {
                    enum_name: enum_name.clone(),
                    discriminant: *variant_idx,
                    fields,
                }))))
            }
            Op::EnumDiscriminant(vid) => {
                let val = frame.get(*vid)?;
                match val {
                    IrValue::EnumVariant(data) => {
                        Ok(IrValue::Int(data.borrow().discriminant as i64))
                    }
                    _ => error(format!("EnumDiscriminant on non-enum value: {val}")),
                }
            }
            Op::EnumGetField(vid, variant_idx, idx) => {
                let val = frame.get(*vid)?;
                match val {
                    IrValue::EnumVariant(data) => {
                        let data = data.borrow();
                        // Validate that the IR's variant_idx matches the
                        // runtime discriminant.  A mismatch indicates a bug
                        // in the IR lowering (e.g. extracting a field from
                        // the wrong variant).
                        if data.discriminant != *variant_idx {
                            return error(format!(
                                "EnumGetField variant_idx ({}) does not match runtime \
                                 discriminant ({}) — IR lowering may have emitted the \
                                 wrong variant index",
                                variant_idx, data.discriminant,
                            ));
                        }
                        data.fields
                            .get(*idx as usize)
                            .cloned()
                            .ok_or_else(|| IrRuntimeError {
                                message: format!("enum field index {} out of bounds", idx),
                            })
                    }
                    _ => error(format!("EnumGetField on non-enum value: {val}")),
                }
            }

            // --- Collection operations ---
            Op::ListAlloc(vids) => {
                let elems: Vec<IrValue> = vids
                    .iter()
                    .map(|vid| frame.get(*vid).cloned())
                    .collect::<Result<_>>()?;
                Ok(IrValue::new_list(elems))
            }
            Op::MapAlloc(pairs) => {
                let entries: Vec<(IrValue, IrValue)> = pairs
                    .iter()
                    .map(|(k, v)| Ok((frame.get(*k)?.clone(), frame.get(*v)?.clone())))
                    .collect::<Result<_>>()?;
                Ok(IrValue::new_map(entries))
            }

            // --- Closure operations ---
            Op::ClosureAlloc(func_id, capture_vids) => {
                let captures: Vec<IrValue> = capture_vids
                    .iter()
                    .map(|vid| frame.get(*vid).cloned())
                    .collect::<Result<_>>()?;
                Ok(IrValue::Closure(*func_id, captures))
            }

            // --- Function calls ---
            Op::Call(func_id, type_args, arg_vids) => {
                // Compiler-invariant violation if non-empty — keep live in
                // release builds too (not `debug_assert!`).
                assert!(
                    type_args.is_empty(),
                    "Op::Call reached interpreter with non-empty type_args ({type_args:?}) \
                     — monomorphization should have cleared them"
                );
                let args: Vec<IrValue> = arg_vids
                    .iter()
                    .map(|vid| frame.get(*vid).cloned())
                    .collect::<Result<_>>()?;
                self.call_function(*func_id, args)
            }
            Op::CallIndirect(callee_vid, arg_vids) => {
                let callee = frame.get(*callee_vid)?.clone();
                let user_args: Vec<IrValue> = arg_vids
                    .iter()
                    .map(|vid| frame.get(*vid).cloned())
                    .collect::<Result<_>>()?;
                self.call_closure(&callee, user_args)
            }
            Op::BuiltinCall(name, arg_vids) => {
                let args: Vec<IrValue> = arg_vids
                    .iter()
                    .map(|vid| frame.get(*vid).cloned())
                    .collect::<Result<_>>()?;
                builtins::dispatch(self, name, args)
            }
            Op::UnresolvedTraitMethod(method, _, _) => error(format!(
                "internal error: unresolved trait-bound method call `.{method}` \
                 reached the IR interpreter — monomorphization was expected to \
                 rewrite it to a concrete Op::Call"
            )),

            // --- Trait object operations ---
            Op::UnresolvedDynAlloc(trait_name, _) => error(format!(
                "internal error: unresolved dyn-alloc coercion into `@{trait_name}` \
                 reached the IR interpreter — monomorphization was expected to \
                 rewrite it to a concrete Op::DynAlloc"
            )),
            Op::DynAlloc(trait_name, concrete_type, value_vid) => {
                let concrete = frame.get(*value_vid)?.clone();
                Ok(IrValue::Dyn {
                    concrete: Box::new(concrete),
                    concrete_type: concrete_type.clone(),
                    trait_name: trait_name.clone(),
                })
            }
            Op::DynCall(trait_name, method_idx, receiver_vid, arg_vids) => {
                let receiver = frame.get(*receiver_vid)?.clone();
                let args: Vec<IrValue> = arg_vids
                    .iter()
                    .map(|vid| frame.get(*vid).cloned())
                    .collect::<Result<_>>()?;
                self.interpret_dyn_call(trait_name, *method_idx, receiver, args)
            }

            // --- Mutable variables ---
            Op::Alloca(_ty) => {
                // Create a fresh alloca slot. The ValueId for this instruction
                // will be used as the slot key in alloca_slots.
                // We don't know the ValueId here — the caller stores it.
                // The default value doesn't matter as Store always happens
                // before Load for well-formed IR.
                Ok(IrValue::Void) // Placeholder; see alloca handling below.
            }
            Op::Load(slot_vid) => {
                if let Some(val) = frame.alloca_slots.get(slot_vid) {
                    Ok(val.clone())
                } else {
                    error(format!("load from uninitialized alloca slot {slot_vid}"))
                }
            }
            Op::Store(slot_vid, val_vid) => {
                let val = frame.get(*val_vid)?.clone();
                frame.alloca_slots.insert(*slot_vid, val);
                Ok(IrValue::Void)
            }

            // --- Miscellaneous ---
            Op::Copy(vid) => Ok(frame.get(*vid)?.clone()),
        }
    }

    // --- Arithmetic sub-dispatch ---

    /// Execute integer and float arithmetic operations.
    fn execute_arithmetic(&self, op: &Op, frame: &CallFrame) -> Result<IrValue> {
        match op {
            Op::IAdd(a, b) => {
                let (a, b) = (self.get_int(frame, *a)?, self.get_int(frame, *b)?);
                Ok(IrValue::Int(a.wrapping_add(b)))
            }
            Op::ISub(a, b) => {
                let (a, b) = (self.get_int(frame, *a)?, self.get_int(frame, *b)?);
                Ok(IrValue::Int(a.wrapping_sub(b)))
            }
            Op::IMul(a, b) => {
                let (a, b) = (self.get_int(frame, *a)?, self.get_int(frame, *b)?);
                Ok(IrValue::Int(a.wrapping_mul(b)))
            }
            Op::IDiv(a, b) => {
                let (a, b) = (self.get_int(frame, *a)?, self.get_int(frame, *b)?);
                if b == 0 {
                    return error("division by zero");
                }
                Ok(IrValue::Int(a.wrapping_div(b)))
            }
            Op::IMod(a, b) => {
                let (a, b) = (self.get_int(frame, *a)?, self.get_int(frame, *b)?);
                if b == 0 {
                    return error("modulo by zero");
                }
                Ok(IrValue::Int(a.wrapping_rem(b)))
            }
            Op::INeg(a) => {
                let a = self.get_int(frame, *a)?;
                Ok(IrValue::Int(a.wrapping_neg()))
            }
            Op::FAdd(a, b) => {
                let (a, b) = (self.get_float(frame, *a)?, self.get_float(frame, *b)?);
                Ok(IrValue::Float(a + b))
            }
            Op::FSub(a, b) => {
                let (a, b) = (self.get_float(frame, *a)?, self.get_float(frame, *b)?);
                Ok(IrValue::Float(a - b))
            }
            Op::FMul(a, b) => {
                let (a, b) = (self.get_float(frame, *a)?, self.get_float(frame, *b)?);
                Ok(IrValue::Float(a * b))
            }
            Op::FDiv(a, b) => {
                let (a, b) = (self.get_float(frame, *a)?, self.get_float(frame, *b)?);
                // No zero check: IEEE 754 produces inf/NaN, matching Cranelift's
                // `fdiv` instruction (the compiler does not guard float division).
                Ok(IrValue::Float(a / b))
            }
            Op::FMod(a, b) => {
                let (a, b) = (self.get_float(frame, *a)?, self.get_float(frame, *b)?);
                // No zero check: matches compiler semantics (IEEE 754 NaN).
                Ok(IrValue::Float(a % b))
            }
            Op::FNeg(a) => {
                let a = self.get_float(frame, *a)?;
                Ok(IrValue::Float(-a))
            }
            _ => unreachable!(),
        }
    }

    // --- Comparison sub-dispatch ---

    /// Execute comparison, boolean logic, and string concatenation operations.
    fn execute_comparison(&self, op: &Op, frame: &CallFrame) -> Result<IrValue> {
        match op {
            Op::IEq(a, b) => {
                let (a, b) = (self.get_int(frame, *a)?, self.get_int(frame, *b)?);
                Ok(IrValue::Bool(a == b))
            }
            Op::INe(a, b) => {
                let (a, b) = (self.get_int(frame, *a)?, self.get_int(frame, *b)?);
                Ok(IrValue::Bool(a != b))
            }
            Op::ILt(a, b) => {
                let (a, b) = (self.get_int(frame, *a)?, self.get_int(frame, *b)?);
                Ok(IrValue::Bool(a < b))
            }
            Op::IGt(a, b) => {
                let (a, b) = (self.get_int(frame, *a)?, self.get_int(frame, *b)?);
                Ok(IrValue::Bool(a > b))
            }
            Op::ILe(a, b) => {
                let (a, b) = (self.get_int(frame, *a)?, self.get_int(frame, *b)?);
                Ok(IrValue::Bool(a <= b))
            }
            Op::IGe(a, b) => {
                let (a, b) = (self.get_int(frame, *a)?, self.get_int(frame, *b)?);
                Ok(IrValue::Bool(a >= b))
            }
            Op::FEq(a, b) => {
                let (a, b) = (self.get_float(frame, *a)?, self.get_float(frame, *b)?);
                Ok(IrValue::Bool(a == b))
            }
            Op::FNe(a, b) => {
                let (a, b) = (self.get_float(frame, *a)?, self.get_float(frame, *b)?);
                Ok(IrValue::Bool(a != b))
            }
            Op::FLt(a, b) => {
                let (a, b) = (self.get_float(frame, *a)?, self.get_float(frame, *b)?);
                Ok(IrValue::Bool(a < b))
            }
            Op::FGt(a, b) => {
                let (a, b) = (self.get_float(frame, *a)?, self.get_float(frame, *b)?);
                Ok(IrValue::Bool(a > b))
            }
            Op::FLe(a, b) => {
                let (a, b) = (self.get_float(frame, *a)?, self.get_float(frame, *b)?);
                Ok(IrValue::Bool(a <= b))
            }
            Op::FGe(a, b) => {
                let (a, b) = (self.get_float(frame, *a)?, self.get_float(frame, *b)?);
                Ok(IrValue::Bool(a >= b))
            }
            Op::StringEq(a, b) => {
                let (a, b) = (self.get_string(frame, *a)?, self.get_string(frame, *b)?);
                Ok(IrValue::Bool(a == b))
            }
            Op::StringNe(a, b) => {
                let (a, b) = (self.get_string(frame, *a)?, self.get_string(frame, *b)?);
                Ok(IrValue::Bool(a != b))
            }
            Op::StringLt(a, b) => {
                let (a, b) = (self.get_string(frame, *a)?, self.get_string(frame, *b)?);
                Ok(IrValue::Bool(a < b))
            }
            Op::StringGt(a, b) => {
                let (a, b) = (self.get_string(frame, *a)?, self.get_string(frame, *b)?);
                Ok(IrValue::Bool(a > b))
            }
            Op::StringLe(a, b) => {
                let (a, b) = (self.get_string(frame, *a)?, self.get_string(frame, *b)?);
                Ok(IrValue::Bool(a <= b))
            }
            Op::StringGe(a, b) => {
                let (a, b) = (self.get_string(frame, *a)?, self.get_string(frame, *b)?);
                Ok(IrValue::Bool(a >= b))
            }
            Op::BoolEq(a, b) => {
                let (a, b) = (self.get_bool(frame, *a)?, self.get_bool(frame, *b)?);
                Ok(IrValue::Bool(a == b))
            }
            Op::BoolNe(a, b) => {
                let (a, b) = (self.get_bool(frame, *a)?, self.get_bool(frame, *b)?);
                Ok(IrValue::Bool(a != b))
            }
            Op::BoolNot(a) => {
                let a = self.get_bool(frame, *a)?;
                Ok(IrValue::Bool(!a))
            }
            Op::StringConcat(a, b) => {
                let a = self.get_string(frame, *a)?;
                let b = self.get_string(frame, *b)?;
                Ok(IrValue::String(format!("{a}{b}")))
            }
            _ => unreachable!(),
        }
    }

    // --- Value extraction helpers ---

    fn get_int(&self, frame: &CallFrame, vid: ValueId) -> Result<i64> {
        match frame.get(vid)? {
            IrValue::Int(n) => Ok(*n),
            other => error(format!("expected Int, got {other}")),
        }
    }

    fn get_float(&self, frame: &CallFrame, vid: ValueId) -> Result<f64> {
        match frame.get(vid)? {
            IrValue::Float(n) => Ok(*n),
            other => error(format!("expected Float, got {other}")),
        }
    }

    fn get_bool(&self, frame: &CallFrame, vid: ValueId) -> Result<bool> {
        match frame.get(vid)? {
            IrValue::Bool(b) => Ok(*b),
            other => error(format!("expected Bool, got {other}")),
        }
    }

    fn get_string(&self, frame: &CallFrame, vid: ValueId) -> Result<String> {
        match frame.get(vid)? {
            IrValue::String(s) => Ok(s.clone()),
            other => error(format!("expected String, got {other}")),
        }
    }

    // --- Public helpers for builtins ---

    /// Get the module reference.
    pub(crate) fn module(&self) -> &IrModule {
        self.module
    }

    /// Write to the interpreter's output.
    pub(crate) fn write_output(&mut self, s: &str) -> Result<()> {
        writeln!(self.output, "{s}").map_err(|e| IrRuntimeError {
            message: format!("write error: {e}"),
        })
    }

    /// Call a closure value with arguments.
    ///
    /// Takes the closure by reference to avoid cloning the entire `IrValue`
    /// on each call in a loop.  The captures are cloned internally.
    pub(crate) fn call_closure(
        &mut self,
        closure: &IrValue,
        user_args: Vec<IrValue>,
    ) -> Result<IrValue> {
        if let IrValue::Closure(func_id, captures) = closure {
            let mut all_args = captures.clone();
            all_args.extend(user_args);
            self.call_function(*func_id, all_args)
        } else {
            error(format!("expected closure, got {closure}"))
        }
    }
}
