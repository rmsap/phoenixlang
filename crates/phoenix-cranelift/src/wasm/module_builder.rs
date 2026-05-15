//! Module-level assembly state for the WASM backend.
//!
//! `ModuleBuilder` owns the per-section builders that `wasm-encoder`
//! exposes and threads them through a two-phase pipeline driven by
//! [`super::compile_wasm_linear`]:
//!
//! 1. **Declare phase** — every function (imports, runtime helpers,
//!    Phoenix functions, `_start`) is added to the function-index
//!    space so call sites have stable targets before any body is
//!    emitted.
//! 2. **Emit phase** — bodies are pushed onto the code section in the
//!    same order their declarations were added.
//!
//! Section order in the emitted module follows the WASM spec: type,
//! import, function, memory, export, code, data. Custom sections are
//! not used in PR 2.

use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use crate::error::CompileError;

use super::helper_usage::HelperUsage;
use super::translate;
use super::type_interner::TypeInterner;

pub(super) struct ModuleBuilder {
    /// Function-signature interning. The same signature shape
    /// (`(i64) -> ()` for `phx_print_i64`, etc.) is reused across
    /// every function that has it, keeping the type section minimal.
    types: TypeInterner,
    /// WASM imports section. Populated by `declare_imports` before any
    /// local function is declared so the import-function indices stay
    /// stable across the rest of the pipeline.
    imports: wasm_encoder::ImportSection,
    /// Local-function type indices. Each entry's position matches the
    /// local function index minus [`Self::import_func_count`].
    functions: wasm_encoder::FunctionSection,
    /// Memory declarations. PR 2 declares exactly one linear memory.
    memories: wasm_encoder::MemorySection,
    /// Exports. PR 2 exports `memory` and `_start`.
    exports: wasm_encoder::ExportSection,
    /// Function bodies. Filled by the emit-phase methods after every
    /// function has been declared in `functions` so call instructions
    /// have stable target indices.
    code: wasm_encoder::CodeSection,
    /// Active data segments. PR 2 emits one segment per Phoenix
    /// `Op::ConstString` encountered, all targeting the default
    /// memory at sequential offsets.
    data: wasm_encoder::DataSection,

    /// Number of imported functions. Used to translate "local
    /// function ordinal N" into "WASM function index N + import_func_count".
    import_func_count: u32,

    /// Function index of the imported `wasi_snapshot_preview1.fd_write`.
    /// `None` until [`Self::declare_imports`] runs.
    fd_write_idx: Option<u32>,
    /// Function index of the imported `wasi_snapshot_preview1.proc_exit`.
    /// `None` until [`Self::declare_imports`] runs.
    proc_exit_idx: Option<u32>,

    /// Function index of the synthesized `phx_print_i64`. `None` when
    /// the module never calls `print(int)`.
    pub(super) print_i64_idx: Option<u32>,
    /// Function index of the synthesized `phx_print_str`. `None` when
    /// no helper that bottoms out in it is needed.
    pub(super) print_str_idx: Option<u32>,
    /// Function index of the synthesized `phx_print_bool`. `None` when
    /// the module never calls `print(bool)`.
    pub(super) print_bool_idx: Option<u32>,

    /// Function index of the WASI-required `_start` entry. Resolved
    /// during `declare_start` and emitted last so it can reference
    /// every Phoenix function by index.
    start_idx: Option<u32>,

    /// Phoenix `main` function index in WASM's flat function space.
    /// `_start` calls this on entry. `None` until
    /// [`Self::declare_phoenix_functions`] finds a function named `main`.
    phx_main_idx: Option<u32>,

    /// Running byte offset where the next data-section payload is
    /// appended. Starts at 0; bumped by [`Self::reserve_data`].
    /// Bool literals (when `print_bool` is needed) occupy the
    /// lowest offsets; PR 3's `Op::ConstString` payloads append
    /// after them.
    data_cursor: u32,
}

