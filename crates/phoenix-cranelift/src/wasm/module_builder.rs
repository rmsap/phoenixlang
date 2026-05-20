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

use phoenix_ir::instruction::FuncId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use crate::error::CompileError;

use super::translate;
use super::type_interner::TypeInterner;

/// Page-count floor for the merged module's linear memory. 17 pages
/// (~1 MB) gives the GC enough room for the modest fixtures we currently
/// target; PR 3c extends this if a fixture pushes past it. The
/// `declare_memory` call below uses `max(MIN_INITIAL_PAGES,
/// runtime_min_pages)` so the runtime's own floor wins when it's larger.
const MIN_INITIAL_PAGES: u64 = 17;

/// Byte offset where user-emitted data segments (string literals from
/// `Op::ConstString`, etc.) start. Decision H in
/// `docs/design-decisions.md` §Phase 2.4 picks low-offset placement
/// inside the runtime's stack region — the stack grows down from
/// [`STACK_REGION_BASE`], so data at `[USER_DATA_BASE, USER_DATA_LIMIT)`
/// sits below the stack's plausible high-water mark
/// ([`STACK_SAFETY_MARGIN`] reserves headroom). Offset 0 is reserved
/// as a NULL sentinel.
pub(super) const USER_DATA_BASE: u32 = 16;

/// Top of the runtime's stack region — the `__stack_pointer` global's
/// initial value, where the stack starts when WASM execution begins.
/// The stack grows *down* from here (so the runtime's actual stack
/// occupies `[low_water_mark, STACK_REGION_BASE)` at any given moment).
pub(super) const STACK_REGION_BASE: u32 = 1_048_576;

/// Conservative bound on the runtime's stack high-water mark — the
/// largest single-program stack excursion we expect any realistic
/// fixture to need. User data must end strictly below
/// `STACK_REGION_BASE - STACK_SAFETY_MARGIN` so that a stack push *can't*
/// trivially clobber the literal bytes (the WASM "decrement-then-store"
/// convention means the very first stack push writes to
/// `[SP-N, SP)` immediately below `STACK_REGION_BASE`, so an upper
/// bound at exactly `STACK_REGION_BASE` would let user data and the
/// stack's first frame fight over the same byte).
///
/// 64 KiB is heuristic: it's an order of magnitude larger than any
/// frame the current fixture set produces, and small enough that user
/// data still gets ~960 KiB of room before the codegen-time tripwire
/// fires. Programs that need more than `USER_DATA_LIMIT` bytes of
/// literal data should revisit decision H rather than expanding the
/// margin — the underlying "stack and literals share a region" design
/// doesn't actually scale.
pub(super) const STACK_SAFETY_MARGIN: u32 = 65_536;

