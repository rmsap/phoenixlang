//! Embed-and-merge: splice a pre-compiled `phoenix_runtime.wasm`
//! module into the wasm-encoder output being built for the user's
//! Phoenix program. Implements [§Phase 2.4 decision F](`docs/design-decisions.md`).
//!
//! # The problem
//!
//! Native compilation has `cc -lphoenix_runtime` to link the runtime's
//! compiled object code into the final binary. WebAssembly has no
//! direct equivalent in the wasm-encoder pipeline — `wasm-encoder`
//! emits a *complete* module, not a relocatable object. We could
//! introduce `wasm-ld` as a build dep, but [decision F](`docs/design-decisions.md`)
//! picked the pure-Rust approach: compile `phoenix-runtime` to a
//! complete `.wasm` separately, then merge its sections into our
//! output at codegen time.
//!
//! # The algorithm
//!
//! Two-pass over the runtime module's payloads:
//!
//! 1. **Collect & declare.** Walk every section *except* the code
//!    section. For each section, append the runtime's contents into
//!    the matching [`ModuleBuilder`] section builder, building remap
//!    tables that translate every runtime-local index (type, function,
//!    global, memory, table, data segment, element segment, tag) to
//!    its merged-module index. Imports are appended verbatim — PR 3a
//!    no longer pre-declares any imports on the builder, so the
//!    runtime's WASI imports are the merged module's only imports and
//!    no dedup is required. If a future PR re-introduces pre-declared
//!    imports (or merges a second runtime module), `merge_func_import`
//!    is the right place to add `(module, name)` dedup.
//! 2. **Emit code.** Walk the code section. With the remap tables
//!    fully populated, every instruction's index references can be
//!    translated through the [`Reencode`] trait hooks. `wasm-encoder`'s
//!    [`wasm_encoder::reencode::Reencode`] machinery handles the
//!    per-instruction parse/re-emit; we only have to override the
//!    `*_index` hooks.
//!
//! # Why two passes
//!
//! The code section can reference any index space (types, functions,
//! globals, memories, tables, data segments, elements). All of those
//! sections precede the code section in the WASM source binary order
//! *except* the data section. We could complicate the single-pass
//! design with a pre-scan of just the data section, but two passes is
//! simpler and the wasmparser overhead is small.
//!
//! # Exported runtime symbols
//!
//! The runtime exports every `phx_*` symbol it provides via its WASM
//! export section. During pass 1 we record the mapping `name →
//! merged_function_index` into the returned [`MergeOutcome`]. Phoenix's
//! IR translator looks up symbols by name (`phx_print_i64`,
//! `phx_gc_alloc`, etc.) and emits `Op::Call(<merged_index>)`.

use std::collections::HashMap;

// `RoundtripReencoder` is wasm-encoder's default "translate every index
// through identity" reencoder; we override the index-translation hooks
// on `MergeReencoder` below and otherwise let `Reencode`'s default-
// method machinery (which the trait provides) do the work.
use wasm_encoder::reencode::Reencode;
use wasmparser::{Parser, Payload};

use crate::error::CompileError;

use super::module_builder::{ModuleBuilder, reencode_err};

/// Outcome of [`merge_runtime`]: the lookup table from runtime-export
/// names (e.g. `"phx_print_i64"`) to merged-module function indices.
/// Consumers (the IR translator, `_start` body emission) consult this
/// when they need to call into the runtime.
pub(super) struct MergeOutcome {
    /// `export_name → merged_function_index`. Populated from the
    /// runtime's export section during pass 1.
    pub phx_funcs: HashMap<String, u32>,
    /// Minimum number of 64-KiB memory pages the runtime expects.
    /// Driven by the runtime's own memory declaration; the merged
    /// module's memory must declare at least this many pages so the
    /// runtime allocator has somewhere to live.
    pub runtime_min_pages: u64,
    /// Maximum number of 64-KiB memory pages the runtime declared,
    /// if any. `None` means the runtime imposed no cap (the wasm32-
    /// wasip1 default). Propagated so a runtime-declared sandbox cap
    /// survives the merge rather than being silently dropped.
    pub runtime_max_pages: Option<u64>,
}

