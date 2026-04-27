//! AST-to-IR lowering pass.
//!
//! The main entry point is [`lower`], which takes a parsed AST and a
//! [`ResolvedModule`] from semantic analysis and produces an [`IrModule`].
//!
//! The lowering is structured as two passes:
//! - **Pass 1 (registration):** Walk all declarations to register struct/enum
//!   layouts, create function stubs, and populate the module's name-to-ID
//!   lookup tables.
//! - **Pass 2 (lowering):** Lower each function body into basic blocks with
//!   SSA instructions and explicit control flow.

use crate::block::BlockId;
use crate::instruction::{FuncId, Op, ValueId};
use crate::module::{IrFunction, IrModule};
use crate::terminator::Terminator;
use crate::types::IrType;
use phoenix_common::span::Span;
use phoenix_parser::ast::Program;
use phoenix_sema::ResolvedModule;
use phoenix_sema::types::Type;
use std::collections::HashMap;

/// Lower a type-checked Phoenix program into an IR module.
///
/// Takes the parsed AST and the [`ResolvedModule`] from semantic analysis,
/// and produces a fully-formed [`IrModule`] with all functions, methods,
/// struct/enum layouts, and dispatch tables populated.
///
/// **Monomorphization is run internally** as the last step, so the returned
/// module is fully concrete with respect to user-defined generics: no call
/// site has non-empty `Op::Call` type-args, and every call to a generic
/// template has been rewritten to target a specialized `FuncId`.
///
/// Templates themselves remain in `module.functions` as inert stubs (with
/// `is_generic_template = true`) to preserve the `FuncId`-as-vector-index
/// invariant. **Downstream consumers must iterate via
/// [`IrModule::concrete_functions`](crate::module::IrModule::concrete_functions)**,
/// not `module.functions` directly, or they will encounter unspecialized
/// bodies that still contain `IrType::TypeVar`. Do not call
/// `monomorphize` separately after `lower` — it is not idempotent and
/// would attempt to re-specialize the already-specialized module.
pub fn lower(program: &Program, check_result: &ResolvedModule) -> IrModule {
    let mut ctx = LoweringContext::new(check_result);
    ctx.lower_program(program);
    let mut module = ctx.module;
    crate::monomorphize::monomorphize(&mut module);
    #[cfg(debug_assertions)]
    debug_assert_no_placeholder_funcs(&module);
    module
}

/// Verify that no `IrFunction` retains the `FuncId(u32::MAX)`
/// sentinel that `register_declarations` writes into pre-allocated
/// slots.  If a slot survived all of registration, body lowering,
/// and monomorphization unfilled, it points at a divergence between
/// the size sema told us to allocate and the entries we actually
/// wrote — i.e. the sema↔IR id contract is broken.  Debug-only
/// because the check is O(N) and the contract is already guarded by
/// the assertion at the end of `register_declarations`; this is
/// belt-and-braces for the post-monomorphization state.
#[cfg(debug_assertions)]
fn debug_assert_no_placeholder_funcs(module: &IrModule) {
    for (i, func) in module.functions.iter().enumerate() {
        assert_ne!(
            func.id.0,
            u32::MAX,
            "IrModule.functions[{i}] retains the FuncId(u32::MAX) placeholder \
             — sema pre-allocated an id with no matching ResolvedModule entry, \
             or a downstream pass wrote a placeholder without filling it"
        );
    }
}

