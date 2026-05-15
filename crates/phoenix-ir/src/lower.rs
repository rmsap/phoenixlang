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
use phoenix_common::module_path::ModulePath;
use phoenix_common::span::Span;
use phoenix_parser::ast::Program;
use phoenix_sema::ResolvedModule;
use phoenix_sema::types::Type;
use std::borrow::Cow;
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
/// Templates themselves remain in `module.functions` as inert
/// [`crate::module::FunctionSlot::Template`] stubs to preserve the
/// `FuncId`-as-vector-index invariant. **Downstream consumers should
/// iterate via
/// [`IrModule::concrete_functions`](crate::module::IrModule::concrete_functions)**
/// — the tagged-slot type makes accidentally walking templates a
/// type-system error rather than a silent miscompile. Do not call
/// `monomorphize` separately after `lower` — it is not idempotent and
/// would attempt to re-specialize the already-specialized module.
pub fn lower(program: &Program, check_result: &ResolvedModule) -> IrModule {
    // `LoweringContext::new` initializes `current_module` to
    // `ModulePath::entry()`, which keeps `module_qualify` a no-op for
    // single-file callers (existing IR / snapshot tests).
    let mut ctx = LoweringContext::new(check_result);
    ctx.lower_program(program);
    ctx.finish()
}

