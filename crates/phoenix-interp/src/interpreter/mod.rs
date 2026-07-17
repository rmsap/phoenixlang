mod json;
mod multi_module;

pub use multi_module::{run_modules, run_modules_with_host};

use crate::env::Environment;
use crate::value::{Value, map_key_eq};
use phoenix_common::host::{CallbackHandle, HostContext, HostValue};
use phoenix_common::span::Span;
use phoenix_parser::ast::{
    BinaryExpr, BinaryOp, Block, CallExpr, CaptureInfo, Declaration, ElseBranch, Expr, ForSource,
    ForStmt, FunctionDecl, IfExpr, LiteralKind, MatchBody, MatchExpr, MethodCallExpr, Param,
    Pattern, Program, Statement, StringSegment, StructLiteralExpr, TryExpr, UnaryExpr, UnaryOp,
    WhileStmt,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::rc::Rc;

/// The result of executing a statement, indicating control flow changes.
pub(crate) enum StmtResult {
    /// Execution should proceed to the next statement.
    Continue,
    /// A `return` statement was encountered, carrying the returned value.
    Return(Value),
    /// A `break` statement was encountered, exiting the enclosing loop.
    Break,
    /// A `continue` statement was encountered, skipping to the next iteration.
    LoopContinue,
}

/// An error encountered during program interpretation.
#[derive(Debug)]
pub struct RuntimeError {
    /// A human-readable description of what went wrong at runtime.
    pub message: String,
    /// If set, this is a `?` operator early-return value, not a real error.
    /// The enclosing function call should return this value instead of
    /// propagating the error.
    pub try_return_value: Option<Value>,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RuntimeError {}

pub(crate) type Result<T> = std::result::Result<T, RuntimeError>;

pub(crate) fn error<T>(msg: impl Into<String>) -> Result<T> {
    Err(RuntimeError {
        message: msg.into(),
        try_return_value: None,
    })
}

/// Constructs an `Option::Some` variant value.
pub(crate) fn some_val(val: Value) -> Value {
    Value::EnumVariant("Option".to_string(), "Some".to_string(), vec![val])
}

/// Constructs an `Option::None` variant value.
pub(crate) fn none_val() -> Value {
    Value::EnumVariant("Option".to_string(), "None".to_string(), vec![])
}

/// Constructs a `Result::Ok` variant value.
pub(crate) fn ok_val(val: Value) -> Value {
    Value::EnumVariant("Result".to_string(), "Ok".to_string(), vec![val])
}

/// Constructs a `Result::Err` variant value.
pub(crate) fn err_val(val: Value) -> Value {
    Value::EnumVariant("Result".to_string(), "Err".to_string(), vec![val])
}

/// Creates a `RuntimeError` for wrong argument count on a method call.
pub(crate) fn arg_count_error(method: &str, expected: usize, got: usize) -> RuntimeError {
    let noun = if expected == 1 {
        "argument"
    } else {
        "arguments"
    };
    RuntimeError {
        message: format!("{}() takes {} {}, got {}", method, expected, noun, got),
        try_return_value: None,
    }
}

/// Applies a checked integer operation and a floating-point operation to two
/// numeric values. Returns a runtime error for non-numeric or overflow cases.
fn checked_numeric_op(
    left: &Value,
    right: &Value,
    verb: &str,
    int_op: fn(i64, i64) -> Option<i64>,
    float_op: fn(f64, f64) -> f64,
) -> Result<Value> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => {
            Ok(Value::Int(int_op(*a, *b).ok_or_else(|| RuntimeError {
                message: "integer overflow".to_string(),
                try_return_value: None,
            })?))
        }
        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(float_op(*a, *b))),
        _ => error(format!(
            "cannot {} {} and {}",
            verb,
            left.type_name(),
            right.type_name()
        )),
    }
}

/// Applies an ordering comparison to two values.
fn compare_values(left: &Value, right: &Value, op: BinaryOp) -> Result<Value> {
    match left.partial_cmp(right) {
        Some(ord) => {
            let result = match op {
                BinaryOp::Lt => ord.is_lt(),
                BinaryOp::Gt => ord.is_gt(),
                BinaryOp::LtEq => ord.is_le(),
                BinaryOp::GtEq => ord.is_ge(),
                _ => unreachable!(),
            };
            Ok(Value::Bool(result))
        }
        None => error(format!(
            "cannot compare {} and {}",
            left.type_name(),
            right.type_name()
        )),
    }
}

/// Struct definition info for the interpreter.
#[derive(Debug, Clone)]
pub(crate) struct StructDef {
    field_names: Vec<String>,
}

/// Enum definition info for the interpreter.
#[derive(Debug, Clone)]
pub(crate) struct EnumDef {
    /// Variant name to field count.
    variants: HashMap<String, usize>,
}

/// Method definition.
///
/// `module` is the module that *declared* this method's host type.
/// `Some(...)` causes [`Interpreter::call_method`] to push the
/// declaring module onto the module stack before evaluating the body
/// (so cross-module name lookups in defaults / body resolve through
/// the callee's scope). `None` is the single-file path that leaves
/// the stack empty.
#[derive(Debug, Clone)]
pub(crate) struct MethodDef {
    func: FunctionDecl,
    module: Option<Rc<phoenix_common::module_path::ModulePath>>,
}

/// Free-function definition with its declaring module attached.
///
/// Bundling the function declaration with its `Option<Rc<ModulePath>>`
/// in a single map value (rather than two parallel maps) makes
/// "function added without module recorded" structurally impossible —
/// every call site that has the decl also has the module hint.
#[derive(Debug, Clone)]
pub(crate) struct FunctionEntry {
    pub(crate) decl: FunctionDecl,
    pub(crate) module: Option<Rc<phoenix_common::module_path::ModulePath>>,
}

/// Maximum call depth before the interpreter aborts with a stack overflow error.
const MAX_CALL_DEPTH: usize = 50;

/// The tree-walk interpreter for Phoenix programs.
///
/// Evaluates a Phoenix AST by walking the tree directly, maintaining an
/// environment of variable bindings and registries for functions, structs,
/// enums, and methods.
pub struct Interpreter {
    pub(crate) env: Environment,
    pub(crate) functions: HashMap<String, FunctionEntry>,
    /// `extern js` host functions declared in the program, mapped to their host
    /// module (`"js"` for an ambient `extern js { ... }` block, the npm package
    /// specifier for `extern js "pkg" { ... }`) and their declared
    /// parameter names in source order. Membership drives call precedence (an
    /// extern shadows a same-named local closure, matching sema's `check_call`);
    /// the module keys the [`host_registry`](Self::host_registry) lookup at
    /// dispatch; the parameter order lets a call's named arguments be reordered
    /// into positional form before marshalling, exactly as the IR backend does.
    /// Keyed exactly like [`functions`](Self::functions): bare name on the
    /// single-file path, module-qualified (`module_qualify`) on the multi-module
    /// path — mirroring sema's per-module extern scoping, so same-named externs
    /// in different modules can't clobber each other's host module. The entry is
    /// behind an `Rc` because dispatch must clone it out of the map (to release
    /// the borrow before evaluating arguments), and an `Rc` bump keeps that
    /// per-call clone free.
    pub(crate) extern_params: HashMap<String, Rc<(String, Vec<String>)>>,
    pub(crate) structs: HashMap<String, StructDef>,
    pub(crate) enums: HashMap<String, EnumDef>, // enum name -> def
    pub(crate) variant_to_enum: HashMap<String, String>, // variant name -> enum name
    pub(crate) methods: HashMap<String, HashMap<String, MethodDef>>, // type -> method_name -> def
    pub(crate) call_depth: usize,
    /// Side-channel for propagating break/continue out of expressions (e.g. match blocks).
    pub(crate) pending_control_flow: Option<PendingControlFlow>,
    /// Tracks whether the most recent `StmtResult::Return` was produced by an
    /// explicit `return` statement (`true`) or by an implicit last-expression
    /// return (`false`).  Used in `eval_match` to distinguish function-level
    /// returns from match arm values.
    pub(crate) last_return_was_explicit: bool,
    /// Output sink for `print()`. Defaults to stdout.
    pub(crate) output: Box<dyn Write>,
    /// Captured variables for each lambda, populated by the semantic checker.
    /// Keyed by the lambda's source span.
    pub(crate) lambda_captures: HashMap<Span, Vec<CaptureInfo>>,

    /// Namespace calls (`lib.func(...)`) keyed by call span to the
    /// resolved callee's qualified key, drained from sema's `Analysis`.
    /// The receiver is a namespace, not a value, so
    /// [`Interpreter::eval_method_call`] consults this map to dispatch
    /// straight to the target function in its owning module instead of
    /// evaluating the receiver as an object. Empty for single-file inputs
    /// without sema metadata.
    pub(crate) namespace_call_targets: HashMap<Span, String>,

    /// Spans of `json.encode(value)` calls, drained from sema.
    /// The tree-walk interpreter encodes the runtime `Value` directly (it
    /// has full type information), so it only needs to recognize which
    /// `json.<method>` sites are encode calls.
    pub(crate) json_encode_spans: std::collections::HashSet<Span>,

    /// Target type `T` at each `json.decode<T>(text)` call, drained from
    /// sema. The tree-walk interpreter decodes a `serde_json`
    /// DOM into a typed `Value` guided by this type.
    pub(crate) json_decode_types: HashMap<Span, phoenix_sema::types::Type>,

    /// Struct field `(name, type)` lists keyed by qualified struct name,
    /// seeded from sema (Phase 4.6). `json.decode` of a struct target needs
    /// each field's type to recurse; the interpreter's own `StructDef` only
    /// tracks field names.
    pub(crate) json_struct_fields: HashMap<String, Vec<(String, phoenix_sema::types::Type)>>,

    /// Enum variant `(name, field-types)` lists keyed by qualified enum name,
    /// seeded from sema alongside [`Self::json_struct_fields`]. `json.decode`
    /// of an enum target needs each variant's field types to recurse; the
    /// interpreter's own `EnumDef` only tracks variant field *counts*.
    pub(crate) json_enum_variants: HashMap<String, Vec<(String, Vec<phoenix_sema::types::Type>)>>,

    /// Per-module visibility scopes (drained from sema's `Analysis`).
    /// Used by [`Interpreter::qualify`] to translate user-source bare
    /// names into qualified registry keys (mirrors what the IR layer
    /// does via `LoweringContext::qualify`). Empty for single-file
    /// inputs that go through [`run`] without sema metadata.
    pub(crate) module_scopes:
        HashMap<phoenix_common::module_path::ModulePath, HashMap<String, String>>,
    /// Stack of `current_module` paths, pushed when entering a function
    /// and popped on return. Top-of-stack is the module owning the
    /// currently-executing body; lookups consult its scope. Stored
    /// as `Rc<ModulePath>` so cross-module call hot-paths bump a
    /// refcount rather than cloning an owned `Vec<String>`. The
    /// declaring-module hint pushed at call time is read out of the
    /// callee's [`FunctionEntry`] / [`MethodDef`] (rather than two
    /// parallel maps), so an "added without module recorded" state
    /// is structurally impossible.
    pub(crate) module_stack: Vec<Rc<phoenix_common::module_path::ModulePath>>,

    /// Pending defer expressions, one frame per active function call
    /// (push on call entry, pop and run in reverse on call exit). The
    /// inner `Vec<Expr>` records each `defer expr` in source order, so
    /// popping back-to-front gives Go-style LIFO semantics. Free
    /// variables are looked up at exit time (lazy semantics) — see
    /// Phase 2.3 design-decisions.md decision G for the tradeoff.
    pub(crate) defer_stack: Vec<Vec<phoenix_parser::ast::Expr>>,

    /// Host-FFI bindings for `extern js` calls. Empty by default —
    /// the bare CLI registers nothing, so an extern call reports a clean "no
    /// host binding registered" error. The embedder / test harness populates it
    /// (see [`Interpreter::register_host`] / [`run_modules_with_host`]).
    ///
    /// Held behind an `Rc` so an extern dispatch can clone a cheap handle for
    /// the lookup while leaving the field populated for the duration of the host
    /// call — a host callback that re-enters Phoenix and calls another extern
    /// must still find the registry. See [`Interpreter::call_extern_host`].
    pub(crate) host_registry: Rc<phoenix_common::host::HostRegistry>,
    /// Phoenix closures handed to the host as callbacks, indexed by
    /// [`phoenix_common::host::CallbackHandle`]. A closure is appended when it
    /// is marshalled out across the `extern js` boundary; the host invokes it
    /// back through [`phoenix_common::host::HostContext::call_callback`].
    /// Retained for the interpreter's lifetime — the interpreter has no event
    /// loop to release them (the callbacks-only async model).
    pub(crate) host_callbacks: Vec<Value>,
}

/// Control flow signal that must escape from an expression context
/// (e.g. a `break` or `continue` inside a match arm block).
#[derive(Debug, Clone, Copy)]
pub(crate) enum PendingControlFlow {
    Break,
    LoopContinue,
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

/// Lets a host function call back into Phoenix (Phase 2.5). A host stub
/// modelling a callback-taking API (`setTimeout(cb, ms)`) invokes the Phoenix
/// closure synchronously via the interpreter's normal [`Interpreter::call_closure`]
/// path. The interpreters have no event loop — the callbacks-only async model.
impl HostContext for Interpreter {
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
        let native_args: Vec<Value> = args
            .into_iter()
            .map(|a| self.host_to_value(a))
            .collect::<std::result::Result<_, String>>()?;
        let result = self
            .call_closure(closure, native_args)
            .map_err(|e| e.message)?;
        self.value_to_host(result)
    }
}

impl Interpreter {
    /// Creates a new interpreter with an empty environment and no registered declarations.
    /// Output from `print()` is written to stdout.
    pub fn new() -> Self {
        Self::with_output(Box::new(std::io::stdout()))
    }

    /// Creates a new interpreter that writes `print()` output to the given sink.
    pub fn with_output(output: Box<dyn Write>) -> Self {
        Self {
            env: Environment::new(),
            functions: HashMap::new(),
            extern_params: HashMap::new(),
            structs: HashMap::new(),
            enums: HashMap::new(),
            variant_to_enum: HashMap::new(),
            methods: HashMap::new(),
            call_depth: 0,
            pending_control_flow: None,
            last_return_was_explicit: false,
            lambda_captures: HashMap::new(),
            namespace_call_targets: HashMap::new(),
            json_encode_spans: std::collections::HashSet::new(),
            json_decode_types: HashMap::new(),
            json_struct_fields: HashMap::new(),
            json_enum_variants: HashMap::new(),
            output,
            module_scopes: HashMap::new(),
            module_stack: Vec::new(),
            defer_stack: Vec::new(),
            host_registry: Rc::new(phoenix_common::host::HostRegistry::new()),
            host_callbacks: Vec::new(),
        }
    }

    /// Register a host binding for an `extern js` function `(module, name)`.
    /// Call before running a program that calls the extern. See
    /// [`phoenix_common::host`] for the host-function contract.
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