/// Convert a source-level [`Type`] to an IR-level [`IrType`].
///
/// Generic type variables (`Type::TypeVar(name)`) lower to
/// [`IrType::TypeVar`], preserving the name so the monomorphization pass
/// can substitute it with a concrete type when specializing a template.
/// Post-monomorphization, no function body should contain `TypeVar`.
///
/// # Panics
///
/// Panics on `Type::Error` or on a `Type::Named` that sema failed to
/// resolve — both indicate that earlier error-handling should have
/// short-circuited before IR lowering.
pub fn lower_type(ty: &Type, check_result: &ResolvedModule) -> IrType {
    match ty {
        Type::Int => IrType::I64,
        Type::Float => IrType::F64,
        Type::Bool => IrType::Bool,
        Type::String => IrType::StringRef,
        Type::Void => IrType::Void,
        Type::Named(name) => {
            if check_result.struct_by_name.contains_key(name) {
                // `Type::Named` has no generic args by construction (that
                // shape is `Type::Generic` below), so args is empty.
                IrType::StructRef(name.clone(), Vec::new())
            } else if check_result.enum_by_name.contains_key(name) {
                // `Type::Named` has no generic args by construction (that
                // shape is `Type::Generic` below), so the args vec is empty.
                IrType::EnumRef(name.clone(), Vec::new())
            } else {
                // Unknown named type — treat as opaque struct reference.
                // This should not happen: sema resolves all named types.
                // Panic to catch bugs early rather than silently emitting
                // a StructRef that may cause downstream miscompilation.
                unreachable!("unknown named type in IR lowering: {name}")
            }
        }
        Type::Function(params, ret) => IrType::ClosureRef {
            param_types: params.iter().map(|p| lower_type(p, check_result)).collect(),
            return_type: Box::new(lower_type(ret, check_result)),
        },
        Type::TypeVar(name) => {
            // Generic type variable — carry the name so monomorphization can
            // substitute it with a concrete type. Post-monomorphization,
            // `IrType::TypeVar` should not appear in any function body.
            IrType::TypeVar(name.clone())
        }
        Type::Dyn(trait_name) => IrType::DynRef(trait_name.clone()),
        Type::Generic(name, args) => match name.as_str() {
            "List" => {
                let elem = args
                    .first()
                    .map(|t| lower_type(t, check_result))
                    .unwrap_or(IrType::Void);
                IrType::ListRef(Box::new(elem))
            }
            "Map" => {
                let key = args
                    .first()
                    .map(|t| lower_type(t, check_result))
                    .unwrap_or(IrType::Void);
                let val = args
                    .get(1)
                    .map(|t| lower_type(t, check_result))
                    .unwrap_or(IrType::Void);
                IrType::MapRef(Box::new(key), Box::new(val))
            }
            crate::types::OPTION_ENUM | crate::types::RESULT_ENUM => {
                // Option and Result are enums at the IR level.  Carry the
                // concrete type args so payload-type inference in the
                // Cranelift backend can read them directly (Strategy 0 in
                // `option_payload_type` / `result_payload_types`).
                IrType::EnumRef(
                    name.clone(),
                    args.iter().map(|t| lower_type(t, check_result)).collect(),
                )
            }
            other => {
                // Generic struct or enum.  Both carry their concrete args
                // through to IR — `StructRef` relies on them for
                // struct-monomorphization (per-instantiation layout +
                // method specialization); `EnumRef` carries them for
                // payload-type inference.
                let ir_args: Vec<IrType> =
                    args.iter().map(|t| lower_type(t, check_result)).collect();
                if check_result.struct_by_name.contains_key(other) {
                    IrType::StructRef(other.to_string(), ir_args)
                } else {
                    IrType::EnumRef(other.to_string(), ir_args)
                }
            }
        },
        Type::Error => {
            // Should never reach IR lowering — type errors must be caught
            // by sema.  Panic here to catch bugs early rather than silently
            // producing Void, which could cause subtle downstream issues.
            unreachable!("Type::Error reached IR lowering — sema should have caught this")
        }
    }
}

/// How a variable is bound in the IR.
#[derive(Debug, Clone)]
pub(crate) enum VarBinding {
    /// An immutable SSA value — the `ValueId` *is* the variable.
    Direct(ValueId, IrType),
    /// A mutable variable stored in an `Alloca` slot — accesses go through
    /// `Load`/`Store`.  The [`IrType`] is the type of the stored value.
    Mutable(ValueId, IrType),
}

/// Information about the current loop, used for `break`/`continue` lowering.
#[derive(Debug, Clone)]
pub(crate) struct LoopContext {
    /// Block to jump to for `continue` (loop header or latch).
    pub(crate) continue_target: BlockId,
    /// Block to jump to for `break` (skips else block if present).
    pub(crate) break_target: BlockId,
}

/// Mutable context carried through the lowering pass.
pub(crate) struct LoweringContext<'a> {
    /// The sema [`ResolvedModule`] for type and signature lookups.
    pub(crate) check: &'a ResolvedModule,
    /// The module being built.
    pub(crate) module: IrModule,

    // --- Per-function state (reset when entering a new function) ---
    /// The function currently being lowered.
    pub(crate) current_func_id: Option<FuncId>,
    /// The block currently being appended to.
    pub(crate) current_block: Option<BlockId>,
    /// Variable name → binding, using a scope stack for lexical scoping.
    pub(crate) var_scopes: Vec<HashMap<String, VarBinding>>,
    /// Loop context stack for `break`/`continue` targeting.
    pub(crate) loop_stack: Vec<LoopContext>,
    /// Counter for generating unique closure function names.
    pub(crate) closure_counter: u32,
}

