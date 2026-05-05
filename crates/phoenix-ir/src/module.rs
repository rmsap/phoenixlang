//! Top-level IR module and function definitions.
//!
//! Trait-registry types and helpers ([`IrTraitInfo`], [`IrTraitMethod`],
//! [`DynVtable`], [`VtableEntry`]) live in the [`trait_registry`]
//! submodule and are re-exported at this level so existing imports
//! (`use phoenix_ir::module::IrTraitInfo`) keep working.

pub mod trait_registry;

pub use trait_registry::{DynVtable, IrTraitInfo, IrTraitMethod, VtableEntry};

use crate::block::{BasicBlock, BlockId};
use crate::instruction::{FuncId, Instruction, Op, ValueId};
use crate::terminator::Terminator;
use crate::types::IrType;
use crate::value_alloc::ValueIdAllocator;
use phoenix_common::span::Span;
use std::collections::HashMap;

/// One slot in [`IrModule::functions`]. Tagged so the consumer must
/// dispatch on the variant before touching the body — it is impossible
/// to look at a function and forget whether it is a template (which may
/// contain `IrType::TypeVar` and is unverifiable / non-codegen-able) or
/// a concrete callable.
///
/// Templates are kept in the module post-monomorphization as inert stubs
/// so that the `FuncId`-as-vector-index invariant survives. Codegen and
/// the verifier walk only `Concrete` slots; monomorphization walks both.
#[derive(Debug, Clone)]
pub enum FunctionSlot {
    /// A fully concrete function — every type annotation is free of
    /// `IrType::TypeVar` and the body is safe to verify and code-gen.
    Concrete(IrFunction),
    /// A generic-template stub. Its body may contain
    /// `IrType::TypeVar` annotations that monomorphization specializes
    /// away; the verifier and backends must skip it.
    Template(IrFunction),
}

/// Why [`IrModule::resolve_concrete`] could not return an `&IrFunction`.
/// Both variants signal a compiler bug — the variants exist so callers
/// can `panic!` (or otherwise diagnose) with the right message.
#[derive(Debug)]
pub enum ResolveError<'a> {
    /// The `FuncId` is past the end of [`IrModule::functions`] —
    /// almost always an id from a different module or a stale id
    /// captured before a module was rebuilt.
    OutOfRange {
        /// The id that was looked up.
        id: FuncId,
        /// The length of `module.functions` at lookup time.
        len: usize,
    },
    /// The slot exists but holds a [`FunctionSlot::Template`] — the
    /// caller expected monomorphization to have rewritten this id to a
    /// specialized concrete `FuncId` before reaching this site.
    Template {
        /// The id whose slot resolved to a template.
        id: FuncId,
        /// The template's name, for diagnostics.
        name: &'a str,
    },
}

impl<'a> std::fmt::Display for ResolveError<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::OutOfRange { id, len } => write!(
                f,
                "FuncId({}) is out of range for module.functions (len={len})",
                id.0,
            ),
            ResolveError::Template { id, name } => write!(
                f,
                "FuncId({}) resolves to template `{name}` — \
                 monomorphization should have rewritten this call to a \
                 specialized FuncId",
                id.0,
            ),
        }
    }
}

impl FunctionSlot {
    /// Return the underlying [`IrFunction`] regardless of variant.
    /// Use this only when the distinction does not matter (e.g. name
    /// inspection, FuncId access). Callers that rely on the body being
    /// concrete (code generation, verification, type-classification
    /// helpers) must use [`Self::as_concrete`] instead.
    pub fn func(&self) -> &IrFunction {
        match self {
            FunctionSlot::Concrete(f) | FunctionSlot::Template(f) => f,
        }
    }

    /// Mutable counterpart of [`Self::func`]. Same caveat applies.
    pub fn func_mut(&mut self) -> &mut IrFunction {
        match self {
            FunctionSlot::Concrete(f) | FunctionSlot::Template(f) => f,
        }
    }

    /// `Some(&func)` if this slot is concrete, `None` if it is a
    /// template.
    pub fn as_concrete(&self) -> Option<&IrFunction> {
        match self {
            FunctionSlot::Concrete(f) => Some(f),
            FunctionSlot::Template(_) => None,
        }
    }

    /// Mutable counterpart of [`Self::as_concrete`].
    pub fn as_concrete_mut(&mut self) -> Option<&mut IrFunction> {
        match self {
            FunctionSlot::Concrete(f) => Some(f),
            FunctionSlot::Template(_) => None,
        }
    }