/// Top-level entry: merge `runtime_bytes` into `builder`. Returns the
/// `phx_*` lookup table plus the memory-pages floor the runtime
/// requires.
///
/// `builder` must be in its initial state — no imports, no functions,
/// no memory. PR 3a's pipeline runs `merge_runtime` before any other
/// section-mutating call on `builder`. `builder.memories` must be empty
/// in particular: the runtime's memory declaration becomes the merged
/// module's sole memory (we don't currently support two memories in
/// the merged module).
pub(super) fn merge_runtime(
    builder: &mut ModuleBuilder,
    runtime_bytes: &[u8],
) -> Result<MergeOutcome, CompileError> {
    let mut merger = RuntimeMerger::new();

    // Pass 1: walk all payloads, build remap tables, emit every
    // section except code.
    for payload in Parser::new(0).parse_all(runtime_bytes) {
        let payload = payload.map_err(|e| {
            CompileError::new(format!(
                "wasm32-linear: parsing phoenix_runtime.wasm failed: {e}"
            ))
        })?;
        merger.collect(payload, builder)?;
    }

    // Pass 2: emit the code section using the now-complete remap tables.
    for payload in Parser::new(0).parse_all(runtime_bytes) {
        let payload = payload.map_err(|e| {
            CompileError::new(format!(
                "wasm32-linear: parsing phoenix_runtime.wasm (code pass) failed: {e}"
            ))
        })?;
        if let Payload::CodeSectionEntry(body) = payload {
            // Borrow scope for the reencoder is the single
            // `parse_function_body` call: re-creating each iteration
            // keeps `merger` available for the next body and makes the
            // `&mut RuntimeMerger` borrow explicit at the call site
            // (versus hiding it inside a helper method).
            let mut reencoder = MergeReencoder {
                remaps: &mut merger,
            };
            reencoder
                .parse_function_body(builder.code_mut(), body)
                .map_err(|e| reencode_err("runtime function body", e))?;
        }
    }

    Ok(MergeOutcome {
        phx_funcs: merger.phx_funcs,
        runtime_min_pages: merger.runtime_min_pages,
        runtime_max_pages: merger.runtime_max_pages,
    })
}

/// Per-merge state. Holds the remap tables and accumulates the
/// runtime-export lookup as the runtime's export section is walked.
///
/// `RuntimeMerger` is constructed fresh per `merge_runtime` call; it
/// borrows `&mut ModuleBuilder` only for the duration of `collect()`
/// and `parse_function_body()` calls (so the type can also be passed
/// to `wasm-encoder`'s `Reencode` trait, which doesn't tolerate the
/// builder borrow in its `self`).
struct RuntimeMerger {
    /// Runtime type-section index → merged-module type-section index.
    type_remap: Vec<u32>,
    /// Runtime function-index → merged function-index. Includes both
    /// imports (low indices) and locals (after imports). Populated as
    /// imports + the function section are observed.
    func_remap: Vec<u32>,
    /// Runtime global-index → merged global-index.
    global_remap: Vec<u32>,
    /// Runtime table-index → merged table-index.
    table_remap: Vec<u32>,
    /// Runtime data-segment index → merged data-segment index.
    data_remap: Vec<u32>,

    /// Runtime export name → merged function index, only populated for
    /// `Func`-kind exports. Other export kinds (memory, table, global)
    /// are observed but not surfaced — we declare our own `memory`
    /// export and the rest are runtime-internal.
    phx_funcs: HashMap<String, u32>,

    /// Memory-page minimum from the runtime's `MemorySection`.
    /// Defaults to 1 if the runtime has no memory section (should
    /// always be present for a wasm32-wasip1 cdylib).
    runtime_min_pages: u64,
    /// Memory-page maximum from the runtime's `MemorySection`, if the
    /// runtime declared one. Propagated to the merged module so a
    /// runtime-declared sandbox cap isn't silently dropped during
    /// merge. `None` is the wasm32-wasip1 default.
    runtime_max_pages: Option<u64>,

    /// Set once a `MemorySection` has been observed so a second one
    /// (an attempt to declare two memories — currently only legal
    /// under the multi-memory proposal) fails loudly rather than
    /// silently keeping the last memory.
    saw_memory_section: bool,
}

impl RuntimeMerger {
    fn new() -> Self {
        Self {
            type_remap: Vec::new(),
            func_remap: Vec::new(),
            global_remap: Vec::new(),
            table_remap: Vec::new(),
            data_remap: Vec::new(),
            phx_funcs: HashMap::new(),
            runtime_min_pages: 1,
            runtime_max_pages: None,
            saw_memory_section: false,
        }
    }

    /// Pass-1 handler: observe one payload, update remap tables, and
    /// emit into the appropriate `builder` section. Skips the code
    /// section (handled in pass 2 by `parse_function_body`).
    fn collect(
        &mut self,
        payload: Payload<'_>,
        builder: &mut ModuleBuilder,
    ) -> Result<(), CompileError> {
        match payload {
            Payload::TypeSection(rdr) => self.collect_types(rdr, builder)?,
            Payload::ImportSection(rdr) => self.collect_imports(rdr, builder)?,
            Payload::FunctionSection(rdr) => self.collect_functions(rdr, builder)?,
            Payload::TableSection(rdr) => self.collect_tables(rdr, builder)?,
            Payload::MemorySection(rdr) => self.collect_memories(rdr)?,
            Payload::GlobalSection(rdr) => self.collect_globals(rdr, builder)?,
            Payload::ExportSection(rdr) => self.collect_exports(rdr)?,
            Payload::DataSection(rdr) => self.collect_data(rdr, builder)?,
            // Code is pass 2.
            Payload::CodeSectionStart { .. } | Payload::CodeSectionEntry(_) => {}
            Payload::ElementSection(rdr) => self.collect_elements(rdr, builder)?,
            // Tag / DataCount / Start / Custom: not used by
            // wasm32-wasip1-built phoenix-runtime today; reject with a
            // pointer to this site if a future Rust toolchain change
            // surfaces one so the silent-truncation footgun is loud.
            Payload::StartSection { .. } => {
                return Err(CompileError::new(
                    "wasm32-linear: phoenix_runtime.wasm declares a start function. \
                     The wasm32-linear backend owns the `_start` export and the \
                     runtime is not expected to declare its own. Investigate why \
                     the runtime build picked up a start function.",
                ));
            }
            Payload::TagSection(_) => {
                return Err(CompileError::new(
                    "wasm32-linear: phoenix_runtime.wasm contains a tag section \
                     (exception-handling). The embed-and-merge step doesn't \
                     handle tags yet.",
                ));
            }
            // Everything else (custom sections, version header, end-of-module
            // marker) is ignored.
            _ => {}
        }
        Ok(())
    }