impl ModuleBuilder {
    pub(super) fn new() -> Self {
        Self {
            types: TypeInterner::default(),
            imports: wasm_encoder::ImportSection::new(),
            functions: wasm_encoder::FunctionSection::new(),
            memories: wasm_encoder::MemorySection::new(),
            exports: wasm_encoder::ExportSection::new(),
            code: wasm_encoder::CodeSection::new(),
            data: wasm_encoder::DataSection::new(),
            import_func_count: 0,
            fd_write_idx: None,
            proc_exit_idx: None,
            print_i64_idx: None,
            print_str_idx: None,
            print_bool_idx: None,
            start_idx: None,
            phx_main_idx: None,
            data_cursor: 0,
        }
    }

    /// Add a function-typed import. Returns its WASM function index.
    /// Imported function indices precede local function indices in
    /// WASM's flat function-index space, so this must run before any
    /// [`Self::add_local_function`] call.
    fn add_import_function(&mut self, module: &str, name: &str, sig: u32) -> u32 {
        let idx = self.import_func_count;
        self.imports
            .import(module, name, wasm_encoder::EntityType::Function(sig));
        self.import_func_count += 1;
        idx
    }

    /// Append a local-function declaration with signature `sig`.
    /// Returns the WASM function index that body will need to live at
    /// in the code section (i.e. position `(idx - import_func_count)`
    /// among the code-section bodies). The matching body must be
    /// pushed onto [`Self::code`] in the same order this is called.
    fn add_local_function(&mut self, sig: u32) -> u32 {
        let idx = self.import_func_count + self.functions.len();
        self.functions.function(sig);
        idx
    }

    /// Reserve `bytes` for an active data segment targeting the
    /// default memory at the current [`Self::data_cursor`] offset.
    /// Returns `(offset, len)` for the caller to remember.
    ///
    /// Panics if the segment would push `data_cursor` past `u32::MAX`
    /// — PR 2's payloads (5/6-byte bool literals) are nowhere near
    /// that bound, but PR 3 will start appending `Op::ConstString`
    /// payloads here, so the bound is checked rather than silently
    /// wrapped.
    pub(super) fn reserve_data(&mut self, bytes: &[u8]) -> (u32, u32) {
        let offset = self.data_cursor;
        let len: u32 = bytes
            .len()
            .try_into()
            .expect("wasm32-linear: data segment exceeds u32 length");
        let new_cursor = offset
            .checked_add(len)
            .expect("wasm32-linear: data section offset overflowed u32");
        self.data.active(
            0,
            &wasm_encoder::ConstExpr::i32_const(offset as i32),
            bytes.iter().copied(),
        );
        self.data_cursor = new_cursor;
        (offset, len)
    }

    pub(super) fn declare_imports(&mut self) {
        // wasi_snapshot_preview1.fd_write: (i32, i32, i32, i32) -> i32
        let fd_write_type = self.types.intern(
            &[
                wasm_encoder::ValType::I32,
                wasm_encoder::ValType::I32,
                wasm_encoder::ValType::I32,
                wasm_encoder::ValType::I32,
            ],
            &[wasm_encoder::ValType::I32],
        );
        self.fd_write_idx =
            Some(self.add_import_function("wasi_snapshot_preview1", "fd_write", fd_write_type));

        // wasi_snapshot_preview1.proc_exit: (i32) -> ()
        let proc_exit_type = self.types.intern(&[wasm_encoder::ValType::I32], &[]);
        self.proc_exit_idx =
            Some(self.add_import_function("wasi_snapshot_preview1", "proc_exit", proc_exit_type));
    }

    /// Resolved index of the imported `wasi_snapshot_preview1.fd_write`.
    /// Panics if called before [`Self::declare_imports`].
    pub(super) fn fd_write_idx(&self) -> u32 {
        self.fd_write_idx
            .expect("declare_imports must run before fd_write_idx is queried")
    }