    /// `Some(&func)` if this slot is a template, `None` if concrete.
    pub fn as_template(&self) -> Option<&IrFunction> {
        match self {
            FunctionSlot::Template(f) => Some(f),
            FunctionSlot::Concrete(_) => None,
        }
    }

    /// `true` if this slot is a template.
    pub fn is_template(&self) -> bool {
        matches!(self, FunctionSlot::Template(_))
    }
}

/// The top-level IR container for a compilation unit.
///
/// Contains all functions (including methods lowered as standalone functions),
/// struct/enum layout metadata, and name-to-ID lookup tables.
#[derive(Debug, Clone)]
pub struct IrModule {
    /// All functions in the module, indexed by [`FuncId`]. Each slot is
    /// tagged as [`FunctionSlot::Concrete`] or
    /// [`FunctionSlot::Template`] so consumers cannot accidentally treat
    /// a template as a concrete function.
    ///
    /// `pub(crate)` so the typed-split is genuinely enforced: external
    /// callers must go through [`Self::concrete_functions`] (codegen /
    /// verifier), [`Self::templates`] (template walks),
    /// [`Self::lookup`] / [`Self::get_concrete`] / [`Self::resolve_concrete`]
    /// (`FuncId`-keyed lookup), [`Self::push_concrete`] /
    /// [`Self::push_template`] (append), [`Self::function_count`] /
    /// [`Self::iter_slots`] (iteration). Direct vector access from
    /// outside this crate would let a caller forget that templates
    /// exist and read a TypeVar-bearing body.
    pub(crate) functions: Vec<FunctionSlot>,
    /// Struct layout info: name → ordered `(field_name, field_type)` pairs.
    pub struct_layouts: HashMap<String, Vec<(String, IrType)>>,
    /// Enum layout info: name → variant list, each variant has
    /// `(variant_name, field_types)`.
    pub enum_layouts: HashMap<String, Vec<(String, Vec<IrType>)>>,
    /// Enum type parameter names: name → ordered list of type param names.
    /// Used for generic substitution during match lowering — maps each
    /// `__generic` placeholder back to its source type parameter by index.
    /// Example: `"Result" → ["T", "E"]`.
    pub enum_type_params: HashMap<String, Vec<String>>,
    /// Struct type parameter names: name → ordered list of type param names.
    /// Mirrors [`Self::enum_type_params`]; empty entry (or absent key)
    /// means the struct is not generic.
    ///
    /// **Lifecycle.**
    ///
    /// - **Written** during IR lowering by
    ///   [`crate::lower_decl::LoweringContext::register_struct`], which
    ///   inserts one entry per generic struct declaration from sema's
    ///   `StructInfo.type_params`. Never mutated after lowering ends.
    /// - **Read** at two distinct points: (1)
    ///   [`crate::lower_expr::LoweringContext::resolve_field_type`] uses
    ///   it at lowering time to substitute `IrType::TypeVar` out of a
    ///   generic struct's field type at concrete use sites (the receiver
    ///   carries `Generic("Container", [Int])`, the layout holds
    ///   `TypeVar("T")`, we substitute here so the emitted
    ///   `StructGetField` has a fully-resolved result type); (2)
    ///   [`crate::monomorphize::monomorphize_structs`] uses it to build
    ///   the per-instantiation TypeVar → concrete substitution map and
    ///   to identify which `StructRef` names refer to generic templates
    ///   worth enqueuing on the specialization worklist.
    ///
    /// The map is keyed by the struct's source-level (bare) name.
    /// Post-monomorphization, specialized layouts registered under
    /// mangled names (e.g. `Container__i64`) do *not* get entries here —
    /// they are already concrete and have no type parameters to track.
    pub struct_type_params: HashMap<String, Vec<String>>,
    /// Function name → [`FuncId`] mapping for call resolution.
    pub function_index: HashMap<String, FuncId>,
    /// Method dispatch table: `(type_name, method_name)` → [`FuncId`].
    pub method_index: HashMap<(String, String), FuncId>,
    /// Default-argument wrapper registry, keyed by (callee FuncId,
    /// param index). Populated by
    /// [`crate::default_wrappers::synthesize_default_wrappers`] for
    /// every **non-generic** (function or method, parameter) pair
    /// whose default expression is not a pure literal — those defaults
    /// are lowered once into a synthesized zero-arg wrapper function
    /// so foreign callers don't see private symbols the default may
    /// reference. Generic callees are excluded by sema's
    /// `default_needs_wrapper` gate (wrapping them would require
    /// per-specialization cloning — a deferred follow-on); their
    /// defaults stay on the inline path, which is privacy-safe within
    /// a module.
    ///
    /// Caller rewrite consults this at every call site that would
    /// otherwise inline the default expression: if a wrapper exists
    /// for `(callee, param_idx)`, emit `Op::Call(wrapper_id, [], [])`
    /// instead. See the "Default-expression visibility across module
    /// boundaries" bug-closure entry in `docs/phases/phase-2.md`
    /// (under §2.6 → "Bugs closed in this phase").
    ///
    /// Empty when every default is a pure literal, which is the
    /// common case in single-file programs.
    pub default_wrapper_index: HashMap<(FuncId, usize), FuncId>,
    /// Trait-object vtable registry: `(concrete_type, trait_name)` →
    /// ordered list of `(method_name, FuncId)` pairs.
    ///
    /// **Slot-index contract:** `entries[i]` corresponds to
    /// `traits[trait_name].methods[i]` — i.e. declaration order is the
    /// vtable slot index. Every `Op::DynCall` carries this pre-resolved
    /// index, and codegen is a direct `vtable_ptr[i * POINTER_SIZE]`
    /// load. Do not sort, reorder, or de-duplicate entries.
    ///
    /// Method names ride alongside `FuncId`s for debug display and so the
    /// interpreter can surface method names in runtime errors.
    ///
    /// Populated by IR lowering (pre-monomorphization) at every
    /// concrete-to-`dyn` coercion site. Consumed by the IR interpreter
    /// (method dispatch) and the Cranelift backend (rodata vtable
    /// emission).
    ///
    /// **Keying invariant.** The `concrete_type` key is a struct / enum
    /// name. For non-generic types it is the source-level name
    /// (`"Point"`). For generic structs it starts life as the bare
    /// template name at lowering time (`"Container"`), and
    /// [`crate::monomorphize::monomorphize_structs`] rekeys each entry
    /// to the mangled per-instantiation name (`"Container__i64"`) and
    /// drops the template entry during Pass 2's vtable rekey. Every
    /// entry's `FuncId` values are also re-resolved through the
    /// specialized `method_index` at that time, so post-mono the map
    /// contains only concrete-instantiation keys pointing at concrete
    /// (non-template) functions. Generic enums are gated off at
    /// trait-registration, so their bare name still suffices.
    pub dyn_vtables: HashMap<(String, String), DynVtable>,
    /// Trait declarations visible at the IR level, keyed by trait name.
    ///
    /// Mirrors sema's `TraitInfo` but holds [`IrType`]-lowered method
    /// signatures so downstream passes (verifier, Cranelift backend, IR
    /// interpreter) do not have to reach back into sema metadata.
    ///
    /// Populated during IR lowering registration for every declared
    /// trait that is *object-safe* (i.e. sema's `object_safety_error` is
    /// `None`). Non-object-safe traits are omitted: they cannot appear
    /// in `DynRef` / `DynCall` positions, so no IR-level consumer needs
    /// their signatures.
    pub traits: HashMap<String, IrTraitInfo>,
    /// Boundary between user-declared callables and synthesized ones
    /// in [`Self::functions`].  Free functions occupy
    /// `FuncId(0..user_method_offset)`, user-declared methods occupy
    /// `FuncId(user_method_offset..user_method_offset + n_user_methods)`,
    /// and any [`FuncId`] past that point is a synthesized callable
    /// (closure or generic specialization) appended during body
    /// lowering and monomorphization.
    ///
    /// Mirrors `phoenix_sema::ResolvedModule::user_method_offset` and
    /// is set in lockstep at the end of
    /// [`crate::lower_decl::LoweringContext::register_declarations`].
    /// Zero on a freshly-constructed `IrModule::new()`.
    pub user_method_offset: u32,
    /// Total count of callables registered from sema's tables (free
    /// functions + user methods).  `FuncId(synthesized_start..)` is
    /// reserved for closures and monomorphized specializations.
    /// Equal to `user_method_offset + n_user_methods` after
    /// `register_declarations` runs.
    pub synthesized_start: u32,
}

