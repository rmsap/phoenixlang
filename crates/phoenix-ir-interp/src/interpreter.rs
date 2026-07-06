//! Core IR interpreter: execution loop, call frames, instruction dispatch.

use crate::builtins;
use crate::error::{IrRuntimeError, Result, error};
use crate::value::{EnumData, IrValue, StructData, map_key_eq};
use phoenix_common::host::{CallbackHandle, HostContext, HostValue};
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
    /// Host-FFI bindings for `extern js` calls. Empty by default —
    /// an unregistered extern is a clean "no host binding" error. Populated via
    /// [`IrInterpreter::register_host`] / the `run_with_host_capture` entry point.
    ///
    /// Held behind an `Rc` so an extern dispatch can clone a cheap handle for
    /// the lookup while leaving the field populated for the duration of the host
    /// call — a host callback that re-enters Phoenix and calls another extern
    /// must still find the registry. See [`IrInterpreter::call_extern_host`].
    host_registry: Rc<phoenix_common::host::HostRegistry>,
    /// Phoenix closures handed to the host as callbacks, indexed by
    /// [`phoenix_common::host::CallbackHandle`]; invoked back through the
    /// [`phoenix_common::host::HostContext`] bridge. Retained for the
    /// interpreter's lifetime (no event loop releases them).
    host_callbacks: Vec<IrValue>,
    /// JSON DOM arena for `json.decode`. The `json.*` builtins
    /// return `i64` indices into this vector — mirroring the compiled
    /// runtime's opaque pointer handles. Grows for the interpreter's
    /// lifetime (`json.free` is a no-op); a `phoenix run-ir` process is
    /// short-lived, so no reclamation is needed.
    pub(crate) json_arena: Vec<crate::builtins::JsonRoot>,
}

/// Lets a host function call back into Phoenix via the IR
/// interpreter's normal closure-call path. Synchronous — the interpreter has no
/// event loop (the callbacks-only async model).
impl HostContext for IrInterpreter<'_> {
    fn call_callback(
        &mut self,
        handle: CallbackHandle,
        args: Vec<HostValue>,
    ) -> std::result::Result<HostValue, String> {
        let closure = self
            .host_callbacks
            .get(handle.0 as usize)
            .cloned()
            .ok_or_else(|| format!("invalid `extern js` callback handle {}", handle.0))?;
        let native_args: Vec<IrValue> = args
            .into_iter()
            .map(|a| self.host_to_ir_value(a))
            .collect::<std::result::Result<_, String>>()?;
        let result = self
            .call_closure(&closure, native_args)
            .map_err(|e| e.message)?;
        self.ir_value_to_host(result)
    }
}

impl<'m> IrInterpreter<'m> {
    /// Create a new interpreter for the given module, with no host bindings.
    pub fn new(module: &'m IrModule, output: Box<dyn Write>) -> Self {
        Self {
            module,
            output,
            depth: 0,
            host_registry: Rc::new(phoenix_common::host::HostRegistry::new()),
            host_callbacks: Vec::new(),
            json_arena: Vec::new(),
        }
    }