    /// Resolved index of the imported `wasi_snapshot_preview1.proc_exit`.
    /// Panics if called before [`Self::declare_imports`].
    pub(super) fn proc_exit_idx(&self) -> u32 {
        self.proc_exit_idx
            .expect("declare_imports must run before proc_exit_idx is queried")
    }

    pub(super) fn declare_memory(&mut self) {
        // One page (64 KiB) is enough for PR 2 — hello-world allocates
        // nothing on the heap; the scratch region near the page top is
        // the only live consumer. PR 3 will revisit when the GC arrives.
        self.memories.memory(wasm_encoder::MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
    }

    /// Declare only the runtime helpers `usage` says we need. Skipping
    /// unneeded helpers keeps both the function-section and the
    /// data-section payload minimal — important so PR 3's
    /// `Op::ConstString` constants can rely on starting from a known
    /// data offset rather than "after whatever bool literals happen to
    /// have been emitted." The bodies are emitted in
    /// [`Self::emit_runtime_bodies`] in the same order.
    pub(super) fn declare_runtime_helpers(&mut self, usage: HelperUsage) {
        if usage.print_i64 {
            // phx_print_i64: (i64) -> ()
            let sig = self.types.intern(&[wasm_encoder::ValType::I64], &[]);
            self.print_i64_idx = Some(self.add_local_function(sig));
        }
        if usage.print_str() {
            // phx_print_str: (i32 ptr, i32 len) -> ()
            let sig = self.types.intern(
                &[wasm_encoder::ValType::I32, wasm_encoder::ValType::I32],
                &[],
            );
            self.print_str_idx = Some(self.add_local_function(sig));
        }
        if usage.print_bool {
            // phx_print_bool: (i32) -> ()
            let sig = self.types.intern(&[wasm_encoder::ValType::I32], &[]);
            self.print_bool_idx = Some(self.add_local_function(sig));
        }
    }

    pub(super) fn declare_phoenix_functions(
        &mut self,
        ir_module: &IrModule,
    ) -> Result<(), CompileError> {
        // Surface the missing-main diagnostic before any per-function
        // diagnostic could fire. Without this early bail, a module with
        // (say) a stray `String`-returning helper but no `main` would
        // produce a "StringRef not supported" error first — the user
        // would fix that, rebuild, and only then learn the real issue
        // was the missing entry point.
        if !ir_module.concrete_functions().any(|f| f.name == "main") {
            return Err(CompileError::new("no main function found"));
        }

        for func in ir_module.concrete_functions() {
            // `main`-specific structural checks run *before*
            // `add_local_function` so a rejected `main` doesn't leave
            // an orphan entry in the function section (the
            // `functions.len() == code.len()` invariant in `finish()`
            // is load-bearing, and keeping reject paths
            // declaration-free keeps that invariant a property of the
            // happy path rather than something to reason about per
            // error case).
            if func.name == "main" {
                Self::validate_main_shape(func)?;
                if self.phx_main_idx.is_some() {
                    // Sema rejects duplicate top-level function
                    // names, but a regression there would otherwise
                    // silently leave `_start` calling whichever
                    // `main` was declared last. Catch it at the
                    // codegen boundary so the failure points at the
                    // duplicate-acceptance bug, not at a mysterious
                    // runtime mismatch.
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

    /// Per-`main` structural checks: WASI's `_start` shim calls `main`
    /// with no arguments and discards no result, so `main` must be
    /// parameterless and void-returning. A mismatch would produce an
    /// operand-stack mismatch at the `Call` site that `wasmparser`
    /// would reject — checking up front means the diagnostic points
    /// at the user's source rather than at the emitted bytes.
    /// Associated function so it runs before any `&mut self`
    /// section-mutation happens.
    fn validate_main_shape(func: &phoenix_ir::module::IrFunction) -> Result<(), CompileError> {
        if !func.param_types.is_empty() {
            return Err(CompileError::new(format!(
                "wasm32-linear: `main` must take no parameters \
                 (found {} parameter(s)); WASI `_start` calls \
                 `main` with no arguments. Multi-arg entry points \
                 land in Phase 2.5 with the host-import surface.",
                func.param_types.len(),
            )));
        }
        if !matches!(func.return_type, IrType::Void) {
            return Err(CompileError::new(format!(
                "wasm32-linear: `main` must return void (found \
                 return type `{:?}`); WASI `_start` discards no \
                 result. Exit-code-returning `main` lands in \
                 Phase 2.5 with the host-import surface.",
                func.return_type,
            )));
        }
        Ok(())
    }

    pub(super) fn declare_start(&mut self) {
        // _start: () -> () — WASI entry point. Just calls phx_main.
        let sig = self.types.intern(&[], &[]);
        self.start_idx = Some(self.add_local_function(sig));
    }

    pub(super) fn emit_exports(&mut self) {
        let start_idx = self
            .start_idx
            .expect("declare_start must run before emit_exports");
        // Memory always exported as "memory" so WASI runtimes can read
        // it (e.g. for stderr formatting of `proc_exit` messages).
        self.exports
            .export("memory", wasm_encoder::ExportKind::Memory, 0);
        self.exports
            .export("_start", wasm_encoder::ExportKind::Func, start_idx);
    }

    pub(super) fn emit_runtime_bodies(&mut self, usage: HelperUsage) {
        // Emit order must match the declaration order in
        // `declare_runtime_helpers` — the code section is positional.
        if usage.print_i64 {
            super::runtime::emit_print_i64(self);
        }
        if usage.print_str() {
            super::runtime::emit_print_str(self);
        }
        if usage.print_bool {
            super::runtime::emit_print_bool(self);
        }
    }

    pub(super) fn emit_phoenix_bodies(&mut self, ir_module: &IrModule) -> Result<(), CompileError> {
        for func in ir_module.concrete_functions() {
            let body = translate::translate_function(self, func)?;
            self.code.function(&body);
        }
        Ok(())
    }

    pub(super) fn emit_start_body(&mut self) {
        // _start just calls Phoenix's main and returns. WASI considers
        // a returning _start to be exit-code 0, which matches Phoenix's
        // current "main returns void" contract (enforced by
        // `declare_phoenix_functions`).
        let phx_main_idx = self
            .phx_main_idx
            .expect("declare_phoenix_functions must run before emit_start_body");
        let mut f = wasm_encoder::Function::new([]);
        f.instruction(&wasm_encoder::Instruction::Call(phx_main_idx));
        f.instruction(&wasm_encoder::Instruction::End);
        self.code.function(&f);
    }

    /// Append a runtime helper body to the code section. Localizes the
    /// "every `add_local_function` must be paired with one body push"
    /// invariant — call sites in `runtime::emit_*` go through this
    /// rather than reaching into `self.code` directly.
    pub(super) fn push_runtime_body(&mut self, body: &wasm_encoder::Function) {
        self.code.function(body);
    }

    /// Finalize the module. Section order matches the WASM spec:
    /// types → imports → funcs → memory → exports → code → data.
    pub(super) fn finish(self) -> Vec<u8> {
        // Every declared local function must have a matching body in
        // the code section, in the same order. A mismatch here means a
        // `declare_*` ran without its paired `emit_*` (or vice versa)
        // and would produce a structurally invalid module — assert so
        // the regression points at the build pipeline rather than at
        // wasmparser's downstream rejection.
        assert_eq!(
            self.functions.len(),
            self.code.len(),
            "WASM function-section count ({}) does not match code-section count ({}); \
             every declare_*/add_local_function call must be paired with exactly one \
             matching emit_* body push",
            self.functions.len(),
            self.code.len(),
        );

        let mut module = wasm_encoder::Module::new();
        module.section(self.types.section());
        module.section(&self.imports);
        module.section(&self.functions);
        module.section(&self.memories);
        module.section(&self.exports);
        module.section(&self.code);
        if !self.data.is_empty() {
            module.section(&self.data);
        }
        module.finish()
    }
}