    fn collect_types(
        &mut self,
        rdr: wasmparser::TypeSectionReader<'_>,
        builder: &mut ModuleBuilder,
    ) -> Result<(), CompileError> {
        for group in rdr {
            let group = group.map_err(parse_err)?;
            // `group.types()` yields `&SubType` borrowed from the
            // reader; the merge API takes the reference directly so
            // we avoid cloning the `Box<[ValType]>`s inside each
            // function signature.
            for ty in group.types() {
                let merged_idx = builder.intern_runtime_type(ty)?;
                self.type_remap.push(merged_idx);
            }
        }
        Ok(())
    }

    fn collect_imports(
        &mut self,
        rdr: wasmparser::ImportSectionReader<'_>,
        builder: &mut ModuleBuilder,
    ) -> Result<(), CompileError> {
        for group in rdr {
            match group.map_err(parse_err)? {
                wasmparser::Imports::Single(_, imp) => {
                    self.observe_import(imp.module, imp.name, imp.ty, builder)?;
                }
                wasmparser::Imports::Compact1 { module, items } => {
                    for item in items {
                        let item = item.map_err(parse_err)?;
                        self.observe_import(module, item.name, item.ty, builder)?;
                    }
                }
                wasmparser::Imports::Compact2 { module, ty, names } => {
                    for name in names {
                        let name = name.map_err(parse_err)?;
                        self.observe_import(module, name, ty, builder)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Handle a single import: dedupe against existing builder
    /// imports by `(module, name)`, push the resulting merged index
    /// into the appropriate per-kind remap table. The kind-specific
    /// remap tables (`func_remap`, `global_remap`, `table_remap`)
    /// each get appended only when an import of that kind is observed
    /// — so the entry at remap[i] corresponds to the i-th import of
    /// that kind in the runtime, in declaration order. Memory imports
    /// are rejected outright (only one merged memory is supported).
    fn observe_import(
        &mut self,
        module: &str,
        name: &str,
        ty: wasmparser::TypeRef,
        builder: &mut ModuleBuilder,
    ) -> Result<(), CompileError> {
        match ty {
            // `Func` carries the type-section index for the signature;
            // `FuncExact` carries the signature itself inline (used for
            // canonicalized function imports — wasmparser surfaces both
            // shapes depending on how the source module encoded them).
            // For PR 3a we only expect `Func` (wasm32-wasip1's
            // canonical output), but the diagnostic for `FuncExact`
            // names the source so a runtime-toolchain change surfaces
            // cleanly rather than silently miscompiling.
            wasmparser::TypeRef::FuncExact(_) => {
                return Err(CompileError::new(format!(
                    "wasm32-linear: runtime import {module}.{name} uses an inline \
                     (canonicalized) function type. wasm32-wasip1 is expected to \
                     emit type-indexed `Func` imports; investigate the runtime \
                     build."
                )));
            }
            wasmparser::TypeRef::Func(type_idx) => {
                let merged_type_idx = self.type_remap.get(type_idx as usize).copied().ok_or_else(|| {
                    CompileError::new(format!(
                        "wasm32-linear: runtime import {module}.{name} references type {type_idx} \
                         which has not been observed yet (type section must precede import section)"
                    ))
                })?;
                let merged_idx = builder.merge_func_import(module, name, merged_type_idx);
                self.func_remap.push(merged_idx);
            }
            wasmparser::TypeRef::Memory(mt) => {
                // Runtime memory imports aren't expected for the
                // standalone cdylib build — the runtime declares its
                // own memory via MemorySection. If a future build
                // shape adds a memory import, append it via the
                // ModuleBuilder.
                return Err(CompileError::new(format!(
                    "wasm32-linear: phoenix_runtime.wasm imports a memory ({module}.{name}, {mt:?}); \
                     not supported by the embed-and-merge step yet"
                )));
            }
            wasmparser::TypeRef::Global(_) => {
                return Err(CompileError::new(format!(
                    "wasm32-linear: phoenix_runtime.wasm imports a global ({module}.{name}); \
                     not supported by the embed-and-merge step yet"
                )));
            }
            wasmparser::TypeRef::Table(_) => {
                return Err(CompileError::new(format!(
                    "wasm32-linear: phoenix_runtime.wasm imports a table ({module}.{name}); \
                     not supported by the embed-and-merge step yet"
                )));
            }
            wasmparser::TypeRef::Tag(_) => {
                return Err(CompileError::new(format!(
                    "wasm32-linear: phoenix_runtime.wasm imports a tag ({module}.{name}); \
                     not supported by the embed-and-merge step yet"
                )));
            }
        }
        Ok(())
    }

    fn collect_functions(
        &mut self,
        rdr: wasmparser::FunctionSectionReader<'_>,
        builder: &mut ModuleBuilder,
    ) -> Result<(), CompileError> {
        for ty in rdr {
            let ty = ty.map_err(parse_err)?;
            let merged_type_idx = self.type_remap.get(ty as usize).copied().ok_or_else(|| {
                CompileError::new(format!(
                    "wasm32-linear: runtime function references type {ty} not yet observed"
                ))
            })?;
            let merged_func_idx = builder.add_local_function(merged_type_idx);
            self.func_remap.push(merged_func_idx);
        }
        Ok(())
    }

    fn collect_tables(
        &mut self,
        rdr: wasmparser::TableSectionReader<'_>,
        builder: &mut ModuleBuilder,
    ) -> Result<(), CompileError> {
        let mut reencoder = MergeReencoder { remaps: self };
        for table in rdr {
            let table = table.map_err(parse_err)?;
            let merged_idx = builder.merge_table(&mut reencoder, table)?;
            reencoder.remaps.table_remap.push(merged_idx);
        }
        Ok(())
    }

    fn collect_memories(
        &mut self,
        rdr: wasmparser::MemorySectionReader<'_>,
    ) -> Result<(), CompileError> {
        // The runtime's memory declaration drives the merged module's
        // memory floor; we don't append a memory to the builder here
        // because the builder already owns the single shared memory.
        // No per-memory remap table: `MergeReencoder::memory_index`
        // hardcodes the single-memory invariant (runtime memory 0 →
        // merged memory 0; any other index is a multi-memory module
        // we reject).
        for mem in rdr {
            let mem = mem.map_err(parse_err)?;
            // Reject every memory-shape flag that's outside what a
            // canonical wasm32-wasip1 cdylib emits. Silently ignoring
            // (or carrying over) `memory64` / `shared` / a non-default
            // page size would produce a merged module whose memory
            // semantics drift from the runtime's compiled-in assumptions
            // — a corruption footgun. Loud failure is the right shape.
            if mem.memory64 {
                return Err(CompileError::new(
                    "wasm32-linear: phoenix_runtime.wasm declares a 64-bit memory \
                     (memory64 proposal). The wasm32-linear backend assumes wasm32 \
                     pointers; investigate the runtime build.",
                ));
            }
            if mem.shared {
                return Err(CompileError::new(
                    "wasm32-linear: phoenix_runtime.wasm declares a shared memory \
                     (threads proposal). The wasm32-linear backend is single-threaded; \
                     investigate the runtime build.",
                ));
            }
            if mem.page_size_log2.is_some() {
                return Err(CompileError::new(format!(
                    "wasm32-linear: phoenix_runtime.wasm declares a non-default page \
                     size ({:?}). The merge step assumes the default 64-KiB page \
                     and would otherwise miscompile memory-page arithmetic.",
                    mem.page_size_log2,
                )));
            }
            // Two memories in the merged module would require routing
            // every load/store between two index spaces — we don't
            // support that today. A wasm32-wasip1 cdylib only ever
            // produces one memory, so seeing a second is a runtime-
            // build regression.
            if self.saw_memory_section {
                return Err(CompileError::new(
                    "wasm32-linear: phoenix_runtime.wasm declares more than one memory \
                     (multi-memory proposal). The merge step expects exactly one.",
                ));
            }
            self.saw_memory_section = true;
            self.runtime_min_pages = self.runtime_min_pages.max(mem.initial);
            self.runtime_max_pages = mem.maximum;
        }
        Ok(())
    }

    fn collect_globals(
        &mut self,
        rdr: wasmparser::GlobalSectionReader<'_>,
        builder: &mut ModuleBuilder,
    ) -> Result<(), CompileError> {
        let mut reencoder = MergeReencoder { remaps: self };
        for global in rdr {
            let global = global.map_err(parse_err)?;
            let merged_idx = builder.merge_global(&mut reencoder, global)?;
            reencoder.remaps.global_remap.push(merged_idx);
        }
        Ok(())
    }

    fn collect_exports(
        &mut self,
        rdr: wasmparser::ExportSectionReader<'_>,
    ) -> Result<(), CompileError> {
        for export in rdr {
            let export = export.map_err(parse_err)?;
            if matches!(export.kind, wasmparser::ExternalKind::Func) {
                let merged_idx = self
                    .func_remap
                    .get(export.index as usize)
                    .copied()
                    .ok_or_else(|| {
                        CompileError::new(format!(
                            "wasm32-linear: runtime exports function {} (`{}`) but \
                             that index has no remap entry",
                            export.index, export.name
                        ))
                    })?;
                // WASM validators reject duplicate export names, so a
                // second insert for the same name signals a malformed
                // runtime — fail in both debug and release builds
                // rather than silently last-write-wins.
                if self
                    .phx_funcs
                    .insert(export.name.to_string(), merged_idx)
                    .is_some()
                {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: phoenix_runtime.wasm exports `{}` more than \
                         once (a validator should have rejected this; investigate \
                         the runtime build)",
                        export.name,
                    )));
                }
            }
            // Other export kinds (memory, table, global) are runtime-
            // internal; we don't surface them. The merged module
            // exports its own `memory` / `_start` separately.
        }
        Ok(())
    }

    fn collect_data(
        &mut self,
        rdr: wasmparser::DataSectionReader<'_>,
        builder: &mut ModuleBuilder,
    ) -> Result<(), CompileError> {
        let mut reencoder = MergeReencoder { remaps: self };
        for data in rdr {
            let data = data.map_err(parse_err)?;
            let merged_idx = builder.merge_data(&mut reencoder, data)?;
            reencoder.remaps.data_remap.push(merged_idx);
        }
        Ok(())
    }

    /// Element-section merge. Element segments initialize the
    /// indirect-function table (used by `call_indirect` for closures
    /// and trait-object dispatch inside the runtime). Each segment's
    /// function-index entries get translated through `func_remap`
    /// via the reencoder.
    ///
    /// Active segments target `table_index` 0 by spec default; the
    /// reencoder's `table_index` hook remaps it. Passive and declared
    /// segments are passed through verbatim (their items are still
    /// remapped via the reencoder).
    fn collect_elements(
        &mut self,
        rdr: wasmparser::ElementSectionReader<'_>,
        builder: &mut ModuleBuilder,
    ) -> Result<(), CompileError> {
        let mut reencoder = MergeReencoder { remaps: self };
        // wasm-encoder's `parse_element_section` walks the reader and
        // emits each element via the reencoder's hooks, which already
        // do the right index translation for function-index entries
        // and table-index references.
        reencoder
            .parse_element_section(builder.elements_mut(), rdr)
            .map_err(|e| reencode_err("element section", e))?;
        // The merged module's element-segment count grows by however
        // many entries the runtime had, but PR 3a deliberately does
        // not build an `element_remap` table — today's wasm32-wasip1
        // runtime emits no `table.init` / `elem.drop` instructions,
        // so no function body references element segments by index.
        // `MergeReencoder::element_index` surfaces an explicit
        // `UserError` if that assumption ever breaks (instead of
        // silently identity-remapping into the wrong segment). If a
        // future runtime adds element-by-index references, populate
        // `element_remap` here matching the order
        // `parse_element_section` emitted them in.
        Ok(())
    }
}

/// `Reencode` impl for translating runtime indices into merged-module
/// indices. Used by `wasm-encoder`'s reencode helpers when re-emitting
/// runtime function bodies (which reference all the index spaces) and
/// when re-emitting table / global / data section entries (which can
/// contain const-expression references to other indices).
///
/// Borrows the [`RuntimeMerger`]'s remap tables via the wrapper struct
/// `MergeReencoder` so the borrow stays scoped to each section-walk
/// call rather than tying up `RuntimeMerger` for the duration of the
/// merge.
struct MergeReencoder<'a> {
    remaps: &'a mut RuntimeMerger,
}

impl Reencode for MergeReencoder<'_> {
    /// User error is `CompileError`, not `Infallible`, so out-of-range
    /// remap lookups can surface a contextual diagnostic naming the
    /// specific index kind and value that fell out of bounds. The
    /// `Reencode::Error<CompileError>` `UserError` variant carries
    /// these to the call site, which unwraps via `reencode_err` in
    /// `module_builder.rs`.
    type Error = CompileError;