/// Lower a multi-module Phoenix project into an IR module.
///
/// Equivalent to [`lower`] but walks every [`ResolvedSourceModule`]
/// produced by the resolver, with `current_module` updated per module
/// before each body-lowering pass so the IR's per-call-site name
/// lookups consult the same module-qualified keys that sema produced.
///
/// On single-element inputs (the entry module only) the result is
/// byte-identical to `lower(&modules[0].program, check_result)` — the
/// entry module qualifies to bare names, single-file IR snapshots stay
/// stable.
///
/// **Scope limitation.** Today this only wires per-module function-body
/// lowering through `current_module`. User struct/enum *types* defined
/// in a non-entry module still hit a separate "unknown named type in IR
/// lowering" panic in [`lower_type`] when consumed via parameter or let
/// bindings, because `ResolvedModule::struct_by_name`/`enum_by_name` are
/// keyed by qualified names and the type-resolution path doesn't
/// qualify yet. Multi-module fixtures should stick to built-in types in
/// non-entry modules until that follow-on lands.
pub fn lower_modules(
    modules: &[phoenix_modules::ResolvedSourceModule],
    check_result: &ResolvedModule,
) -> IrModule {
    // Public-API contract: `phoenix_modules::resolve` always returns the
    // entry module, so a non-empty slice is the well-formed shape. Use
    // an unconditional `assert!` (not `debug_assert!`) so a misuse from
    // a release build still fails loudly instead of dereferencing into
    // an empty slice.
    assert!(
        !modules.is_empty(),
        "lower_modules requires at least one module (the entry)"
    );
    let mut ctx = LoweringContext::new(check_result);

    // Pass 1 — registration. Drives off `ResolvedModule`'s id-indexed
    // tables, so it does not need a per-module loop and does not read
    // `current_module`. Run once.
    ctx.register_builtin_enum_layouts();
    ctx.register_declarations();

    // Pass 1.5 — see [`crate::default_wrappers`] for the rationale.
    // Must run after Pass 1 (so wrapper bodies can refer to function
    // stubs by `FuncId`) and before Pass 2 (so call sites consult
    // `default_wrapper_index` instead of inlining AST defaults).
    crate::default_wrappers::synthesize_default_wrappers(&mut ctx);

    // Pass 2 — lower each module's function bodies with `current_module`
    // set to that module's path. This is what lets bare-name lookups
    // inside body-lowering (`function_index.get(&name)`,
    // `method_index.get((&type, &method))`) qualify against the right
    // module before probing the sema-mangled tables.
    for module in modules {
        ctx.current_module = module.module_path.clone();
        ctx.lower_function_bodies(&module.program);
    }

    ctx.finish()
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
    for (i, slot) in module.functions.iter().enumerate() {
        assert_ne!(
            slot.func().id.0,
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
            "ListBuilder" => {
                let elem = args
                    .first()
                    .map(|t| lower_type(t, check_result))
                    .unwrap_or(IrType::Void);
                IrType::ListBuilderRef(Box::new(elem))
            }
            "MapBuilder" => {
                let key = args
                    .first()
                    .map(|t| lower_type(t, check_result))
                    .unwrap_or(IrType::Void);
                let val = args
                    .get(1)
                    .map(|t| lower_type(t, check_result))
                    .unwrap_or(IrType::Void);
                IrType::MapBuilderRef(Box::new(key), Box::new(val))
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
    /// The Phoenix module whose body is being lowered right now. Used by
    /// body-lowering passes to translate user-source bare names into
    /// the qualified keys that `IrModule::function_index` and
    /// `method_index` are stored under (matching sema's mangling).
    /// Defaults to [`ModulePath::entry()`]; `lower_modules` updates it
    /// per-module before each body-lowering call.
    pub(crate) current_module: ModulePath,

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
    /// Pending defer expressions for the current function in source
    /// order. Lowered (in reverse) immediately before every emitted
    /// `Terminator::Return` so all exit paths fire defers LIFO. Reset
    /// per-function in `lower_function`. Phase 2.3 decision G; lazy-
    /// capture semantics — free variables are looked up at exit time.
    pub(crate) pending_defers: Vec<phoenix_parser::ast::Expr>,
    /// Snapshot of `var_scopes.len()` taken in
    /// [`Self::lower_function_body_block`] immediately after the
    /// function body's outermost scope is pushed. Used by
    /// [`Self::lower_defers_for_exit`] to hide inner scopes (loop
    /// bodies, if-arms, match arms) while a defer is being lowered:
    /// without that, an inner-scope `let` that shadows a top-level
    /// binding would intercept the defer's free-variable lookup at
    /// any `return`/`?` exit reached from the inner scope (the AST
    /// interpreter pops inner scopes before firing defers, so it has
    /// no equivalent footgun). Saved/restored across nested
    /// function-body lowerings (e.g. lambdas).
    pub(crate) defer_outer_scope_depth: usize,
    /// IR type the value of the current initializer / RHS will land in,
    /// if known. Set by [`crate::lower_stmt::LoweringContext::lower_var_decl`]
    /// from the let-binding annotation, by
    /// [`crate::lower_expr::LoweringContext::lower_assignment`] from the
    /// LHS slot's declared type, and saved/restored across nested
    /// lowering (inner lets, assignments, lambda bodies) so it never
    /// observes a stale enclosing value.
    ///
    /// Today only consulted by the `List.builder()` / `Map.builder()`
    /// carve-out in `lower_method_call`. Sema types the constructor
    /// expression as `ListBuilder<TypeVar(T)>` /
    /// `MapBuilder<TypeVar(K), TypeVar(V)>` regardless of the
    /// surrounding context — the constructor takes no args from which
    /// to pin `T` / `K` / `V`. After monomorphization the type vars
    /// erase to the `GENERIC_PLACEHOLDER` (8-byte default), which
    /// silently miscompiles a `String`-keyed builder (key fat pointer
    /// is 16 bytes). This hint carries the concrete IR type down to
    /// the alloc emission so the runtime layout matches the eventual
    /// `.set()` / `.push()` argument widths — for both `let
    /// b: ListBuilder<String> = List.builder()` (annotation source)
    /// and `b = List.builder()` reassignment (slot-type source).
    pub(crate) current_target_type: Option<IrType>,
}

impl<'a> LoweringContext<'a> {
    /// Creates a new lowering context.
    pub(crate) fn new(check: &'a ResolvedModule) -> Self {
        Self {
            check,
            module: IrModule::new(),
            current_module: ModulePath::entry(),
            current_func_id: None,
            current_block: None,
            var_scopes: Vec::new(),
            loop_stack: Vec::new(),
            closure_counter: 0,
            pending_defers: Vec::new(),
            defer_outer_scope_depth: 0,
            current_target_type: None,
        }
    }

    /// Qualify a bare user-source name into the symbol-table key for
    /// the current module. The lookup chain mirrors sema's:
    ///
    /// 1. Consult [`ResolvedModule::resolve_visible`] first — that
    ///    map carries own-module decls, builtins, and imported items
    ///    (each pointing at the correct qualified key, including
    ///    `lib::add` for an `import lib { add }` in the entry).
    /// 2. Fall back to a `module_qualify`-style prefix against the
    ///    current module. The scope is populated by the time body
    ///    lowering runs, so in steady state every user-source name
    ///    hits step 1; this branch covers the bootstrap window
    ///    before scopes exist (e.g. lowering of declaration headers
    ///    that runs before `module_scopes` are drained from sema)
    ///    and any internal compiler name that was never registered
    ///    in scope. If you suspect this branch is firing for a
    ///    user-source name in a callable body, that's a sign sema
    ///    failed to register the name in the module's scope and the
    ///    bug is upstream of `qualify`.
    ///
    /// Returns `Cow::Borrowed` whenever possible (entry / builtin /
    /// scope-borrowed) so the per-call-site lookup in body lowering
    /// stays allocation-free; falls back to an owned `String` only
    /// when a real prefix has to be produced.
    pub(crate) fn qualify<'n>(&self, name: &'n str) -> Cow<'n, str> {
        if let Some(qualified) = self.check.resolve_visible(&self.current_module, name) {
            // Hot-path fast: when the scope maps `name → name` (the
            // entry-module case for own decls and bare builtins),
            // return `Cow::Borrowed` to keep the per-call-site
            // lookup allocation-free. Cross-module lookups (where
            // `qualified != name`) allocate exactly once.
            if qualified == name {
                return Cow::Borrowed(name);
            }
            return Cow::Owned(qualified.to_string());
        }
        if self.current_module.is_entry() || self.current_module.is_builtin() {
            Cow::Borrowed(name)
        } else {
            Cow::Owned(format!("{}::{}", self.current_module.dotted(), name))
        }
    }

    /// Like [`Self::qualify`] but accepts a name that may already be
    /// sema-qualified. Names containing `::` are treated as canonical
    /// and returned unchanged; bare names are qualified against the
    /// current module.
    ///
    /// Use this for names sourced from sema's `Type` (e.g. `Type::Named`),
    /// which is inconsistent today: most paths produce bare names, but
    /// the no-field enum variant lookup in
    /// `phoenix_sema::check_expr::lower_ident` iterates the qualified-
    /// keyed `enums` table and produces `Type::Named("lib::Color")`. A
    /// blind `qualify` would re-qualify and miss the `method_index`
    /// slot.
    pub(crate) fn qualify_resolved<'n>(&self, name: &'n str) -> Cow<'n, str> {
        if name.contains("::") {
            Cow::Borrowed(name)
        } else {
            self.qualify(name)
        }
    }

    /// Run monomorphization and the post-lowering invariant check, then
    /// surrender the [`IrModule`]. Shared finishing step between
    /// [`lower`] (single-file) and [`lower_modules`] (multi-module) so
    /// the two entry points cannot drift on what "done lowering" means.
    ///
    /// Default-argument wrapper synthesis is **not** part of `finish`;
    /// it runs earlier as Pass 1.5 between declaration registration
    /// and body lowering — see [`crate::default_wrappers`] for the
    /// reasons. By the time `finish` runs, every wrapper is already
    /// present in `module.functions` and indexed in
    /// `default_wrapper_index`, and monomorphization can treat them
    /// like any other concrete function.
    fn finish(self) -> IrModule {
        let mut module = self.module;
        crate::monomorphize::monomorphize(&mut module);
        #[cfg(debug_assertions)]
        debug_assert_no_placeholder_funcs(&module);
        module
    }

    /// Orchestrates the full lowering of a program.
    fn lower_program(&mut self, program: &Program) {
        // Register built-in Option/Result enum layouts so their constructors
        // use EnumAlloc (with a discriminant) instead of StructAlloc.
        self.register_builtin_enum_layouts();

        // Pass 1: Register all declarations (layouts, function stubs, indices).
        self.register_declarations();

        // Pass 1.5 — see [`crate::default_wrappers`] for the rationale.
        // Must run after Pass 1 (so wrapper bodies can refer to function
        // stubs by `FuncId`) and before Pass 2 (so call sites consult
        // `default_wrapper_index` instead of inlining AST defaults).
        crate::default_wrappers::synthesize_default_wrappers(self);

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
        self.module.functions[func_id.index()].func()
    }

    /// Returns a mutable reference to the current function being lowered.
    pub(crate) fn current_func_mut(&mut self) -> &mut IrFunction {
        let func_id = self.current_func_id.expect("no current function");
        self.module.functions[func_id.index()].func_mut()
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
