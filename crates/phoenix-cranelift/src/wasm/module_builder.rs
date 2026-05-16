//! Module-level assembly state for the WASM backend.
//!
//! `ModuleBuilder` owns the per-section builders that `wasm-encoder`
//! exposes and threads them through a multi-phase pipeline driven by
//! [`super::compile_wasm_linear`]:
//!
//! 1. **Merge phase** — splice the pre-compiled `phoenix_runtime.wasm`
//!    module's sections into our in-progress builder. Adds the runtime's
//!    types, imports (WASI), local functions + their bodies, globals,
//!    tables, and data segments; records a name → merged-function-index
//!    lookup so Phoenix-IR callers can resolve `phx_*` symbols.
//! 2. **Declare phase** — every Phoenix function plus the WASI `_start`
//!    entry is added to the function-index space so call sites have
//!    stable targets before any body is emitted.
//! 3. **Emit phase** — Phoenix function bodies and the `_start` body
//!    are pushed onto the code section in the same order their
//!    declarations were added.
//!
//! Section order in the emitted module follows the WASM spec:
//! type → import → function → table → memory → global → export →
//! code → data. Custom sections are not used.

use std::collections::HashMap;

use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use crate::error::CompileError;

use super::translate;
use super::type_interner::TypeInterner;

/// Page-count floor for the merged module's linear memory. 17 pages
/// (~1 MB) gives the GC enough room for the modest fixtures we currently
/// target; PR 3b extends this if a fixture pushes past it. The
/// `declare_memory` call below uses `max(MIN_INITIAL_PAGES,
/// runtime_min_pages)` so the runtime's own floor wins when it's larger.
const MIN_INITIAL_PAGES: u64 = 17;

pub(super) struct ModuleBuilder {
    /// Function-signature interning. Both user-emitted types and
    /// runtime types (via [`Self::intern_runtime_type`]) flow through
    /// the same interner — dedup is always safe and keeps the type
    /// section minimal when runtime and user signatures coincide.
    types: TypeInterner,
    /// WASM imports section. Populated by [`Self::merge_func_import`]
    /// as the runtime's imports are walked; the merge phase is the
    /// only declarer in PR 3a (the WASI imports we used to add up
    /// front in PR 2 now come in via the runtime's import section).
    imports: wasm_encoder::ImportSection,
    /// Local-function type indices. Each entry's position matches the
    /// local function index minus [`Self::import_func_count`].
    functions: wasm_encoder::FunctionSection,
    /// Table declarations. PR 3a expects one indirect-function table
    /// imported from the runtime (used for trait-object dispatch and
    /// closures inside the runtime); user code does not declare any.
    tables: wasm_encoder::TableSection,
    /// Memory declarations. The merged module owns exactly one memory.
    /// The runtime expects to be the only writer above `__heap_base`.
    memories: wasm_encoder::MemorySection,
    /// Global declarations. The runtime declares at least
    /// `__stack_pointer` (Rust std's wasm32 stack pointer); merged in
    /// via [`Self::merge_global`].
    globals: wasm_encoder::GlobalSection,
    /// Exports. PR 3a exports `memory` and `_start`. Runtime exports
    /// (every `phx_*` symbol) are observed during merge but not
    /// re-exported — they're for internal use.
    exports: wasm_encoder::ExportSection,
    /// Element segments — initializers for the indirect-function
    /// table. The runtime needs these for `call_indirect` dispatch
    /// (closures, trait-object methods). Merged in during the merge
    /// phase; user code doesn't add to this section in PR 3a.
    elements: wasm_encoder::ElementSection,
    /// Function bodies. Filled by the merge phase (runtime bodies),
    /// then [`Self::emit_phoenix_bodies`] (user bodies), then
    /// [`Self::emit_start_body`] (the WASI entry).
    code: wasm_encoder::CodeSection,
    /// Active data segments. Runtime data segments are merged in
    /// during the merge phase; PR 3b will append user-emitted
    /// `Op::ConstString` segments after the runtime's.
    data: wasm_encoder::DataSection,