    fn type_index(&mut self, ty: u32) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        self.remaps
            .type_remap
            .get(ty as usize)
            .copied()
            .ok_or_else(|| {
                wasm_encoder::reencode::Error::UserError(unreachable_index(
                    "type",
                    ty,
                    self.remaps.type_remap.len(),
                ))
            })
    }

    fn function_index(
        &mut self,
        func: u32,
    ) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        self.remaps
            .func_remap
            .get(func as usize)
            .copied()
            .ok_or_else(|| {
                wasm_encoder::reencode::Error::UserError(unreachable_index(
                    "function",
                    func,
                    self.remaps.func_remap.len(),
                ))
            })
    }

    fn global_index(
        &mut self,
        global: u32,
    ) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        self.remaps
            .global_remap
            .get(global as usize)
            .copied()
            .ok_or_else(|| {
                wasm_encoder::reencode::Error::UserError(unreachable_index(
                    "global",
                    global,
                    self.remaps.global_remap.len(),
                ))
            })
    }

    fn memory_index(
        &mut self,
        memory: u32,
    ) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        // Single shared memory: runtime memory index 0 → merged
        // memory index 0. Any non-zero runtime memory index would be
        // a multi-memory module, which we reject above.
        if memory == 0 {
            Ok(0)
        } else {
            Err(wasm_encoder::reencode::Error::UserError(unreachable_index(
                "memory", memory, 1,
            )))
        }
    }

    fn table_index(
        &mut self,
        table: u32,
    ) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        self.remaps
            .table_remap
            .get(table as usize)
            .copied()
            .ok_or_else(|| {
                wasm_encoder::reencode::Error::UserError(unreachable_index(
                    "table",
                    table,
                    self.remaps.table_remap.len(),
                ))
            })
    }

    fn data_index(&mut self, data: u32) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        self.remaps
            .data_remap
            .get(data as usize)
            .copied()
            .ok_or_else(|| {
                wasm_encoder::reencode::Error::UserError(unreachable_index(
                    "data",
                    data,
                    self.remaps.data_remap.len(),
                ))
            })
    }

    /// Element-segment index remap. PR 3a does not build an
    /// `element_remap` table; this hook unconditionally errors. See the
    /// note on `collect_elements` for the rationale and the steps a
    /// future PR needs to follow to support `table.init` / `elem.drop`
    /// references from runtime function bodies.
    fn element_index(
        &mut self,
        element: u32,
    ) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        Err(wasm_encoder::reencode::Error::UserError(CompileError::new(
            format!(
                "wasm32-linear: runtime references element segment {element}; \
                 `element_remap` is not built yet (see `collect_elements`)."
            ),
        )))
    }
}