impl IrModule {
    /// Iterate over the functions that any backend or verifier should
    /// consume: all concrete (non-template) functions in insertion order.
    ///
    /// Generic templates remain in `self.functions` as inert stubs after
    /// monomorphization (to preserve the `FuncId`-as-vector-index
    /// invariant), but they contain `IrType::TypeVar` and have no valid
    /// lowering. Every consumer that walks functions should go through
    /// this iterator.
    pub fn concrete_functions(&self) -> impl Iterator<Item = &IrFunction> {
        self.functions.iter().filter_map(FunctionSlot::as_concrete)
    }

    /// Mutable counterpart of [`Self::concrete_functions`].
    pub fn concrete_functions_mut(&mut self) -> impl Iterator<Item = &mut IrFunction> {
        self.functions
            .iter_mut()
            .filter_map(FunctionSlot::as_concrete_mut)
    }

    /// Iterate over the generic-template stubs, paired with their
    /// `FuncId` so callers know which slot they came from.
    pub fn templates(&self) -> impl Iterator<Item = (FuncId, &IrFunction)> {
        self.functions
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_template().map(|f| (FuncId(i as u32), f)))
    }

    /// Look up a function by `FuncId` regardless of whether it is
    /// concrete or a template. Returns `None` if the id is out of range.
    /// Use [`Self::get_concrete`] when the caller cannot tolerate a
    /// template body (codegen, runtime call dispatch).
    pub fn lookup(&self, id: FuncId) -> Option<&IrFunction> {
        self.functions.get(id.index()).map(FunctionSlot::func)
    }

    /// Mutable counterpart of [`Self::lookup`].
    pub fn lookup_mut(&mut self, id: FuncId) -> Option<&mut IrFunction> {
        self.functions
            .get_mut(id.index())
            .map(FunctionSlot::func_mut)
    }

    /// Look up a concrete function by `FuncId`. Returns `None` if the
    /// slot is a template (caller almost certainly has a bug — codegen
    /// and the IR interpreter should never resolve a template at
    /// runtime).
    pub fn get_concrete(&self, id: FuncId) -> Option<&IrFunction> {
        self.functions
            .get(id.index())
            .and_then(FunctionSlot::as_concrete)
    }

    /// Resolve `id` to a concrete function or report which way it
    /// failed. Use this when both failure modes are bugs but you want
    /// distinct diagnostics (out-of-range vs. template-resolution).
    /// Codegen and the IR interpreter typically `unwrap` the result and
    /// panic — both paths are reachable only through compiler bugs, but
    /// the variants make the panic message accurate without a two-step
    /// `lookup` / `get_concrete` dance at the call site.
    pub fn resolve_concrete(&self, id: FuncId) -> Result<&IrFunction, ResolveError<'_>> {
        match self.functions.get(id.index()) {
            None => Err(ResolveError::OutOfRange {
                id,
                len: self.functions.len(),
            }),
            Some(FunctionSlot::Concrete(f)) => Ok(f),
            Some(FunctionSlot::Template(f)) => Err(ResolveError::Template { id, name: &f.name }),
        }
    }

    /// Append a new concrete function. Returns its `FuncId`, which is
    /// equal to the slot index just appended; the function's own
    /// `func.id` field is overwritten to match. Centralizes the
    /// "id agrees with vector position" invariant for new appends.
    pub fn push_concrete(&mut self, mut func: IrFunction) -> FuncId {
        let id = FuncId(self.functions.len() as u32);
        func.id = id;
        self.functions.push(FunctionSlot::Concrete(func));
        id
    }

    /// Append a new generic-template function. Companion to
    /// [`Self::push_concrete`]; same id-as-position contract. Used by
    /// tests that want to construct a template stub explicitly.
    pub fn push_template(&mut self, mut func: IrFunction) -> FuncId {
        let id = FuncId(self.functions.len() as u32);
        func.id = id;
        self.functions.push(FunctionSlot::Template(func));
        id
    }

    /// Number of [`FunctionSlot`]s in the module — concrete functions
    /// plus template stubs combined. Use this when you need a count
    /// for diagnostics or `FuncId`-bound checks; for "how many
    /// runnable functions are in the module" use
    /// [`Self::concrete_functions`]`.count()`.
    pub fn function_count(&self) -> usize {
        self.functions.len()
    }

    /// Iterate over every [`FunctionSlot`] paired with its [`FuncId`].
    /// Use this for tests that need to inspect both concrete and
    /// template slots without unwrapping the variant; otherwise prefer
    /// [`Self::concrete_functions`] or [`Self::templates`] which
    /// return `&IrFunction` directly.
    pub fn iter_slots(&self) -> impl Iterator<Item = (FuncId, &FunctionSlot)> {
        self.functions
            .iter()
            .enumerate()
            .map(|(i, s)| (FuncId(i as u32), s))
    }

    /// Look up a [`FunctionSlot`] by `FuncId`. Returns `None` if the
    /// id is out of range. Use this when you need to dispatch on the
    /// concrete/template variant explicitly (e.g. monomorphization
    /// passes that walk both kinds).
    pub fn slot_at(&self, id: FuncId) -> Option<&FunctionSlot> {
        self.functions.get(id.index())
    }

    /// Register a `(concrete_type, trait_name)` entry in
    /// [`Self::dyn_vtables`] if one is not already present. Idempotent.
    ///
    /// This is the single source of truth for vtable construction:
    /// lowering-time registration (`LoweringContext::register_dyn_vtable`
    /// in `phoenix-ir/src/lower_dyn.rs`; `LoweringContext` is private
    /// so not rustdoc-linkable) and monomorphization-time registration
    /// (`resolve_unresolved_dyn_allocs` in
    /// `phoenix-ir/src/monomorphize/placeholder_resolution.rs`) both
    /// route through here. They differ only in where `method_names` is
    /// sourced from — sema's `TraitInfo` vs. the IR's [`IrTraitInfo`] —
    /// which the caller is expected to resolve first.
    ///
    /// # Slot-index contract
    ///
    /// `method_names[i]` becomes `entries[i]` in the stored vtable.
    /// Every [`Op::DynCall`] carries a pre-resolved slot index that
    /// indexes this vector, so the caller must pass method names in
    /// trait-declaration order.
    ///
    /// # Panics
    ///
    /// Via `unreachable!` if any method in `method_names` has no entry
    /// in [`Self::method_index`] for `concrete_type`. Sema's
    /// `check_impl_block` rejects incomplete impls before this function
    /// is ever called (from either lowering or mono), so reaching here
    /// is a compiler bug.
    pub fn register_dyn_vtable(
        &mut self,
        trait_name: &str,
        concrete_type: &str,
        method_names: &[String],
    ) {
        let key = (concrete_type.to_owned(), trait_name.to_owned());
        if self.dyn_vtables.contains_key(&key) {
            return;
        }
        let entries: DynVtable = method_names
            .iter()
            .map(|name| {
                let fid = self
                    .method_index
                    .get(&(concrete_type.to_owned(), name.clone()))
                    .copied()
                    .unwrap_or_else(|| {
                        unreachable!(
                            "vtable for `{concrete_type}` as dyn `{trait_name}`: method \
                             `{name}` not found in method_index — sema's \
                             `check_impl_block` must reject incomplete impls before \
                             this function is called"
                        )
                    });
                (name.clone(), fid)
            })
            .collect();
        self.dyn_vtables.insert(key, entries);
    }

    /// Look up the (params, return_type) of `trait_name`'s method at slot
    /// `method_idx`.
    ///
    /// Reads directly from [`Self::traits`] — the IR-level trait metadata
    /// populated at lowering time — so the result is available for every
    /// object-safe trait in the program, including traits whose
    /// `impl` blocks have not been exercised by any coercion site.
    pub fn trait_method_signature(
        &self,
        trait_name: &str,
        method_idx: usize,
    ) -> Option<(&[IrType], &IrType)> {
        self.traits.get(trait_name)?.method_signature(method_idx)
    }

    /// Creates an empty module.
    pub fn new() -> Self {
        Self {
            functions: Vec::new(),
            struct_layouts: HashMap::new(),
            enum_layouts: HashMap::new(),
            enum_type_params: HashMap::new(),
            struct_type_params: HashMap::new(),
            function_index: HashMap::new(),
            method_index: HashMap::new(),
            default_wrapper_index: HashMap::new(),
            dyn_vtables: HashMap::new(),
            traits: HashMap::new(),
            user_method_offset: 0,
            synthesized_start: 0,
        }
    }

    /// `true` if `id` was synthesized post-registration (closure or
    /// monomorphized specialization), as opposed to coming from
    /// sema's id pre-allocation.
    #[inline]
    pub fn is_synthesized(&self, id: FuncId) -> bool {
        id.0 >= self.synthesized_start
    }
}