    /// Marshal a native [`Value`] out across the `extern js` boundary into a
    /// [`HostValue`]. A closure registers a callback handle (so the host can
    /// invoke it later); aggregates are an internal error, since sema rejects
    /// non-marshallable types at the extern signature.
    fn value_to_host(&mut self, v: Value) -> std::result::Result<HostValue, String> {
        Ok(match v {
            Value::Int(n) => HostValue::Int(n),
            Value::Float(n) => HostValue::Float(n),
            Value::Bool(b) => HostValue::Bool(b),
            Value::String(s) => HostValue::Str(s),
            Value::Void => HostValue::Void,
            Value::JsValue(h) => HostValue::JsValue(h),
            c @ Value::Closure { .. } => {
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

    /// Marshal a [`HostValue`] from the host back into a native [`Value`].
    fn host_to_value(&self, hv: HostValue) -> std::result::Result<Value, String> {
        Ok(match hv {
            HostValue::Int(n) => Value::Int(n),
            HostValue::Float(n) => Value::Float(n),
            HostValue::Bool(b) => Value::Bool(b),
            HostValue::Str(s) => Value::String(s),
            HostValue::Void => Value::Void,
            HostValue::JsValue(h) => Value::JsValue(h),
            HostValue::Callback(_) => {
                return Err("a host function returned a callback handle, which Phoenix \
                            cannot receive across the `extern js` boundary"
                    .to_string());
            }
        })
    }

    /// Dispatch an `extern js` call to its registered host binding (Phase 2.5).
    /// Marshals args out, invokes the host function (which may call Phoenix
    /// callbacks back through [`HostContext`]), and marshals the result in.
    /// Reports a clean error if no binding is registered for `(module, name)`.
    fn call_extern_host(&mut self, module: &str, name: &str, args: Vec<Value>) -> Result<Value> {
        let to_err = |m: String| RuntimeError {
            message: m,
            try_return_value: None,
        };
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
                 — register one before running (`phoenix run` provides none; a host \
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
            .map(|a| self.value_to_host(a))
            .collect::<std::result::Result<_, String>>()
        {
            Ok(host_args) => host_args,
            Err(m) => {
                self.host_callbacks.truncate(checkpoint);
                return Err(to_err(m));
            }
        };
        let host_result = f(self, host_args).map_err(to_err)?;
        self.host_to_value(host_result).map_err(to_err)
    }

    /// Reorder an `extern js` call's positional + named arguments into a single
    /// positional vector matching the extern's declared parameter order (Phase
    /// 2.5), mirroring the IR backend's `assemble_call_args`. Extern signatures
    /// permit no default values, so every parameter must be supplied — sema
    /// validates arity and names, so a gap or unknown name here is an internal
    /// error rather than a user error.
    fn assemble_extern_args(
        &self,
        name: &str,
        param_names: &[String],
        positional: Vec<Value>,
        named: Vec<(String, Value)>,
    ) -> Result<Vec<Value>> {
        // Sema validates arity, so an overflow here is an internal error (a
        // lowering/resolution bug), never user input. Fail loudly — in release
        // as well as debug — rather than silently dropping the extra args.
        if positional.len() > param_names.len() {
            return error(format!(
                "internal error: extern js call `{name}`: {} positional args for {} parameters",
                positional.len(),
                param_names.len()
            ));
        }
        let mut slots: Vec<Option<Value>> = vec![None; param_names.len()];
        for (i, val) in positional.into_iter().enumerate() {
            // `positional.len() <= param_names.len() == slots.len()` (checked
            // above), so every index is in bounds.
            slots[i] = Some(val);
        }
        for (arg_name, val) in named {
            match param_names.iter().position(|p| *p == arg_name) {
                Some(idx) => slots[idx] = Some(val),
                // Sema validates argument names, so an unknown name here is an
                // internal error. Fail loudly rather than dropping the arg
                // silently — otherwise it would resurface below as a misleading
                // "missing argument" for whatever slot stayed unfilled.
                None => {
                    return error(format!(
                        "internal error: extern js call `{name}`: unknown argument `{arg_name}`"
                    ));
                }
            }
        }
        let mut args = Vec::with_capacity(param_names.len());
        for (i, slot) in slots.into_iter().enumerate() {
            match slot {
                Some(v) => args.push(v),
                None => {
                    return error(format!(
                        "extern js call `{name}`: missing argument for parameter `{}`",
                        param_names[i]
                    ));
                }
            }
        }
        Ok(args)
    }

    /// Run every pending defer in the current function's frame in reverse
    /// (LIFO) order. Each defer's result value is discarded.
    ///
    /// **Error policy (Go-style):** an error in one defer does not stop
    /// later defers from running — every registered defer fires exactly
    /// once, and the *first* error encountered is propagated after the
    /// sequence completes. Subsequent errors are dropped on the floor.
    fn run_defers(&mut self) -> Result<()> {
        let Some(defers) = self.defer_stack.last_mut() else {
            return Ok(());
        };
        // Drain so the same frame can't accidentally re-enter and run defers twice.
        let drained = std::mem::take(defers);
        let mut first_err: Option<RuntimeError> = None;
        for expr in drained.into_iter().rev() {
            if let Err(e) = self.eval_expr(&expr)
                && first_err.is_none()
            {
                first_err = Some(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Push a defer frame, run `body`, run any registered defers, then
    /// pop the frame — returning the body's value if both succeeded,
    /// the body's error if it failed (defers still ran for their side
    /// effects), or the defers' first error if the body succeeded.
    ///
    /// Centralises the defer lifecycle for `call_function_body`,
    /// `call_method_body`, and `call_closure` so they can't drift on
    /// push/pop ordering. Callers still own env and call_depth setup
    /// (which differs between functions, methods, and closures).
    fn with_defer_frame(&mut self, body: &Block) -> Result<Value> {
        // Snapshot env scope depth so we can assert the body left it
        // balanced before defers fire — see the assert below.
        let entry_scope_depth = self.env.scope_depth();
        self.defer_stack.push(Vec::new());

        // Phase 1 — run the body and unwrap its result. The unwrap
        // runs *before* defers so the `?`-induced early-return
        // error→value translation lands first; defers then fire
        // while the body's locals are still in scope (callers pop
        // the env after `with_defer_frame` returns).
        let exec_result = self.exec_block_implicit(body);
        let body_outcome = self.unwrap_call_result(exec_result);

        // Defers' free-variable lookups depend on the env still
        // matching the function's scope chain at this point — the
        // body's inner-block push/pops must have netted to zero, and
        // the caller's function scope must still be alive (callers
        // pop it AFTER `with_defer_frame` returns). A regression
        // here (an inner `pop_scope` without the matching push, or a
        // future caller forgetting to push the function scope at all)
        // would silently shift defer lookups to the wrong scope
        // chain. Cheap assert; only meaningful in debug builds.
        debug_assert_eq!(
            self.env.scope_depth(),
            entry_scope_depth,
            "body must restore env scope depth before defers run; otherwise \
             deferred free-variable lookups resolve against the wrong scope chain",
        );

        // Phase 2 — run defers. `last_return_was_explicit` is global
        // state read by `eval_branch_block` / match-arm code to
        // decide whether a `StmtResult::Return` was explicit, so any
        // function call inside a defer can clobber it. Snapshot and
        // restore around `run_defers` so defer side effects don't
        // leak into the caller's view of *this* call's return shape.
        let saved_return_flag = self.last_return_was_explicit;
        let defer_outcome = self.run_defers();
        self.last_return_was_explicit = saved_return_flag;

        self.defer_stack.pop();

        // Body error wins over defer error — the body's diagnosis
        // ran first and is more useful. Defer errors surface only
        // when the body succeeded.
        body_outcome.and_then(|v| defer_outcome.map(|()| v))
    }

    /// Registers a slice of function declarations as methods on the given type.
    /// Each [`MethodDef`] carries an `Option<Rc<ModulePath>>` recording the
    /// declaring module, so [`Self::call_method`] can push the right scope
    /// when dispatching across modules.
    fn register_methods(
        &mut self,
        type_name: &str,
        methods: &[FunctionDecl],
        module_path: Option<&Rc<phoenix_common::module_path::ModulePath>>,
    ) {
        if methods.is_empty() {
            return;
        }
        let type_methods = self.methods.entry(type_name.to_string()).or_default();
        for func in methods {
            type_methods.insert(
                func.name.clone(),
                MethodDef {
                    func: func.clone(),
                    module: module_path.map(Rc::clone),
                },
            );
        }
    }

    /// Unwraps a block execution result into a function return value.
    fn unwrap_call_result(&self, result: Result<StmtResult>) -> Result<Value> {
        match result {
            Ok(StmtResult::Return(value)) => Ok(value),
            Ok(_) => Ok(Value::Void),
            Err(mut e) if e.try_return_value.is_some() => {
                Ok(e.try_return_value.take().expect("guarded by is_some()"))
            }
            Err(e) => Err(e),
        }
    }

    /// Pre-register the built-in Option / Result enums and their
    /// variant→enum mapping. Shared between [`Self::run_program`] and
    /// [`Self::run_modules_inner`] so the two paths can't drift on
    /// what counts as a builtin.
    pub(crate) fn register_builtin_enums(&mut self) {
        self.variant_to_enum
            .insert("Some".to_string(), "Option".to_string());
        self.variant_to_enum
            .insert("None".to_string(), "Option".to_string());
        self.variant_to_enum
            .insert("Ok".to_string(), "Result".to_string());
        self.variant_to_enum
            .insert("Err".to_string(), "Result".to_string());
        self.enums.insert(
            "Option".to_string(),
            EnumDef {
                variants: HashMap::from([("Some".to_string(), 1), ("None".to_string(), 0)]),
            },
        );
        self.enums.insert(
            "Result".to_string(),
            EnumDef {
                variants: HashMap::from([("Ok".to_string(), 1), ("Err".to_string(), 1)]),
            },
        );
        // Built-in `JsonError`: non-generic variants, each carrying
        // a single `String` message (arity 1). Variant set is shared with sema
        // via `JSON_ERROR_VARIANTS` so the two can't drift.
        for variant in phoenix_sema::types::JSON_ERROR_VARIANTS {
            self.variant_to_enum
                .insert(variant.to_string(), "JsonError".to_string());
        }
        self.enums.insert(
            "JsonError".to_string(),
            EnumDef {
                variants: phoenix_sema::types::JSON_ERROR_VARIANTS
                    .iter()
                    .map(|v| (v.to_string(), 1))
                    .collect(),
            },
        );
    }

    /// Runs a complete Phoenix program by registering all declarations and then
    /// invoking the `main()` function. Returns an error if no `main` is found.
    ///
    /// Before processing user declarations, the built-in `Option<T>` and
    /// `Result<T, E>` enum definitions and variant mappings are pre-registered.
    pub fn run_program(&mut self, program: &Program) -> Result<()> {
        self.register_builtin_enums();

        // Register all declarations
        for decl in &program.declarations {
            match decl {
                Declaration::Function(func) => {
                    self.functions.insert(
                        func.name.clone(),
                        FunctionEntry {
                            decl: func.clone(),
                            module: None,
                        },
                    );
                }
                Declaration::Struct(s) => {
                    let field_names: Vec<String> =
                        s.fields.iter().map(|f| f.name.clone()).collect();
                    self.structs
                        .insert(s.name.clone(), StructDef { field_names });
                    self.register_methods(&s.name, &s.methods, None);
                    for ti in &s.trait_impls {
                        self.register_methods(&s.name, &ti.methods, None);
                    }
                }
                Declaration::Enum(e) => {
                    let mut variants = HashMap::new();
                    for v in &e.variants {
                        variants.insert(v.name.clone(), v.fields.len());
                        self.variant_to_enum.insert(v.name.clone(), e.name.clone());
                    }
                    self.enums.insert(e.name.clone(), EnumDef { variants });
                    self.register_methods(&e.name, &e.methods, None);
                    for ti in &e.trait_impls {
                        self.register_methods(&e.name, &ti.methods, None);
                    }
                }
                Declaration::Impl(imp) => {
                    self.register_methods(&imp.type_name, &imp.methods, None);
                }
                Declaration::ExternJs(block) => {
                    // No Phoenix body to register, but record each extern's host
                    // module and parameter order. The name drives call precedence
                    // and a clean unbound-host error (vs. "undefined function");
                    // the module routes dispatch to the right host-registry
                    // binding (the ambient `js` or an npm package specifier); the
                    // parameter order lets a call's named args be reordered into
                    // positional form before marshalling (see `check_call`).
                    let host_module = block.module.as_deref().unwrap_or("js");
                    for item in &block.items {
                        let params = item.params.iter().map(|p| p.name.clone()).collect();
                        self.extern_params.insert(
                            item.name.clone(),
                            Rc::new((host_module.to_string(), params)),
                        );
                    }
                }
                Declaration::Trait(_)
                | Declaration::TypeAlias(_)
                | Declaration::Endpoint(_)
                | Declaration::Schema(_)
                | Declaration::Import(_) => {}
            }
        }

        let main_entry = self.functions.get("main").cloned();
        match main_entry {
            Some(entry) => {
                // Single-file path: no module stack, no scope to push.
                self.call_function_in_module(&entry.decl, vec![], vec![], None)?;
                Ok(())
            }
            None => error("no main() function found"),
        }
    }

    /// Calls a user-defined function with the given arguments, managing
    /// scope and call-depth tracking. `def_module` is the callee's
    /// owning module — `Some(...)` causes the body to evaluate with
    /// that module pushed on `module_stack` (so cross-module name
    /// lookups and default-arg evaluation resolve through the
    /// callee's scope), `None` is the single-file path that leaves
    /// the stack empty.
    ///
    /// Supports named arguments and default parameter values.
    ///
    /// Cleanup discipline: this wrapper owns the `call_depth` and
    /// `module_stack` lifecycle as a *single exit*. The body helper
    /// [`Self::call_function_body`] is structured so every fallible
    /// step (default-arg evaluation, missing-arg detection) runs
    /// *before* `env.push_scope()`, so it can return `Err` without
    /// any environment cleanup of its own.
    fn call_function_in_module(
        &mut self,
        func: &FunctionDecl,
        args: Vec<Value>,
        named_args: Vec<(String, Value)>,
        def_module: Option<Rc<phoenix_common::module_path::ModulePath>>,
    ) -> Result<Value> {
        self.call_depth += 1;
        if self.call_depth > MAX_CALL_DEPTH {
            self.call_depth -= 1;
            return error("stack overflow: maximum recursion depth exceeded");
        }
        let pushed_module = def_module.is_some();
        if let Some(mp) = def_module {
            self.module_stack.push(mp);
        }

        let result = self.call_function_body(func, args, named_args);

        if pushed_module {
            self.module_stack.pop();
        }
        self.call_depth -= 1;
        result
    }

    /// Inner body of [`Self::call_function_in_module`]. Resolves
    /// arguments + defaults *before* pushing the env scope, runs the
    /// body, then pops. Errors raised before `push_scope` need no
    /// cleanup; errors raised after are routed through `pop_scope`
    /// exactly once.
    fn call_function_body(
        &mut self,
        func: &FunctionDecl,
        args: Vec<Value>,
        named_args: Vec<(String, Value)>,
    ) -> Result<Value> {
        let non_self_params: Vec<&Param> =
            func.params.iter().filter(|p| p.name != "self").collect();
        let total_params = non_self_params.len();

        let mut param_values: Vec<Option<Value>> = vec![None; total_params];

        for (i, val) in args.into_iter().enumerate() {
            if i < total_params {
                param_values[i] = Some(val);
            }
        }

        for (name, val) in named_args {
            if let Some(idx) = non_self_params.iter().position(|p| p.name == name) {
                param_values[idx] = Some(val);
            }
        }

        // Evaluate defaults in the *callee's* module scope (the
        // caller pushed it before invoking us). Cross-module defaults
        // — e.g. a public function whose default-arg expression
        // references a private same-module helper — resolve through
        // the callee's scope rather than the caller's.
        for (i, param) in non_self_params.iter().enumerate() {
            if param_values[i].is_none()
                && let Some(ref default_expr) = param.default_value
            {
                param_values[i] = Some(self.eval_expr(default_expr)?);
            }
        }

        // Verify every parameter has a value *before* pushing the
        // env scope, so a missing-arg error needs no cleanup.
        for (i, param) in non_self_params.iter().enumerate() {
            if param_values[i].is_none() {
                return error(format!(
                    "function `{}`: missing argument for parameter `{}`",
                    func.name, param.name
                ));
            }
        }

        self.env.push_scope();
        for (i, param) in non_self_params.iter().enumerate() {
            self.env.define(
                param.name.clone(),
                param_values[i].take().expect("checked above"),
            );
        }

        let value = self.with_defer_frame(&func.body);
        self.env.pop_scope();
        value
    }

    /// Calls a method on a value by looking up the method in the method
    /// registry and binding `self` before evaluating the body.
    ///
    /// # `dyn Trait` dispatch — divergence from the IR interpreter
    ///
    /// The AST interpreter has no explicit trait-object value (no `Value::Dyn`).
    /// It resolves a `dyn Trait` method call by extracting the *concrete*
    /// runtime type tag from `self_val` (e.g. `Value::Struct(name, ...)`) and
    /// looking up `(type_name, method_name)` in [`Interpreter::methods`].
    /// This is late binding by type tag, not vtable dispatch.
    ///
    /// The IR interpreter and Cranelift backend, by contrast, materialize a
    /// `(data_ptr, vtable_ptr)` fat pointer and dispatch through a
    /// pre-computed vtable slot. The two paths agree today because every
    /// `dyn Trait` value observed at runtime carries a recoverable concrete
    /// type tag.
    ///
    /// This divergence will surface the first time `dyn Trait` is stored in
    /// a position where the concrete tag is *not* directly readable — e.g.
    /// heterogeneous `List<dyn Trait>` once that lands (see
    /// `docs/known-issues.md`: "`List<dyn Trait>` literal initialization in
    /// compiled mode"). At that point the AST interpreter must either gain
    /// an explicit `Value::Dyn` variant or route method dispatch through a
    /// trait-impl lookup that mirrors `IrModule::dyn_vtables`.
    fn call_method(
        &mut self,
        type_name: &str,
        method_name: &str,
        self_val: Value,
        args: Vec<Value>,
    ) -> Result<Value> {
        self.call_depth += 1;
        if self.call_depth > MAX_CALL_DEPTH {
            self.call_depth -= 1;
            return error("stack overflow: maximum recursion depth exceeded");
        }

        // Resolve the method *before* pushing its module, so a
        // missing-method error needs no stack cleanup. The method's
        // `MethodDef::module` (populated by `register_methods` on the
        // multi-module path) tells us which module's scope to push;
        // single-file dispatch leaves it `None`.
        let method = match self
            .methods
            .get(type_name)
            .and_then(|m| m.get(method_name))
            .cloned()
        {
            Some(m) => m,
            None => {
                self.call_depth -= 1;
                return Err(RuntimeError {
                    message: format!(
                        "no method `{}` on type `{}`",
                        method_name,
                        phoenix_common::module_path::bare_name(type_name),
                    ),
                    try_return_value: None,
                });
            }
        };

        let pushed_module = method.module.is_some();
        if let Some(ref mp) = method.module {
            self.module_stack.push(Rc::clone(mp));
        }

        let result = self.call_method_body(&method, type_name, method_name, self_val, args);

        if pushed_module {
            self.module_stack.pop();
        }
        self.call_depth -= 1;
        result
    }

    /// Inner body of [`Self::call_method`]. Same single-exit
    /// discipline as [`Self::call_function_body`]: resolve everything
    /// fallible before `env.push_scope()`, then push, run, pop.
    fn call_method_body(
        &mut self,
        method: &MethodDef,
        type_name: &str,
        method_name: &str,
        self_val: Value,
        args: Vec<Value>,
    ) -> Result<Value> {
        let non_self_params: Vec<&Param> = method
            .func
            .params
            .iter()
            .filter(|p| p.name != "self")
            .collect();
        let total_params = non_self_params.len();

        // "Too many" positional remains an error.  Under-fill is allowed
        // when every missing slot has a default.
        if args.len() > total_params {
            return error(format!(
                "method `{}` on `{}` takes {} argument(s), got {}",
                method_name,
                phoenix_common::module_path::bare_name(type_name),
                total_params,
                args.len()
            ));
        }

        let mut param_values: Vec<Option<Value>> = vec![None; total_params];
        for (i, val) in args.into_iter().enumerate() {
            param_values[i] = Some(val);
        }

        // Evaluate defaults in the *callee's* module scope (already
        // pushed by `call_method`).
        for (i, param) in non_self_params.iter().enumerate() {
            if param_values[i].is_none()
                && let Some(ref default_expr) = param.default_value
            {
                param_values[i] = Some(self.eval_expr(default_expr)?);
            }
        }

        // Verify every parameter has a value *before* pushing the
        // env scope, so a missing-arg error needs no cleanup.
        for (i, param) in non_self_params.iter().enumerate() {
            if param_values[i].is_none() {
                return error(format!(
                    "method `{}` on `{}`: missing argument for parameter `{}`",
                    method_name,
                    phoenix_common::module_path::bare_name(type_name),
                    param.name
                ));
            }
        }

        self.env.push_scope();
        self.env.define("self".to_string(), self_val);
        for (i, param) in non_self_params.iter().enumerate() {
            self.env.define(
                param.name.clone(),
                param_values[i].take().expect("checked above"),
            );
        }

        let value = self.with_defer_frame(&method.func.body);
        self.env.pop_scope();
        value
    }

    /// Executes a block without implicit return.
    ///
    /// Delegates to [`exec_block_inner`] with `implicit_return` set to `false`,
    /// meaning a trailing bare expression is evaluated for side effects only
    /// and does not produce a return value.
    fn exec_block(&mut self, block: &Block) -> Result<StmtResult> {
        self.exec_block_inner(block, false)
    }

    /// Run `body` with a fresh lexical scope pushed, popping it on every
    /// exit path (including `Err`-returning bodies).
    ///
    /// The interpreter's `try`-style escape — explicit `return` inside an
    /// expression context — propagates as `Err(try_return_value: ...)`.
    /// Any caller that pushed a scope before invoking such a body has to
    /// pop the scope on the error path too. If they don't, the leftover
    /// scope rides up to the next `with_defer_frame` call, where the
    /// `debug_assert_eq!(self.env.scope_depth(), entry_scope_depth)`
    /// guard fires with the message *"body must restore env scope depth
    /// before defers run"* — that's the symptom you'll see in a stack
    /// trace. Funneling all push/pop pairs through this helper makes
    /// that hard to get wrong.
    ///
    /// Note: a panic inside `body` still leaks the scope (no
    /// `catch_unwind` here). The interpreter treats panics as
    /// unrecoverable today, so this is intentional — but if a future
    /// change starts catching panics in callers, this helper needs an
    /// RAII guard to stay correct.
    fn with_scope<R>(&mut self, body: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        self.env.push_scope();
        let outcome = body(self);
        self.env.pop_scope();
        outcome
    }

    /// Executes a block with implicit return enabled (for function/closure bodies).
    ///
    /// When `implicit_return` is `true` and the last statement is a bare
    /// expression, its value is returned instead of being discarded.
    pub(crate) fn exec_block_implicit(&mut self, block: &Block) -> Result<StmtResult> {
        self.exec_block_inner(block, true)
    }

    /// Executes a block of statements, optionally treating the last expression
    /// as an implicit return value.
    ///
    /// When `implicit_return` is `true` and the final statement is a bare
    /// expression that produces a non-Void value, that value is wrapped in
    /// [`StmtResult::Return`]. This powers Phoenix's implicit-return semantics
    /// for function and closure bodies.
    fn exec_block_inner(&mut self, block: &Block, implicit_return: bool) -> Result<StmtResult> {
        let last_idx = block.statements.len().saturating_sub(1);
        for (i, stmt) in block.statements.iter().enumerate() {
            let is_last = implicit_return && i == last_idx && !block.statements.is_empty();
            // If this is the last statement and it's a bare expression,
            // evaluate it and return its value (implicit return).
            if is_last && let Statement::Expression(expr_stmt) = stmt {
                let value = self.eval_expr(&expr_stmt.expr)?;
                if !matches!(value, Value::Void) {
                    self.last_return_was_explicit = false;
                    return Ok(StmtResult::Return(value));
                }
                return Ok(StmtResult::Continue);
            }
            let result = self.exec_statement(stmt)?;
            // Check for pending control flow from expressions (e.g. break inside match)
            if let Some(cf) = self.pending_control_flow.take() {
                return Ok(match cf {
                    PendingControlFlow::Break => StmtResult::Break,
                    PendingControlFlow::LoopContinue => StmtResult::LoopContinue,
                });
            }
            match &result {
                StmtResult::Continue => {}
                StmtResult::Return(_) | StmtResult::Break | StmtResult::LoopContinue => {
                    return Ok(result);
                }
            }
        }
        Ok(StmtResult::Continue)
    }

    /// Executes a single statement and returns the resulting control flow signal.
    fn exec_statement(&mut self, stmt: &Statement) -> Result<StmtResult> {
        match stmt {
            Statement::VarDecl(var) => {
                let value = self.eval_expr(&var.initializer)?;
                match &var.target {
                    phoenix_parser::ast::VarDeclTarget::Simple(name) => {
                        self.env.define(name.clone(), value);
                    }
                    phoenix_parser::ast::VarDeclTarget::StructDestructure {
                        type_name: _,
                        field_names,
                    } => {
                        if let Value::Struct(_, ref fields) = value {
                            for field_name in field_names {
                                if let Some(field_value) = fields.get(field_name) {
                                    self.env.define(field_name.clone(), field_value.clone());
                                } else {
                                    return error(format!(
                                        "destructuring: no field `{}` in struct",
                                        field_name
                                    ));
                                }
                            }
                        } else {
                            return error("destructuring requires a struct value");
                        }
                    }
                }
                Ok(StmtResult::Continue)
            }
            Statement::Expression(expr_stmt) => {
                self.eval_expr(&expr_stmt.expr)?;
                Ok(StmtResult::Continue)
            }
            Statement::Return(ret) => {
                let value = match &ret.value {
                    Some(expr) => self.eval_expr(expr)?,
                    None => Value::Void,
                };
                self.last_return_was_explicit = true;
                Ok(StmtResult::Return(value))
            }
            Statement::While(w) => self.exec_while(w),
            Statement::For(f) => self.exec_for(f),
            Statement::Break(_) => Ok(StmtResult::Break),
            Statement::Continue(_) => Ok(StmtResult::LoopContinue),
            Statement::Defer(d) => {
                // Phoenix's grammar admits `Statement::Defer` only inside
                // a `parse_block`, and `parse_block` is only reachable
                // from a function/method/lambda body or a nested control
                // construct (`if`/`match`/loop) inside such a body. So
                // every `Defer` reaching `exec_stmt` is dynamically
                // executed within an active function call, and every
                // entry path (`call_function_body`, `call_method_body`,
                // `call_closure`) pushes a defer frame via
                // `with_defer_frame` before running the body.
                //
                // The `expect` is therefore a load-bearing assertion: if
                // it ever fires, a future entry path forgot to push a
                // frame (silent dropped defer would be the alternative
                // failure mode).
                self.defer_stack
                    .last_mut()
                    .expect("defer statement evaluated outside a function frame")
                    .push(d.expr.clone());
                Ok(StmtResult::Continue)
            }
        }
    }

    /// Evaluates a slice of expressions into a `Vec<Value>`.
    fn eval_args(&mut self, args: &[Expr]) -> Result<Vec<Value>> {
        args.iter().map(|a| self.eval_expr(a)).collect()
    }

    /// Checks that a method call has the expected argument count, evaluates the
    /// arguments, and returns them. Returns an error on count mismatch.
    pub(crate) fn expect_args(
        &mut self,
        method: &str,
        mc: &MethodCallExpr,
        expected: usize,
    ) -> Result<Vec<Value>> {
        if mc.args.len() != expected {
            return Err(arg_count_error(method, expected, mc.args.len()));
        }
        self.eval_args(&mc.args)
    }

    /// Executes a loop's else-block if the loop did not `break`.
    ///
    /// Routed through [`Self::with_scope`] so an explicit `return`
    /// inside the else block (which propagates as
    /// `Err(try_return_value: ...)`) doesn't skip `pop_scope`.
    fn exec_loop_else(&mut self, broke: bool, else_block: &Option<Block>) -> Result<StmtResult> {
        if !broke && let Some(ref block) = *else_block {
            let result = self.with_scope(|interp| interp.exec_block(block))?;
            if let StmtResult::Return(_) = result {
                return Ok(result);
            }
        }
        Ok(StmtResult::Continue)
    }

    /// Evaluates an `if`/`else if`/`else` expression, returning the [`Value`]
    /// of the taken branch.
    ///
    /// When the condition is false and no `else` branch is present, returns
    /// [`Value::Void`] — preserving statement-like behavior when wrapped in
    /// [`Statement::Expression`].
    ///
    /// Explicit `return` inside a branch propagates out via the `try_return_value`
    /// error channel, just like [`Self::eval_match`].
    fn eval_if(&mut self, if_expr: &IfExpr) -> Result<Value> {
        let condition = self.eval_expr(&if_expr.condition)?;
        if condition.is_truthy() {
            self.env.push_scope();
            let result = self.eval_branch_block(&if_expr.then_block);
            self.env.pop_scope();
            result
        } else if let Some(ref else_branch) = if_expr.else_branch {
            match else_branch {
                ElseBranch::Block(block) => {
                    self.env.push_scope();
                    let result = self.eval_branch_block(block);
                    self.env.pop_scope();
                    result
                }
                ElseBranch::ElseIf(elif) => self.eval_if(elif),
            }
        } else {
            Ok(Value::Void)
        }
    }

    /// Evaluates a block as a value-producing expression (for `if`-branch bodies).
    ///
    /// Propagates explicit `return` via `try_return_value`; maps loop-control
    /// flow to `pending_control_flow` mirroring the pattern used in match-arm
    /// block evaluation.
    fn eval_branch_block(&mut self, block: &Block) -> Result<Value> {
        match self.exec_block_implicit(block)? {
            StmtResult::Return(v) => {
                if self.last_return_was_explicit {
                    Err(RuntimeError {
                        message: String::new(),
                        try_return_value: Some(v),
                    })
                } else {
                    Ok(v)
                }
            }
            StmtResult::Break => {
                self.pending_control_flow = Some(PendingControlFlow::Break);
                Ok(Value::Void)
            }
            StmtResult::LoopContinue => {
                self.pending_control_flow = Some(PendingControlFlow::LoopContinue);
                Ok(Value::Void)
            }
            StmtResult::Continue => Ok(Value::Void),
        }
    }

    /// Executes a `while` loop. On each iteration the condition is
    /// re-evaluated; the loop exits when the condition is falsy **or**
    /// when a `break` statement is encountered. A `continue` statement
    /// skips the remaining body and proceeds to the next condition check.
    fn exec_while(&mut self, w: &WhileStmt) -> Result<StmtResult> {
        let mut broke = false;
        loop {
            let condition = self.eval_expr(&w.condition)?;
            if !condition.is_truthy() {
                break;
            }
            let result = self.with_scope(|interp| interp.exec_block(&w.body))?;
            match result {
                StmtResult::Break => {
                    broke = true;
                    break;
                }
                StmtResult::Return(_) => return Ok(result),
                StmtResult::LoopContinue | StmtResult::Continue => {}
            }
        }
        self.exec_loop_else(broke, &w.else_block)
    }

    /// Executes a `for` loop over a range or collection.
    fn exec_for(&mut self, f: &ForStmt) -> Result<StmtResult> {
        let items: Vec<Value> = match &f.source {
            ForSource::Range { start, end } => {
                let start_val = self.eval_expr(start)?;
                let end_val = self.eval_expr(end)?;
                match (&start_val, &end_val) {
                    (Value::Int(s), Value::Int(e)) => (*s..*e).map(Value::Int).collect(),
                    _ => return error("for loop range must be integers"),
                }
            }
            ForSource::Iterable(iter_expr) => {
                let collection = self.eval_expr(iter_expr)?;
                match collection {
                    Value::List(elements) => elements,
                    _ => return error("for...in requires a List"),
                }
            }
        };

        let mut broke = false;
        for item in items {
            let result = self.with_scope(|interp| {
                interp.env.define(f.var_name.clone(), item);
                interp.exec_block(&f.body)
            })?;
            match result {
                StmtResult::Break => {
                    broke = true;
                    break;
                }
                StmtResult::Return(_) => return Ok(result),
                StmtResult::LoopContinue | StmtResult::Continue => {}
            }
        }
        self.exec_loop_else(broke, &f.else_block)
    }

    /// Evaluates an expression AST node and returns the resulting runtime [`Value`].
    pub(crate) fn eval_expr(&mut self, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Literal(lit) => Ok(match &lit.kind {
                LiteralKind::Int(n) => Value::Int(*n),
                LiteralKind::Float(n) => Value::Float(*n),
                LiteralKind::String(s) => Value::String(s.clone()),
                LiteralKind::Bool(b) => Value::Bool(*b),
            }),

            Expr::Ident(ident) => {
                // Check if it's an enum variant with no fields
                if let Some(enum_name) = self.variant_to_enum.get(&ident.name).cloned() {
                    return Ok(Value::EnumVariant(enum_name, ident.name.clone(), vec![]));
                }
                self.env.get(&ident.name).ok_or_else(|| RuntimeError {
                    message: format!("undefined variable `{}`", ident.name),
                    try_return_value: None,
                })
            }

            Expr::Binary(binary) => self.eval_binary(binary),
            Expr::Unary(unary) => self.eval_unary(unary),
            Expr::Call(call) => self.eval_call(call),

            Expr::Assignment(assign) => {
                let value = self.eval_expr(&assign.value)?;
                if !self.env.set(&assign.name, value.clone()) {
                    return error(format!("undefined variable `{}`", assign.name));
                }
                Ok(value)
            }

            // Field assignment: evaluates the right-hand side, then delegates
            // to `assign_field` which handles both direct and nested struct
            // field mutation. Returns the assigned value.
            Expr::FieldAssignment(fa) => {
                let value = self.eval_expr(&fa.value)?;
                self.assign_field(&fa.object, &fa.field, value.clone())?;
                Ok(value)
            }

            Expr::FieldAccess(fa) => {
                let obj = self.eval_expr(&fa.object)?;
                match obj {
                    Value::Struct(_, ref fields) => {
                        fields.get(&fa.field).cloned().ok_or_else(|| RuntimeError {
                            message: format!("no field `{}`", fa.field),
                            try_return_value: None,
                        })
                    }
                    _ => error(format!("cannot access field on {}", obj.type_name())),
                }
            }

            Expr::MethodCall(mc) => self.eval_method_call(mc),
            Expr::StructLiteral(sl) => self.eval_struct_or_variant(sl),
            Expr::Match(m) => self.eval_match(m),
            Expr::If(if_expr) => self.eval_if(if_expr),

            Expr::ListLiteral(list) => {
                let elements: Vec<Value> = list
                    .elements
                    .iter()
                    .map(|e| self.eval_expr(e))
                    .collect::<Result<_>>()?;
                Ok(Value::List(elements))
            }

            Expr::MapLiteral(map) => {
                // Duplicate keys dedup last-wins, first position kept —
                // matching the runtime's `phx_map_from_pairs` (native /
                // wasm32-linear / wasm32-gc). A map can't hold two
                // same-key entries; the prior keep-all behavior diverged
                // from the compiled backends.
                let mut entries: Vec<(Value, Value)> = Vec::with_capacity(map.entries.len());
                for (k, v) in &map.entries {
                    let kv = self.eval_expr(k)?;
                    let vv = self.eval_expr(v)?;
                    if let Some(slot) = entries.iter_mut().find(|(ek, _)| map_key_eq(ek, &kv)) {
                        slot.1 = vv;
                    } else {
                        entries.push((kv, vv));
                    }
                }
                Ok(Value::Map(entries))
            }

            Expr::Try(try_expr) => self.eval_try(try_expr),

            // String interpolation: concatenates literal segments and the
            // display-formatted results of embedded expression segments into
            // a single String value.
            Expr::StringInterpolation(interp) => {
                let mut result = String::new();
                for segment in &interp.segments {
                    match segment {
                        StringSegment::Literal(s) => result.push_str(s),
                        StringSegment::Expr(expr) => {
                            let value = self.eval_expr(expr)?;
                            result.push_str(&value.to_string());
                        }
                    }
                }
                Ok(Value::String(result))
            }

            Expr::Lambda(lambda) => {
                let params: Vec<String> = lambda.params.iter().map(|p| p.name.clone()).collect();
                // Use sema-provided captures if available, otherwise compute
                // free variables from the AST (needed when interpreter-only
                // unit tests that skip the sema pass).
                let capture_names: Vec<String> =
                    if let Some(sema_captures) = self.lambda_captures.get(&lambda.span) {
                        sema_captures.iter().map(|c| c.name.clone()).collect()
                    } else {
                        phoenix_parser::free_vars::collect_free_variables(&lambda.body, &params)
                            .into_iter()
                            .collect()
                    };
                let mut captures = HashMap::new();
                for name in capture_names {
                    if let Some(cell) = self.env.get_cell(&name) {
                        captures.insert(name, cell);
                    }
                }
                Ok(Value::Closure {
                    params,
                    body: lambda.body.clone(),
                    captures,
                })
            }
        }
    }

    /// Evaluates a `match` expression by testing each arm's pattern against the subject.
    fn eval_match(&mut self, m: &MatchExpr) -> Result<Value> {
        let subject = self.eval_expr(&m.subject)?;

        for arm in &m.arms {
            if let Some(bindings) = self.match_pattern(&arm.pattern, &subject) {
                return self.with_scope(|interp| {
                    for (name, value) in bindings {
                        interp.env.define(name, value);
                    }
                    match &arm.body {
                        MatchBody::Expr(e) => interp.eval_expr(e),
                        MatchBody::Block(b) => match interp.exec_block_implicit(b)? {
                            StmtResult::Return(v) => {
                                if interp.last_return_was_explicit {
                                    // Explicit `return` — propagate as a
                                    // function return via try_return_value.
                                    // `with_scope` still pops on this Err
                                    // path, so we don't pop here.
                                    Err(RuntimeError {
                                        message: String::new(),
                                        try_return_value: Some(v),
                                    })
                                } else {
                                    // Implicit return (last expression value)
                                    Ok(v)
                                }
                            }
                            StmtResult::Break => {
                                interp.pending_control_flow = Some(PendingControlFlow::Break);
                                Ok(Value::Void)
                            }
                            StmtResult::LoopContinue => {
                                interp.pending_control_flow =
                                    Some(PendingControlFlow::LoopContinue);
                                Ok(Value::Void)
                            }
                            StmtResult::Continue => Ok(Value::Void),
                        },
                    }
                });
            }
        }

        error("non-exhaustive match: no pattern matched")
    }

    /// Attempts to match a value against a pattern, returning bindings on success.
    fn match_pattern(&self, pattern: &Pattern, value: &Value) -> Option<Vec<(String, Value)>> {
        match pattern {
            Pattern::Wildcard(_) => Some(vec![]),

            Pattern::Binding(name, _) => Some(vec![(name.clone(), value.clone())]),

            Pattern::Literal(lit) => {
                let pat_val = match &lit.kind {
                    LiteralKind::Int(n) => Value::Int(*n),
                    LiteralKind::Float(n) => Value::Float(*n),
                    LiteralKind::String(s) => Value::String(s.clone()),
                    LiteralKind::Bool(b) => Value::Bool(*b),
                };
                if &pat_val == value {
                    Some(vec![])
                } else {
                    None
                }
            }

            Pattern::Variant(vp) => {
                if let Value::EnumVariant(_, variant_name, fields) = value
                    && &vp.variant == variant_name
                {
                    let mut bindings = Vec::new();
                    for (i, binding) in vp.bindings.iter().enumerate() {
                        if binding != "_" && i < fields.len() {
                            bindings.push((binding.clone(), fields[i].clone()));
                        }
                    }
                    return Some(bindings);
                }
                None
            }
        }
    }

    /// Evaluates a method call, dispatching to built-in type methods or
    /// user-defined methods.
    fn eval_method_call(&mut self, mc: &MethodCallExpr) -> Result<Value> {
        // Namespace call (`lib.func(...)`): sema recorded the resolved
        // qualified target. The receiver is a namespace, not a value, so
        // dispatch straight to that function in its owning module — the
        // same path `eval_call` takes for a direct call, minus the callee
        // resolution (already done by sema).
        if let Some(qualified) = self.namespace_call_targets.get(&mc.span).cloned() {
            let Some(entry) = self.functions.get(&qualified).cloned() else {
                return error(format!(
                    "namespace call target `{qualified}` was recorded by sema but is not \
                     a registered function — sema/interpreter tables are out of sync"
                ));
            };
            let args = self.eval_args(&mc.args)?;
            return self.call_function_in_module(&entry.decl, args, Vec::new(), entry.module);
        }
        // `json.encode(value)`: the receiver is the intrinsic
        // `json` namespace, not a value. Evaluate the argument and encode
        // the runtime value directly — the interpreter has full type info,
        // so it needs no synthesized per-type encoder.
        if self.json_encode_spans.contains(&mc.span) {
            let value = self.eval_expr(&mc.args[0])?;
            return Ok(Value::String(self.json_encode_value(&value)?));
        }
        // `json.decode<T>(text)`: parse the string and build a
        // `Result<T, JsonError>` value guided by the target type `T`.
        if let Some(ty) = self.json_decode_types.get(&mc.span).cloned() {
            let text = match self.eval_expr(&mc.args[0])? {
                Value::String(s) => s,
                other => {
                    return error(format!("json.decode expects a String, got {other}"));
                }
            };
            return self.json_decode(&text, &ty);
        }
        // Recognize the builtin static constructors `List.builder()` and
        // `Map.builder()` before evaluating the object. The parser models
        // `Type.method(...)` as a method call with `object: Ident("Type")`;
        // evaluating the object first would hit the "undefined variable
        // `List`" path. Mirrors sema's `check_builtin_static_method`. The
        // carve-out skips when the receiver name shadows a local binding,
        // so a user `let List = some_value` then `List.builder()` falls
        // through to the normal value-receiver path.
        if let Expr::Ident(ident) = &mc.object
            && self.env.get(&ident.name).is_none()
            && let Some(result) = self.eval_builtin_static_method(&ident.name, mc)?
        {
            return Ok(result);
        }

        let obj = self.eval_expr(&mc.object)?;
        // Built-in type methods — match to move data instead of cloning
        match obj {
            Value::String(s) => return self.eval_string_method(s, mc),
            Value::Map(entries) => return self.eval_map_method(entries, mc),
            Value::List(elements) => return self.eval_list_method(elements, mc),
            Value::ListBuilder(ref buf) => {
                let buf = Rc::clone(buf);
                return self.eval_list_builder_method(buf, mc);
            }
            Value::MapBuilder(ref buf) => {
                let buf = Rc::clone(buf);
                return self.eval_map_builder_method(buf, mc);
            }
            Value::EnumVariant(ref enum_name, ref variant, ref fields) => {
                match enum_name.as_str() {
                    "Option" => {
                        if let Some(result) = self.eval_option_method(variant, fields, &obj, mc)? {
                            return Ok(result);
                        }
                    }
                    "Result" => {
                        if let Some(result) = self.eval_result_method(variant, fields, &obj, mc)? {
                            return Ok(result);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }

        // Dispatch by the canonical key (qualified for cross-module
        // user types) so the methods table — keyed under the qualified
        // receiver — resolves. Display in error messages goes through
        // `type_name()` which strips the prefix.
        let type_key = obj.type_key().to_string();
        let args = self.eval_args(&mc.args)?;
        self.call_method(&type_key, &mc.method, obj, args)
    }

    /// Evaluates a struct constructor or enum variant constructor.
    ///
    /// Lookup keys differ deliberately between the two branches:
    /// - **Structs** are resolved through `self.qualify(&sl.name)` so a
    ///   bare user-source name (`User`) maps to the qualified registry
    ///   key (`models::User`) it was registered under. The `Value::Struct`
    ///   built here carries that *qualified* name so `obj.type_key()`
    ///   later matches the methods table's key for cross-module dispatch.
    /// - **Enum variants** are resolved by the bare variant name
    ///   directly against `variant_to_enum`. Sema's
    ///   `lookup_visible_enum_variant` is the gatekeeper that rejects
    ///   ambiguous variant references (two visible enums sharing a
    ///   variant name) before this code runs, so the bare-key probe is
    ///   unambiguous at runtime. The constructed `Value::EnumVariant`
    ///   carries the *qualified* enum name so cross-module method
    ///   dispatch on the value resolves through the qualified key.
    ///
    /// Display in both cases goes through `bare_name`, so user-visible
    /// output (`print(u)`) shows `User`, not `models::User`.
    fn eval_struct_or_variant(&mut self, sl: &StructLiteralExpr) -> Result<Value> {
        let qualified = self.qualify(&sl.name);
        if let Some(struct_def) = self.structs.get(&qualified).cloned() {
            if sl.args.len() != struct_def.field_names.len() {
                return error(format!(
                    "struct `{}` takes {} field(s), got {}",
                    sl.name,
                    struct_def.field_names.len(),
                    sl.args.len()
                ));
            }
            let mut fields = BTreeMap::new();
            for (i, arg) in sl.args.iter().enumerate() {
                let value = self.eval_expr(arg)?;
                fields.insert(struct_def.field_names[i].clone(), value);
            }
            return Ok(Value::Struct(qualified, fields));
        }

        if let Some(enum_name) = self.variant_to_enum.get(&sl.name).cloned() {
            if let Some(enum_def) = self.enums.get(&enum_name)
                && let Some(&expected) = enum_def.variants.get(&sl.name)
                && sl.args.len() != expected
            {
                return error(format!(
                    "variant `{}` takes {} argument(s), got {}",
                    sl.name,
                    expected,
                    sl.args.len()
                ));
            }
            let field_vals = self.eval_args(&sl.args)?;
            return Ok(Value::EnumVariant(enum_name, sl.name.clone(), field_vals));
        }

        error(format!("undefined struct or variant `{}`", sl.name))
    }

    /// Evaluates the `?` (try/propagation) operator on a `Result` or `Option`.
    fn eval_try(&mut self, try_expr: &TryExpr) -> Result<Value> {
        let operand = self.eval_expr(&try_expr.operand)?;
        match &operand {
            Value::EnumVariant(enum_name, variant, fields) => {
                match (enum_name.as_str(), variant.as_str()) {
                    ("Result", "Ok") | ("Option", "Some") => {
                        Ok(fields.first().cloned().unwrap_or(Value::Void))
                    }
                    ("Result", "Err") | ("Option", "None") => Err(RuntimeError {
                        message: String::new(),
                        try_return_value: Some(operand),
                    }),
                    _ => error(format!("unexpected variant `{}` for ? operator", variant)),
                }
            }
            _ => error(format!(
                "the ? operator requires a Result or Option value, got {}",
                operand.type_name()
            )),
        }
    }

    /// Evaluates a binary expression by evaluating both operands and applying the operator.
    ///
    /// Boolean `and`/`or` use short-circuit evaluation: the right operand is
    /// only evaluated when the left operand does not determine the result.
    fn eval_binary(&mut self, binary: &BinaryExpr) -> Result<Value> {
        // Short-circuit: evaluate right side only when needed.
        if binary.op == BinaryOp::And {
            let left = self.eval_expr(&binary.left)?;
            return match left {
                Value::Bool(false) => Ok(Value::Bool(false)),
                Value::Bool(true) => {
                    let right = self.eval_expr(&binary.right)?;
                    match right {
                        Value::Bool(b) => Ok(Value::Bool(b)),
                        _ => error("`&&` requires Bool operands"),
                    }
                }
                _ => error("`&&` requires Bool operands"),
            };
        }
        if binary.op == BinaryOp::Or {
            let left = self.eval_expr(&binary.left)?;
            return match left {
                Value::Bool(true) => Ok(Value::Bool(true)),
                Value::Bool(false) => {
                    let right = self.eval_expr(&binary.right)?;
                    match right {
                        Value::Bool(b) => Ok(Value::Bool(b)),
                        _ => error("`||` requires Bool operands"),
                    }
                }
                _ => error("`||` requires Bool operands"),
            };
        }

        let left = self.eval_expr(&binary.left)?;
        let right = self.eval_expr(&binary.right)?;

        match binary.op {
            BinaryOp::Add => match (&left, &right) {
                (Value::String(a), Value::String(b)) => Ok(Value::String(a.clone() + b)),
                _ => checked_numeric_op(&left, &right, "add", i64::checked_add, |a, b| a + b),
            },
            BinaryOp::Sub => {
                checked_numeric_op(&left, &right, "subtract", i64::checked_sub, |a, b| a - b)
            }
            BinaryOp::Mul => {
                checked_numeric_op(&left, &right, "multiply", i64::checked_mul, |a, b| a * b)
            }
            BinaryOp::Div => match (&left, &right) {
                (Value::Int(_), Value::Int(0)) => error("division by zero"),
                (Value::Float(_), Value::Float(b)) if *b == 0.0 => error("division by zero"),
                _ => checked_numeric_op(&left, &right, "divide", i64::checked_div, |a, b| a / b),
            },
            BinaryOp::Mod => match (&left, &right) {
                (Value::Int(_), Value::Int(0)) => error("modulo by zero"),
                (Value::Float(_), Value::Float(b)) if *b == 0.0 => error("modulo by zero"),
                _ => checked_numeric_op(&left, &right, "modulo", i64::checked_rem, |a, b| a % b),
            },
            BinaryOp::Eq => Ok(Value::Bool(left == right)),
            BinaryOp::NotEq => Ok(Value::Bool(left != right)),
            BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq => {
                compare_values(&left, &right, binary.op)
            }
            // And/Or are handled above with short-circuit evaluation.
            BinaryOp::And | BinaryOp::Or => unreachable!(),
        }
    }

    /// Evaluates a unary expression (negation or logical not).
    fn eval_unary(&mut self, unary: &UnaryExpr) -> Result<Value> {
        let operand = self.eval_expr(&unary.operand)?;
        match unary.op {
            UnaryOp::Neg => match operand {
                Value::Int(n) => Ok(Value::Int(n.checked_neg().ok_or_else(|| RuntimeError {
                    message: "integer overflow".to_string(),
                    try_return_value: None,
                })?)),
                Value::Float(n) => Ok(Value::Float(-n)),
                _ => error(format!("cannot negate {}", operand.type_name())),
            },
            UnaryOp::Not => match operand {
                Value::Bool(b) => Ok(Value::Bool(!b)),
                _ => error(format!("cannot apply `!` to {}", operand.type_name())),
            },
        }
    }

    /// Evaluates a function call, handling built-in functions, named functions,
    /// closures, and lambda calls.
    fn eval_call(&mut self, call: &CallExpr) -> Result<Value> {
        // Check for built-in functions first
        if let Expr::Ident(ident) = &call.callee {
            match ident.name.as_str() {
                "print" => {
                    if call.args.len() != 1 {
                        return Err(arg_count_error("print", 1, call.args.len()));
                    }
                    let value = self.eval_expr(&call.args[0])?;
                    writeln!(self.output, "{}", value).map_err(|e| RuntimeError {
                        message: format!("write error: {e}"),
                        try_return_value: None,
                    })?;
                    return Ok(Value::Void);
                }
                "toString" => {
                    if call.args.len() != 1 {
                        return Err(arg_count_error("toString", 1, call.args.len()));
                    }
                    let value = self.eval_expr(&call.args[0])?;
                    return Ok(Value::String(value.to_string()));
                }
                _ => {}
            }
        }

        // Evaluate named arguments
        let named_args: Vec<(String, Value)> = call
            .named_args
            .iter()
            .map(|(name, expr)| Ok((name.clone(), self.eval_expr(expr)?)))
            .collect::<Result<_>>()?;

        // Check for named function or variable holding a closure
        if let Expr::Ident(ident) = &call.callee {
            // Named function
            // Qualify through the current module's scope so a call
            // to an imported function (`add` resolving to `lib::add`)
            // hits the right entry. Single-file callers leave the
            // module stack empty, so `qualify` falls back to the bare
            // name and registry lookup is unchanged.
            let qname = self.qualify(&ident.name);
            if let Some(entry) = self.functions.get(&qname).cloned() {
                let args = self.eval_args(&call.args)?;
                // Pull the def_module out of the entry so the callee's
                // body (and its default-arg evaluations) run with the
                // correct scope on the module stack — this is what
                // lets a call to an imported `tagged` whose default
                // is `defaultTag()` resolve `defaultTag` through
                // `lib`'s scope rather than the caller's.
                return self.call_function_in_module(&entry.decl, args, named_args, entry.module);
            }

            // `extern js` host call. Dispatch to the registered host
            // binding for the extern's declared module — the ambient `js` or an
            // npm package specifier; an unregistered extern is a
            // clean error (the bare CLI registers none). Checked *before* the
            // closure-variable arm to match sema's `check_call` precedence
            // (function -> extern -> variable), so a name bound to both an extern
            // and a local closure resolves to the extern in both sema and the
            // interpreter. Looked up by the qualified name (like `functions`
            // above) so a same-named extern in another module — possibly bound
            // to a different host module — can never satisfy this call site.
            if let Some(entry) = self.extern_params.get(&qname).cloned() {
                let (host_module, param_names) = &*entry;
                let positional = self.eval_args(&call.args)?;
                let args =
                    self.assemble_extern_args(&ident.name, param_names, positional, named_args)?;
                return self.call_extern_host(host_module, &ident.name, args);
            }

            // Variable holding a closure
            if let Some(val) = self.env.get(&ident.name) {
                let args = self.eval_args(&call.args)?;
                return self.call_closure(val, args);
            }

            return error(format!("undefined function `{}`", ident.name));
        }

        // Non-ident callee — evaluate it (could be a lambda expression)
        let callee_val = self.eval_expr(&call.callee)?;
        let args = self.eval_args(&call.args)?;
        self.call_closure(callee_val, args)
    }

    /// Assigns a value to a struct field, handling nested field access chains.
    ///
    /// For a simple case like `point.x = 10`, this retrieves the struct from
    /// the environment, mutates the field, and writes it back. For nested
    /// chains like `user.address.city = "NYC"`, the method recursively walks
    /// the chain, mutates the innermost field, and propagates the updated
    /// struct back to the root variable.
    fn assign_field(&mut self, object: &Expr, field: &str, value: Value) -> Result<()> {
        match object {
            Expr::Ident(ident) => {
                let mut obj = self.env.get(&ident.name).ok_or_else(|| RuntimeError {
                    message: format!("undefined variable `{}`", ident.name),
                    try_return_value: None,
                })?;
                if let Value::Struct(ref _name, ref mut fields) = obj {
                    fields.insert(field.to_string(), value);
                    if !self.env.set(&ident.name, obj) {
                        return error(format!("undefined variable `{}`", ident.name));
                    }
                    Ok(())
                } else {
                    error(format!("cannot assign field on {}", obj.type_name()))
                }
            }
            Expr::FieldAccess(fa) => {
                // Nested: e.g. user.address.city = value
                // First get the intermediate struct
                let mut parent = self.eval_expr(&fa.object)?;
                if let Value::Struct(ref _name, ref mut fields) = parent {
                    if let Some(inner) = fields.get_mut(&fa.field) {
                        if let Value::Struct(_, inner_fields) = inner {
                            inner_fields.insert(field.to_string(), value);
                        } else {
                            return error(format!("cannot assign field on {}", inner.type_name()));
                        }
                    } else {
                        return error(format!("no field `{}`", fa.field));
                    }
                    // Write updated parent back
                    self.assign_field_value(&fa.object, parent)?;
                    Ok(())
                } else {
                    error(format!("cannot assign field on {}", parent.type_name()))
                }
            }
            _ => error("invalid field assignment target".to_string()),
        }
    }

    /// Writes back a fully-constructed value to its root variable after nested
    /// field mutation. Walks the field access chain to find the root identifier,
    /// then sets the root variable to the updated struct.
    fn assign_field_value(&mut self, object: &Expr, value: Value) -> Result<()> {
        match object {
            Expr::Ident(ident) => {
                if !self.env.set(&ident.name, value) {
                    return error(format!("undefined variable `{}`", ident.name));
                }
                Ok(())
            }
            Expr::FieldAccess(fa) => {
                let mut parent = self.eval_expr(&fa.object)?;
                if let Value::Struct(_, ref mut fields) = parent {
                    fields.insert(fa.field.clone(), value);
                    self.assign_field_value(&fa.object, parent)
                } else {
                    error(format!("cannot assign field on {}", parent.type_name()))
                }
            }
            _ => error("invalid field assignment target".to_string()),
        }
    }

    /// Calls a closure value with the given arguments.
    ///
    /// A fresh environment is created containing only the closure's captured
    /// variable cells (in the base scope) and the parameter bindings (in a
    /// second scope).  Because captures are shared `Rc<RefCell<Value>>` cells,
    /// mutations inside the closure are visible in the enclosing scope and
    /// vice versa.
    pub(crate) fn call_closure(&mut self, callee: Value, args: Vec<Value>) -> Result<Value> {
        match callee {
            Value::Closure {
                params,
                body,
                captures,
            } => {
                self.call_depth += 1;
                if self.call_depth > MAX_CALL_DEPTH {
                    self.call_depth -= 1;
                    return error("stack overflow: maximum recursion depth exceeded");
                }

                if params.len() != args.len() {
                    self.call_depth -= 1;
                    return error(format!(
                        "closure takes {} argument(s), got {}",
                        params.len(),
                        args.len()
                    ));
                }

                // Build a fresh environment with captures in the base scope
                let mut closure_env = Environment::new();
                for (name, cell) in &captures {
                    closure_env.define_cell(name.clone(), Rc::clone(cell));
                }

                // Push a scope for parameter bindings
                closure_env.push_scope();
                for (name, value) in params.iter().zip(args) {
                    closure_env.define(name.clone(), value);
                }

                // Swap in the closure environment, execute, then restore.
                // Closures get their own defer frame (matching the IR side,
                // where each closure is lowered as its own function with a
                // fresh `pending_defers`), so a `defer` inside a closure
                // body fires when the closure returns — not at the
                // enclosing function's exit.
                let saved = std::mem::replace(&mut self.env, closure_env);
                let value = self.with_defer_frame(&body);
                self.env = saved;
                self.call_depth -= 1;
                value
            }
            _ => error(format!("cannot call value of type {}", callee.type_name())),
        }
    }
}

/// Interprets a single-module Phoenix program by executing its `main()`
/// function.
///
/// **Library API for embedders without `phoenix-modules`.** The Phoenix
/// driver (`cmd_run`) routes every program — including single-file
/// inputs — through [`run_modules`] so it can supply sema's
/// `module_scopes`. This entry point exists for embedders that lex,
/// parse, and check a single `Program` directly without going through
/// the resolver. Multi-file programs (`import` declarations referencing
/// sibling files) require [`run_modules`].
///
/// Registers all declarations (functions, structs, enums, traits,
/// impls), then calls `main()`. Returns `Ok(())` on success, or a
/// [`RuntimeError`] if execution fails (e.g., division by zero, stack
/// overflow, undefined variable).
///
/// `lambda_captures` provides the captured variables for each lambda,
/// as computed by the semantic checker. Pass an empty map if sema was
/// skipped.
///
/// # Examples
///
/// Run a simple program through the full pipeline:
///
/// ```
/// use phoenix_lexer::lexer::tokenize;
/// use phoenix_common::span::SourceId;
/// use phoenix_parser::parser;
/// use phoenix_sema::checker;
/// use phoenix_interp::interpreter;
///
/// let tokens = tokenize("function main() { print(1 + 2) }", SourceId(0));
/// let (program, parse_errors) = parser::parse(&tokens);
/// assert!(parse_errors.is_empty());
/// let check_result = checker::check(&program);
/// assert!(check_result.diagnostics.is_empty());
///
/// let result = interpreter::run(&program, check_result.module.lambda_captures);
/// assert!(result.is_ok());
/// ```
pub fn run(
    program: &Program,
    lambda_captures: HashMap<Span, Vec<CaptureInfo>>,
) -> std::result::Result<(), RuntimeError> {
    let mut interpreter = Interpreter::new();
    interpreter.lambda_captures = lambda_captures;
    interpreter.run_program(program)
}

/// A `Write` wrapper around `Rc<RefCell<Vec<u8>>>` so the buffer can be
/// shared with (and read after) the interpreter finishes.
struct SharedWriter(Rc<RefCell<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

/// Interprets a Phoenix program and captures all `print()` output as lines.
///
/// Returns `Ok(lines)` on success, where each element is one printed line,
/// or a [`RuntimeError`] if execution fails.
///
/// `lambda_captures` provides the captured variables for each lambda,
/// as computed by the semantic checker. Pass an empty map if sema was skipped.
///
/// # Examples
///
/// Capture the output of a program:
///
/// ```
/// use phoenix_lexer::lexer::tokenize;
/// use phoenix_common::span::SourceId;
/// use phoenix_parser::parser;
/// use phoenix_sema::checker;
/// use phoenix_interp::interpreter;
///
/// let source = r#"
/// function main() {
///   print("hello")
///   print(42)
/// }
/// "#;
/// let tokens = tokenize(source, SourceId(0));
/// let (program, _) = parser::parse(&tokens);
/// let check_result = checker::check(&program);
///
/// let output = interpreter::run_and_capture(&program, check_result.module.lambda_captures).unwrap();
/// assert_eq!(output, vec!["hello", "42"]);
/// ```
pub fn run_and_capture(
    program: &Program,
    lambda_captures: HashMap<Span, Vec<CaptureInfo>>,
) -> std::result::Result<Vec<String>, RuntimeError> {
    run_with_host_capture(program, lambda_captures, |_| {})
}

/// Like [`run_and_capture`], but registers host-FFI bindings (built by
/// `register`) before running, so `extern js` calls dispatch to the registered
/// Rust host functions. The AST-interpreter counterpart to
/// `phoenix_ir_interp::run_with_host_capture`: the two share one host-stub
/// contract ([`phoenix_common::host`]), which is what lets the five-backend
/// interop matrix register the *same* closures on both interpreters and assert
/// line-identical output (the byte-exact baseline is the Node tier's job).
/// Captures `print()` output as lines.
pub fn run_with_host_capture(
    program: &Program,
    lambda_captures: HashMap<Span, Vec<CaptureInfo>>,
    register: impl FnOnce(&mut Interpreter),
) -> std::result::Result<Vec<String>, RuntimeError> {
    let buffer = Rc::new(RefCell::new(Vec::<u8>::new()));
    let writer = SharedWriter(buffer.clone());
    let mut interpreter = Interpreter::with_output(Box::new(writer));
    interpreter.lambda_captures = lambda_captures;
    register(&mut interpreter);
    interpreter.run_program(program)?;
    let bytes = buffer.borrow();
    let output = String::from_utf8_lossy(&bytes);
    Ok(output.lines().map(String::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_common::span::SourceId;
    use phoenix_lexer::lexer::tokenize;
    use phoenix_parser::parser;

    fn run_source(source: &str) -> std::result::Result<(), RuntimeError> {
        let tokens = tokenize(source, SourceId(0));
        let (program, errors) = parser::parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);
        run(&program, HashMap::new())
    }

    #[test]
    fn run_empty_main() {
        run_source("function main() { }").unwrap();
    }

    #[test]
    fn run_simple_print() {
        run_source("function main() { print(42) }").unwrap();
    }

    #[test]
    fn run_while_loop() {
        run_source("function main() { let mut x: Int = 0\n while x < 5 { x = x + 1 }\n print(x) }")
            .unwrap();
    }

    #[test]
    fn run_for_loop() {
        run_source("function main() { let mut sum: Int = 0\n for i in 0..5 { sum = sum + i }\n print(sum) }").unwrap();
    }

    #[test]
    fn run_else_if() {
        run_source(
            "function main() {\n  let x: Int = 2\n  if x == 1 {\n    print(\"one\")\n  } else if x == 2 {\n    print(\"two\")\n  } else {\n    print(\"other\")\n  }\n}"
        ).unwrap();
    }

    #[test]
    fn run_struct() {
        run_source(
            "struct Point {\n  x: Int\n  y: Int\n}\nfunction main() {\n  let p: Point = Point(3, 4)\n  print(p.x)\n  print(p.y)\n}"
        ).unwrap();
    }

    #[test]
    fn run_method() {
        run_source(
            "struct Counter {\n  value: Int\n}\nimpl Counter {\n  function get(self) -> Int {\n    return self.value\n  }\n}\nfunction main() {\n  let c: Counter = Counter(42)\n  print(c.get())\n}"
        ).unwrap();
    }

    #[test]
    fn run_enum_and_match() {
        run_source(
            "enum Color {\n  Red\n  Green\n  Blue\n}\nfunction main() {\n  let c: Color = Red\n  match c {\n    Red -> print(\"red\")\n    Green -> print(\"green\")\n    Blue -> print(\"blue\")\n  }\n}"
        ).unwrap();
    }

    #[test]
    fn run_enum_with_fields() {
        run_source(
            "enum Shape {\n  Circle(Float)\n  Rect(Float, Float)\n}\nfunction main() {\n  let s: Shape = Circle(3.14)\n  match s {\n    Circle(r) -> print(r)\n    Rect(w, h) -> print(w)\n  }\n}"
        ).unwrap();
    }

    #[test]
    fn run_recursive_with_while() {
        run_source(
            "function main() {\n  let mut i: Int = 1\n  let mut result: Int = 1\n  while i <= 5 {\n    result = result * i\n    i = i + 1\n  }\n  print(result)\n}"
        ).unwrap();
    }

    #[test]
    fn run_for_loop_sum() {
        run_source(
            "function main() {\n  let mut sum: Int = 0\n  for i in 1..11 {\n    sum = sum + i\n  }\n  print(sum)\n}"
        ).unwrap();
    }

    #[test]
    fn run_no_main() {
        let result = run_source("function foo() { }");
        assert!(result.is_err());
    }

    #[test]
    fn run_division_by_zero() {
        let result = run_source("function main() { print(1 / 0) }");
        assert!(result.is_err());
    }

    /// Pins the Go-style defer error policy in `run_defers`:
    /// when one defer errors, every later (LIFO-ordered) defer still
    /// runs, and the *first* error encountered is the one propagated.
    ///
    /// Layout: `defer print("ran second")` is registered first, then
    /// `defer 1 / 0`. LIFO evaluation runs the divide-by-zero defer
    /// first (errors), then `print("ran second")` (succeeds, observed
    /// in stdout), and the function returns the divide-by-zero error.
    #[test]
    fn defer_error_propagation_first_error_wins_other_defers_run() {
        let source = "function main() {\n  defer print(\"ran second\")\n  defer 1 / 0\n}";
        let tokens = tokenize(source, SourceId(0));
        let (program, errors) = parser::parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);

        let buffer = Rc::new(RefCell::new(Vec::<u8>::new()));
        let writer = SharedWriter(buffer.clone());
        let mut interpreter = Interpreter::with_output(Box::new(writer));
        let result = interpreter.run_program(&program);
        drop(interpreter);

        let bytes = buffer.borrow().clone();
        let output = String::from_utf8_lossy(&bytes);
        let lines: Vec<String> = output.lines().map(String::from).collect();
        assert_eq!(lines, vec!["ran second"], "later defer must still run");

        let err = result.expect_err("first defer should have errored");
        assert!(
            err.message.contains("division by zero"),
            "expected first-encountered error, got: {}",
            err.message,
        );
    }

    /// Pins that defers fire on a runtime body error (not just on
    /// fall-through or explicit return). The body errors with division
    /// by zero *after* the defer is registered, so the defer must
    /// still run and the body's error must surface to the caller.
    #[test]
    fn defer_fires_on_body_runtime_error() {
        let source =
            "function main() {\n  defer print(\"ran on body error\")\n  let _: Int = 1 / 0\n}";
        let tokens = tokenize(source, SourceId(0));
        let (program, errors) = parser::parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);

        let buffer = Rc::new(RefCell::new(Vec::<u8>::new()));
        let writer = SharedWriter(buffer.clone());
        let mut interpreter = Interpreter::with_output(Box::new(writer));
        let result = interpreter.run_program(&program);
        drop(interpreter);

        let bytes = buffer.borrow().clone();
        let output = String::from_utf8_lossy(&bytes);
        let lines: Vec<String> = output.lines().map(String::from).collect();
        assert_eq!(
            lines,
            vec!["ran on body error"],
            "defer must still fire on body runtime error",
        );

        let err = result.expect_err("body should have errored");
        assert!(
            err.message.contains("division by zero"),
            "body error must surface, got: {}",
            err.message,
        );
    }

    /// Sibling of `defer_error_propagation_first_error_wins_other_defers_run`
    /// covering the symmetric layout: the *source-order-first* defer
    /// errors (so it runs LAST in LIFO), and the source-order-second
    /// defer succeeds (running FIRST in LIFO). Pins that the
    /// "first error encountered during draining" rule isn't quietly
    /// "first error in source order" — the propagated error must be
    /// the divide-by-zero from the LIFO-second defer, observed *after*
    /// the print from the LIFO-first defer hits stdout.
    #[test]
    fn defer_error_propagation_source_order_first_defer_errors_last() {
        let source = "function main() {\n  defer 1 / 0\n  defer print(\"ran first\")\n}";
        let tokens = tokenize(source, SourceId(0));
        let (program, errors) = parser::parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);

        let buffer = Rc::new(RefCell::new(Vec::<u8>::new()));
        let writer = SharedWriter(buffer.clone());
        let mut interpreter = Interpreter::with_output(Box::new(writer));
        let result = interpreter.run_program(&program);
        drop(interpreter);

        let bytes = buffer.borrow().clone();
        let output = String::from_utf8_lossy(&bytes);
        let lines: Vec<String> = output.lines().map(String::from).collect();
        assert_eq!(
            lines,
            vec!["ran first"],
            "LIFO-first defer must still run before the erroring defer",
        );

        let err = result.expect_err("source-order-first defer should have errored");
        assert!(
            err.message.contains("division by zero"),
            "expected divide-by-zero from the LIFO-last defer, got: {}",
            err.message,
        );
    }

    /// Pins error propagation across nested defer frames: a defer
    /// inside a closure errors → the closure call returns that error
    /// → the enclosing function's body sees a body error → the
    /// enclosing function's own defer still fires → body-error wins
    /// over defer-error, so the caller sees the closure's
    /// divide-by-zero. Stdout pins that the outer defer ran (proving
    /// the closure's error was treated as a body error in main, not
    /// silently swallowed by defer-error precedence).
    #[test]
    fn defer_error_inside_closure_propagates_through_outer_defer_frame() {
        let source = "function main() {\n  defer print(\"outer ran\")\n  let f = function() { defer 1 / 0 }\n  f()\n}";
        let tokens = tokenize(source, SourceId(0));
        let (program, errors) = parser::parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);

        let buffer = Rc::new(RefCell::new(Vec::<u8>::new()));
        let writer = SharedWriter(buffer.clone());
        let mut interpreter = Interpreter::with_output(Box::new(writer));
        let result = interpreter.run_program(&program);
        drop(interpreter);

        let bytes = buffer.borrow().clone();
        let output = String::from_utf8_lossy(&bytes);
        let lines: Vec<String> = output.lines().map(String::from).collect();
        assert_eq!(
            lines,
            vec!["outer ran"],
            "outer defer must fire after the closure's defer error became a body error",
        );

        let err = result.expect_err("closure defer error should have surfaced");
        assert!(
            err.message.contains("division by zero"),
            "closure's defer error must propagate through main's defer frame, got: {}",
            err.message,
        );
    }

    /// Pins the precedence rule in `with_defer_frame`: when the body
    /// errors *and* a defer also errors, the body's error is what the
    /// caller sees (defer errors are silently dropped). Distinguish
    /// the two using disjoint error messages — body raises division
    /// by zero, defer raises a list out-of-bounds — so the assert can
    /// confirm which one propagated.
    #[test]
    fn body_error_wins_over_defer_error() {
        let source = "function main() {\n  let nums: List<Int> = [1, 2, 3]\n  defer nums.get(-1)\n  let _: Int = 1 / 0\n}";
        let tokens = tokenize(source, SourceId(0));
        let (program, errors) = parser::parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);

        let mut interpreter = Interpreter::new();
        let err = interpreter
            .run_program(&program)
            .expect_err("body and defer both error; expected an error");

        assert!(
            err.message.contains("division by zero"),
            "body error must win over defer error, got: {}",
            err.message,
        );
        assert!(
            !err.message.contains("out of bounds"),
            "defer error must not surface when body errored, got: {}",
            err.message,
        );
    }

    /// Pins that defers fire on a value-less explicit `return` in a
    /// void function. The IR side has a separate `r.value.is_none()`
    /// arm in `lower_stmt::Statement::Return` that calls
    /// `lower_defers_for_exit` before `Terminator::Return(None)`; the
    /// AST interp routes through `unwrap_call_result` returning
    /// `Value::Void`. Either path regressing would skip the defer.
    #[test]
    fn defer_fires_on_void_explicit_return() {
        let source = "function main() {\n  defer print(\"cleanup\")\n  return\n}";
        let tokens = tokenize(source, SourceId(0));
        let (program, errors) = parser::parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);

        let buffer = Rc::new(RefCell::new(Vec::<u8>::new()));
        let writer = SharedWriter(buffer.clone());
        let mut interpreter = Interpreter::with_output(Box::new(writer));
        interpreter
            .run_program(&program)
            .expect("void return should succeed");
        drop(interpreter);

        let bytes = buffer.borrow().clone();
        let output = String::from_utf8_lossy(&bytes);
        let lines: Vec<String> = output.lines().map(String::from).collect();
        assert_eq!(
            lines,
            vec!["cleanup"],
            "defer must fire before a value-less explicit return",
        );
    }

    /// Pins lazy-capture re-entrancy: a defer's free-variable lookup
    /// must observe the value of the variable at function exit, even
    /// when an intermediate user-function call (which has its own
    /// independent defer frame) mutates that variable. This is the
    /// nested-frame counterpart to `defer_lazy_capture.phx` (which
    /// only mutates inside `main`); the test pins that pushing/popping
    /// a defer frame for the helper call doesn't somehow snapshot
    /// `main`'s deferred expressions.
    ///
    /// Layout: main has `let mut x = 1; defer print(x); helper(); print(x)`.
    /// `helper` itself has a defer (its own frame, observed via stdout
    /// to confirm both frames fired). After `helper` returns, main
    /// reassigns `x = 99` and falls through. The deferred `print(x)`
    /// in main observes `99`, not `1`.
    #[test]
    fn defer_lazy_capture_across_nested_function_call() {
        let source = "function helper() {\n  defer print(\"helper defer\")\n  print(\"helper body\")\n}\nfunction main() {\n  let mut x: Int = 1\n  defer print(x)\n  helper()\n  x = 99\n  print(x)\n}";
        let tokens = tokenize(source, SourceId(0));
        let (program, errors) = parser::parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);

        let buffer = Rc::new(RefCell::new(Vec::<u8>::new()));
        let writer = SharedWriter(buffer.clone());
        let mut interpreter = Interpreter::with_output(Box::new(writer));
        interpreter
            .run_program(&program)
            .expect("program should succeed");
        drop(interpreter);

        let bytes = buffer.borrow().clone();
        let output = String::from_utf8_lossy(&bytes);
        let lines: Vec<String> = output.lines().map(String::from).collect();
        assert_eq!(
            lines,
            vec!["helper body", "helper defer", "99", "99"],
            "helper's defer fires when helper returns; main's defer fires \
             at main's exit and observes the post-mutation value of `x`",
        );
    }

    #[test]
    fn run_match_wildcard() {
        run_source(
            "enum Color {\n  Red\n  Green\n  Blue\n}\nfunction main() {\n  let c: Color = Green\n  match c {\n    Red -> print(\"red\")\n    _ -> print(\"other\")\n  }\n}"
        ).unwrap();
    }

    #[test]
    fn run_enum_impl_method() {
        run_source(
            "enum Shape {\n  Circle(Float)\n  Rect(Float, Float)\n}\nimpl Shape {\n  function area(self) -> Float {\n    return match self {\n      Circle(r) -> 3.14 * r * r\n      Rect(w, h) -> w * h\n    }\n  }\n}\nfunction main() {\n  let s: Shape = Circle(5.0)\n  print(s.area())\n}"
        ).unwrap();
    }

    #[test]
    fn run_break_in_while() {
        run_source(
            "function main() { let mut x: Int = 0\n while true { x = x + 1\n if x == 5 { break } }\n print(x) }",
        ).unwrap();
    }

    #[test]
    fn run_continue_in_while() {
        run_source(
            "function main() { let mut sum: Int = 0\n let mut i: Int = 0\n while i < 10 { i = i + 1\n if i % 2 == 0 { continue }\n sum = sum + i }\n print(sum) }",
        ).unwrap();
    }

    #[test]
    fn run_break_in_for() {
        run_source(
            "function main() { let mut last: Int = 0\n for i in 0..100 { if i == 5 { break }\n last = i }\n print(last) }",
        ).unwrap();
    }

    #[test]
    fn run_continue_in_for() {
        run_source(
            "function main() { let mut sum: Int = 0\n for i in 0..10 { if i % 2 == 0 { continue }\n sum = sum + i }\n print(sum) }",
        ).unwrap();
    }

    #[test]
    fn run_enum_impl_shared_method() {
        run_source(
            "enum Color {\n  Red\n  Green\n  Blue\n}\nimpl Color {\n  function is_warm(self) -> Bool {\n    return match self {\n      Red -> true\n      _ -> false\n    }\n  }\n}\nfunction main() {\n  let r: Color = Red\n  let b: Color = Blue\n  print(r.is_warm())\n  print(b.is_warm())\n}"
        ).unwrap();
    }

    /// A basic lambda can be created, stored in a variable, and called.
    #[test]
    fn run_lambda_basic() {
        run_source(
            "function main() {\n  let double: (Int) -> Int = function(x: Int) -> Int { return x * 2 }\n  print(double(5))\n}"
        ).unwrap();
    }

    /// A lambda can be passed as an argument to a higher-order function.
    #[test]
    fn run_lambda_as_argument() {
        run_source(
            "function apply(f: (Int) -> Int, x: Int) -> Int {\n  return f(x)\n}\nfunction main() {\n  let triple: (Int) -> Int = function(x: Int) -> Int { return x * 3 }\n  print(apply(triple, 4))\n}"
        ).unwrap();
    }

    /// A closure captures variables from its enclosing scope (make_adder pattern).
    #[test]
    fn run_closure_capture() {
        run_source(
            "function make_adder(n: Int) -> (Int) -> Int {\n  return function(x: Int) -> Int { return x + n }\n}\nfunction main() {\n  let add5: (Int) -> Int = make_adder(5)\n  print(add5(10))\n}"
        ).unwrap();
    }

    /// A lambda can be created inline and passed directly to a function.
    #[test]
    fn run_lambda_inline_call() {
        run_source(
            "function apply(f: (Int) -> Int, x: Int) -> Int {\n  return f(x)\n}\nfunction main() {\n  let result: Int = apply(function(x: Int) -> Int { return x + 1 }, 9)\n  print(result)\n}"
        ).unwrap();
    }

    /// A generic identity function works with both Int and String values
    /// at runtime (type erasure means no runtime type checks).
    #[test]
    fn run_generic_identity() {
        run_source(
            "function identity<T>(x: T) -> T { return x }\nfunction main() {\n  let a: Int = identity(42)\n  print(a)\n  let b: String = identity(\"hello\")\n  print(b)\n}"
        ).unwrap();
    }

    /// A generic struct can be constructed and its fields accessed at runtime.
    #[test]
    fn run_generic_struct() {
        run_source(
            "struct Pair<A, B> {\n  first: A\n  second: B\n}\nfunction main() {\n  let p: Pair<Int, String> = Pair(1, \"hello\")\n  print(p.first)\n  print(p.second)\n}"
        ).unwrap();
    }

    /// A generic enum with `Some` and `None` variants works with match at runtime.
    #[test]
    fn run_generic_enum_match() {
        run_source(
            "enum Option<T> {\n  Some(T)\n  None\n}\nfunction main() {\n  let a: Option<Int> = Some(42)\n  match a {\n    Some(v) -> print(v)\n    None -> print(0)\n  }\n  let b: Option<Int> = None\n  match b {\n    Some(v) -> print(v)\n    None -> print(0)\n  }\n}"
        ).unwrap();
    }

    #[test]
    fn run_list_literal() {
        run_source("function main() { let nums: List<Int> = [1, 2, 3]\n print(nums) }").unwrap();
    }

    #[test]
    fn run_list_empty() {
        run_source("function main() { let nums: List<Int> = []\n print(nums) }").unwrap();
    }

    #[test]
    fn run_list_length() {
        run_source("function main() { let nums: List<Int> = [10, 20, 30]\n print(nums.length()) }")
            .unwrap();
    }

    #[test]
    fn run_list_get() {
        run_source("function main() { let nums: List<Int> = [10, 20, 30]\n print(nums.get(1)) }")
            .unwrap();
    }

    #[test]
    fn run_list_get_out_of_bounds() {
        let result =
            run_source("function main() { let nums: List<Int> = [1]\n print(nums.get(5)) }");
        assert!(result.is_err());
    }

    #[test]
    fn run_list_push() {
        run_source("function main() { let nums: List<Int> = [1, 2]\n let nums2: List<Int> = nums.push(3)\n print(nums2) }").unwrap();
    }

    #[test]
    fn run_list_iterate_with_while() {
        run_source(
            "function main() {\n  let nums: List<Int> = [10, 20, 30]\n  let mut i: Int = 0\n  let mut sum: Int = 0\n  while i < nums.length() {\n    sum = sum + nums.get(i)\n    i = i + 1\n  }\n  print(sum)\n}"
        ).unwrap();
    }

    /// A generic higher-order function (map) that takes a value and a closure,
    /// applying the closure to transform the value.
    #[test]
    fn run_generic_function_with_closure() {
        run_source(
            "function map<T, U>(x: T, f: (T) -> U) -> U {\n  return f(x)\n}\nfunction main() {\n  let result: String = map(42, function(n: Int) -> String { return toString(n) })\n  print(result)\n}"
        ).unwrap();
    }

    // --- Built-in Option and Result runtime tests ---

    /// `Some(42).unwrap()` returns 42.
    #[test]
    fn run_option_some_unwrap() {
        run_source("function main() {\n  let x: Option<Int> = Some(42)\n  print(x.unwrap())\n}")
            .unwrap();
    }

    /// `None.unwrap()` produces a runtime error.
    #[test]
    fn run_option_none_unwrap_panics() {
        let result =
            run_source("function main() {\n  let x: Option<Int> = None\n  print(x.unwrap())\n}");
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("unwrap()"));
    }

    /// `None.unwrapOr(10)` returns 10.
    #[test]
    fn run_option_unwrap_or() {
        run_source("function main() {\n  let x: Option<Int> = None\n  print(x.unwrapOr(10))\n}")
            .unwrap();
    }

    /// `Ok(42).unwrap()` returns 42.
    #[test]
    fn run_result_ok_unwrap() {
        run_source(
            "function main() {\n  let x: Result<Int, String> = Ok(42)\n  print(x.unwrap())\n}",
        )
        .unwrap();
    }

    /// `Err("fail").unwrap()` produces a runtime error.
    #[test]
    fn run_result_err_unwrap_panics() {
        let result = run_source(
            "function main() {\n  let x: Result<Int, String> = Err(\"fail\")\n  print(x.unwrap())\n}",
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("unwrap()"));
    }

    /// `Ok(1).isOk()` returns true, `Ok(1).isErr()` returns false.
    #[test]
    fn run_result_is_ok() {
        run_source(
            "function main() {\n  let x: Result<Int, String> = Ok(1)\n  print(x.isOk())\n  print(x.isErr())\n}"
        ).unwrap();
    }

    /// Match on Option with Some/None arms.
    #[test]
    fn run_option_match() {
        run_source(
            "function main() {\n  let x: Option<Int> = Some(42)\n  match x {\n    Some(v) -> print(v)\n    None -> print(0)\n  }\n}"
        ).unwrap();
    }

    /// A closure that returns another closure; both levels capture correctly.
    #[test]
    fn run_nested_closure() {
        run_source(
            "function main() {\n  let base: Int = 10\n  let make: (Int) -> (Int) -> Int = function(a: Int) -> (Int) -> Int {\n    return function(b: Int) -> Int { return base + a + b }\n  }\n  let add5: (Int) -> Int = make(5)\n  print(add5(3))\n}"
        ).unwrap();
    }

    /// A struct with a list field can be constructed and its list accessed.
    #[test]
    fn run_list_in_struct() {
        run_source(
            "struct Bag {\n  label: String\n  items: List<Int>\n}\nfunction main() {\n  let b: Bag = Bag(\"nums\", [10, 20, 30])\n  print(b.label)\n  print(b.items.get(1))\n}"
        ).unwrap();
    }

    /// `Option<List<Int>>` with Some([1,2,3]) can be matched and the list accessed.
    #[test]
    fn run_option_with_list() {
        run_source(
            "function main() {\n  let x: Option<List<Int>> = Some([1, 2, 3])\n  match x {\n    Some(nums) -> print(nums.get(0))\n    None -> print(0)\n  }\n}"
        ).unwrap();
    }

    /// `list.get(-1)` produces a runtime error (negative index out of bounds).
    #[test]
    fn run_list_negative_index() {
        let result =
            run_source("function main() { let nums: List<Int> = [1, 2, 3]\n print(nums.get(-1)) }");
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("out of bounds"));
    }

    /// A function that returns a closure; the returned closure is called successfully.
    #[test]
    fn run_closure_as_return_value() {
        run_source(
            "function make_multiplier(factor: Int) -> (Int) -> Int {\n  return function(x: Int) -> Int { return x * factor }\n}\nfunction main() {\n  let times3: (Int) -> Int = make_multiplier(3)\n  print(times3(7))\n}"
        ).unwrap();
    }

    /// Float division by zero returns a runtime error.
    #[test]
    fn run_float_division_by_zero() {
        let result = run_source("function main() { print(1.0 / 0.0) }");
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("division by zero"));
    }

    /// Float modulo by zero returns a runtime error.
    #[test]
    fn run_float_modulo_by_zero() {
        let result = run_source("function main() { print(1.0 % 0.0) }");
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("modulo by zero"));
    }

    /// Infinite recursion is caught with a stack overflow error.
    #[test]
    fn run_infinite_recursion_caught() {
        let result = run_source("function boom() { boom() }\nfunction main() { boom() }");
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("stack overflow"));
    }

    /// Mutual recursion that overflows is caught.
    #[test]
    fn run_mutual_recursion_caught() {
        let result =
            run_source("function a() { b() }\nfunction b() { a() }\nfunction main() { a() }");
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("stack overflow"));
    }

    /// String escape sequences are processed correctly.
    #[test]
    fn run_string_escape_sequences() {
        // Literal backslash-n should remain as backslash + n, not become a newline
        run_source(
            r#"function main() { let s: String = "hello\nworld"
print(s) }"#,
        )
        .unwrap();
    }