    /// Register a host binding for an `extern js` function `(module, name)`.
    /// See [`phoenix_common::host`] for the host-function contract.
    pub fn register_host(
        &mut self,
        module: impl Into<String>,
        name: impl Into<String>,
        f: phoenix_common::host::HostFunction,
    ) {
        // Registration happens during setup, before any run begins, so the `Rc`
        // is uniquely owned and `get_mut` succeeds. (It is only ever aliased
        // transiently inside [`Self::call_extern_host`], which never registers.)
        Rc::get_mut(&mut self.host_registry)
            .expect("register host bindings before running the program")
            .register(module, name, f);
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

        let func = self
            .module
            .resolve_concrete(func_id)
            .unwrap_or_else(|e| panic!("call_function: {e}"));
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
                // Duplicate keys in a literal dedup last-wins, keeping
                // the first insertion position — matching the runtime's
                // `phx_map_from_pairs` (and so native / wasm32-linear /
                // wasm32-gc). A map can't hold two entries with the same
                // key; the prior keep-all behavior diverged from the
                // compiled backends (resolved 2026-06-14 with the maps
                // slice — see the wasm32-gc K.9 decision).
                let mut entries: Vec<(IrValue, IrValue)> = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    let kv = frame.get(*k)?.clone();
                    let vv = frame.get(*v)?.clone();
                    if let Some(slot) = entries.iter_mut().find(|(ek, _)| map_key_eq(ek, &kv)) {
                        slot.1 = vv;
                    } else {
                        entries.push((kv, vv));
                    }
                }
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
            Op::ClosureLoadCapture(env_vid, capture_idx) => {
                let env = frame.get(*env_vid)?;
                match env {
                    IrValue::Closure(_, captures) => captures
                        .get(*capture_idx as usize)
                        .cloned()
                        .ok_or_else(|| IrRuntimeError {
                            message: format!(
                                "Op::ClosureLoadCapture: capture index {capture_idx} out of range \
                                 (closure has {} captures)",
                                captures.len()
                            ),
                        }),
                    other => error(format!(
                        "Op::ClosureLoadCapture: env value is not a closure, got {other}"
                    )),
                }
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
            Op::ExternCall(module, name, arg_vids) => {
                // Resolve args from the frame, then dispatch to the host binding
                // (which may invoke Phoenix callbacks back through HostContext).
                let args: Vec<IrValue> = arg_vids
                    .iter()
                    .map(|vid| frame.get(*vid).cloned())
                    .collect::<Result<_>>()?;
                self.call_extern_host(module, name, args)
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
                let (a, b) = (
                    self.get_int_or_handle(frame, *a)?,
                    self.get_int_or_handle(frame, *b)?,
                );
                Ok(IrValue::Bool(a == b))
            }
            Op::INe(a, b) => {
                let (a, b) = (
                    self.get_int_or_handle(frame, *a)?,
                    self.get_int_or_handle(frame, *b)?,
                );
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

    /// Extract the comparable identity of an `IEq`/`INe` operand: an `Int`'s
    /// value, or a `JsValue`'s opaque handle. `JsValue` reaches the
    /// integer-compare ops because `==`/`!=` on it lower to `Op::IEq`/`Op::INe`
    /// (its identity is bit-equality of its handle — see `lower_binary`). The
    /// `u64`→`i64` cast preserves that bit identity for equality. Sema
    /// guarantees both operands share a type, so an `Int`/`JsValue` mix never
    /// arises.
    fn get_int_or_handle(&self, frame: &CallFrame, vid: ValueId) -> Result<i64> {
        match frame.get(vid)? {
            IrValue::Int(n) => Ok(*n),
            IrValue::JsValue(h) => Ok(*h as i64),
            other => error(format!("expected Int or JsValue, got {other}")),
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

    /// Marshal a native [`IrValue`] out across the `extern js` boundary into a
    /// [`HostValue`]. A closure registers a callback handle so the
    /// host can invoke it; aggregates are an internal error (sema rejects
    /// non-marshallable extern types).
    fn ir_value_to_host(&mut self, v: IrValue) -> std::result::Result<HostValue, String> {
        Ok(match v {
            IrValue::Int(n) => HostValue::Int(n),
            IrValue::Float(n) => HostValue::Float(n),
            IrValue::Bool(b) => HostValue::Bool(b),
            IrValue::String(s) => HostValue::Str(s),
            IrValue::Void => HostValue::Void,
            IrValue::JsValue(h) => HostValue::JsValue(h),
            c @ IrValue::Closure(_, _) => {
                let handle = CallbackHandle(self.host_callbacks.len() as u64);
                self.host_callbacks.push(c);
                HostValue::Callback(handle)
            }
            other => {
                return Err(format!(
                    "value of type `{}` cannot cross the `extern js` boundary \
                     (only Int / Float / Bool / String / JsValue / Void and closures \
                     are marshallable)",
                    other.type_name()
                ));
            }
        })
    }

    /// Marshal a [`HostValue`] from the host back into a native [`IrValue`].
    fn host_to_ir_value(&self, hv: HostValue) -> std::result::Result<IrValue, String> {
        Ok(match hv {
            HostValue::Int(n) => IrValue::Int(n),
            HostValue::Float(n) => IrValue::Float(n),
            HostValue::Bool(b) => IrValue::Bool(b),
            HostValue::Str(s) => IrValue::String(s),
            HostValue::Void => IrValue::Void,
            HostValue::JsValue(h) => IrValue::JsValue(h),
            HostValue::Callback(_) => {
                return Err("a host function returned a callback handle, which Phoenix \
                            cannot receive across the `extern js` boundary"
                    .to_string());
            }
        })
    }

    /// Dispatch an `extern js` call to its registered host binding.
    /// Marshals args out, invokes the host function (which may call Phoenix
    /// callbacks back through [`HostContext`]), and marshals the result in.
    /// Reports a clean error if no binding is registered for `(module, name)`.
    fn call_extern_host(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<IrValue>,
    ) -> Result<IrValue> {
        let to_err = |m: String| IrRuntimeError { message: m };
        // Clone a cheap `Rc` handle so `self.host_registry` stays populated while
        // the host function borrows `&mut self` (as a `HostContext`, for
        // callbacks): a callback that re-enters Phoenix and calls another extern
        // then still finds the registry. The clone also lets the resolved `&f`
        // live across the marshalling `&mut self` borrows below, since it borrows
        // the local `registry`, not `self`.
        let registry = Rc::clone(&self.host_registry);
        // Resolve the binding *before* marshalling. Marshalling a closure arg
        // mints a callback handle in `host_callbacks`; doing it first on the
        // unbound path would leave an orphan entry behind for an extern that
        // never runs. The lookup is a cheap pair of `HashMap` probes.
        let Some(f) = registry.get(module, name) else {
            return Err(to_err(format!(
                "no host binding registered for `extern js` function `{module}.{name}` \
                 — register one before running (`phoenix run-ir` provides none; a host \
                 binding lands with the WASM / native backends)"
            )));
        };
        // Snapshot the callback table before marshalling so a failure partway
        // through the arg list — a non-marshallable arg *after* an earlier
        // closure arg already minted a handle — rolls those orphan handles back
        // rather than leaking them, matching the resolve-binding-first intent
        // above (no handle survives for a call that never reaches the host).
        let checkpoint = self.host_callbacks.len();
        let host_args: Vec<HostValue> = match args
            .into_iter()
            .map(|a| self.ir_value_to_host(a))
            .collect::<std::result::Result<_, String>>()
        {
            Ok(host_args) => host_args,
            Err(m) => {
                self.host_callbacks.truncate(checkpoint);
                return Err(to_err(m));
            }
        };
        let host_result = f(self, host_args).map_err(to_err)?;
        self.host_to_ir_value(host_result).map_err(to_err)
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
        if let IrValue::Closure(func_id, _captures) = closure {
            // Env-pointer calling convention: pass the closure value
            // itself as the first arg. The callee reads its captures
            // via Op::ClosureLoadCapture indexed off this env value.
            let mut all_args = Vec::with_capacity(user_args.len() + 1);
            all_args.push(closure.clone());
            all_args.extend(user_args);
            self.call_function(*func_id, all_args)
        } else {
            error(format!("expected closure, got {closure}"))
        }
    }
}

#[cfg(test)]
mod closure_load_capture_tests {
    //! Focused unit tests for [`Op::ClosureLoadCapture`] interpreter
    //! semantics. The end-to-end roundtrip suite in
    //! `tests/roundtrip_closures.rs` exercises the same op via lowered
    //! source programs; these tests pin the op's contract independently
    //! of the lowering path so an IR-side regression surfaces here even
    //! if frontend lowering happens to mask it.
    use super::*;
    use phoenix_ir::module::ENV_PARAM_NAME;
    use phoenix_ir::types::IrType;

    /// Build a minimal closure function `__closure(env: ClosureRef) -> Int`
    /// whose body returns the capture at `capture_idx`. Captures are
    /// recorded in `func.capture_types` and supplied at call time via
    /// `IrValue::Closure(func_id, captures)`.
    fn closure_loading(
        capture_types: Vec<IrType>,
        capture_idx: u32,
        return_type: IrType,
    ) -> IrModule {
        let mut module = IrModule::new();
        let env_ty = IrType::ClosureRef {
            param_types: vec![],
            return_type: Box::new(return_type.clone()),
        };
        let mut func = IrFunction::new_closure(
            FuncId(u32::MAX),
            "__closure_test".into(),
            vec![env_ty.clone()],
            vec![ENV_PARAM_NAME.into()],
            return_type.clone(),
            None,
            capture_types,
        );
        let entry = func.create_block();
        let env = func.add_block_param(entry, env_ty);
        let loaded = func
            .emit(
                entry,
                Op::ClosureLoadCapture(env, capture_idx),
                return_type,
                None,
            )
            .expect("non-void emit returns Some");
        func.set_terminator(entry, Terminator::Return(Some(loaded)));
        module.push_concrete(func);
        module
    }

    #[test]
    fn loads_capture_at_index_zero() {
        let module = closure_loading(vec![IrType::I64], 0, IrType::I64);
        let mut interp = IrInterpreter::new(&module, Box::new(std::io::sink()));
        let closure = IrValue::Closure(FuncId(0), vec![IrValue::Int(42)]);
        let result = interp.call_closure(&closure, vec![]).unwrap();
        assert_eq!(result, IrValue::Int(42));
    }

    #[test]
    fn loads_capture_at_higher_index() {
        let module = closure_loading(vec![IrType::I64, IrType::I64, IrType::I64], 2, IrType::I64);
        let mut interp = IrInterpreter::new(&module, Box::new(std::io::sink()));
        let closure = IrValue::Closure(
            FuncId(0),
            vec![IrValue::Int(10), IrValue::Int(20), IrValue::Int(30)],
        );
        let result = interp.call_closure(&closure, vec![]).unwrap();
        assert_eq!(result, IrValue::Int(30));
    }

    #[test]
    fn out_of_range_capture_index_errors() {
        // `capture_idx = 5` against a closure value with 1 capture →
        // `Op::ClosureLoadCapture` must surface a runtime error rather
        // than panic or read past the end of the capture vector.
        let module = closure_loading(vec![IrType::I64], 5, IrType::I64);
        let mut interp = IrInterpreter::new(&module, Box::new(std::io::sink()));
        let closure = IrValue::Closure(FuncId(0), vec![IrValue::Int(1)]);
        let err = interp.call_closure(&closure, vec![]).unwrap_err();
        assert!(
            err.message.contains("ClosureLoadCapture") && err.message.contains("out of range"),
            "expected out-of-range error, got: {}",
            err.message
        );
    }
}