impl Default for IrModule {
    fn default() -> Self {
        Self::new()
    }
}

/// An IR function.  Methods are lowered as functions with an explicit
/// `self` parameter as the first argument.
#[derive(Debug, Clone)]
pub struct IrFunction {
    /// The unique identifier of this function within the module.
    pub id: FuncId,
    /// The fully qualified name.  Methods are named `"TypeName.method_name"`.
    pub name: String,
    /// Parameter types (including explicit `self` for methods).
    pub param_types: Vec<IrType>,
    /// Parameter names, parallel to `param_types`.
    pub param_names: Vec<String>,
    /// Return type.
    pub return_type: IrType,
    /// The basic blocks, in order.  `blocks[0]` is always the entry block.
    pub blocks: Vec<BasicBlock>,
    /// Owns the [`ValueId`] counter and the parallel per-value type
    /// index. The only way to mint a `ValueId` is via
    /// [`ValueIdAllocator::alloc`], which atomically bumps the counter
    /// and records the type — so the index can never desync from the
    /// counter (it *is* the counter).
    ///
    /// Function parameters appear in here too because lowering binds
    /// them as entry-block parameters (see
    /// `phoenix-ir/src/lower_stmt.rs::lower_function_body`).
    values: ValueIdAllocator,
    /// Counter for fresh [`BlockId`] allocation.
    next_block_id: u32,
    /// Source span of the original function declaration (for debug info).
    pub span: Option<Span>,
    /// Declared generic type parameter names in source order (e.g.,
    /// `["T", "U"]` for `function foo<T, U>`). Empty for non-generic
    /// functions. Monomorphization uses this to build the substitution map
    /// from `IrType::TypeVar(name)` to concrete types.
    pub type_param_names: Vec<String>,
    /// Capture types in capture-slot order, for closure functions
    /// only. Empty for non-closure functions. Indexed by
    /// [`Op::ClosureLoadCapture`]'s `capture_idx` field — backends
    /// walk this vector to compute byte/slot offsets into the closure
    /// heap object (slot widths vary, e.g. `StringRef` is 2 slots).
    pub capture_types: Vec<IrType>,
}