    /// Literal backslash in strings is handled correctly.
    #[test]
    fn run_string_literal_backslash() {
        run_source(
            r#"function main() { let s: String = "a\\b"
print(s) }"#,
        )
        .unwrap();
    }

    /// Match expression with block body executes correctly.
    #[test]
    fn run_match_with_block_body() {
        run_source(
            r#"
enum Shape {
  Circle(Float)
  Rect(Float, Float)
}
function main() {
  let s: Shape = Rect(3.0, 4.0)
  let result: String = match s {
    Circle(r) -> "circle"
    Rect(w, h) -> {
      if w == h { return "square" }
      return "rectangle"
    }
  }
  print(result)
}"#,
        )
        .unwrap();
    }

    /// Deeply nested generics work at runtime.
    #[test]
    fn run_deeply_nested_generics() {
        run_source(
            r#"
function main() {
  let x: Option<List<Int>> = Some([1, 2, 3])
  match x {
    Some(nums) -> print(nums.get(0))
    None -> print(0)
  }
  let items: List<Option<Int>> = [Some(1), None, Some(3)]
  print(items.get(0))
}"#,
        )
        .unwrap();
    }

    /// Closures capturing closures (3 levels deep) work correctly.
    #[test]
    fn run_triple_nested_closure() {
        run_source(
            r#"
function main() {
  let a: Int = 1
  let f: (Int) -> (Int) -> (Int) -> Int = function(b: Int) -> (Int) -> (Int) -> Int {
    return function(c: Int) -> (Int) -> Int {
      return function(d: Int) -> Int {
        return a + b + c + d
      }
    }
  }
  let g: (Int) -> (Int) -> Int = f(2)
  let h: (Int) -> Int = g(3)
  print(h(4))
}"#,
        )
        .unwrap();
    }

    // --- Phase 1.8 feature tests ---

    /// Field assignment basic case.
    #[test]
    fn run_field_assignment_basic() {
        run_source(
            r#"
struct Point {
  x: Int
  y: Int
}
function main() {
  let mut p: Point = Point(1, 2)
  p.x = 10
  print(p.x)
}"#,
        )
        .unwrap();
    }

    /// Field assignment nested.
    #[test]
    fn run_field_assignment_nested() {
        run_source(
            r#"
struct Inner { value: Int }
struct Outer { inner: Inner }
function main() {
  let mut o: Outer = Outer(Inner(1))
  o.inner.value = 42
  print(o.inner.value)
}"#,
        )
        .unwrap();
    }

    /// String interpolation runtime output.
    #[test]
    fn run_string_interpolation() {
        run_source(
            r#"
function main() {
  let name: String = "world"
  let greeting: String = "hello {name}"
  print(greeting)
}"#,
        )
        .unwrap();
    }

    /// `?` operator with Ok unwraps the value.
    #[test]
    fn run_try_operator_ok() {
        run_source(
            r#"
function get_value() -> Result<Int, String> {
  return Ok(42)
}
function compute() -> Result<Int, String> {
  let v: Int = get_value()?
  return Ok(v + 1)
}
function main() {
  let r: Result<Int, String> = compute()
  print(r)
}"#,
        )
        .unwrap();
    }

    /// `?` operator with Err propagates the error.
    #[test]
    fn run_try_operator_err() {
        run_source(
            r#"
function fail() -> Result<Int, String> {
  return Err("oops")
}
function compute() -> Result<Int, String> {
  let v: Int = fail()?
  return Ok(v + 1)
}
function main() {
  let r: Result<Int, String> = compute()
  print(r)
}"#,
        )
        .unwrap();
    }

    /// Implicit return works in functions.
    #[test]
    fn run_implicit_return_function() {
        run_source(
            r#"
function add(a: Int, b: Int) -> Int {
  a + b
}
function main() {
  let result: Int = add(3, 4)
  print(result)
}"#,
        )
        .unwrap();
    }

    /// Implicit return works in closures.
    #[test]
    fn run_implicit_return_closure() {
        run_source(
            r#"
function main() {
  let double: (Int) -> Int = function(x: Int) -> Int { x * 2 }
  print(double(5))
}"#,
        )
        .unwrap();
    }

    // Helper that captures printed output as lines.
    fn run_capturing_source(source: &str) -> Vec<String> {
        let tokens = tokenize(source, SourceId(0));
        let (program, errors) = parser::parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);
        run_and_capture(&program, HashMap::new()).unwrap()
    }

    // ---- List higher-order method tests ----

    /// `[1, 2, 3].map(fn)` doubles each element.
    #[test]
    fn run_list_map() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let doubled: List<Int> = nums.map(function(x: Int) -> Int { x * 2 })
  print(doubled)
}"#,
        );
        assert_eq!(output, vec!["[2, 4, 6]"]);
    }

    /// `[1, 2, 3, 4, 5].filter(fn)` keeps elements greater than 2.
    #[test]
    fn run_list_filter() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3, 4, 5]
  let filtered: List<Int> = nums.filter(function(x: Int) -> Bool { x > 2 })
  print(filtered)
}"#,
        );
        assert_eq!(output, vec!["[3, 4, 5]"]);
    }

    /// `[1, 2, 3, 4].reduce(0, fn)` sums all elements.
    #[test]
    fn run_list_reduce() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3, 4]
  let total: Int = nums.reduce(0, function(acc: Int, x: Int) -> Int { acc + x })
  print(total)
}"#,
        );
        assert_eq!(output, vec!["10"]);
    }

    /// `[1, 2, 3].find(fn)` returns Some(2) when predicate matches.
    #[test]
    fn run_list_find_some() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let found: Option<Int> = nums.find(function(x: Int) -> Bool { x == 2 })
  print(found)
}"#,
        );
        assert_eq!(output, vec!["Some(2)"]);
    }

    /// `[1, 2, 3].find(fn)` returns None when no element matches.
    #[test]
    fn run_list_find_none() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let found: Option<Int> = nums.find(function(x: Int) -> Bool { x == 5 })
  print(found)
}"#,
        );
        assert_eq!(output, vec!["None"]);
    }

    /// `[1, 2, 3].any(fn)` returns true when at least one element matches.
    #[test]
    fn run_list_any_true() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let result: Bool = nums.any(function(x: Int) -> Bool { x == 2 })
  print(result)
}"#,
        );
        assert_eq!(output, vec!["true"]);
    }

    /// `[1, 2, 3].any(fn)` returns false when no element matches.
    #[test]
    fn run_list_any_false() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let result: Bool = nums.any(function(x: Int) -> Bool { x == 5 })
  print(result)
}"#,
        );
        assert_eq!(output, vec!["false"]);
    }

    /// `[1, 2, 3].all(fn)` returns true when all elements match.
    #[test]
    fn run_list_all_true() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let result: Bool = nums.all(function(x: Int) -> Bool { x > 0 })
  print(result)
}"#,
        );
        assert_eq!(output, vec!["true"]);
    }

    /// `[1, 2, 3].all(fn)` returns false when not all elements match.
    #[test]
    fn run_list_all_false() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let result: Bool = nums.all(function(x: Int) -> Bool { x > 1 })
  print(result)
}"#,
        );
        assert_eq!(output, vec!["false"]);
    }

    /// `[1, 2, 3].flatMap(fn)` flattens nested lists.
    #[test]
    fn run_list_flat_map() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let result: List<Int> = nums.flatMap(function(x: Int) -> List<Int> { [x, x * 10] })
  print(result)
}"#,
        );
        assert_eq!(output, vec!["[1, 10, 2, 20, 3, 30]"]);
    }

    /// `[3, 1, 2].sortBy(fn)` sorts by comparator.
    #[test]
    fn run_list_sort_by() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [3, 1, 2]
  let sorted: List<Int> = nums.sortBy(function(a: Int, b: Int) -> Int { a - b })
  print(sorted)
}"#,
        );
        assert_eq!(output, vec!["[1, 2, 3]"]);
    }

    /// `.first()` and `.last()` return Option values.
    #[test]
    fn run_list_first_last() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [10, 20, 30]
  print(nums.first())
  print(nums.last())
}"#,
        );
        assert_eq!(output, vec!["Some(10)", "Some(30)"]);
    }

    /// `.contains(2)` on a list returns true.
    #[test]
    fn run_list_contains() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  print(nums.contains(2))
}"#,
        );
        assert_eq!(output, vec!["true"]);
    }

    /// `.take(3)` and `.drop(2)` return sub-lists.
    #[test]
    fn run_list_take_drop() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3, 4, 5]
  print(nums.take(3))
  print(nums.drop(2))
}"#,
        );
        assert_eq!(output, vec!["[1, 2, 3]", "[3, 4, 5]"]);
    }

    // ---- Map method tests ----

    /// `.set()` adds or updates a key in a map.
    #[test]
    fn run_map_set() {
        let output = run_capturing_source(
            r#"
function main() {
  let mut m: Map<String, Int> = {"a": 1, "b": 2}
  m = m.set("b", 99)
  m = m.set("c", 3)
  print(m)
}"#,
        );
        assert_eq!(output, vec!["{a: 1, b: 99, c: 3}"]);
    }

    /// `.remove()` removes a key from a map.
    #[test]
    fn run_map_remove() {
        let output = run_capturing_source(
            r#"
function main() {
  let mut m: Map<String, Int> = {"a": 1, "b": 2, "c": 3}
  m = m.remove("b")
  print(m)
}"#,
        );
        assert_eq!(output, vec!["{a: 1, c: 3}"]);
    }

    /// `.keys()` and `.values()` return lists.
    #[test]
    fn run_map_keys_values() {
        let output = run_capturing_source(
            r#"
function main() {
  let m: Map<String, Int> = {"x": 10, "y": 20}
  print(m.keys())
  print(m.values())
}"#,
        );
        assert_eq!(output, vec!["[x, y]", "[10, 20]"]);
    }

    // ---- Result/Option combinator tests ----

    /// `Err("bad").mapErr(fn)` transforms the error value.
    #[test]
    fn run_result_map_err() {
        let output = run_capturing_source(
            r#"
function main() {
  let r: Result<Int, String> = Err("bad")
  let mapped: Result<Int, String> = r.mapErr(function(e: String) -> String { "error: " + e })
  print(mapped)
}"#,
        );
        assert_eq!(output, vec!["Err(error: bad)"]);
    }

    /// `.ok()` on Ok returns Some, on Err returns None.
    #[test]
    fn run_result_ok() {
        let output = run_capturing_source(
            r#"
function main() {
  let good: Result<Int, String> = Ok(42)
  let bad: Result<Int, String> = Err("x")
  print(good.ok())
  print(bad.ok())
}"#,
        );
        assert_eq!(output, vec!["Some(42)", "None"]);
    }

    /// `.err()` on Err returns Some, on Ok returns None.
    #[test]
    fn run_result_err() {
        let output = run_capturing_source(
            r#"
function main() {
  let bad: Result<Int, String> = Err("fail")
  let good: Result<Int, String> = Ok(1)
  print(bad.err())
  print(good.err())
}"#,
        );
        assert_eq!(output, vec!["Some(fail)", "None"]);
    }

    // ---- Edge case tests ----

    /// Negating i64::MIN overflows and produces a runtime error.
    #[test]
    fn run_integer_negation_overflow() {
        let result = run_source(
            r#"
function main() {
  let min: Int = -9223372036854775807 - 1
  let boom: Int = -min
  print(boom)
}"#,
        );
        assert!(result.is_err());
        assert!(
            result.unwrap_err().message.contains("overflow"),
            "expected overflow error"
        );
    }

    /// `break` inside a match arm in a loop exits the loop.
    #[test]
    fn run_break_in_match_arm() {
        let output = run_capturing_source(
            r#"
enum Token {
  Num(Int)
  Stop
}
function main() {
  let tokens: List<Token> = [Num(1), Num(2), Stop, Num(3)]
  let mut sum: Int = 0
  let mut i: Int = 0
  while i < tokens.length() {
    let t: Token = tokens.get(i)
    match t {
      Num(n) -> { sum = sum + n }
      Stop -> { break }
    }
    i = i + 1
  }
  print(sum)
}"#,
        );
        assert_eq!(output, vec!["3"]);
    }

    // ─── If as a first-class expression ──────────────────────────────────
    // Value semantics of `if` expressions: let initializers,
    // arithmetic operands, else-if chains,
    // and the no-else Void case.

    #[test]
    fn run_if_expr_value_true_branch() {
        let output = run_capturing_source(
            r#"
function main() {
  let x: Int = if true { 10 } else { 20 }
  print(x)
}"#,
        );
        assert_eq!(output, vec!["10"]);
    }

    #[test]
    fn run_if_expr_value_false_branch() {
        let output = run_capturing_source(
            r#"
function main() {
  let x: Int = if false { 10 } else { 20 }
  print(x)
}"#,
        );
        assert_eq!(output, vec!["20"]);
    }

    #[test]
    fn run_if_expr_in_arithmetic() {
        let output = run_capturing_source(
            r#"
function main() {
  let y: Int = 1 + if true { 2 } else { 3 }
  print(y)
}"#,
        );
        assert_eq!(output, vec!["3"]);
    }

    #[test]
    fn run_if_expr_else_if_chain_chooses_middle() {
        let output = run_capturing_source(
            r#"
function main() {
  let x: Int = if false { 1 } else if true { 2 } else { 3 }
  print(x)
}"#,
        );
        assert_eq!(output, vec!["2"]);
    }

    #[test]
    fn run_if_expr_no_else_is_void_statement() {
        // `if` without else in statement position: value is discarded,
        // subsequent statements still execute.
        let output = run_capturing_source(
            r#"
function main() {
  if false { print("hidden") }
  print("visible")
}"#,
        );
        assert_eq!(output, vec!["visible"]);
    }

    #[test]
    fn run_if_expr_tail_recursive_fib() {
        let output = run_capturing_source(
            r#"
function fib(n: Int) -> Int {
  if n <= 1 { n } else { fib(n - 1) + fib(n - 2) }
}
function main() { print(fib(10)) }"#,
        );
        assert_eq!(output, vec!["55"]);
    }

    /// Nested field assignment through 3 levels of structs.
    #[test]
    fn run_nested_field_assignment_3_levels() {
        let output = run_capturing_source(
            r#"
struct C { value: Int }
struct B { c: C }
struct A { b: B }
function main() {
  let mut a: A = A(B(C(0)))
  a.b.c.value = 42
  print(a.b.c.value)
}"#,
        );
        assert_eq!(output, vec!["42"]);
    }

    /// Build a single-entry module slice from `source` and run it,
    /// capturing stdout. Thin wrapper over [`run_resolved_modules_capturing`].
    fn run_modules_capturing(source: &str) -> Vec<String> {
        use phoenix_modules::{ModulePath, ResolvedSourceModule};
        use std::path::PathBuf;
        let tokens = tokenize(source, SourceId(0));
        let (program, parse_errors) = parser::parse(&tokens);
        assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
        let module = ResolvedSourceModule {
            module_path: ModulePath::entry(),
            source_id: SourceId(0),
            program,
            is_entry: true,
            file_path: PathBuf::from("<test>"),
            import_targets: Default::default(),
        };
        run_resolved_modules_capturing(&[module])
    }

    /// Type-check an already-resolved multi-module slice, seed an
    /// interpreter from the analysis, and drive `run_modules_inner` with a
    /// capturing writer — returning stdout split into lines. Asserts the
    /// slice type-checks cleanly and runs without a runtime error.
    fn run_resolved_modules_capturing(
        modules: &[phoenix_modules::ResolvedSourceModule],
    ) -> Vec<String> {
        let mut analysis = phoenix_sema::checker::check_modules(modules);
        assert!(
            analysis.diagnostics.is_empty(),
            "sema diagnostics: {:?}",
            analysis.diagnostics
        );

        let buffer = Rc::new(RefCell::new(Vec::<u8>::new()));
        let writer = SharedWriter(buffer.clone());
        let mut interpreter = Interpreter::with_output(Box::new(writer));
        interpreter.seed_from_resolved(&mut analysis.module);
        interpreter
            .run_modules_inner(modules)
            .expect("runtime error");
        let bytes = buffer.borrow();
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(String::from)
            .collect()
    }

    /// Namespace calls (`lib.func(...)`) execute by dispatching to the
    /// resolved target function in its owning module. Drives
    /// `run_modules_inner` (via [`run_resolved_modules_capturing`]) with a
    /// capturing writer so the drained `namespace_call_targets` and the
    /// `eval_method_call` dispatch are exercised end to end — including a
    /// call that relies on a callee default argument (`scaled(5)`), which
    /// the namespace path must fill just like a direct call.
    #[test]
    fn namespace_call_executes_across_modules() {
        use phoenix_modules::{ModulePath, ResolvedSourceModule};
        use std::path::PathBuf;
        let mk = |path: ModulePath, src: &str, id: SourceId, is_entry: bool| {
            let tokens = tokenize(src, id);
            let (program, errs) = parser::parse(&tokens);
            assert!(errs.is_empty(), "parse errors: {:?}", errs);
            ResolvedSourceModule {
                module_path: path,
                source_id: id,
                program,
                is_entry,
                file_path: PathBuf::from("<test>"),
                import_targets: Default::default(),
            }
        };
        let entry = mk(
            ModulePath::entry(),
            "import lib\nfunction main() { print(lib.greet(\"Ada\")) print(lib.scaled(5)) }",
            SourceId(0),
            true,
        );
        let lib = mk(
            ModulePath(vec!["lib".to_string()]),
            "public function greet(name: String) -> String { \"hi {name}\" }\n\
             public function scaled(x: Int, by: Int = 10) -> Int { x * by }",
            SourceId(1),
            false,
        );
        let out = run_resolved_modules_capturing(&[entry, lib]);
        assert_eq!(out, vec!["hi Ada".to_string(), "50".to_string()]);
    }

    /// Like [`run_modules_capturing`], but registers host-FFI bindings (built
    /// by `register`) on the interpreter before running. Exercises the
    /// multi-module `extern js` path — `register_module_declarations`'s
    /// `ExternJs` branch (distinct from the single-module `run_program` loop)
    /// plus dispatch + marshalling end to end.
    fn run_modules_with_host_capturing(
        source: &str,
        register: impl FnOnce(&mut Interpreter),
    ) -> std::result::Result<Vec<String>, RuntimeError> {
        use phoenix_modules::{ModulePath, ResolvedSourceModule};
        use std::path::PathBuf;
        let tokens = tokenize(source, SourceId(0));
        let (program, parse_errors) = parser::parse(&tokens);
        assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
        let module = ResolvedSourceModule {
            module_path: ModulePath::entry(),
            source_id: SourceId(0),
            program,
            is_entry: true,
            file_path: PathBuf::from("<test>"),
            import_targets: Default::default(),
        };
        run_resolved_modules_with_host_capturing(&[module], register)
    }

    /// Slice-taking core of [`run_modules_with_host_capturing`]: type-check an
    /// already-resolved multi-module slice, seed an interpreter, register the
    /// host bindings, and drive `run_modules_inner` with a capturing writer.
    fn run_resolved_modules_with_host_capturing(
        modules: &[phoenix_modules::ResolvedSourceModule],
        register: impl FnOnce(&mut Interpreter),
    ) -> std::result::Result<Vec<String>, RuntimeError> {
        let mut analysis = phoenix_sema::checker::check_modules(modules);
        assert!(
            analysis.diagnostics.is_empty(),
            "sema diagnostics: {:?}",
            analysis.diagnostics
        );

        let buffer = Rc::new(RefCell::new(Vec::<u8>::new()));
        let writer = SharedWriter(buffer.clone());
        let mut interpreter = Interpreter::with_output(Box::new(writer));
        interpreter.seed_from_resolved(&mut analysis.module);
        register(&mut interpreter);
        interpreter.run_modules_inner(modules)?;
        let bytes = buffer.borrow();
        Ok(String::from_utf8_lossy(&bytes)
            .lines()
            .map(String::from)
            .collect())
    }

    /// The multi-module path (`run_modules` / `register_module_declarations`)
    /// has its own `extern js` registration branch, separate from the
    /// single-module `run_program` loop the other host tests exercise. Route an
    /// extern-calling program through it to pin that the branch records
    /// `extern_params` and that dispatch + marshalling work end to end — a
    /// regression dropping the multi-module registration would surface here as
    /// "undefined function `alert`".
    #[test]
    fn run_modules_dispatches_extern_js_host_call() {
        let seen = Rc::new(RefCell::new(Vec::<String>::new()));
        let seen2 = seen.clone();
        let out = run_modules_with_host_capturing(
            "extern js { function alert(message: String) }\n\
             function main() { alert(\"hi\") }",
            move |interp| {
                interp.register_host(
                    "js",
                    "alert",
                    Box::new(move |_ctx, args| {
                        if let Some(HostValue::Str(s)) = args.into_iter().next() {
                            seen2.borrow_mut().push(s);
                        }
                        Ok(HostValue::Void)
                    }),
                );
            },
        )
        .expect("program should run with the host binding registered");
        assert_eq!(out, Vec::<String>::new());
        assert_eq!(*seen.borrow(), vec!["hi".to_string()]);
    }

    /// Two modules may each declare a same-named extern bound to *different*
    /// host modules — sema scopes externs per module, so this is a legal
    /// program. Each call site must dispatch to its own declaration's host
    /// module: `extern_params` is keyed module-qualified (like `functions`),
    /// so a bare-keyed map regression — where whichever module registers last
    /// hijacks the other's call sites, routing the entry's ambient `tag()` to
    /// the npm-package host — surfaces here as swapped output.
    #[test]
    fn run_modules_routes_same_named_externs_to_their_own_host_modules() {
        use phoenix_modules::{ModulePath, ResolvedSourceModule};
        use std::path::PathBuf;
        let mk = |path: ModulePath, src: &str, id: SourceId, is_entry: bool| {
            let tokens = tokenize(src, id);
            let (program, errs) = parser::parse(&tokens);
            assert!(errs.is_empty(), "parse errors: {:?}", errs);
            ResolvedSourceModule {
                module_path: path,
                source_id: id,
                program,
                is_entry,
                file_path: PathBuf::from("<test>"),
                import_targets: Default::default(),
            }
        };
        let entry = mk(
            ModulePath::entry(),
            "import lib\n\
             extern js { function tag() -> String }\n\
             function main() { print(tag()) print(lib.libTag()) }",
            SourceId(0),
            true,
        );
        // `lib` registers *after* the entry, so a bare-keyed `extern_params`
        // would overwrite the entry's `tag` with the npm-package binding.
        let lib = mk(
            ModulePath(vec!["lib".to_string()]),
            "extern js \"pkg\" { function tag() -> String }\n\
             public function libTag() -> String { tag() }",
            SourceId(1),
            false,
        );
        let out = run_resolved_modules_with_host_capturing(&[entry, lib], |interp| {
            interp.register_host(
                "js",
                "tag",
                Box::new(|_ctx, _args| Ok(HostValue::Str("ambient".to_string()))),
            );
            interp.register_host(
                "pkg",
                "tag",
                Box::new(|_ctx, _args| Ok(HostValue::Str("package".to_string()))),
            );
        })
        .expect("both externs should dispatch to their own host bindings");
        assert_eq!(out, vec!["ambient".to_string(), "package".to_string()]);
    }

    /// The public [`run_modules_with_host`] entry point threads a *pre-built*
    /// [`HostRegistry`] (by value) onto the interpreter — the form an embedder
    /// uses, distinct from the closure-builder `register_host` the capturing
    /// helper above drives. Pin that the registry-by-value path actually
    /// dispatches: a regression dropping the `host_registry` assignment in
    /// `run_modules_with_host` would surface here as a clean "no host binding
    /// registered" error instead of the stub firing.
    #[test]
    fn run_modules_with_host_threads_prebuilt_registry() {
        use phoenix_modules::{ModulePath, ResolvedSourceModule};
        use std::path::PathBuf;
        let source = "extern js { function alert(message: String) }\n\
                      function main() { alert(\"hi\") }";
        let tokens = tokenize(source, SourceId(0));
        let (program, parse_errors) = parser::parse(&tokens);
        assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
        let module = ResolvedSourceModule {
            module_path: ModulePath::entry(),
            source_id: SourceId(0),
            program,
            is_entry: true,
            file_path: PathBuf::from("<test>"),
            import_targets: Default::default(),
        };
        let modules = [module];
        let mut analysis = phoenix_sema::checker::check_modules(&modules);
        assert!(
            analysis.diagnostics.is_empty(),
            "sema diagnostics: {:?}",
            analysis.diagnostics
        );

        let seen = Rc::new(RefCell::new(Vec::<String>::new()));
        let seen2 = seen.clone();
        let mut registry = phoenix_common::host::HostRegistry::new();
        registry.register(
            "js",
            "alert",
            Box::new(move |_ctx, args| {
                if let Some(HostValue::Str(s)) = args.into_iter().next() {
                    seen2.borrow_mut().push(s);
                }
                Ok(HostValue::Void)
            }),
        );

        run_modules_with_host(&modules, &mut analysis, registry)
            .expect("program should run with the pre-built host registry");
        assert_eq!(*seen.borrow(), vec!["hi".to_string()]);
    }

    /// Regression: a user enum that re-uses a builtin variant name
    /// (`enum Foo { Some }`) used to panic in debug builds when run
    /// through the multi-module path because `register_decl_in_module`
    /// asserted that no variant ever shadowed an existing entry.
    /// Sema accepts this program (only the *enum name* is checked
    /// against builtin shadowing, not its variants), so the assert was
    /// firing on legal code. The fix lets later registrations
    /// overwrite (matching the pre-existing single-file behaviour);
    /// this test routes the program through `run_modules` so a
    /// regression that re-introduces the assert would re-trip here.
    #[test]
    fn run_modules_user_variant_shadows_builtin_does_not_panic() {
        let output = run_modules_capturing("enum Foo { Some }\nfunction main() { print(1) }");
        assert_eq!(output, vec!["1"]);
    }

    /// Pin actual variant-construction-and-dispatch through
    /// `run_modules`: a non-shadowing user enum constructed and
    /// pattern-matched in the entry module. A regression where
    /// `register_decl_in_module` failed to populate `variant_to_enum`,
    /// or where `Value::EnumVariant` carried a stale enum-name tag
    /// that didn't match the registered key, would surface here as
    /// "undefined variable `Bar`" or as a `match`-arm mismatch.
    #[test]
    fn run_modules_constructs_and_matches_user_enum_variant() {
        let output = run_modules_capturing(
            r#"
enum Color { Red Green Blue }
function main() {
  let c: Color = Red
  let label: String = match c {
    Red -> "r"
    Green -> "g"
    Blue -> "b"
  }
  print(label)
}"#,
        );
        assert_eq!(output, vec!["r"]);
    }

    /// Regression for the scope-leak triggered when an explicit `return`
    /// inside an `if`-arm of a `for` body propagates as
    /// `Err(try_return_value: ...)`. Pre-fix, `?` propagated the error
    /// past `pop_scope`, so the iteration scope leaked up to the
    /// function frame and tripped the function-level scope-depth
    /// invariant. The function should return `1` (first matching item)
    /// without panicking.
    #[test]
    fn for_body_explicit_return_does_not_leak_iteration_scope() {
        let output = run_capturing_source(
            r#"
function find_first_even(xs: List<Int>) -> Int {
  for x in xs {
    if x % 2 == 0 {
      return x
    }
  }
  return -1
}
function main() {
  print(find_first_even([1, 3, 4, 5, 6]))
}"#,
        );
        assert_eq!(output, vec!["4"]);
    }

    /// Same shape as above but in a `while` loop.
    #[test]
    fn while_body_explicit_return_does_not_leak_iteration_scope() {
        let output = run_capturing_source(
            r#"
function first_above(limit: Int) -> Int {
  let mut i: Int = 0
  while i < 100 {
    if i > limit {
      return i
    }
    i = i + 1
  }
  return -1
}
function main() {
  print(first_above(7))
}"#,
        );
        assert_eq!(output, vec!["8"]);
    }

    /// Same shape but inside a `match` arm with a block body. Pre-fix,
    /// the bare `?` on `exec_block_implicit` skipped the arm-binding
    /// scope's `pop_scope`.
    #[test]
    fn match_arm_block_explicit_return_does_not_leak_arm_scope() {
        let output = run_capturing_source(
            r#"
enum Outcome { Hit(Int) Miss }
function classify(o: Outcome) -> Int {
  match o {
    Hit(n) -> { return n * 2 }
    Miss -> { return 0 }
  }
}
function main() {
  print(classify(Hit(5)))
  print(classify(Miss))
}"#,
        );
        assert_eq!(output, vec!["10", "0"]);
    }

    /// `for`/`while` `else` blocks were the last `push_scope`-with-`?`
    /// site on the same shape: `exec_loop_else` pushed a scope, called
    /// `exec_block(...)?`, then popped. If a `return` propagated as
    /// `Err(try_return_value: ...)` (e.g. through a `match` arm in the
    /// else-block body), the `?` skipped `pop_scope`. Post-fix it goes
    /// through `with_scope`.
    #[test]
    fn loop_else_explicit_return_does_not_leak_else_scope() {
        let output = run_capturing_source(
            r#"
enum Tag {
  A
  B
}
function choose(t: Tag) -> Int {
  for x in [1, 2, 3] {
    if x > 100 { break }
  } else {
    match t {
      A -> { return 11 }
      B -> { return 22 }
    }
  }
  return -1
}
function main() {
  print(choose(A))
  print(choose(B))
}"#,
        );
        assert_eq!(output, vec!["11", "22"]);
    }

    // ---- ListBuilder / MapBuilder ----

    /// `List.builder()` + `push` across a binding, then `freeze()`
    /// yields a `List` in push order. The shared-mutable buffer must
    /// persist across separate `push` statements on the same binding.
    #[test]
    fn run_list_builder_push_freeze_in_order() {
        let output = run_capturing_source(
            r#"
function main() {
  let b: ListBuilder<Int> = List.builder()
  b.push(3)
  b.push(1)
  b.push(2)
  let xs: List<Int> = b.freeze()
  print(xs)
  print(xs.length())
}"#,
        );
        assert_eq!(output, vec!["[3, 1, 2]", "3"]);
    }

    /// `push` inside a loop accumulates on the same shared buffer — the
    /// load-bearing case for `Rc<RefCell<…>>` (a cloned `Vec` would lose
    /// each iteration's push).
    #[test]
    fn run_list_builder_push_in_loop() {
        let output = run_capturing_source(
            r#"
function main() {
  let b: ListBuilder<Int> = List.builder()
  let mut i: Int = 0
  while i < 5 {
    b.push(i * 10)
    i = i + 1
  }
  let xs: List<Int> = b.freeze()
  print(xs)
}"#,
        );
        assert_eq!(output, vec!["[0, 10, 20, 30, 40]"]);
    }

    /// `MapBuilder.freeze` dedups last-wins while keeping each key's
    /// first-insertion position — byte-for-byte with native's
    /// `phx_map_builder_freeze` → `phx_map_from_pairs`. Key 3 keeps its
    /// first slot but takes the later value 99.
    #[test]
    fn run_map_builder_freeze_dedups_last_wins_first_position() {
        let output = run_capturing_source(
            r#"
function main() {
  let mb: MapBuilder<Int, Int> = Map.builder()
  mb.set(3, 1)
  mb.set(1, 2)
  mb.set(3, 99)
  mb.set(2, 5)
  let m: Map<Int, Int> = mb.freeze()
  let ks: List<Int> = m.keys()
  print(ks)
  print(m.get(3).unwrapOr(-1))
  print(m.length())
}"#,
        );
        assert_eq!(output, vec!["[3, 1, 2]", "99", "3"]);
    }

    /// `Map.builder()` + `set` in a loop, then `freeze`/`get`. Exercises
    /// the shared-mutable buffer and the O(n) dedup index together.
    #[test]
    fn run_map_builder_set_in_loop() {
        let output = run_capturing_source(
            r#"
function main() {
  let mb: MapBuilder<Int, Int> = Map.builder()
  let mut i: Int = 0
  while i < 4 {
    mb.set(i, i * 7)
    i = i + 1
  }
  let m: Map<Int, Int> = mb.freeze()
  print(m.length())
  print(m.get(2).unwrapOr(-1))
}"#,
        );
        assert_eq!(output, vec!["4", "14"]);
    }

    /// `push` after `freeze` is a runtime error — use-after-freeze is
    /// rejected on every backend (native aborts, wasm-gc traps), so the
    /// interpreter must too. Without this the interpreters would silently
    /// diverge from the compiled backends.
    #[test]
    fn list_builder_push_after_freeze_errors() {
        let err = run_source(
            r#"
function main() {
  let b: ListBuilder<Int> = List.builder()
  b.push(1)
  let xs: List<Int> = b.freeze()
  b.push(2)
}"#,
        )
        .expect_err("push after freeze should error");
        assert!(
            err.to_string().contains("frozen"),
            "expected a use-after-freeze error, got: {err}"
        );
    }

    /// A second `freeze` on a `MapBuilder` is a runtime error, matching
    /// native's single-use builder contract.
    #[test]
    fn map_builder_double_freeze_errors() {
        let err = run_source(
            r#"
function main() {
  let mb: MapBuilder<Int, Int> = Map.builder()
  mb.set(1, 10)
  let m1: Map<Int, Int> = mb.freeze()
  let m2: Map<Int, Int> = mb.freeze()
}"#,
        )
        .expect_err("double freeze should error");
        assert!(
            err.to_string().contains("frozen"),
            "expected a use-after-freeze error, got: {err}"
        );
    }

    // ── extern js host-FFI binding ──────────────

    /// Parse `source`, register declarations, register the host bindings built
    /// by `register`, run `main`, and return captured stdout (or the error).
    fn run_with_host(
        source: &str,
        register: impl FnOnce(&mut Interpreter),
    ) -> std::result::Result<String, RuntimeError> {
        let tokens = tokenize(source, SourceId(0));
        let (program, errors) = parser::parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);
        let buffer = Rc::new(RefCell::new(Vec::<u8>::new()));
        let mut interp = Interpreter::with_output(Box::new(SharedWriter(buffer.clone())));
        register(&mut interp);
        let result = interp.run_program(&program);
        drop(interp);
        result.map(|()| String::from_utf8_lossy(&buffer.borrow()).into_owned())
    }

    #[test]
    fn extern_js_call_dispatches_to_registered_host() {
        // A host stub for `alert` records what it was passed; the program's call
        // marshals the String argument across the boundary to it.
        let seen = Rc::new(RefCell::new(Vec::<String>::new()));
        let seen2 = seen.clone();
        let out = run_with_host(
            "extern js { function alert(message: String) }\n\
             function main() { alert(\"hi\") }",
            move |interp| {
                interp.register_host(
                    "js",
                    "alert",
                    Box::new(move |_ctx, args| {
                        if let Some(HostValue::Str(s)) = args.into_iter().next() {
                            seen2.borrow_mut().push(s);
                        }
                        Ok(HostValue::Void)
                    }),
                );
            },
        )
        .expect("program should run with the host binding registered");
        assert_eq!(out, "");
        assert_eq!(*seen.borrow(), vec!["hi".to_string()]);
    }

    #[test]
    fn extern_js_return_value_marshals_back() {
        // `getLength` returns an Int the program uses in arithmetic.
        let out = run_with_host(
            "extern js { function getLength(s: String) -> Int }\n\
             function main() { print(getLength(\"abc\") + 1) }",
            |interp| {
                interp.register_host(
                    "js",
                    "getLength",
                    Box::new(|_ctx, args| match args.into_iter().next() {
                        Some(HostValue::Str(s)) => Ok(HostValue::Int(s.len() as i64)),
                        _ => Err("expected a string".to_string()),
                    }),
                );
            },
        )
        .unwrap();
        assert_eq!(out.trim(), "4");
    }

    #[test]
    fn extern_js_callback_invokes_phoenix_closure() {
        // A host stub for a callback-taking API invokes the Phoenix closure it
        // is handed (synchronously) via the HostContext bridge.
        let out = run_with_host(
            "extern js { function callNow(cb: () -> Void) }\n\
             function main() { callNow(function() { print(\"called back\") }) }",
            |interp| {
                interp.register_host(
                    "js",
                    "callNow",
                    Box::new(|ctx, args| match args.into_iter().next() {
                        Some(HostValue::Callback(h)) => {
                            ctx.call_callback(h, vec![])?;
                            Ok(HostValue::Void)
                        }
                        _ => Err("expected a callback".to_string()),
                    }),
                );
            },
        )
        .unwrap();
        assert_eq!(out.trim(), "called back");
    }

    #[test]
    fn extern_js_jsvalue_round_trips_through_host() {
        // `getEl` returns an opaque JsValue handle; `tagOf` receives the same
        // handle back — Phoenix never inspects it, it only round-trips it.
        let out = run_with_host(
            "extern js {\n\
               function getEl(id: String) -> JsValue\n\
               function tagOf(e: JsValue) -> String\n\
             }\n\
             function main() {\n\
               let e: JsValue = getEl(\"root\")\n\
               print(tagOf(e))\n\
             }",
            |interp| {
                interp.register_host("js", "getEl", Box::new(|_c, _a| Ok(HostValue::JsValue(7))));
                interp.register_host(
                    "js",
                    "tagOf",
                    Box::new(|_c, args| match args.into_iter().next() {
                        Some(HostValue::JsValue(7)) => Ok(HostValue::Str("DIV".to_string())),
                        other => Err(format!("unexpected handle: {other:?}")),
                    }),
                );
            },
        )
        .unwrap();
        assert_eq!(out.trim(), "DIV");
    }

    #[test]
    fn extern_js_unbound_host_errors_cleanly() {
        // No host registered → a clean "no host binding" error, never a panic
        // or a silent no-op.
        let err = run_with_host(
            "extern js { function alert(message: String) }\n\
             function main() { alert(\"x\") }",
            |_interp| {},
        )
        .expect_err("an unbound extern call should error");
        assert!(
            err.message.contains("no host binding registered") && err.message.contains("js.alert"),
            "expected a clean unbound-host error, got: {}",
            err.message
        );
    }

    #[test]
    fn extern_js_npm_module_dispatches_by_module() {
        // An `extern js "pkg" { ... }` extern (Phase 3.1.2) dispatches to the
        // binding registered under the *package* module, not the ambient `js`.
        let out = run_with_host(
            "extern js \"left-pad\" { function leftPad(s: String, width: Int) -> String }\n\
             function main() { print(leftPad(\"4\", 3)) }",
            |interp| {
                interp.register_host(
                    "left-pad",
                    "leftPad",
                    Box::new(|_ctx, args| {
                        let mut it = args.into_iter();
                        match (it.next(), it.next()) {
                            (Some(HostValue::Str(s)), Some(HostValue::Int(w))) => {
                                Ok(HostValue::Str(format!("{s:>width$}", width = w as usize)))
                            }
                            other => Err(format!("unexpected args: {other:?}")),
                        }
                    }),
                );
            },
        )
        .unwrap();
        assert_eq!(out.trim_end(), "  4");
    }

    #[test]
    fn extern_js_npm_module_does_not_fall_back_to_the_ambient_host() {
        // A binding registered under the ambient `js` module must NOT satisfy a
        // same-named extern declared against an npm package — that would
        // silently mis-route the call. The unbound error names the package.
        let err = run_with_host(
            "extern js \"left-pad\" { function leftPad(s: String, width: Int) -> String }\n\
             function main() { print(leftPad(\"4\", 3)) }",
            |interp| {
                interp.register_host(
                    "js",
                    "leftPad",
                    Box::new(|_ctx, _args| {
                        Err("the ambient binding must not be reached".to_string())
                    }),
                );
            },
        )
        .expect_err("an npm extern with only an ambient binding should error");
        assert!(
            err.message.contains("no host binding registered")
                && err.message.contains("left-pad.leftPad"),
            "expected an unbound-host error naming the package, got: {}",
            err.message
        );
    }

    #[test]
    fn extern_js_callback_can_call_another_extern() {
        // Re-entrancy: the host `run` invokes the Phoenix callback, which itself
        // calls a *second* extern (`shout`). The registry must stay populated for
        // the duration of the outer host call so the nested dispatch resolves —
        // a regression guard for the registry handling in `call_extern_host`.
        let out = run_with_host(
            "extern js {\n\
               function run(cb: () -> Void)\n\
               function shout(s: String) -> String\n\
             }\n\
             function main() { run(function() { print(shout(\"hi\")) }) }",
            |interp| {
                interp.register_host(
                    "js",
                    "run",
                    Box::new(|ctx, args| match args.into_iter().next() {
                        Some(HostValue::Callback(h)) => {
                            ctx.call_callback(h, vec![])?;
                            Ok(HostValue::Void)
                        }
                        _ => Err("expected a callback".to_string()),
                    }),
                );
                interp.register_host(
                    "js",
                    "shout",
                    Box::new(|_ctx, args| match args.into_iter().next() {
                        Some(HostValue::Str(s)) => Ok(HostValue::Str(s.to_uppercase())),
                        _ => Err("expected a string".to_string()),
                    }),
                );
            },
        )
        .expect("a nested extern through a callback should dispatch");
        assert_eq!(out.trim(), "HI");
    }

    #[test]
    fn extern_js_named_args_reorder_to_positional() {
        // Sema accepts named args on extern calls (same validation path as
        // regular functions), so the interpreter must reorder them into the
        // declared parameter order before marshalling — matching the IR backend.
        // Here `pair` is declared `(name, id)` but called `(id:, name:)`.
        let out = run_with_host(
            "extern js { function pair(name: String, id: Int) -> String }\n\
             function main() { print(pair(id: 7, name: \"x\")) }",
            |interp| {
                interp.register_host(
                    "js",
                    "pair",
                    Box::new(|_ctx, args| {
                        let mut it = args.into_iter();
                        match (it.next(), it.next()) {
                            (Some(HostValue::Str(name)), Some(HostValue::Int(id))) => {
                                Ok(HostValue::Str(format!("{name}={id}")))
                            }
                            _ => Err("expected (String, Int) in declared order".to_string()),
                        }
                    }),
                );
            },
        )
        .unwrap();
        assert_eq!(out.trim(), "x=7");
    }

    #[test]
    fn extern_shadows_same_named_local_closure() {
        // Precedence guard: a bare name bound to *both* an `extern js` function
        // and a local closure variable must dispatch to the extern, matching
        // sema's `check_call` resolution order (function -> extern -> variable).
        // The extern arm is checked before the closure-variable arm in
        // `eval_call`; this pins that the interpreter agrees with sema rather
        // than calling the shadowing local closure.
        let fired = Rc::new(RefCell::new(false));
        let fired2 = fired.clone();
        let out = run_with_host(
            "extern js { function greet() }\n\
             function main() {\n\
               let greet = function() { print(\"LOCAL\") }\n\
               greet()\n\
             }",
            move |interp| {
                interp.register_host(
                    "js",
                    "greet",
                    Box::new(move |_ctx, _args| {
                        *fired2.borrow_mut() = true;
                        Ok(HostValue::Void)
                    }),
                );
            },
        )
        .expect("program should run with the host binding registered");
        assert!(*fired.borrow(), "the extern binding should have fired");
        assert_eq!(out, "", "the shadowing local closure must not be called");
    }

    #[test]
    fn extern_js_callback_receives_marshalled_args() {
        // The host invokes the Phoenix callback *with* a value (the
        // `setTimeout(cb, x)` shape), exercising the inbound-arg marshalling in
        // `call_callback` that the empty-arg callback tests never reach.
        let out = run_with_host(
            "extern js { function withValue(cb: (Int) -> Void) }\n\
             function main() { withValue(function(n: Int) { print(n + 1) }) }",
            |interp| {
                interp.register_host(
                    "js",
                    "withValue",
                    Box::new(|ctx, args| match args.into_iter().next() {
                        Some(HostValue::Callback(h)) => {
                            ctx.call_callback(h, vec![HostValue::Int(41)])?;
                            Ok(HostValue::Void)
                        }
                        _ => Err("expected a callback".to_string()),
                    }),
                );
            },
        )
        .unwrap();
        assert_eq!(out.trim(), "42");
    }

    #[test]
    fn extern_js_host_error_surfaces_cleanly() {
        // A host function returning `Err` must surface as a clean runtime error
        // carrying the host's message — not a panic, not a swallowed failure.
        let err = run_with_host(
            "extern js { function boom() }\n\
             function main() { boom() }",
            |interp| {
                interp.register_host(
                    "js",
                    "boom",
                    Box::new(|_ctx, _args| Err("host blew up".to_string())),
                );
            },
        )
        .expect_err("a host function returning Err should error");
        assert!(
            err.message.contains("host blew up"),
            "expected the host error message to surface, got: {}",
            err.message
        );
    }

    #[test]
    fn extern_js_host_returning_callback_is_rejected() {
        // A host that hands back a callback handle (not a receivable value) is
        // marshalled-in as a clean error rather than silently producing a bogus
        // value Phoenix cannot represent.
        let err = run_with_host(
            "extern js { function evil() }\n\
             function main() { evil() }",
            |interp| {
                interp.register_host(
                    "js",
                    "evil",
                    Box::new(|_ctx, _args| Ok(HostValue::Callback(CallbackHandle(0)))),
                );
            },
        )
        .expect_err("a host returning a callback handle should error");
        assert!(
            err.message.contains("callback handle"),
            "expected a clean rejection of the returned callback, got: {}",
            err.message
        );
    }

    #[test]
    fn jsvalue_equality_is_by_handle() {
        // Sema permits `==` on `JsValue` (any type-compatible pair is
        // equatable), so the interpreter must compare opaque handles by identity:
        // the same host handle is equal to itself; distinct handles are not.
        let out = run_with_host(
            "extern js {\n\
               function getEl(id: String) -> JsValue\n\
             }\n\
             function main() {\n\
               let a: JsValue = getEl(\"x\")\n\
               let b: JsValue = getEl(\"x\")\n\
               let c: JsValue = getEl(\"y\")\n\
               print(a == a)\n\
               print(a == b)\n\
               print(a == c)\n\
             }",
            |interp| {
                // Same id ↔ same object; `getEl(\"y\")` is a different handle.
                interp.register_host(
                    "js",
                    "getEl",
                    Box::new(|_c, args| match args.into_iter().next() {
                        Some(HostValue::Str(s)) if s == "y" => Ok(HostValue::JsValue(2)),
                        Some(HostValue::Str(_)) => Ok(HostValue::JsValue(1)),
                        _ => Err("expected a string".to_string()),
                    }),
                );
            },
        )
        .unwrap();
        assert_eq!(out, "true\ntrue\nfalse\n");
    }

    #[test]
    fn extern_js_float_round_trips_through_host() {
        // A Float crosses the boundary out (as an arg) and back (as the result),
        // exercising the `Float` marshalling arm the scalar tests otherwise skip.
        let out = run_with_host(
            "extern js { function twice(x: Float) -> Float }\n\
             function main() { print(twice(1.5)) }",
            |interp| {
                interp.register_host(
                    "js",
                    "twice",
                    Box::new(|_ctx, args| match args.into_iter().next() {
                        Some(HostValue::Float(x)) => Ok(HostValue::Float(x * 2.0)),
                        _ => Err("expected a float".to_string()),
                    }),
                );
            },
        )
        .unwrap();
        assert_eq!(out.trim(), "3.0");
    }

    #[test]
    fn extern_js_non_marshallable_arg_is_rejected() {
        // Marshalling-*out* a value sema would never permit at an extern
        // signature (here a `List`) is a clean internal error, not a panic. Sema
        // normally rejects this at the signature; `run_with_host` skips sema, so
        // this drives the defensive `value_to_host` aggregate arm directly.
        let err = run_with_host(
            "extern js { function take(xs: List<Int>) }\n\
             function main() { take([1, 2, 3]) }",
            |interp| {
                interp.register_host("js", "take", Box::new(|_ctx, _args| Ok(HostValue::Void)));
            },
        )
        .expect_err("a non-marshallable arg should error");
        assert!(
            err.message
                .contains("cannot cross the `extern js` boundary"),
            "expected a clean non-marshallable-value error, got: {}",
            err.message
        );
    }

    #[test]
    fn assemble_extern_args_reports_internal_errors() {
        // The arity/name guards in `assemble_extern_args` back sema's validation;
        // a sema-valid program never trips them, so exercise them directly. Each
        // must fail loudly rather than silently drop or mis-attribute an arg.
        let interp = Interpreter::new();
        let params = vec!["a".to_string(), "b".to_string()];

        // Too many positional args.
        let err = interp
            .assemble_extern_args(
                "f",
                &params,
                vec![Value::Int(1), Value::Int(2), Value::Int(3)],
                vec![],
            )
            .expect_err("over-arity should error");
        assert!(
            err.message.contains("positional args"),
            "got: {}",
            err.message
        );

        // A parameter left unfilled.
        let err = interp
            .assemble_extern_args("f", &params, vec![Value::Int(1)], vec![])
            .expect_err("a missing argument should error");
        assert!(
            err.message.contains("missing argument for parameter `b`"),
            "got: {}",
            err.message
        );

        // A named arg whose name matches no parameter — must fail loudly, not
        // vanish and resurface as a misleading "missing argument".
        let err = interp
            .assemble_extern_args(
                "f",
                &params,
                vec![Value::Int(1)],
                vec![("zzz".to_string(), Value::Int(2))],
            )
            .expect_err("an unknown named arg should error");
        assert!(
            err.message.contains("unknown argument `zzz`"),
            "got: {}",
            err.message
        );
    }
}