fn parse_err(e: wasmparser::BinaryReaderError) -> CompileError {
    CompileError::new(format!(
        "wasm32-linear: wasmparser error reading phoenix_runtime.wasm: {e}"
    ))
}

fn unreachable_index(kind: &str, idx: u32, len: usize) -> CompileError {
    CompileError::new(format!(
        "wasm32-linear: runtime {kind} index {idx} out of range \
         (only {len} {kind} entries have been observed during merge)"
    ))
}

#[cfg(test)]
mod tests {
    //! Unit coverage for the merge-pass rejection paths. These exercise
    //! the "we don't handle this yet" diagnostics by feeding tiny
    //! synthetic wasm modules (built with `wasm-encoder` itself) through
    //! `merge_runtime`. The happy-path merge is covered end-to-end by
    //! the integration test (`tests/compile_wasm_linear.rs`), which
    //! runs the real `phoenix_runtime.wasm`; this module covers the
    //! rejection branches the integration test can't easily reach
    //! without a custom-built runtime.
    use super::*;
    use wasm_encoder::{
        CompositeInnerType, CompositeType, DataSection, EntityType, FieldType, FuncType,
        FunctionSection, ImportSection, MemoryType, Module, StartSection, StorageType, StructType,
        SubType, TypeSection, ValType,
    };