    /// Number of imported functions. Used to translate "local function
    /// ordinal N" into "WASM function index N + import_func_count".
    import_func_count: u32,

    /// Function index of the WASI-required `_start` entry. Resolved
    /// during [`Self::declare_start`] and emitted last so it can
    /// reference every Phoenix function plus the merged runtime by index.
    start_idx: Option<u32>,

    /// Phoenix `main` function index in WASM's flat function space.
    /// `_start` calls this on entry. `None` until
    /// [`Self::declare_phoenix_functions`] finds a function named `main`.
    phx_main_idx: Option<u32>,

    /// Runtime export name → merged WASM function index. Populated by
    /// [`Self::finalize_merge`] at the end of the merge phase. Queried
    /// by the IR translator (via [`Self::get_phx_func`]) to resolve
    /// runtime calls such as `phx_print_i64`, `phx_gc_alloc`.
    phx_func_lookup: HashMap<String, u32>,

    /// Memory-pages floor required by the runtime, recorded during
    /// merge. The merged module's [`Self::declare_memory`] grows to
    /// at least this many pages so the runtime allocator has the
    /// space its compiled image expects.
    runtime_min_pages: u64,
    /// Memory-pages cap the runtime declared, if any. Propagated to
    /// the merged module's memory section verbatim so a runtime-
    /// declared sandbox cap is preserved. `None` means uncapped,
    /// which is the wasm32-wasip1 default.
    runtime_max_pages: Option<u64>,

    /// Running byte offset where the next *user* data segment is
    /// appended. `Some(N)` is the high-water mark of runtime data
    /// segments observed so far (PR 3b's user-data appender uses this
    /// to know where the free region starts). `None` means at least
    /// one runtime data segment had an offset expression the merge
    /// couldn't statically decode (e.g. a globals reference) — PR 3b
    /// must refuse to append in that case, since user data could
    /// collide with the unknown-position segment. PR 3a doesn't emit
    /// user data (hello.phx has none), so this field is write-only
    /// today; the contract exists for PR 3b's appender.
    data_cursor: Option<u32>,
}

impl ModuleBuilder {
    pub(super) fn new() -> Self {
        Self {
            types: TypeInterner::default(),
            imports: wasm_encoder::ImportSection::new(),
            functions: wasm_encoder::FunctionSection::new(),
            tables: wasm_encoder::TableSection::new(),
            memories: wasm_encoder::MemorySection::new(),
            globals: wasm_encoder::GlobalSection::new(),
            exports: wasm_encoder::ExportSection::new(),
            elements: wasm_encoder::ElementSection::new(),
            code: wasm_encoder::CodeSection::new(),
            data: wasm_encoder::DataSection::new(),
            import_func_count: 0,
            start_idx: None,
            phx_main_idx: None,
            phx_func_lookup: HashMap::new(),
            // 0 rather than 1: the merge always overwrites this with the
            // runtime's declared minimum (every wasm32-wasip1 cdylib has a
            // memory section), and `declare_memory` clamps to
            // `MIN_INITIAL_PAGES` regardless — so a non-zero "we never
            // observed a memory section" value would just be misleading.
            runtime_min_pages: 0,
            runtime_max_pages: None,
            data_cursor: Some(0),
        }
    }

    // --- Merge-phase API --------------------------------------------------
    //
    // These methods are called by `super::runtime_merge` while walking
    // the runtime module's payloads. Each returns the merged-module
    // index of the appended item so the merger can populate its
    // remap tables.