impl IrFunction {
    /// Creates a new function with an empty body.
    pub fn new(
        id: FuncId,
        name: String,
        param_types: Vec<IrType>,
        param_names: Vec<String>,
        return_type: IrType,
        span: Option<Span>,
    ) -> Self {
        Self {
            id,
            name,
            param_types,
            param_names,
            return_type,
            blocks: Vec::new(),
            values: ValueIdAllocator::new(),
            next_block_id: 0,
            span,
            type_param_names: Vec::new(),
            capture_types: Vec::new(),
        }
    }

    /// Construct a closure function: same shape as [`Self::new`] but
    /// records `capture_types` in one step. Use this from
    /// [`crate::lower_expr`]'s lambda lowering so the
    /// "closure functions carry their capture_types" invariant is
    /// constructor-enforced rather than relying on a post-construction
    /// field assignment that the next refactor might forget.
    pub fn new_closure(
        id: FuncId,
        name: String,
        param_types: Vec<IrType>,
        param_names: Vec<String>,
        return_type: IrType,
        span: Option<Span>,
        capture_types: Vec<IrType>,
    ) -> Self {
        let mut f = Self::new(id, name, param_types, param_names, return_type, span);
        f.capture_types = capture_types;
        f
    }

    /// Returns the number of [`ValueId`]s allocated in this function.
    pub fn value_count(&self) -> u32 {
        self.values.next_value_id().0
    }