impl<'a> LoweringContext<'a> {
    /// Creates a new lowering context.
    fn new(check: &'a ResolvedModule) -> Self {
        Self {
            check,
            module: IrModule::new(),
            current_func_id: None,
            current_block: None,
            var_scopes: Vec::new(),
            loop_stack: Vec::new(),
            closure_counter: 0,
        }
    }

    /// Orchestrates the full lowering of a program.
    fn lower_program(&mut self, program: &Program) {
        // Register built-in Option/Result enum layouts so their constructors
        // use EnumAlloc (with a discriminant) instead of StructAlloc.
        self.register_builtin_enum_layouts();

        // Pass 1: Register all declarations (layouts, function stubs, indices).
        self.register_declarations();

        // Pass 2: Lower all function bodies.
        self.lower_function_bodies(program);
    }

    /// Register enum layouts for built-in Option and Result types.
    ///
    /// Option has `Some(T)` and `None`; Result has `Ok(T)` and `Err(E)`.
    /// Since these are generic, the field types use a placeholder
    /// (`StructRef(GENERIC_PLACEHOLDER)`).  The concrete types are determined at
    /// each use site via type inference.
    fn register_builtin_enum_layouts(&mut self) {
        for name in &[crate::types::OPTION_ENUM, crate::types::RESULT_ENUM] {
            if let Some(info) = self.check.enum_info_by_name(name) {
                let variants: Vec<(String, Vec<IrType>)> = info
                    .variants
                    .iter()
                    .map(|(vname, fields)| {
                        // Enum layouts use the nameless `__generic`
                        // placeholder for type-parameter fields because
                        // built-in enums (Option/Result/List/Map) resolve
                        // concrete payload types at use sites via
                        // inference strategies in the Cranelift backend
                        // (see `enum_type_inference.rs`), NOT via
                        // monomorphization. Convert any TypeVar emitted by
                        // `lower_type` back to the placeholder.
                        let ir_fields: Vec<IrType> = fields
                            .iter()
                            .map(|t| lower_type(t, self.check).erase_type_vars())
                            .collect();
                        (vname.clone(), ir_fields)
                    })
                    .collect();
                self.module.enum_layouts.insert(name.to_string(), variants);
                // Store type parameter names for generic substitution in
                // match lowering (see resolve_field_type).
                if !info.type_params.is_empty() {
                    self.module
                        .enum_type_params
                        .insert(name.to_string(), info.type_params.clone());
                }
            } else {
                unreachable!(
                    "builtin enum '{name}' not found in sema — \
                     sema should always register Option and Result"
                );
            }
        }
    }

    // --- Scope management ---

    /// Pushes a new variable scope.
    pub(crate) fn push_scope(&mut self) {
        self.var_scopes.push(HashMap::new());
    }

    /// Pops the innermost variable scope.
    pub(crate) fn pop_scope(&mut self) {
        self.var_scopes.pop();
    }

    /// Defines a variable in the current scope.
    pub(crate) fn define_var(&mut self, name: String, binding: VarBinding) {
        if let Some(scope) = self.var_scopes.last_mut() {
            scope.insert(name, binding);
        }
    }

    /// Looks up a variable by name, searching from innermost to outermost scope.
    pub(crate) fn lookup_var(&self, name: &str) -> Option<&VarBinding> {
        for scope in self.var_scopes.iter().rev() {
            if let Some(binding) = scope.get(name) {
                return Some(binding);
            }
        }
        None
    }

    // --- Loop context ---

    /// Pushes a loop context for `break`/`continue` lowering.
    pub(crate) fn push_loop(&mut self, ctx: LoopContext) {
        self.loop_stack.push(ctx);
    }

    /// Pops the innermost loop context.
    pub(crate) fn pop_loop(&mut self) {
        self.loop_stack.pop();
    }

    /// Returns the innermost loop context.
    pub(crate) fn current_loop(&self) -> Option<&LoopContext> {
        self.loop_stack.last()
    }

    // --- IR construction helpers ---

    /// Returns a shared reference to the current function being lowered.
    pub(crate) fn current_func(&self) -> &IrFunction {
        let func_id = self.current_func_id.expect("no current function");
        &self.module.functions[func_id.index()]
    }