    /// Build a minimal valid WASM module that has a `(start <idx>)`
    /// section pointing at a no-op local function. Used to verify the
    /// `Payload::StartSection` rejection branch.
    fn module_with_start_section() -> Vec<u8> {
        let mut types = TypeSection::new();
        types.ty().function([], []);
        let mut funcs = FunctionSection::new();
        funcs.function(0);
        let mut code = wasm_encoder::CodeSection::new();
        let mut f = wasm_encoder::Function::new([]);
        f.instruction(&wasm_encoder::Instruction::End);
        code.function(&f);
        let start = StartSection { function_index: 0 };

        let mut module = Module::new();
        module.section(&types);
        module.section(&funcs);
        module.section(&start);
        module.section(&code);
        module.finish()
    }

    /// Build a module that imports a memory. Used to verify the
    /// `TypeRef::Memory` rejection branch.
    fn module_with_memory_import() -> Vec<u8> {
        let mut imports = ImportSection::new();
        imports.import(
            "env",
            "memory",
            EntityType::Memory(MemoryType {
                minimum: 1,
                maximum: None,
                memory64: false,
                shared: false,
                page_size_log2: None,
            }),
        );
        let mut module = Module::new();
        module.section(&imports);
        module.finish()
    }

    /// Build a module with a passive data segment. Used to verify the
    /// `DataKind::Passive` rejection branch.
    fn module_with_passive_data() -> Vec<u8> {
        let mut data = DataSection::new();
        data.passive([0xDE, 0xAD].iter().copied());
        let mut module = Module::new();
        module.section(&data);
        module.finish()
    }

    /// Build a module whose type section contains a non-final
    /// (open-recursive) function type. Used to verify the
    /// `!ty.is_final` rejection branch in `intern_runtime_type`.
    fn module_with_non_final_type() -> Vec<u8> {
        let mut types = TypeSection::new();
        types.ty().subtype(&SubType {
            is_final: false,
            supertype_idx: None,
            composite_type: CompositeType {
                inner: CompositeInnerType::Func(FuncType::new([], [])),
                shared: false,
                descriptor: None,
                describes: None,
            },
        });
        let mut module = Module::new();
        module.section(&types);
        module.finish()
    }

    /// Build a module whose type section contains a struct type
    /// (a WASM-GC composite). Used to verify the non-Func
    /// `CompositeInnerType::Struct` rejection branch.
    fn module_with_struct_type() -> Vec<u8> {
        let mut types = TypeSection::new();
        types.ty().subtype(&SubType {
            is_final: true,
            supertype_idx: None,
            composite_type: CompositeType {
                inner: CompositeInnerType::Struct(StructType {
                    fields: Box::new([FieldType {
                        element_type: StorageType::Val(ValType::I32),
                        mutable: false,
                    }]),
                }),
                shared: false,
                descriptor: None,
                describes: None,
            },
        });
        let mut module = Module::new();
        module.section(&types);
        module.finish()
    }