    /// The entry block (`blocks[0]` by Phoenix-IR convention).
    ///
    /// Function parameters are bound as the parameters of this block,
    /// so backends emitting per-function setup (shadow-stack frame
    /// push, parameter rooting, etc.) anchor on this method rather
    /// than re-encoding the `blocks[0]` invariant at every site.
    ///
    /// # Panics
    ///
    /// Panics if the function has no blocks. Every IR function created
    /// via [`Self::new`] gets at least one entry block during lowering;
    /// this should be unreachable in production paths.
    pub fn entry_block(&self) -> &BasicBlock {
        self.blocks.first().expect(
            "IrFunction::entry_block: function has no blocks; \
             entry-block convention violated",
        )
    }

    /// Creates a new basic block and returns its [`BlockId`].
    /// The block is appended to `self.blocks` with an empty body and
    /// a [`Terminator::None`] placeholder.
    pub fn create_block(&mut self) -> BlockId {
        let id = BlockId(self.next_block_id);
        self.next_block_id += 1;
        self.blocks.push(BasicBlock {
            id,
            params: Vec::new(),
            instructions: Vec::new(),
            terminator: Terminator::None,
        });
        id
    }

    /// Returns a mutable reference to the block with the given ID.
    ///
    /// # Panics
    ///
    /// Panics if the block ID does not correspond to a block in this function.
    pub fn block_mut(&mut self, id: BlockId) -> &mut BasicBlock {
        &mut self.blocks[id.0 as usize]
    }