/// Upper bound on user-data offsets (exclusive). See
/// [`STACK_SAFETY_MARGIN`] for the rationale. Reservations that would
/// push the cursor past this fail at codegen time with a diagnostic
/// pointing at decision H.
pub(super) const USER_DATA_LIMIT: u32 = STACK_REGION_BASE - STACK_SAFETY_MARGIN;

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
    /// during the merge phase; PR 3c will append user-emitted
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

    /// Phoenix [`FuncId`] → merged-module WASM function index. Used by
    /// the IR translator to resolve direct `Op::Call(func_id, ..., ..)`
    /// to a concrete WASM `call` target. Populated by
    /// [`Self::declare_phoenix_functions`] as each concrete Phoenix
    /// function is appended to the function section, before any body is
    /// emitted, so calls can target functions that have not yet had
    /// their body lowered (mutual recursion).
    phx_user_funcs: HashMap<FuncId, u32>,

    /// Runtime export name → merged WASM function index. Populated by
    /// [`Self::finalize_merge`] at the end of the merge phase. Queried
    /// by the IR translator (via [`Self::get_phx_func`]) to resolve
    /// runtime calls such as `phx_print_i64`, `phx_gc_alloc`.
    phx_func_lookup: HashMap<String, u32>,

    /// Merged-module global index of the runtime's `__stack_pointer`
    /// (mutable `i32`, initialized to `1048576` — the top of the wasm32-
    /// wasip1 stack region). Populated by [`Self::finalize_merge`].
    /// Consulted by the IR translator's *sret* call sequences (which
    /// must reserve stack space for the callee's struct return) and
    /// PR 3c's shadow-stack root emission. `None` if the runtime
    /// didn't declare a stack-pointer global — which would be an
    /// unexpected runtime-build change and is surfaced as a clean
    /// error rather than silently leaving sret calls broken.
    phx_stack_pointer_global: Option<u32>,

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

    /// Running byte offset where the next *user* data segment would
    /// be appended. Initialized to [`USER_DATA_BASE`] and bumped by
    /// [`Self::reserve_user_data`] for each `Op::ConstString` emission.
    /// Per decision H, the user-data region (`[USER_DATA_BASE,
    /// USER_DATA_LIMIT)`) is disjoint from where the runtime declares
    /// its own data segments (typically at or above
    /// [`STACK_REGION_BASE`]), and leaves [`STACK_SAFETY_MARGIN`] bytes
    /// of headroom for the runtime's stack to grow down without
    /// overrunning user data.
    data_cursor: u32,
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
            phx_user_funcs: HashMap::new(),
            phx_func_lookup: HashMap::new(),
            phx_stack_pointer_global: None,
            // 0 rather than 1: the merge always overwrites this with the
            // runtime's declared minimum (every wasm32-wasip1 cdylib has a
            // memory section), and `declare_memory` clamps to
            // `MIN_INITIAL_PAGES` regardless — so a non-zero "we never
            // observed a memory section" value would just be misleading.
            runtime_min_pages: 0,
            runtime_max_pages: None,
            // User data segments place themselves starting at
            // USER_DATA_BASE per decision H — the runtime's stack
            // grows down from STACK_REGION_BASE, and STACK_SAFETY_MARGIN
            // reserves headroom so user data ending at USER_DATA_LIMIT
            // can't be clobbered by typical-depth stack excursions.
            // Runtime data segments live at much higher offsets
            // (typically STACK_REGION_BASE+) and don't interact with
            // this cursor; the `data_cursor` field tracks only the
            // user-data high-water mark.
            data_cursor: USER_DATA_BASE,
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
                // Decision H assumes the runtime's data segments live
                // at offsets `>= STACK_REGION_BASE`, leaving
                // `[USER_DATA_BASE, STACK_REGION_BASE)` free for user
                // data. Verify that invariant *before* re-encoding so
                // a future rustc/runtime change that puts data low
                // produces a clean diagnostic instead of a runtime
                // overlap with user-emitted string literals. An
                // opaque offset (anything not `i32.const N; end`)
                // also fails: we can't prove disjointness from a
                // shape we can't decode.
                let baked_offset = const_i32_offset(&offset_expr).ok_or_else(|| {
                    CompileError::new(
                        "wasm32-linear: phoenix_runtime.wasm has a data \
                         segment with a non-`i32.const` offset expression. \
                         Decision H requires runtime data segments to live \
                         at offsets >= STACK_REGION_BASE so they stay \
                         disjoint from user-emitted string literals; an \
                         opaque offset can't be proved disjoint. If a \
                         runtime build legitimately needs a globals-based \
                         offset, revisit decision H.",
                    )
                })?;
                if baked_offset < STACK_REGION_BASE {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: phoenix_runtime.wasm declares a \
                         data segment at offset {baked_offset}, below the \
                         user-data region's upper bound \
                         (STACK_REGION_BASE = {STACK_REGION_BASE}). \
                         Decision H places user string literals in \
                         [USER_DATA_BASE, STACK_REGION_BASE); a runtime \
                         segment below that boundary would overlap with \
                         them. Investigate the runtime build (likely a \
                         linker-script change that moved `__data_end`).",
                    )));
                }
                let offset = reencoder
                    .const_expr(offset_expr)
                    .map_err(|e| reencode_err("data segment offset expression", e))?;
                self.data
                    .active(merged_mem, &offset, data.data.iter().copied());
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

    /// Append `bytes` as an active data segment at the current user-
    /// data cursor (initialized to [`USER_DATA_BASE`]). Returns the
    /// `(offset, len)` pair the caller embeds as the two i32 slots of
    /// a `StringRef` fat pointer.
    ///
    /// Decision H places user data at `[USER_DATA_BASE, USER_DATA_LIMIT)`
    /// below the runtime's stack region. Reserving past
    /// [`USER_DATA_LIMIT`] would place literal bytes where a deep-but-
    /// realistic stack excursion could reach them; that's rejected with
    /// a `CompileError`. The runtime-level "stack actually grew far
    /// enough down to clobber user data" case isn't statically
    /// detectable from here (the deepest excursion depends on program
    /// input) and is documented as a bounded limitation in decision H —
    /// the [`STACK_SAFETY_MARGIN`] headroom is heuristic.
    pub(super) fn reserve_user_data(&mut self, bytes: &[u8]) -> Result<(u32, u32), CompileError> {
        // Empty reservations (e.g. `Op::ConstString("")`) emit no data
        // segment — there's nothing to write, and an empty active
        // segment just bloats the binary. Return `USER_DATA_BASE` as a
        // fixed sentinel rather than the current cursor: after a full
        // reservation that lands `data_cursor` at exactly
        // [`USER_DATA_LIMIT`], returning the cursor would point an
        // empty literal into the stack-safety margin
        // `[USER_DATA_LIMIT, STACK_REGION_BASE)`. With `len == 0` the
        // runtime never dereferences the pointer, so today both
        // values are equivalent at runtime — but pinning the offset
        // to `USER_DATA_BASE` keeps every empty-literal pointer
        // unambiguously inside the user-data region. All empty
        // literals alias to the same harmless in-bounds offset; the
        // `phx_str_*` surface treats zero-length slices as empty
        // regardless of pointer.
        if bytes.is_empty() {
            // All empty literals alias to USER_DATA_BASE — see the
            // method-level doc-comment above for the rationale.
            return Ok((USER_DATA_BASE, 0));
        }
        // Both the `u32::try_from(len)` failure (literal > 4 GiB) and
        // the `checked_add` overflow are unreachable on a 32-bit target
        // (you'd need the program text itself to be multi-gigabyte
        // before either could fire), so a single combined error is
        // honest about the practical failure mode: any sane Phoenix
        // program that trips this is hitting the safety-margin bound
        // far sooner.
        let len = u32::try_from(bytes.len()).map_err(|_| {
            CompileError::new(format!(
                "wasm32-linear: user-data segment is {} bytes — over u32::MAX \
                 (this is a >4 GiB literal, which can't be referenced by a \
                 wasm32 fat pointer).",
                bytes.len(),
            ))
        })?;
        let offset = self.data_cursor;
        let new_cursor = offset.checked_add(len).ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-linear: appending a {len}-byte user-data segment at \
                 offset {offset} would overflow the u32 data cursor — the \
                 cumulative user-data size is over 4 GiB.",
            ))
        })?;
        if new_cursor > USER_DATA_LIMIT {
            return Err(CompileError::new(format!(
                "wasm32-linear: appending a {len}-byte user-data segment at offset \
                 {offset} would extend user data to {new_cursor}, past the \
                 user-data ceiling ({USER_DATA_LIMIT} = STACK_REGION_BASE - \
                 STACK_SAFETY_MARGIN). The runtime's stack grows down from \
                 STACK_REGION_BASE ({STACK_REGION_BASE}); placing literal \
                 bytes within {STACK_SAFETY_MARGIN} bytes of that boundary \
                 risks a stack excursion overwriting them. Decision H places \
                 user data in `[USER_DATA_BASE, USER_DATA_LIMIT)`; revisit it \
                 if larger literal capacity is needed.",
            )));
        }
        self.data.active(
            0,
            &wasm_encoder::ConstExpr::i32_const(offset as i32),
            bytes.iter().copied(),
        );
        self.data_cursor = new_cursor;
        Ok((offset, len))
    }

    /// Record the post-merge memory floor / cap and the `phx_*` name
    /// → merged-index lookup table. Called once per `compile_wasm_linear`
    /// after the runtime merge completes.
    pub(super) fn finalize_merge(
        &mut self,
        phx_funcs: HashMap<String, u32>,
        runtime_min_pages: u64,
        runtime_max_pages: Option<u64>,
        stack_pointer_global: Option<u32>,
    ) {
        self.phx_func_lookup = phx_funcs;
        self.runtime_min_pages = runtime_min_pages;
        self.runtime_max_pages = runtime_max_pages;
        self.phx_stack_pointer_global = stack_pointer_global;
    }

    /// Merged-module global index of the runtime's `__stack_pointer`.
    /// `Err` if no such global was observed during merge — surfaces a
    /// clean diagnostic at the *sret* call site (or shadow-stack frame
    /// emission, etc.) rather than letting the WASM bytecode reference
    /// a missing global index that wasmparser would reject later.
    pub(super) fn require_stack_pointer_global(&self) -> Result<u32, CompileError> {
        self.phx_stack_pointer_global.ok_or_else(|| {
            CompileError::new(
                "wasm32-linear: phoenix_runtime.wasm did not declare a \
                 `__stack_pointer` global; *sret* call sequences require it \
                 (callees with struct returns need a caller-allocated stack \
                 region). The runtime build is mismatched with this backend's \
                 expectations — rebuild `phoenix-runtime` for wasm32-wasip1 \
                 from the same workspace and re-run.",
            )
        })
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

            // See `translate::flatten_param_types` for the multi-slot
            // expansion (`StringRef` → `[i32, i32]`, etc.).
            let params = translate::flatten_param_types(&func.param_types)?;
            let returns = translate::wasm_return_valtypes(&func.return_type)?;
            let sig = self.types.intern(&params, &returns);
            let wasm_idx = self.add_local_function(sig);
            // `IrModule::push_concrete` assigns a fresh `FuncId` per
            // function, so the map is duplicate-free by IR-builder
            // construction. A future refactor that reuses `FuncId`s
            // (e.g. for trampolines) would silently overwrite the prior
            // mapping, miscompiling `Op::Call` to the wrong target —
            // catch that at codegen time rather than as a confusing
            // wrong-stdout test failure.
            let prev = self.phx_user_funcs.insert(func.id, wasm_idx);
            debug_assert!(
                prev.is_none(),
                "wasm32-linear: duplicate FuncId {:?} declared (was {:?}, now {}) \
                 — IR builder invariant violated (internal compiler bug)",
                func.id,
                prev,
                wasm_idx,
            );
            if func.name == "main" {
                self.phx_main_idx = Some(wasm_idx);
            }
        }
        Ok(())
    }

    /// Look up the merged WASM function index for a Phoenix user
    /// function ([`FuncId`]). Used by the IR translator to emit a
    /// `call <idx>` for `Op::Call(func_id, ..., ..)`. Returns `None`
    /// for unknown ids — but the IR verifier rejects calls to
    /// undeclared functions before codegen, so reaching `None` here
    /// indicates an internal compiler bug (sema → IR → codegen drift).
    /// Most call sites should prefer [`Self::require_phx_user_func`],
    /// which folds the missing-id case into a uniform diagnostic.
    pub(super) fn get_phx_user_func(&self, id: FuncId) -> Option<u32> {
        self.phx_user_funcs.get(&id).copied()
    }

    /// Like [`Self::get_phx_user_func`] but produces the uniform
    /// internal-compiler-bug diagnostic on miss. Used by `Op::Call`
    /// translation today; PR 3c's `Op::CallIndirect` and closure-target
    /// resolution will route through here too so the phrasing stays in
    /// one place.
    pub(super) fn require_phx_user_func(&self, id: FuncId) -> Result<u32, CompileError> {
        self.get_phx_user_func(id).ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-linear: `Op::Call({id:?}, ..)` references an unknown \
                 user function (internal compiler bug — IR verifier should \
                 have caught this before codegen)"
            ))
        })
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
        // Emit a minimal `name` custom section naming the merged
        // `__stack_pointer` global so downstream tooling (and the
        // structural-assertion test helpers) can resolve it by symbol
        // rather than re-running shape inference against the merged
        // output. Rustc's name section on the runtime is consumed
        // during merge and not propagated, so without this the merged
        // module would have no name section at all.
        //
        // **Test-helper contract:** `compile_wasm_linear.rs`'s
        // `sp_global_access_count_in_module` reads this section to
        // locate `__stack_pointer` in any function body. If a future
        // change ever stops emitting it — or adds other subsections
        // before the global subsection — that helper still works
        // (it iterates subsections), but anything that drops the
        // `__stack_pointer` entry outright will break the SP-traffic
        // assertions on the structural-test fixtures (fizzbuzz, etc.).
        if let Some(sp_idx) = self.phx_stack_pointer_global {
            let mut name_section = wasm_encoder::NameSection::new();
            let mut global_names = wasm_encoder::NameMap::new();
            global_names.append(sp_idx, "__stack_pointer");
            name_section.globals(&global_names);
            module.section(&name_section);
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

/// Decode a const-expression of the exact shape `i32.const N; end`
/// and return `N`. Returns `None` for any other shape (globals.get,
/// arithmetic, longer expressions, other operand types, etc.).
///
/// Shared by `const_i32_offset` (data-segment offset decoding in
/// `merge_data`) and the runtime-merge module's stack-pointer-init
/// heuristic. Both callers add their own additional filters (positivity,
/// sign reinterpretation) on top of this raw decoded value.
pub(super) fn decode_const_i32(expr: &wasmparser::ConstExpr<'_>) -> Option<i32> {
    let mut reader = expr.get_operators_reader();
    let value = match reader.read().ok()? {
        wasmparser::Operator::I32Const { value } => value,
        _ => return None,
    };
    match reader.read().ok()? {
        wasmparser::Operator::End => {}
        _ => return None,
    }
    if !reader.eof() {
        return None;
    }
    Some(value)
}

/// Best-effort extraction of an `i32.const N` baked offset from an
/// active data segment's offset expression. Returns `Some(N as u32)`
/// for the common `i32.const N; end` shape (which is what every cdylib
/// build of `phoenix-runtime` produces today); `None` for any other
/// const-expression shape (globals references, more complex math,
/// etc.). Used by `merge_data` to enforce decision H's invariant that
/// runtime data segments live at offsets `>= STACK_REGION_BASE`.
///
/// Sign-bit-set values (i.e. `i32` interpreted as negative) are
/// reinterpreted bitwise as `u32` — that's the wasm semantics for an
/// `i32.const` used as a memory offset. Such offsets are wildly
/// unrealistic for a `phoenix-runtime.wasm` data segment (`0x80000000`
/// is at the 2 GiB mark, well past any sane runtime image size), but
/// reinterpreting honestly is more robust than silently treating "sign
/// bit set" as "unknown".
fn const_i32_offset(expr: &wasmparser::ConstExpr<'_>) -> Option<u32> {
    decode_const_i32(expr).map(|v| v as u32)
}

#[cfg(test)]
mod tests {
    //! Unit coverage for the missing-runtime-symbol error paths.
    //! These exercise the diagnostics callers see when the runtime
    //! build is mismatched with the backend's expectations — paths
    //! that integration tests can't reach without a custom-built
    //! runtime artifact.

    use super::*;

    /// `require_stack_pointer_global` must surface the rebuild-the-
    /// runtime guidance when `finalize_merge` recorded no SP global.
    /// The integration test fleet exercises the happy path; this
    /// guards the error message so a copy-edit doesn't silently lose
    /// the "rebuild" hint that lets users self-resolve runtime skew.
    #[test]
    fn require_stack_pointer_global_diagnostic_includes_remediation() {
        let mut builder = ModuleBuilder::new();
        builder.finalize_merge(HashMap::new(), 0, None, None);
        let err = builder
            .require_stack_pointer_global()
            .expect_err("missing SP global must error");
        let msg = err.to_string();
        assert!(
            msg.contains("__stack_pointer"),
            "diagnostic must name the missing symbol: {msg}"
        );
        assert!(
            msg.contains("rebuild") || msg.contains("Rebuild"),
            "diagnostic must point users at the remediation (rebuild the runtime): {msg}"
        );
    }

    /// `require_phx_func` must name the missing symbol verbatim and
    /// point at the same rebuild remediation. Mismatched names are the
    /// most common runtime/codegen-skew failure mode (e.g. a builtin
    /// added in the backend but not yet exported by the runtime), and
    /// the diagnostic is the user's main signal.
    #[test]
    fn require_phx_func_diagnostic_names_missing_symbol() {
        let mut builder = ModuleBuilder::new();
        builder.finalize_merge(HashMap::new(), 0, None, None);
        let err = builder
            .require_phx_func("phx_does_not_exist")
            .expect_err("missing runtime symbol must error");
        let msg = err.to_string();
        assert!(
            msg.contains("phx_does_not_exist"),
            "diagnostic must name the missing symbol: {msg}"
        );
        assert!(
            msg.contains("rebuild") || msg.contains("Rebuild"),
            "diagnostic must point users at the remediation: {msg}"
        );
    }

    /// `reserve_user_data` must reject reservations that would cross
    /// the [`USER_DATA_LIMIT`] ceiling (stack-region base minus the
    /// safety margin). Decision H places user data below that
    /// boundary; growing past it puts literal bytes within a single
    /// stack frame's worth of where the stack writes, which would
    /// silently corrupt them. The error path is the codegen-time
    /// tripwire, so the diagnostic must point at decision H so the
    /// caller can decide whether to revisit the scheme or split the
    /// literal.
    #[test]
    fn reserve_user_data_rejects_past_user_data_limit() {
        let mut builder = ModuleBuilder::new();
        // First reservation: fills exactly up to USER_DATA_LIMIT. The
        // tighter bound is `new_cursor > USER_DATA_LIMIT`, so a
        // reservation that lands the cursor at exactly USER_DATA_LIMIT
        // is the boundary case that must still succeed.
        let near_max_len = (USER_DATA_LIMIT - USER_DATA_BASE) as usize;
        let big = vec![0u8; near_max_len];
        let (offset, len) = builder
            .reserve_user_data(&big)
            .expect("reservation up to USER_DATA_LIMIT must succeed");
        assert_eq!(offset, USER_DATA_BASE);
        assert_eq!(len as usize, near_max_len);

        // Second reservation: a single byte past the boundary.
        let err = builder
            .reserve_user_data(&[0u8])
            .expect_err("reservation crossing USER_DATA_LIMIT must error");
        let msg = err.to_string();
        assert!(
            msg.contains("user-data ceiling") || msg.contains("STACK_SAFETY_MARGIN"),
            "diagnostic must mention the ceiling / safety margin: {msg}"
        );
        assert!(
            msg.contains("decision H") || msg.contains("Decision H"),
            "diagnostic should reference decision H so callers can find the design context: {msg}"
        );
    }

    /// Empty `Op::ConstString("")` boundary: `reserve_user_data(&[])`
    /// must succeed without advancing the cursor and without producing
    /// a runtime trap. The returned `(offset, len)` is the empty fat
    /// pointer the runtime's `phx_print_str` treats as an empty slice.
    #[test]
    fn reserve_user_data_accepts_empty_bytes() {
        let mut builder = ModuleBuilder::new();
        let (offset_a, len_a) = builder
            .reserve_user_data(&[])
            .expect("empty reservation must succeed");
        assert_eq!(offset_a, USER_DATA_BASE);
        assert_eq!(len_a, 0);

        // A second empty reservation lands at the same offset because
        // the cursor didn't advance. That's acceptable; it just means
        // multiple empty literals alias. Documented here in case a
        // future caller depends on distinct offsets.
        let (offset_b, len_b) = builder
            .reserve_user_data(&[])
            .expect("second empty reservation must succeed");
        assert_eq!(offset_b, USER_DATA_BASE);
        assert_eq!(len_b, 0);
    }

    /// Build a minimal runtime fixture with a data segment at a low
    /// offset (below `STACK_REGION_BASE`) so `merge_data`'s
    /// disjointness check fires.
    fn module_with_low_offset_data_segment() -> Vec<u8> {
        use wasm_encoder::{
            CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, FunctionSection,
            Instruction, MemorySection, MemoryType, Module, TypeSection,
        };
        let mut types = TypeSection::new();
        types.ty().function([], []);
        let mut funcs = FunctionSection::new();
        funcs.function(0);
        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: 17,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        let mut exports = ExportSection::new();
        exports.export("phx_x", ExportKind::Func, 0);
        let mut code = CodeSection::new();
        let mut f = wasm_encoder::Function::new([]);
        f.instruction(&Instruction::End);
        code.function(&f);
        let mut data = DataSection::new();
        // Data at offset 256 — well below STACK_REGION_BASE — should
        // be rejected because it would overlap with the user-data
        // region the merger reserves for `Op::ConstString` outputs.
        data.active(0, &ConstExpr::i32_const(256), [0xAA, 0xBB].iter().copied());
        let mut module = Module::new();
        module.section(&types);
        module.section(&funcs);
        module.section(&memories);
        module.section(&exports);
        module.section(&code);
        module.section(&data);
        module.finish()
    }

    /// `merge_data` must reject runtime data segments whose baked
    /// offset is below [`STACK_REGION_BASE`] — they would overlap with
    /// the user-data region. This is a future-proofing check against
    /// rustc/linker changes that move runtime data into low offsets.
    #[test]
    fn merge_data_rejects_runtime_segment_below_stack_region() {
        let bytes = module_with_low_offset_data_segment();
        wasmparser::validate(&bytes).expect("low-offset fixture must validate");
        let mut builder = ModuleBuilder::new();
        let err = match super::super::runtime_merge::merge_runtime(&mut builder, &bytes) {
            Ok(_) => panic!("low-offset data segment must be rejected"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("below") && msg.contains("STACK_REGION_BASE"),
            "diagnostic must name the boundary that was crossed: {msg}"
        );
    }

    // Note on the "non-`i32.const` data-segment offset" rejection
    // branch in `merge_data`: there is intentionally no test for it.
    // The wasm MVP only allows `global.get` of imported immutable
    // globals inside const expressions, and the merger rejects
    // imported globals during section processing (before any data
    // segment is decoded), so the branch is unreachable in practice
    // for any current wasm32-wasip1 cdylib build. The branch remains
    // as future-proofing against the extended-const-expressions
    // proposal (which would let local globals appear in init exprs);
    // if that proposal lands, this is the place to add a fixture
    // built with hand-crafted bytes.
}