    /// Append a function-signature type from the runtime's type
    /// section. PR 3a only handles function types (the runtime is
    /// wasm32-wasip1, which doesn't use struct/array/cont types).
    /// Returns the merged type-section index.
    pub(super) fn intern_runtime_type(
        &mut self,
        ty: &wasmparser::SubType,
    ) -> Result<u32, CompileError> {
        // Reject features the runtime doesn't currently exercise. The
        // diagnostic names which runtime function the unsupported type
        // is for (when wasmparser gives us that — it doesn't, so we
        // surface the type-section position instead, which a future
        // debugger can correlate with `wasm-tools dump` output).
        if !ty.is_final {
            return Err(CompileError::new(
                "wasm32-linear: phoenix_runtime.wasm contains a non-final \
                 (open-recursive) type. wasm32-wasip1 builds aren't \
                 expected to produce these; investigate the runtime build.",
            ));
        }
        if ty.composite_type.shared {
            return Err(CompileError::new(
                "wasm32-linear: phoenix_runtime.wasm contains a shared \
                 composite type (shared-everything-threads proposal). \
                 wasm32-wasip1 should not produce these; investigate the \
                 runtime build.",
            ));
        }
        match &ty.composite_type.inner {
            wasmparser::CompositeInnerType::Func(func_ty) => {
                let params: Vec<wasm_encoder::ValType> = func_ty
                    .params()
                    .iter()
                    .map(|t| translate::wasm_valtype_from_parser(*t))
                    .collect::<Result<_, _>>()?;
                let returns: Vec<wasm_encoder::ValType> = func_ty
                    .results()
                    .iter()
                    .map(|t| translate::wasm_valtype_from_parser(*t))
                    .collect::<Result<_, _>>()?;
                Ok(self.types.intern(&params, &returns))
            }
            wasmparser::CompositeInnerType::Array(_)
            | wasmparser::CompositeInnerType::Struct(_)
            | wasmparser::CompositeInnerType::Cont(_) => Err(CompileError::new(
                "wasm32-linear: phoenix_runtime.wasm contains a non-function \
                 type (array/struct/cont — WASM GC or stack-switching \
                 features). wasm32-wasip1 should not produce these; \
                 investigate the runtime build.",
            )),
        }
    }

    /// Append an imported function from the runtime. PR 3a does not
    /// dedupe against pre-existing imports because we no longer
    /// pre-declare any (the runtime's WASI imports are the merged
    /// module's only WASI imports). Returns the merged function index.
    pub(super) fn merge_func_import(
        &mut self,
        module: &str,
        name: &str,
        merged_type_idx: u32,
    ) -> u32 {
        let idx = self.import_func_count;
        self.imports.import(
            module,
            name,
            wasm_encoder::EntityType::Function(merged_type_idx),
        );
        self.import_func_count += 1;
        idx
    }

    /// Append a local-function declaration with signature `sig` and
    /// return its WASM function index. Single entry point for both the
    /// merge phase (runtime functions) and the declare phase (Phoenix
    /// functions, `_start`) so the `function-index = import_func_count
    /// + functions.len()` invariant can't drift between callers.
    ///
    /// The matching body must be pushed onto [`Self::code`] in the
    /// same order this is called.
    pub(super) fn add_local_function(&mut self, sig: u32) -> u32 {
        let idx = self.import_func_count + self.functions.len();
        self.functions.function(sig);
        idx
    }

    /// Append a table declaration from the runtime. Returns the
    /// merged table index. `reencoder` is consulted for any const-expr
    /// reference inside the table type (table initializers can carry
    /// `ref.func` references through reencode).
    pub(super) fn merge_table(
        &mut self,
        reencoder: &mut impl wasm_encoder::reencode::Reencode<Error = CompileError>,
        table: wasmparser::Table<'_>,
    ) -> Result<u32, CompileError> {
        let idx = self.tables.len();
        let table_type = reencoder
            .table_type(table.ty)
            .map_err(|e| reencode_err("table type", e))?;
        match table.init {
            wasmparser::TableInit::RefNull => {
                self.tables.table(table_type);
            }
            wasmparser::TableInit::Expr(expr) => {
                let init = reencoder
                    .const_expr(expr)
                    .map_err(|e| reencode_err("table init expression", e))?;
                self.tables.table_with_init(table_type, &init);
            }
        }
        Ok(idx)
    }