    /// Returns a mutable reference to the current function being lowered.
    pub(crate) fn current_func_mut(&mut self) -> &mut IrFunction {
        let func_id = self.current_func_id.expect("no current function");
        &mut self.module.functions[func_id.index()]
    }

    /// Returns the current function ID.
    #[allow(dead_code)]
    pub(crate) fn current_func_id(&self) -> FuncId {
        self.current_func_id.expect("no current function")
    }

    /// Returns the current block ID.
    pub(crate) fn current_block(&self) -> BlockId {
        self.current_block.expect("no current block")
    }

    /// Creates a new basic block in the current function and returns its ID.
    pub(crate) fn create_block(&mut self) -> BlockId {
        self.current_func_mut().create_block()
    }

    /// Switches the insertion point to the specified block.
    pub(crate) fn switch_to_block(&mut self, block: BlockId) {
        self.current_block = Some(block);
    }

    /// Emits an instruction into the current block and returns its result
    /// [`ValueId`].  For void-typed operations, [`VOID_SENTINEL`] is
    /// returned — callers must not use it as an operand (the verifier
    /// checks this).
    pub(crate) fn emit(&mut self, op: Op, result_type: IrType, span: Option<Span>) -> ValueId {
        let block = self.current_block();
        if result_type == IrType::Void {
            self.current_func_mut().emit(block, op, IrType::Void, span);
            crate::instruction::VOID_SENTINEL
        } else {
            self.current_func_mut()
                .emit_value(block, op, result_type, span)
        }
    }

    /// Emits a void instruction into the current block (no result value).
    pub(crate) fn emit_void(&mut self, op: Op, span: Option<Span>) {
        let block = self.current_block();
        self.current_func_mut().emit(block, op, IrType::Void, span);
    }

    /// Sets the terminator for the current block.
    pub(crate) fn terminate(&mut self, term: Terminator) {
        let block = self.current_block();
        self.current_func_mut().set_terminator(block, term);
    }

    /// Adds a block parameter to the specified block and returns its [`ValueId`].
    pub(crate) fn add_block_param(&mut self, block: BlockId, ty: IrType) -> ValueId {
        self.current_func_mut().add_block_param(block, ty)
    }

    /// Looks up the IR type of a source expression by its span.
    pub(crate) fn expr_type(&self, span: &Span) -> IrType {
        if let Some(ty) = self.check.expr_types.get(span) {
            lower_type(ty, self.check)
        } else {
            IrType::Void
        }
    }

    /// Looks up the source-level type of an expression by its span.
    pub(crate) fn source_type(&self, span: &Span) -> Option<&Type> {
        self.check.expr_types.get(span)
    }

    /// Like [`source_type`](Self::source_type) but panics if the type is
    /// missing.  Use this only where sema guarantees a type annotation exists.
    pub(crate) fn require_source_type(&self, span: &Span) -> Type {
        self.check.expr_types.get(span).cloned().unwrap_or_else(|| {
            unreachable!(
                "missing sema type at {span:?} — sema should always type-annotate expressions"
            )
        })
    }

    /// Converts a source-level [`Type`] to an [`IrType`].
    pub(crate) fn lower_type(&self, ty: &Type) -> IrType {
        lower_type(ty, self.check)
    }

    /// Look up the concrete generic type arguments inferred at this call
    /// site (keyed by the call expression's span) and lower each to
    /// [`IrType`]. Returns an empty vector for non-generic calls. If sema
    /// failed to resolve a type parameter and fell back to `Type::Error`,
    /// the entry is suppressed here so that `lower_type` is never invoked
    /// on `Type::Error` (which would panic).
    pub(crate) fn resolve_call_type_args(&self, call_span: Span) -> Vec<IrType> {
        let Some(targs) = self.check.call_type_args.get(&call_span) else {
            return Vec::new();
        };
        if targs.iter().any(Type::is_error) {
            return Vec::new();
        }
        targs.iter().map(|t| lower_type(t, self.check)).collect()
    }

    /// Emit a dummy value for void-typed expressions that must still return a
    /// [`ValueId`] (e.g., match arms whose result is discarded).
    ///
    /// The value is never used at runtime — it exists only to satisfy the SSA
    /// builder's requirement that every expression produces a value.
    pub(crate) fn void_placeholder(&mut self, span: Option<Span>) -> ValueId {
        self.emit(Op::ConstBool(false), IrType::Bool, span)
    }
}