    /// Run `merge_runtime` on `bytes` against a fresh builder and
    /// return the resulting error message — panics if the merge
    /// unexpectedly succeeded, since every fixture in this module is
    /// expected to be rejected.
    fn expect_merge_error(bytes: &[u8]) -> String {
        let mut builder = ModuleBuilder::new();
        match merge_runtime(&mut builder, bytes) {
            Ok(_) => panic!("merge_runtime unexpectedly succeeded on rejection fixture"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn rejects_start_section() {
        let err = expect_merge_error(&module_with_start_section());
        assert!(
            err.contains("declares a start function"),
            "expected start-section rejection, got: {err}"
        );
    }

    #[test]
    fn rejects_memory_import() {
        let err = expect_merge_error(&module_with_memory_import());
        assert!(
            err.contains("imports a memory"),
            "expected memory-import rejection, got: {err}"
        );
    }

    #[test]
    fn rejects_passive_data_segment() {
        let err = expect_merge_error(&module_with_passive_data());
        assert!(
            err.contains("passive data"),
            "expected passive-data rejection, got: {err}"
        );
    }

    #[test]
    fn rejects_non_final_type() {
        let err = expect_merge_error(&module_with_non_final_type());
        assert!(
            err.contains("non-final"),
            "expected non-final-type rejection, got: {err}"
        );
    }

    #[test]
    fn rejects_struct_composite_type() {
        let err = expect_merge_error(&module_with_struct_type());
        assert!(
            err.contains("non-function"),
            "expected non-function-type rejection, got: {err}"
        );
    }

    /// Build a module whose type section contains a shared function
    /// type (shared-everything-threads proposal). Used to verify the
    /// `composite_type.shared` rejection branch in
    /// `intern_runtime_type`.
    fn module_with_shared_func_type() -> Vec<u8> {
        let mut types = TypeSection::new();
        types.ty().subtype(&SubType {
            is_final: true,
            supertype_idx: None,
            composite_type: CompositeType {
                inner: CompositeInnerType::Func(FuncType::new([], [])),
                shared: true,
                descriptor: None,
                describes: None,
            },
        });
        let mut module = Module::new();
        module.section(&types);
        module.finish()
    }

    #[test]
    fn rejects_shared_composite_type() {
        let err = expect_merge_error(&module_with_shared_func_type());
        assert!(
            err.contains("shared"),
            "expected shared-composite rejection, got: {err}"
        );
    }

    /// Build a minimal valid runtime-shaped wasm module with one
    /// memory, one global (`__stack_pointer`-shaped), one exported
    /// function (`phx_foo`), and one active data segment. Used by
    /// `merges_minimal_runtime` below to harden the merge happy path
    /// independently of whether the real `phoenix_runtime.wasm` has
    /// been built. The shape mirrors what a stripped wasm32-wasip1
    /// cdylib would emit but stays small enough to assert against.
    fn minimal_runtime_module() -> Vec<u8> {
        use wasm_encoder::{
            ConstExpr, ExportKind, ExportSection, GlobalSection, GlobalType, Instruction,
        };

        let mut types = TypeSection::new();
        types.ty().function([], []);

        let mut funcs = FunctionSection::new();
        funcs.function(0);

        let mut memories = wasm_encoder::MemorySection::new();
        memories.memory(MemoryType {
            minimum: 3,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });

        let mut globals = GlobalSection::new();
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(1024),
        );

        let mut exports = ExportSection::new();
        exports.export("phx_foo", ExportKind::Func, 0);

        let mut code = wasm_encoder::CodeSection::new();
        let mut f = wasm_encoder::Function::new([]);
        f.instruction(&Instruction::End);
        code.function(&f);

        let mut data = DataSection::new();
        data.active(0, &ConstExpr::i32_const(0), [0xAA, 0xBB].iter().copied());

        let mut module = Module::new();
        module.section(&types);
        module.section(&funcs);
        module.section(&memories);
        module.section(&globals);
        module.section(&exports);
        module.section(&code);
        module.section(&data);
        module.finish()
    }

    /// Positive happy-path coverage of `merge_runtime`. The integration
    /// test in `tests/compile_wasm_linear.rs` exercises this against
    /// the real `phoenix_runtime.wasm`, but it's gated on the runtime
    /// being built; this synthetic fixture pins the contract
    /// regardless of CI state — a regression in the remap-table
    /// population (e.g. exports not finding the right merged index, or
    /// the memory floor not being recorded) would fail here even when
    /// the runtime artifact isn't present.
    #[test]
    fn merges_minimal_runtime() {
        let bytes = minimal_runtime_module();
        // Validate first so we know the fixture itself is well-formed
        // — otherwise a fixture bug would surface as a merge failure
        // and we'd chase the wrong root cause.
        wasmparser::validate(&bytes).expect("minimal runtime fixture must validate");

        let mut builder = ModuleBuilder::new();
        let outcome = merge_runtime(&mut builder, &bytes).expect("happy-path merge must succeed");

        // Expected index 0 is load-bearing on the fixture having zero
        // imports: WASM's flat function-index space puts imports first,
        // then locals, so the first local function lands at index 0
        // only when no imports precede it. If a future revision of
        // `minimal_runtime_module` adds an import, this expectation
        // shifts to `Some(&1)`; update the fixture's docstring at the
        // same time so the invariant stays paired.
        assert_eq!(
            outcome.phx_funcs.get("phx_foo"),
            Some(&0),
            "phx_foo should map to merged function index 0 (the fixture has no \
             imports, so the first local function lands at index 0); \
             got phx_funcs={:?}",
            outcome.phx_funcs,
        );
        assert_eq!(
            outcome.runtime_min_pages, 3,
            "runtime_min_pages should reflect the fixture's memory minimum (3); \
             a mismatch means `collect_memories` is dropping or overwriting the floor",
        );
        assert_eq!(
            outcome.runtime_max_pages, None,
            "runtime_max_pages should reflect the fixture's lack of a maximum; \
             a mismatch means `collect_memories` is fabricating a cap",
        );

        // Structural well-formedness check: finish the builder (minus
        // the Phoenix-specific declare/emit phases the production
        // pipeline runs) and feed it through `wasmparser::validate`.
        // This guards against merge regressions that would produce a
        // section-count mismatch, an out-of-range index, or a duplicate
        // section — issues the field-level asserts above can't catch.
        builder.finalize_merge(
            outcome.phx_funcs.clone(),
            outcome.runtime_min_pages,
            outcome.runtime_max_pages,
        );
        builder.declare_memory();
        let merged_bytes = builder.finish();
        wasmparser::validate(&merged_bytes).unwrap_or_else(|e| {
            panic!(
                "merged minimal-runtime module failed validation: {e}\n\
                 merge outcome was: phx_funcs={:?}, min_pages={}, max_pages={:?}",
                outcome.phx_funcs, outcome.runtime_min_pages, outcome.runtime_max_pages,
            )
        });
    }

    /// Build a minimal runtime fixture that declares a memory `maximum`
    /// (sandbox cap). Used to verify that `collect_memories` doesn't
    /// silently drop the cap on the way to the merged module — a real
    /// regression risk because the wasm32-wasip1 default is no cap, so
    /// the integration test wouldn't catch a drop.
    fn minimal_runtime_with_max_memory() -> Vec<u8> {
        use wasm_encoder::{ExportKind, ExportSection};
        let mut types = TypeSection::new();
        types.ty().function([], []);
        let mut funcs = FunctionSection::new();
        funcs.function(0);
        let mut memories = wasm_encoder::MemorySection::new();
        memories.memory(MemoryType {
            minimum: 2,
            maximum: Some(20),
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        let mut exports = ExportSection::new();
        exports.export("phx_x", ExportKind::Func, 0);
        let mut code = wasm_encoder::CodeSection::new();
        let mut f = wasm_encoder::Function::new([]);
        f.instruction(&wasm_encoder::Instruction::End);
        code.function(&f);
        let mut module = Module::new();
        module.section(&types);
        module.section(&funcs);
        module.section(&memories);
        module.section(&exports);
        module.section(&code);
        module.finish()
    }

    #[test]
    fn propagates_runtime_memory_maximum() {
        let bytes = minimal_runtime_with_max_memory();
        wasmparser::validate(&bytes).expect("max-memory fixture must validate");
        let mut builder = ModuleBuilder::new();
        let outcome = merge_runtime(&mut builder, &bytes).expect("merge must succeed");
        assert_eq!(
            outcome.runtime_max_pages,
            Some(20),
            "runtime-declared memory maximum (20) was dropped during merge"
        );
    }

    /// Build a runtime fixture declaring two memory sections (multi-
    /// memory proposal). Used to verify the explicit rejection branch
    /// in `collect_memories`.
    fn module_with_two_memories() -> Vec<u8> {
        let mut memories = wasm_encoder::MemorySection::new();
        memories.memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        memories.memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        let mut module = Module::new();
        module.section(&memories);
        module.finish()
    }

    #[test]
    fn rejects_multi_memory() {
        let err = expect_merge_error(&module_with_two_memories());
        assert!(
            err.contains("more than one memory"),
            "expected multi-memory rejection, got: {err}"
        );
    }

    /// Build a runtime fixture declaring a 64-bit memory. Used to
    /// verify the `memory64` rejection branch.
    fn module_with_memory64() -> Vec<u8> {
        let mut memories = wasm_encoder::MemorySection::new();
        memories.memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: true,
            shared: false,
            page_size_log2: None,
        });
        let mut module = Module::new();
        module.section(&memories);
        module.finish()
    }

    #[test]
    fn rejects_memory64() {
        let err = expect_merge_error(&module_with_memory64());
        assert!(
            err.contains("64-bit memory"),
            "expected memory64 rejection, got: {err}"
        );
    }
}