    /// Append a global declaration from the runtime. Returns the
    /// merged global index. The const-expression initializer is
    /// rewritten through `reencoder` so any embedded global / function
    /// references get remapped.
    pub(super) fn merge_global(
        &mut self,
        reencoder: &mut impl wasm_encoder::reencode::Reencode<Error = CompileError>,
        global: wasmparser::Global<'_>,
    ) -> Result<u32, CompileError> {
        let idx = self.globals.len();
        let global_type = reencoder
            .global_type(global.ty)
            .map_err(|e| reencode_err("global type", e))?;
        let init = reencoder
            .const_expr(global.init_expr)
            .map_err(|e| reencode_err("global init expression", e))?;
        self.globals.global(global_type, &init);
        Ok(idx)
    }

    /// Append a data segment from the runtime. Returns the merged
    /// data-segment index. PR 3a expects only active segments (i.e.,
    /// segments with a fixed memory offset baked at instantiation);
    /// passive segments (used via `memory.init`) are rejected because
    /// the runtime's wasm32-wasip1 build does not produce them today.
    pub(super) fn merge_data(
        &mut self,
        reencoder: &mut impl wasm_encoder::reencode::Reencode<Error = CompileError>,
        data: wasmparser::Data<'_>,
    ) -> Result<u32, CompileError> {
        let idx = self.data.len();
        match data.kind {
            wasmparser::DataKind::Active {
                memory_index,
                offset_expr,
            } => {
                let merged_mem = reencoder
                    .memory_index(memory_index)
                    .map_err(|e| reencode_err("data segment memory index", e))?;
                // Walk the offset expression *before* handing it to
                // the reencoder (which consumes via the operators
                // reader internally) so we can record the segment's
                // baked offset for the data-cursor advancement. The
                // reencoder takes the ConstExpr by value, so this
                // pre-scan uses a borrow from the same source.
                let baked_offset = const_i32_offset(&offset_expr);
                let offset = reencoder
                    .const_expr(offset_expr)
                    .map_err(|e| reencode_err("data segment offset expression", e))?;
                self.data
                    .active(merged_mem, &offset, data.data.iter().copied());
                // PR 3b's user-data appender relies on `data_cursor`
                // being either `Some(high-water mark)` or `None`
                // (unknown — refuse to append). Anything opaque
                // (non-`i32.const` offset, or >4 GiB segment length)
                // poisons the cursor irreversibly, because we can no
                // longer prove a user-data append wouldn't collide
                // with the unknown-position segment.
                match (baked_offset, u32::try_from(data.data.len())) {
                    (Some(start), Ok(seg_len)) => {
                        let seg_end = start.saturating_add(seg_len);
                        if let Some(cursor) = self.data_cursor.as_mut()
                            && seg_end > *cursor
                        {
                            *cursor = seg_end;
                        }
                    }
                    _ => self.data_cursor = None,
                }
            }
            wasmparser::DataKind::Passive => {
                return Err(CompileError::new(
                    "wasm32-linear: phoenix_runtime.wasm contains a passive data \
                     segment. PR 3a doesn't handle these; if the runtime build \
                     starts producing them, extend `merge_data`.",
                ));
            }
        }
        Ok(idx)
    }

    /// `&mut CodeSection` accessor used by the runtime-merge pass to
    /// emit function bodies through `wasm_encoder::reencode::Reencode::
    /// parse_function_body`. Public-to-super so the merger doesn't
    /// have to re-discover the section.
    pub(super) fn code_mut(&mut self) -> &mut wasm_encoder::CodeSection {
        &mut self.code
    }

    /// `&mut ElementSection` accessor used by the runtime-merge pass
    /// to emit element segments through
    /// `wasm_encoder::reencode::Reencode::parse_element_section`. The
    /// element segments carry the indirect-function-table
    /// initializers the runtime uses for `call_indirect` dispatch.
    pub(super) fn elements_mut(&mut self) -> &mut wasm_encoder::ElementSection {
        &mut self.elements
    }

    /// Record the post-merge memory floor / cap and the `phx_*` name
    /// → merged-index lookup table. Called once per `compile_wasm_linear`
    /// after the runtime merge completes.
    pub(super) fn finalize_merge(
        &mut self,
        phx_funcs: HashMap<String, u32>,
        runtime_min_pages: u64,
        runtime_max_pages: Option<u64>,
    ) {
        self.phx_func_lookup = phx_funcs;
        self.runtime_min_pages = runtime_min_pages;
        self.runtime_max_pages = runtime_max_pages;
    }