    /// Returns a reference to the block with the given ID.
    ///
    /// # Panics
    ///
    /// Panics if the block ID does not correspond to a block in this function.
    pub fn block(&self, id: BlockId) -> &BasicBlock {
        &self.blocks[id.0 as usize]
    }

    /// Look up the IR type that was recorded for a [`ValueId`] when it
    /// was emitted into one of this function's blocks, attached as a
    /// block parameter, or bound as a function parameter (function
    /// parameters are represented as entry-block parameters in Phoenix
    /// IR).
    ///
    /// Returns `None` for a `ValueId` that belongs to a different
    /// function (out-of-range index). O(1): the type is recorded by
    /// [`ValueIdAllocator::alloc`] at allocation time. Within a
    /// function, every allocated id has a type by construction — there
    /// is no public API to mint a `ValueId` without recording its type.
    ///
    /// Used by IR lowering's dyn-coercion path (see
    /// [`crate::lower::LoweringContext::coerce_args_to_expected`]) to
    /// recover an argument's current IR type at the call site.
    pub fn instruction_result_type(&self, value: ValueId) -> Option<&IrType> {
        self.values.type_of(value)
    }

    /// Appends an instruction to the specified block and returns its result
    /// [`ValueId`] (if the instruction produces a value).
    pub fn emit(
        &mut self,
        block: BlockId,
        op: Op,
        result_type: IrType,
        span: Option<Span>,
    ) -> Option<ValueId> {
        let result = if result_type != IrType::Void {
            Some(self.values.alloc(result_type.clone()))
        } else {
            None
        };
        let inst = Instruction {
            result,
            result_type,
            op,
            span,
        };
        self.block_mut(block).instructions.push(inst);
        result
    }

    /// Appends an instruction that always produces a value.
    ///
    /// # Panics
    ///
    /// Panics if `result_type` is `Void`.
    pub fn emit_value(
        &mut self,
        block: BlockId,
        op: Op,
        result_type: IrType,
        span: Option<Span>,
    ) -> ValueId {
        assert!(
            result_type != IrType::Void,
            "emit_value called with Void type"
        );
        self.emit(block, op, result_type, span)
            .expect("non-void instruction must produce a value")
    }

    /// Sets the terminator for the specified block.
    pub fn set_terminator(&mut self, block: BlockId, term: Terminator) {
        self.block_mut(block).terminator = term;
    }

    /// Adds a block parameter and returns its [`ValueId`].
    pub fn add_block_param(&mut self, block: BlockId, ty: IrType) -> ValueId {
        let id = self.values.alloc(ty.clone());
        self.block_mut(block).params.push((id, ty));
        id
    }

    /// Call `f` on every mutable [`IrType`] annotation inside this
    /// function: parameters, return, block-parameter types, every
    /// instruction's result type, and the per-value type index.
    ///
    /// Intended for passes that substitute types after the fact
    /// (monomorphization is the canonical caller). Going through this
    /// method instead of hand-rolling a walk keeps the four parallel
    /// type annotations on [`IrFunction`] in sync — forgetting one of
    /// them is a silent mis-compile, and adding a new list-of-types to
    /// this struct requires updating exactly one place here.
    pub fn for_each_type_mut(&mut self, mut f: impl FnMut(&mut IrType)) {
        for pt in &mut self.param_types {
            f(pt);
        }
        f(&mut self.return_type);
        for ct in &mut self.capture_types {
            f(ct);
        }
        for block in &mut self.blocks {
            for instr in &mut block.instructions {
                f(&mut instr.result_type);
            }
            for bp in &mut block.params {
                f(&mut bp.1);
            }
        }
        self.values.for_each_type_mut(|ty| f(ty));
    }
}