    /// Look up the merged WASM function index for a runtime export
    /// name (e.g. `"phx_print_i64"`). Returns `None` if the runtime
    /// doesn't export that symbol, letting callers produce a
    /// diagnostic that names the missing symbol rather than panicking.
    pub(super) fn get_phx_func(&self, name: &str) -> Option<u32> {
        self.phx_func_lookup.get(name).copied()
    }

    /// Like [`Self::get_phx_func`] but produces a uniform diagnostic
    /// when the symbol is missing, so a runtime/codegen version skew
    /// is debuggable from the error message alone. Used by the
    /// IR translator (for `phx_print_i64` / `phx_print_bool` /
    /// `phx_*` callouts) and by `_start` body emission (for
    /// `phx_gc_enable` / `phx_gc_shutdown`) — sharing one phrasing so
    /// future copy-edits stay consistent across call sites.
    pub(super) fn require_phx_func(&self, name: &str) -> Result<u32, CompileError> {
        self.get_phx_func(name).ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-linear: phoenix_runtime.wasm does not export `{name}`; \
                 the runtime is mismatched with this backend's expectations. \
                 Rebuild `phoenix-runtime` for wasm32-wasip1 from the same \
                 workspace and re-run."
            ))
        })
    }

    // --- Pipeline -------------------------------------------------------

    /// Declare the merged module's memory section. Called after merge
    /// so the page floor can absorb whatever the runtime declared.
    /// The user-visible minimum is `max(MIN_INITIAL_PAGES,
    /// runtime_min_pages)`. The runtime's compiled image already embeds
    /// its data-segment offsets and stack-pointer initial value
    /// relative to *its* memory shape, so honoring the runtime floor is
    /// non-negotiable.
    ///
    /// A runtime-declared `maximum` (sandbox cap) is propagated through
    /// verbatim. If the runtime's cap is lower than our floor, we drop
    /// the cap rather than emit an invalid `minimum > maximum` memory
    /// type — the floor wins because the runtime can't actually run in
    /// less than its declared minimum, regardless of any cap. A warning
    /// is emitted in that case so a runtime build that quietly tightens
    /// its cap below our floor doesn't have its sandbox silently
    /// disabled.
    pub(super) fn declare_memory(&mut self) {
        let min_pages = self.runtime_min_pages.max(MIN_INITIAL_PAGES);
        let max_pages = match self.runtime_max_pages {
            Some(cap) if cap >= min_pages => Some(cap),
            Some(cap) => {
                eprintln!(
                    "warning: wasm32-linear: phoenix_runtime.wasm declared a memory \
                     cap of {cap} pages but the backend floor is {min_pages} pages \
                     (max({MIN_INITIAL_PAGES}, runtime_min={})); dropping the cap \
                     because `minimum > maximum` would be an invalid memory type. \
                     Raise the runtime's cap or lower MIN_INITIAL_PAGES.",
                    self.runtime_min_pages,
                );
                None
            }
            None => None,
        };
        self.memories.memory(wasm_encoder::MemoryType {
            minimum: min_pages,
            maximum: max_pages,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
    }

    pub(super) fn declare_phoenix_functions(
        &mut self,
        ir_module: &IrModule,
    ) -> Result<(), CompileError> {
        if !ir_module.concrete_functions().any(|f| f.name == "main") {
            return Err(CompileError::new("no main function found"));
        }

        for func in ir_module.concrete_functions() {
            if func.name == "main" {
                Self::validate_main_shape(func)?;
                if self.phx_main_idx.is_some() {
                    return Err(CompileError::new(
                        "wasm32-linear: more than one `main` function found \
                         (internal compiler bug — sema should reject duplicate \
                         top-level function names)"
                            .to_string(),
                    ));
                }
            }

            let params: Vec<wasm_encoder::ValType> = func
                .param_types
                .iter()
                .map(translate::wasm_valtype_for)
                .collect::<Result<_, _>>()?;
            let returns: Vec<wasm_encoder::ValType> =
                translate::wasm_return_valtypes(&func.return_type)?;
            let sig = self.types.intern(&params, &returns);
            let wasm_idx = self.add_local_function(sig);
            if func.name == "main" {
                self.phx_main_idx = Some(wasm_idx);
            }
        }
        Ok(())
    }

    fn validate_main_shape(func: &phoenix_ir::module::IrFunction) -> Result<(), CompileError> {
        if !func.param_types.is_empty() {
            return Err(CompileError::new(format!(
                "wasm32-linear: `main` must take no parameters \
                 (found {} parameter(s)); WASI `_start` calls \
                 `main` with no arguments.",
                func.param_types.len(),
            )));
        }
        if !matches!(func.return_type, IrType::Void) {
            return Err(CompileError::new(format!(
                "wasm32-linear: `main` must return void (found \
                 return type `{:?}`); WASI `_start` discards no result.",
                func.return_type,
            )));
        }
        Ok(())
    }

    pub(super) fn declare_start(&mut self) {
        let sig = self.types.intern(&[], &[]);
        self.start_idx = Some(self.add_local_function(sig));
    }

    pub(super) fn emit_exports(&mut self) {
        let start_idx = self
            .start_idx
            .expect("declare_start must run before emit_exports");
        self.exports
            .export("memory", wasm_encoder::ExportKind::Memory, 0);
        self.exports
            .export("_start", wasm_encoder::ExportKind::Func, start_idx);
    }

    pub(super) fn emit_phoenix_bodies(&mut self, ir_module: &IrModule) -> Result<(), CompileError> {
        for func in ir_module.concrete_functions() {
            let body = translate::translate_function(self, func)?;
            self.code.function(&body);
        }
        Ok(())
    }

    /// Emit the WASI `_start` body. The runtime lifecycle is:
    ///
    /// 1. (Optional) `_initialize` / `__wasm_call_ctors` — runs the
    ///    runtime's static constructors (allocator setup, anything a
    ///    transitive dep registers via `#[ctor]`-style hooks). Rust's
    ///    `wasm32-wasip1` cdylib emits one of these symbols when it has
    ///    ctors to run; today's `phoenix-runtime` has none and neither
    ///    symbol is exported, so we skip. Calling the symbol when it
    ///    *is* present keeps the merged module correct against a future
    ///    runtime that gains ctors (a transitive dep change is enough)
    ///    rather than silently UB'ing on uninitialized state.
    /// 2. `phx_gc_enable` — installs the runtime's panic hook and primes
    ///    the GC.
    /// 3. user `main` — the Phoenix entry point.
    /// 4. `phx_gc_shutdown` — runs leak detection if enabled and tears
    ///    the GC down cleanly.
    ///
    /// WASI considers a returning `_start` to be exit-code 0; that
    /// matches Phoenix's "main returns void" contract.
    pub(super) fn emit_start_body(&mut self) -> Result<(), CompileError> {
        let phx_main_idx = self
            .phx_main_idx
            .expect("declare_phoenix_functions must run before emit_start_body");
        let gc_enable_idx = self.require_phx_func("phx_gc_enable")?;
        let gc_shutdown_idx = self.require_phx_func("phx_gc_shutdown")?;
        // Look for either name — `_initialize` is the wasi-preview1
        // cdylib convention; `__wasm_call_ctors` is the older
        // toolchain-internal name. Different rustc / wasi-libc
        // combinations emit one or the other when ctors exist.
        let ctors_idx = self
            .get_phx_func("_initialize")
            .or_else(|| self.get_phx_func("__wasm_call_ctors"));
        let mut f = wasm_encoder::Function::new([]);
        if let Some(idx) = ctors_idx {
            f.instruction(&wasm_encoder::Instruction::Call(idx));
        }
        f.instruction(&wasm_encoder::Instruction::Call(gc_enable_idx));
        f.instruction(&wasm_encoder::Instruction::Call(phx_main_idx));
        f.instruction(&wasm_encoder::Instruction::Call(gc_shutdown_idx));
        f.instruction(&wasm_encoder::Instruction::End);
        self.code.function(&f);
        Ok(())
    }

    /// Finalize the module. Section order matches the WASM spec:
    /// types → imports → funcs → table → memory → global → exports →
    /// code → data.
    pub(super) fn finish(self) -> Vec<u8> {
        assert_eq!(
            self.functions.len(),
            self.code.len(),
            "WASM function-section count ({}) does not match code-section count ({}); \
             every declare_*/merge_*-funcs call must be paired with exactly one \
             matching body push",
            self.functions.len(),
            self.code.len(),
        );

        let mut module = wasm_encoder::Module::new();
        module.section(self.types.section());
        // The per-section `is_empty()` guards below skip emitting empty
        // sections so smaller unit-test fixtures (e.g. a runtime with
        // no globals or no element segments) still validate.
        // Production merges of the real `phoenix_runtime.wasm` always
        // populate every section, so these branches are dead in the
        // happy path.
        if !self.imports.is_empty() {
            module.section(&self.imports);
        }
        module.section(&self.functions);
        if !self.tables.is_empty() {
            module.section(&self.tables);
        }
        module.section(&self.memories);
        if !self.globals.is_empty() {
            module.section(&self.globals);
        }
        module.section(&self.exports);
        if !self.elements.is_empty() {
            module.section(&self.elements);
        }
        module.section(&self.code);
        if !self.data.is_empty() {
            module.section(&self.data);
        }
        module.finish()
    }
}

/// Convert a `wasm_encoder::reencode::Error<CompileError>` into a
/// `CompileError`. `UserError` carries our own `CompileError` (from
/// `MergeReencoder`'s index-bound-check diagnostics) and is unwrapped
/// directly; every other variant is a wasm-encoder-side failure that
/// we wrap with the site name so the failure points at which merge
/// step tripped the parser.
pub(super) fn reencode_err(
    site: &str,
    err: wasm_encoder::reencode::Error<CompileError>,
) -> CompileError {
    match err {
        wasm_encoder::reencode::Error::UserError(inner) => inner,
        other => CompileError::new(format!(
            "wasm32-linear: re-encoding {site} from phoenix_runtime.wasm failed: {other}"
        )),
    }
}

/// Best-effort extraction of an `i32.const N` baked offset from an
/// active data segment's offset expression. Returns `Some(N as u32)`
/// for the common `i32.const N; end` shape (which is what every cdylib
/// build of `phoenix-runtime` produces today); `None` for any other
/// const expression shape (globals references, more complex i64/f32/f64
/// math, etc.). Used to advance `data_cursor` past runtime data
/// segments so PR 3b's user-data appender starts at the right offset.
///
/// Sign-bit-set values (i.e. `i32` interpreted as negative) are
/// reinterpreted bitwise as `u32` — that's the wasm semantics for an
/// `i32.const` used as a memory offset. Such offsets are wildly
/// unrealistic for a `phoenix-runtime.wasm` data segment (`0x80000000`
/// is at the 2 GiB mark, well past any sane runtime image size), but
/// reinterpreting honestly is more robust than silently treating "sign
/// bit set" as "unknown" — returning `None` here would poison the
/// merger's `data_cursor` (PR 3b's appender then refuses to append),
/// which is correct in principle but unhelpfully cautious in practice
/// for a sign-extended offset that does decode cleanly.
fn const_i32_offset(expr: &wasmparser::ConstExpr<'_>) -> Option<u32> {
    let mut reader = expr.get_operators_reader();
    let first = reader.read().ok()?;
    let value = match first {
        wasmparser::Operator::I32Const { value } => value,
        _ => return None,
    };
    // After the i32.const we expect End and nothing else.
    match reader.read().ok()? {
        wasmparser::Operator::End => {}
        _ => return None,
    }
    if !reader.eof() {
        return None;
    }
    Some(value as u32)
}
