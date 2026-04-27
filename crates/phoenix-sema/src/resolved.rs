//! Post-sema product: [`ResolvedModule`] and [`Analysis`].
//!
//! Sema's output is split across two types so consumers can take only
//! the slice they need:
//!
//! - [`ResolvedModule`] is the **id-indexed schema** of resolved
//!   declarations: callables (free functions, user methods, built-in
//!   methods), types (structs, enums, traits), and the per-span maps
//!   that downstream type-walking consumers (IR lowering, the IR
//!   interpreter, the Cranelift backend) need.  This type is what
//!   `phoenix-ir` consumes.  Stable [`phoenix_common::ids`] newtypes
//!   round-trip into IR unchanged.
//!
//! - [`Analysis`] is the **complete sema product**: a
//!   [`ResolvedModule`] plus the auxiliary outputs that don't
//!   participate in the IR-facing schema (semantic diagnostics,
//!   resolved endpoint declarations for `phoenix-codegen`, symbol
//!   references for the LSP, the trait-implementation membership set,
//!   and the type-alias registry kept around for LSP completion /
//!   hover).  [`Analysis`] is what [`crate::checker::check`] returns
//!   and what `phoenix-codegen`, `phoenix-lsp`, the driver, and the
//!   bench harness consume.
//!
//! The split means a `ResolvedModule` has a meaningful contract
//! ("the resolved schema") that doesn't drift as the language gains
//! new sema-only outputs — those land on [`Analysis`] without
//! touching IR's argument types.
//!
//! See `docs/design-decisions.md` *Post-sema ownership: `ResolvedModule`*
//! for the motivation.
//!
//! # Id allocation
//!
//! [`FuncId`], [`StructId`], [`EnumId`], and [`TraitId`] are allocated
//! by [`crate::checker::Checker`] during the registration pass
//! ([`Checker::check_program`](crate::checker::Checker::check_program))
//! in two phases that AST-order each:
//!
//! 1. **Free functions** receive `FuncId(0..N)` in declaration order.
//! 2. **User-declared methods** receive `FuncId(N..N+M)` in declaration
//!    order, where inline `methods { … }` and inline `impl Trait { … }`
//!    on a struct/enum are visited at the struct/enum declaration site
//!    (inherent methods first, then trait impls in source order),
//!    matching IR lowering's registration order.
//!
//! Built-in stdlib methods (`Option::unwrap`, `List::push`, `String::length`, …)
//! do **not** receive [`FuncId`]s — they have no IR function (the
//! Cranelift backend inlines each one) and live in
//! [`ResolvedModule::builtin_methods`] keyed by `(type_name, method_name)`.
//!
//! # Sema ↔ IR id correspondence
//!
//! `IrModule.functions[id.0]` corresponds to either
//! [`ResolvedModule::functions`]`[id.0]` (when `id.0 < user_method_offset`)
//! or [`ResolvedModule::user_methods`]`[id.0 - user_method_offset]`
//! (when `id.0` falls in the user-method range). IR lowering adopts
//! sema's [`FuncId`]s verbatim for user-declared callables and appends
//! synthesized callables (closures, generic specializations) past the
//! end of the user-method range.

use crate::checker::{
    Checker, EndpointInfo, EnumInfo, FunctionInfo, MethodInfo, StructInfo, SymbolRef, TraitInfo,
    TypeAliasInfo,
};
use crate::types::Type;
use phoenix_common::diagnostics::Diagnostic;
use phoenix_common::ids::{
    EnumId, FIRST_USER_ENUM_ID, FuncId, OPTION_ENUM_ID, RESULT_ENUM_ID, StructId, TraitId,
};
use phoenix_common::span::Span;
use phoenix_parser::ast::{CaptureInfo, Declaration, Program};
use std::collections::{HashMap, HashSet};

/// `(type_name, method_name) → FuncId` lookup table for user-declared
/// methods.  Keyed as a nested map so accessors can borrow `&str`
/// directly without allocating temporary owned `String`s.  Built-in
/// methods are stored separately in [`ResolvedModule::builtin_methods`].
pub type MethodIndex = HashMap<String, HashMap<String, FuncId>>;

/// The IR-facing schema of resolved declarations.
///
/// Contains every callable (free function, user method, built-in
/// method), every type (struct, enum, trait), and the per-span maps
/// (`expr_types`, `call_type_args`, `var_annotation_types`,
/// `lambda_captures`) that IR lowering and the IR interpreter
/// consume.  Indexed by the stable [`FuncId`] / [`StructId`] /
/// [`EnumId`] / [`TraitId`] newtypes from [`phoenix_common::ids`];
/// IR lowering adopts those ids verbatim so the two id spaces agree.
///
/// **What's *not* here:** semantic diagnostics, endpoint
/// declarations, symbol references, the trait-impl membership set,
/// and the type-alias registry — those are auxiliary outputs that
/// IR doesn't read, and they live on [`Analysis`] alongside this
/// struct.
///
/// # Invariants
///
/// Enforced at the end of [`build_from_checker`] (release-mode
/// `assert!`, because IR lowering and the Cranelift backend index the
/// `Vec`s without bounds checks):
///
/// 1. **`Vec` ↔ `*_by_name` are 1:1.** `functions.len() ==
///    function_by_name.len()`, and equivalently for `structs` /
///    `enums` / `traits`.  Every entry in `*_by_name` references a
///    valid index in the corresponding `Vec`, and every index in the
///    `Vec` is named by exactly one entry in `*_by_name`.
/// 2. **No unfilled callable slots.** Every entry in `functions` and
///    `user_methods` was populated from a registered declaration —
///    there are no placeholder / sentinel slots.  ([`build_from_checker`]
///    builds these `Vec`s with `Option<…>` slots that it `unwrap`s
///    after every id has been written.)
/// 3. **`user_method_offset == functions.len()`.** Free functions
///    occupy `FuncId(0..functions.len())`; user methods occupy
///    `FuncId(functions.len()..functions.len() + user_methods.len())`.
/// 4. **Reserved enum ids are pinned.** `enum_by_name["Option"] ==
///    `[`OPTION_ENUM_ID`] and `enum_by_name["Result"] ==
///    `[`RESULT_ENUM_ID`]; the first user-declared enum, if any,
///    receives [`FIRST_USER_ENUM_ID`].
/// 5. **No `Type::Error` in `call_type_args`.** Checked in debug
///    only; a release-mode violation would surface as a panic in
///    monomorphization.
#[derive(Debug, Clone)]
pub struct ResolvedModule {
    // ── Callables ────────────────────────────────────────────────────
    /// User-declared free functions in declaration order.  Indexed by
    /// [`FuncId`] in the range `0..user_method_offset`.  Built-in
    /// functions (`print`, `toString`) are runtime-provided and do not
    /// appear here.
    pub functions: Vec<FunctionInfo>,
    /// `function_name → FuncId` for free-function name lookup.
    pub function_by_name: HashMap<String, FuncId>,

    /// User-declared methods in declaration order.  Indexed by
    /// `FuncId(user_method_offset + i) → user_methods[i]`.  Inline
    /// `methods { … }` / `impl Trait { … }` on a struct/enum are
    /// recorded at the struct/enum's declaration site (inherent first,
    /// then trait impls in source order).
    pub user_methods: Vec<MethodInfo>,
    /// FuncId at which the user-method range begins.  Equal to
    /// `functions.len()` (free functions occupy `0..user_method_offset`).
    pub user_method_offset: u32,
    /// `(type_name, method_name) → FuncId` for user-declared methods,
    /// stored as a nested map (`type_name → method_name → FuncId`) so
    /// that lookup accessors can borrow `&str` directly without
    /// allocating temporary `String` keys.  Built-in methods are
    /// absent; query [`Self::builtin_methods`] when this lookup misses.
    /// See [`MethodIndex`] for the concrete shape.
    pub method_index: MethodIndex,

    /// Stdlib built-in methods (`Option.unwrap`, `List.push`,
    /// `String.length`, …), keyed `type_name → method_name → info`.
    /// These have no [`FuncId`] (`MethodInfo::func_id` is `None`)
    /// because the Cranelift backend inlines each one as bespoke
    /// instruction sequences rather than emitting a callable function.
    pub builtin_methods: HashMap<String, HashMap<String, MethodInfo>>,

    // ── Types ────────────────────────────────────────────────────────
    /// User-declared struct definitions in declaration order.
    pub structs: Vec<StructInfo>,
    /// `struct_name → StructId` for by-name lookup.
    pub struct_by_name: HashMap<String, StructId>,

    /// User-declared and built-in enum definitions in registration
    /// order.  Built-in `Option` then `Result` appear first (in that
    /// order) ahead of any user enum.
    pub enums: Vec<EnumInfo>,
    /// `enum_name → EnumId` for by-name lookup.
    pub enum_by_name: HashMap<String, EnumId>,

    /// User-declared traits in declaration order.  Non-object-safe
    /// traits are included (still usable as `<T: Trait>` bounds);
    /// consumers that only want `dyn`-usable traits should filter on
    /// [`TraitInfo::object_safety_error`].
    pub traits: Vec<TraitInfo>,
    /// `trait_name → TraitId` for by-name lookup.
    pub trait_by_name: HashMap<String, TraitId>,

    // ── Per-span maps ───────────────────────────────────────────────
    /// Resolved type for each expression, keyed by source span.
    pub expr_types: HashMap<Span, Type>,
    /// Concrete type arguments inferred at each generic call site,
    /// keyed by the call expression's source span. Values are ordered
    /// by the callee's declared type-parameter list (e.g.
    /// `function pair<A, B>` produces `[type_of_A, type_of_B]`).
    /// Non-generic calls are absent.
    ///
    /// **Invariants enforced by
    /// [`Checker::record_inferred_type_args`](crate::checker::Checker::record_inferred_type_args):**
    /// - No entry contains [`Type::Error`] or any unresolved [`Type::TypeVar`].
    /// - An entry is present *only* when every declared type parameter
    ///   of the callee was inferable from the call site. If any
    ///   parameter is unresolvable, a diagnostic is emitted and no
    ///   entry is recorded (so IR lowering never sees a partial
    ///   binding).
    /// - Covers both free-function generic calls and user-defined
    ///   method generic calls (keyed by the `MethodCallExpr` span for
    ///   the latter).
    ///
    /// **Known architectural limitation (deferred to Phase 3).**
    /// Keying by [`Span`] makes the sema → lowering handoff fragile
    /// under any transformation that reparents or synthesizes AST
    /// nodes (macro expansion, cross-file inlining). The intended
    /// Phase-3 fix is to assign a stable `CallId: u32` at parse time
    /// and key this map on it. For the single-file, single-pass Phase
    /// 2 compiler, spans are immutable per `SourceId` and unique per
    /// syntactic call expression, so the current keying is sound but
    /// should not be generalized. Tracked in `docs/known-issues.md`.
    // TODO(phase-3): replace `Span` with a stable `CallId: u32`
    // assigned at parse time; see docs/known-issues.md and the
    // doc-comment above for the rationale.
    pub call_type_args: HashMap<Span, Vec<Type>>,
    /// Resolved type annotation for each `let` binding that carried
    /// one, keyed by the `VarDecl`'s source span. Absent entries mean
    /// the binding was unannotated.
    ///
    /// **Internal sema↔IR-lowering contract.** Consumed only by
    /// [`phoenix_ir::lower`] so its dyn-coercion path sees the
    /// resolved type (alias-expanded) rather than re-walking the
    /// parser `TypeExpr`. External consumers should prefer
    /// [`Self::expr_types`].
    ///
    /// Shares the [`Span`]-keying caveat noted on
    /// [`Self::call_type_args`].
    // TODO(phase-3): switch to the stable `CallId`-style key; same
    // rationale as `call_type_args` above.
    pub var_annotation_types: HashMap<Span, Type>,
    /// Captured variables for each lambda expression, keyed by the
    /// lambda's source span. IR lowering uses this for `Op::ClosureAlloc`
    /// metadata; the AST interpreter uses it to populate closure
    /// environments at call time.
    pub lambda_captures: HashMap<Span, Vec<CaptureInfo>>,
}

/// The complete sema product returned by [`crate::checker::check`].
///
/// Wraps a [`ResolvedModule`] (the IR-facing schema) alongside the
/// auxiliary outputs that don't participate in the schema:
///
/// - `diagnostics` — semantic errors and warnings found during
///   analysis.  Empty iff the program is semantically valid.  A
///   non-empty `diagnostics` means the contained `module` may be
///   partial: some declarations may have been skipped at registration
///   time (duplicates) or carry [`Type::Error`] components downstream
///   should not lower.
/// - `endpoints` — resolved endpoint declarations with all types
///   checked.  Consumed exclusively by `phoenix-codegen`.
/// - `symbol_references` — use-site → symbol map for LSP
///   go-to-definition, find-references, and rename.
/// - `trait_impls` — `(type_name, trait_name)` membership set used
///   during sema's own trait-bound dispatch checks; preserved
///   post-sema for future LSP / tooling consumers.
/// - `type_aliases` — registered aliases.  Aliases are expanded
///   away in resolved [`Type`] values, so this map exists primarily
///   so the LSP can surface alias names in completion / hover.
///
/// Consumers that only need the schema (IR lowering, the IR
/// interpreter, the Cranelift backend) take `&ResolvedModule`.
/// Consumers that need both the schema and one or more auxiliaries
/// (codegen, LSP, the driver, the bench harness) take `&Analysis`.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// The IR-facing schema of resolved declarations.
    pub module: ResolvedModule,
    /// Semantic errors and warnings found during analysis.  Empty
    /// iff the program is semantically valid.
    pub diagnostics: Vec<Diagnostic>,
    /// Resolved endpoint declarations with all types checked.
    /// Consumed exclusively by `phoenix-codegen`.
    pub endpoints: Vec<EndpointInfo>,
    /// Symbol references: maps each use-site span to the symbol it
    /// refers to. Used by the LSP for go-to-definition,
    /// find-references, and rename.
    pub symbol_references: HashMap<Span, SymbolRef>,
    /// Set of `(type_name, trait_name)` pairs recording which types
    /// implement which traits.  Sema-internal during checking;
    /// preserved post-sema for future tooling consumers.
    pub trait_impls: HashSet<(String, String)>,
    /// Registered type aliases (`alias_name → info`).  Aliases are
    /// expanded away in resolved [`Type`] values, so this map exists
    /// primarily so the LSP can surface alias names in completion /
    /// hover.
    pub type_aliases: HashMap<String, TypeAliasInfo>,
}

impl ResolvedModule {
    /// Look up a free function's id by source name.  `None` for
    /// names that are unknown or refer to a method.
    pub fn function_id(&self, name: &str) -> Option<FuncId> {
        self.function_by_name.get(name).copied()
    }

    /// Look up a struct by source name.
    pub fn struct_id(&self, name: &str) -> Option<StructId> {
        self.struct_by_name.get(name).copied()
    }

    /// Look up an enum by source name.
    pub fn enum_id(&self, name: &str) -> Option<EnumId> {
        self.enum_by_name.get(name).copied()
    }

    /// Look up a trait by source name.
    pub fn trait_id(&self, name: &str) -> Option<TraitId> {
        self.trait_by_name.get(name).copied()
    }

    /// Look up a *user-declared* method's [`FuncId`] by `(type_name,
    /// method_name)`.  Returns `None` for unknown pairs **or** for
    /// built-in stdlib methods (which carry no [`FuncId`]).  To look
    /// up a method's full info — user-declared *or* built-in — call
    /// [`Self::method_info_by_name`].
    ///
    /// Lookup is allocation-free: the nested map shape lets the inner
    /// `HashMap::get(&str)` borrow directly via the `Borrow` impl on
    /// `String`.
    pub fn method_func_id(&self, type_name: &str, method_name: &str) -> Option<FuncId> {
        self.method_index
            .get(type_name)
            .and_then(|methods| methods.get(method_name))
            .copied()
    }

    /// Convert a user-method [`FuncId`] to its index into
    /// [`Self::user_methods`].  Returns `None` when the id addresses a
    /// free function (i.e. `id.index() < user_method_offset`).
    #[inline]
    pub fn user_method_index(&self, id: FuncId) -> Option<usize> {
        id.index().checked_sub(self.user_method_offset as usize)
    }

    /// Resolve a free function by id.  Panics if `id` is out of range
    /// or addresses a user method (use [`Self::user_method`] instead).
    pub fn function(&self, id: FuncId) -> &FunctionInfo {
        assert!(
            id.index() < self.functions.len(),
            "FuncId({}) addresses a user method or is out of range; use user_method() instead",
            id.0
        );
        &self.functions[id.index()]
    }

    /// Resolve a user method by id.  Panics if `id` is out of range
    /// or addresses a free function.
    pub fn user_method(&self, id: FuncId) -> &MethodInfo {
        let idx = self.user_method_index(id).unwrap_or_else(|| {
            panic!(
                "FuncId({}) addresses a free function (id < user_method_offset {}); use function() instead",
                id.0, self.user_method_offset
            )
        });
        self.user_methods.get(idx).unwrap_or_else(|| {
            panic!(
                "FuncId({}) is past the end of user_methods (len {}, offset {})",
                id.0,
                self.user_methods.len(),
                self.user_method_offset
            )
        })
    }

    /// Resolve a struct by id.  Panics if `id` is out of range.
    pub fn struct_info(&self, id: StructId) -> &StructInfo {
        assert!(
            id.index() < self.structs.len(),
            "StructId({}) is out of range (len {})",
            id.0,
            self.structs.len()
        );
        &self.structs[id.index()]
    }

    /// Resolve an enum by id.  Panics if `id` is out of range.
    pub fn enum_info(&self, id: EnumId) -> &EnumInfo {
        assert!(
            id.index() < self.enums.len(),
            "EnumId({}) is out of range (len {})",
            id.0,
            self.enums.len()
        );
        &self.enums[id.index()]
    }

    /// Resolve a trait by id.  Panics if `id` is out of range.
    pub fn trait_info(&self, id: TraitId) -> &TraitInfo {
        assert!(
            id.index() < self.traits.len(),
            "TraitId({}) is out of range (len {})",
            id.0,
            self.traits.len()
        );
        &self.traits[id.index()]
    }

    /// Convenience: free-function name → info in one shot.
    pub fn function_info_by_name(&self, name: &str) -> Option<&FunctionInfo> {
        self.function_id(name).map(|id| self.function(id))
    }

    /// Convenience: struct name → info in one shot.
    pub fn struct_info_by_name(&self, name: &str) -> Option<&StructInfo> {
        self.struct_id(name).map(|id| self.struct_info(id))
    }

    /// Convenience: enum name → info in one shot.
    pub fn enum_info_by_name(&self, name: &str) -> Option<&EnumInfo> {
        self.enum_id(name).map(|id| self.enum_info(id))
    }

    /// Convenience: trait name → info in one shot.
    pub fn trait_info_by_name(&self, name: &str) -> Option<&TraitInfo> {
        self.trait_id(name).map(|id| self.trait_info(id))
    }

    /// Look up a method (user-declared *or* built-in) by
    /// `(type_name, method_name)`.  User methods are checked first
    /// (they shadow any same-name built-in entry).
    pub fn method_info_by_name(&self, type_name: &str, method_name: &str) -> Option<&MethodInfo> {
        if let Some(fid) = self.method_func_id(type_name, method_name) {
            return Some(self.user_method(fid));
        }
        self.builtin_methods
            .get(type_name)
            .and_then(|m| m.get(method_name))
    }

    /// `(trait_name, &TraitInfo)` pairs in [`TraitId`] order.
    /// Output order is deterministic: `self.traits.iter()` walks in
    /// id order, and the inverted `trait_by_name` lookup is purely
    /// for attaching names (its HashMap iteration order does not
    /// affect output).
    pub fn traits_with_names(&self) -> impl Iterator<Item = (&str, &TraitInfo)> {
        let names_by_id = self.invert_trait_names();
        self.traits
            .iter()
            .enumerate()
            .filter_map(move |(i, info)| names_by_id[i].map(|n| (n, info)))
    }

    /// `(name, FuncId, &FunctionInfo)` triples in [`FuncId`] order.
    /// Lets IR lowering register every free function from the
    /// resolved tables without re-walking the AST.
    pub fn functions_with_names(&self) -> impl Iterator<Item = (&str, FuncId, &FunctionInfo)> {
        let names_by_id = self.invert_function_names();
        self.functions
            .iter()
            .enumerate()
            .filter_map(move |(i, info)| names_by_id[i].map(|n| (n, FuncId(i as u32), info)))
    }

    /// `((type_name, method_name), FuncId, &MethodInfo)` triples for
    /// every user-declared method in [`FuncId`] order (i.e. parallel
    /// to [`Self::user_methods`]).  Lets IR lowering register every
    /// user method from the resolved tables without re-walking the
    /// AST.  Built-in methods are not included.
    pub fn user_methods_with_names(
        &self,
    ) -> impl Iterator<Item = ((&str, &str), FuncId, &MethodInfo)> {
        let names_by_idx = self.invert_user_method_names();
        let offset = self.user_method_offset;
        self.user_methods
            .iter()
            .enumerate()
            .filter_map(move |(i, info)| {
                names_by_idx[i].map(|(t, m)| ((t, m), FuncId(offset + i as u32), info))
            })
    }

    /// `(name, StructId, &StructInfo)` triples in [`StructId`] order.
    pub fn structs_with_names(&self) -> impl Iterator<Item = (&str, StructId, &StructInfo)> {
        let names_by_id = self.invert_struct_names();
        self.structs
            .iter()
            .enumerate()
            .filter_map(move |(i, info)| names_by_id[i].map(|n| (n, StructId(i as u32), info)))
    }

    /// `(name, EnumId, &EnumInfo)` triples in [`EnumId`] order
    /// (built-in `Option` and `Result` come first).
    pub fn enums_with_names(&self) -> impl Iterator<Item = (&str, EnumId, &EnumInfo)> {
        let names_by_id = self.invert_enum_names();
        self.enums
            .iter()
            .enumerate()
            .filter_map(move |(i, info)| names_by_id[i].map(|n| (n, EnumId(i as u32), info)))
    }

    fn invert_function_names(&self) -> Vec<Option<&str>> {
        invert_name_map(&self.function_by_name, self.functions.len(), |id| {
            id.index()
        })
    }

    fn invert_user_method_names(&self) -> Vec<Option<(&str, &str)>> {
        let offset = self.user_method_offset;
        let mut out: Vec<Option<(&str, &str)>> = vec![None; self.user_methods.len()];
        for (type_name, methods) in &self.method_index {
            for (method_name, id) in methods {
                let idx = (id.index())
                    .checked_sub(offset as usize)
                    .unwrap_or_else(|| {
                        panic!(
                            "FuncId({}) in method_index is below user_method_offset ({}); \
                         build_from_checker invariants violated",
                            id.0, offset
                        )
                    });
                assert!(
                    idx < out.len(),
                    "FuncId({}) in method_index past end of user_methods (len {}); \
                     build_from_checker invariants violated",
                    id.0,
                    out.len()
                );
                out[idx] = Some((type_name.as_str(), method_name.as_str()));
            }
        }
        out
    }

    fn invert_struct_names(&self) -> Vec<Option<&str>> {
        invert_name_map(&self.struct_by_name, self.structs.len(), |id| id.index())
    }

    fn invert_enum_names(&self) -> Vec<Option<&str>> {
        invert_name_map(&self.enum_by_name, self.enums.len(), |id| id.index())
    }

    fn invert_trait_names(&self) -> Vec<Option<&str>> {
        invert_name_map(&self.trait_by_name, self.traits.len(), |id| id.index())
    }
}

/// Invert a `name → id` map into a `Vec<Option<&str>>` indexed by id,
/// where each slot holds the name registered at that id (or `None` if
/// the slot was never written, which the post-build invariants in
/// [`build_from_checker`] forbid).  Shared helper for the
/// `*_with_names` iterators on [`ResolvedModule`].
fn invert_name_map<Id: Copy>(
    map: &HashMap<String, Id>,
    len: usize,
    index_of: impl Fn(Id) -> usize,
) -> Vec<Option<&str>> {
    let mut out: Vec<Option<&str>> = vec![None; len];
    for (name, id) in map {
        let idx = index_of(*id);
        debug_assert!(
            idx < out.len(),
            "name-map id out of range; build_from_checker invariants violated"
        );
        out[idx] = Some(name.as_str());
    }
    out
}

/// Construct an [`Analysis`] by consuming a [`Checker`] that has
/// finished its checking pass.  Moves rather than clones — the
/// `Checker` is dropped after this call.
///
/// `program` drives id-table construction order:
///
/// - **Functions:** flattened from `checker.functions` into a Vec
///   indexed by the [`FuncId`]s pre-allocated during pre-pass A.
///   AST-declaration order: walk `program.declarations`, take each
///   `Declaration::Function` in order, place at its pre-allocated id.
/// - **User methods:** flattened from `checker.methods` into a Vec
///   indexed by `[func_id - user_method_offset]`.  AST-declaration
///   order matches pre-pass B's allocation order (struct/enum inline
///   methods at the type's declaration site, inherent first then
///   trait impls in source order; standalone `impl` blocks at their
///   own declaration site).
/// - **Built-in methods** (entries in `checker.methods` whose
///   `MethodInfo::func_id` is `None`) are partitioned out into
///   [`ResolvedModule::builtin_methods`] for inline-codegen lookup.
/// - **Structs / enums / traits** are flattened in the order
///   sema's registration pass walked them: built-in `Option` then
///   `Result` first for enums, then user-declared types in AST
///   order; structs and traits in AST order.
/// - **Per-span maps and auxiliary outputs** transfer ownership
///   directly without traversal.
///
/// **Dedup contract.**  Sema rejects duplicate function, struct,
/// enum, trait, and `(type, method)` declarations at registration
/// time (each `register_*` checks `contains_key` before inserting
/// and emits a diagnostic on collision).  This guarantees every
/// `*_by_name` lookup table is 1:1 with its underlying `Vec`,
/// which the `assert!` block at the end of this function pins (in
/// release as well as debug, because IR lowering and the Cranelift
/// backend index these Vecs without bounds checks and a structural
/// mismatch would surface far downstream as an opaque panic).  If
/// a future error-recovery mode allows sema to continue past
/// duplicate declarations and overwrite existing entries, the
/// dedup-via-`!contains_key` checks in the `Declaration::*`
/// matches below would silently keep the first entry while the
/// last entry remained in `checker.{functions, structs, …}` —
/// re-evaluate this function's behavior under that scenario.
pub(crate) fn build_from_checker(program: &Program, mut checker: Checker) -> Analysis {
    let mut rm = empty_resolved_module();

    build_functions(&mut rm, &mut checker);
    build_user_and_builtin_methods(&mut rm, &mut checker);
    build_enums(&mut rm, &mut checker, program);
    build_structs(&mut rm, &mut checker, program);
    build_traits(&mut rm, &mut checker, program);

    // ── Per-span maps onto ResolvedModule: move, don't clone.
    rm.expr_types = std::mem::take(&mut checker.expr_types);
    rm.call_type_args = std::mem::take(&mut checker.call_type_args);
    rm.var_annotation_types = std::mem::take(&mut checker.var_annotation_types);
    rm.lambda_captures = std::mem::take(&mut checker.lambda_captures);

    assert_post_build_invariants(&rm);

    Analysis {
        module: rm,
        diagnostics: std::mem::take(&mut checker.diagnostics),
        endpoints: std::mem::take(&mut checker.endpoints),
        symbol_references: std::mem::take(&mut checker.symbol_references),
        trait_impls: std::mem::take(&mut checker.trait_impls),
        type_aliases: std::mem::take(&mut checker.type_aliases),
    }
}

/// Construct an empty [`ResolvedModule`].  All `Vec`s and `HashMap`s
/// start empty; offsets are zero.  `build_from_checker` populates
/// every field before the module is released.
fn empty_resolved_module() -> ResolvedModule {
    ResolvedModule {
        functions: Vec::new(),
        function_by_name: HashMap::new(),
        user_methods: Vec::new(),
        user_method_offset: 0,
        method_index: HashMap::new(),
        builtin_methods: HashMap::new(),
        structs: Vec::new(),
        struct_by_name: HashMap::new(),
        enums: Vec::new(),
        enum_by_name: HashMap::new(),
        traits: Vec::new(),
        trait_by_name: HashMap::new(),
        expr_types: HashMap::new(),
        call_type_args: HashMap::new(),
        var_annotation_types: HashMap::new(),
        lambda_captures: HashMap::new(),
    }
}

/// Place each registered free function into the slot pre-allocated
/// for it during pre-pass A.  Builds a `Vec<Option<FunctionInfo>>`
/// first and `unwrap`s at the end, so there's never a sentinel slot
/// in the released `Vec<FunctionInfo>` — an unfilled id panics with
/// a clear message at construction time.
fn build_functions(rm: &mut ResolvedModule, checker: &mut Checker) {
    let n_functions = checker.pending_function_ids.len();
    let mut functions_buf: Vec<Option<FunctionInfo>> = Vec::with_capacity(n_functions);
    functions_buf.resize_with(n_functions, || None);
    for (name, info) in checker.functions.drain() {
        let id = info.func_id;
        assert!(
            id.index() < n_functions,
            "FuncId({}) out of range (n_functions={n_functions})",
            id.0
        );
        let slot = &mut functions_buf[id.index()];
        assert!(
            slot.is_none(),
            "FuncId({}) populated twice — sema registered two functions with the same id",
            id.0
        );
        *slot = Some(info);
        rm.function_by_name.insert(name, id);
    }
    rm.functions = functions_buf
        .into_iter()
        .enumerate()
        .map(|(i, slot)| {
            slot.unwrap_or_else(|| {
                panic!(
                    "FuncId({i}) was pre-allocated by pre-pass A but never registered \
                     — registration walk order disagrees with id allocation walk order"
                )
            })
        })
        .collect();
}

/// Partition `checker.methods` into user methods (with `func_id`)
/// and built-in methods (without).  User methods land in
/// [`ResolvedModule::user_methods`] at their pre-allocated slots and
/// in [`ResolvedModule::method_index`]; built-ins land in
/// [`ResolvedModule::builtin_methods`].
fn build_user_and_builtin_methods(rm: &mut ResolvedModule, checker: &mut Checker) {
    rm.user_method_offset = checker.user_method_offset;
    let n_user_methods = checker.pending_user_method_ids.len();
    let mut user_methods_buf: Vec<Option<MethodInfo>> = Vec::with_capacity(n_user_methods);
    user_methods_buf.resize_with(n_user_methods, || None);
    let user_method_offset = rm.user_method_offset;
    for (type_name, type_methods) in checker.methods.drain() {
        for (method_name, info) in type_methods {
            match info.func_id {
                Some(fid) => {
                    let idx = fid
                        .index()
                        .checked_sub(user_method_offset as usize)
                        .unwrap_or_else(|| {
                            panic!(
                                "user-method FuncId({}) is below user_method_offset {}",
                                fid.0, user_method_offset
                            )
                        });
                    assert!(
                        idx < n_user_methods,
                        "user-method FuncId({}) out of range (n_user_methods={n_user_methods})",
                        fid.0
                    );
                    let slot = &mut user_methods_buf[idx];
                    assert!(
                        slot.is_none(),
                        "user-method FuncId({}) populated twice — sema registered two methods with the same id",
                        fid.0
                    );
                    *slot = Some(info);
                    rm.method_index
                        .entry(type_name.clone())
                        .or_default()
                        .insert(method_name, fid);
                }
                None => {
                    rm.builtin_methods
                        .entry(type_name.clone())
                        .or_default()
                        .insert(method_name, info);
                }
            }
        }
    }
    rm.user_methods = user_methods_buf
        .into_iter()
        .enumerate()
        .map(|(i, slot)| {
            slot.unwrap_or_else(|| {
                panic!(
                    "user-method slot {i} (FuncId({})) was pre-allocated but never registered \
                     — registration walk order disagrees with id allocation walk order",
                    user_method_offset as usize + i
                )
            })
        })
        .collect();
}

/// Built-in `Option` then `Result` first (pinned to
/// [`OPTION_ENUM_ID`] / [`RESULT_ENUM_ID`]), then user enums in AST
/// order.
fn build_enums(rm: &mut ResolvedModule, checker: &mut Checker, program: &Program) {
    for (builtin, expected_id) in [("Option", OPTION_ENUM_ID), ("Result", RESULT_ENUM_ID)] {
        if let Some(info) = checker.enums.remove(builtin) {
            let id = next_enum_id(rm.enums.len());
            assert_eq!(
                id, expected_id,
                "built-in enum `{builtin}` placed at unexpected id (expected {expected_id}, got {id})"
            );
            rm.enum_by_name.insert(builtin.to_string(), id);
            rm.enums.push(info);
        }
    }
    debug_assert_eq!(
        rm.enums.len(),
        FIRST_USER_ENUM_ID.index(),
        "built-in enum prefix length disagrees with FIRST_USER_ENUM_ID"
    );
    for decl in &program.declarations {
        if let Declaration::Enum(e) = decl
            && !rm.enum_by_name.contains_key(&e.name)
            && let Some(info) = checker.enums.remove(&e.name)
        {
            let id = next_enum_id(rm.enums.len());
            rm.enum_by_name.insert(e.name.clone(), id);
            rm.enums.push(info);
        }
    }
}

/// User-declared structs in AST order.
fn build_structs(rm: &mut ResolvedModule, checker: &mut Checker, program: &Program) {
    for decl in &program.declarations {
        if let Declaration::Struct(s) = decl
            && !rm.struct_by_name.contains_key(&s.name)
            && let Some(info) = checker.structs.remove(&s.name)
        {
            let id = next_struct_id(rm.structs.len());
            rm.struct_by_name.insert(s.name.clone(), id);
            rm.structs.push(info);
        }
    }
}

/// User-declared traits in AST order.
fn build_traits(rm: &mut ResolvedModule, checker: &mut Checker, program: &Program) {
    for decl in &program.declarations {
        if let Declaration::Trait(t) = decl
            && !rm.trait_by_name.contains_key(&t.name)
            && let Some(info) = checker.traits.remove(&t.name)
        {
            let id = next_trait_id(rm.traits.len());
            rm.trait_by_name.insert(t.name.clone(), id);
            rm.traits.push(info);
        }
    }
}

/// Verify the post-build invariants documented on [`ResolvedModule`].
///
/// Length-agreement assertions run in release as well as debug
/// because IR lowering and the Cranelift backend index these `Vec`s
/// by id without bounds checks; a length mismatch would surface far
/// downstream as an opaque panic.  Sema rejects duplicate
/// declarations at registration time, so these should always hold;
/// if a future error-recovery mode allows duplicates through, this
/// is the gate that catches the regression.  (Unfilled-slot
/// detection is enforced upstream by the `Option::unwrap` collects
/// in [`build_functions`] / [`build_user_and_builtin_methods`].)
fn assert_post_build_invariants(rm: &ResolvedModule) {
    assert_eq!(
        rm.functions.len(),
        rm.function_by_name.len(),
        "functions/function_by_name length mismatch"
    );
    assert_eq!(
        rm.structs.len(),
        rm.struct_by_name.len(),
        "structs/struct_by_name length mismatch"
    );
    assert_eq!(
        rm.enums.len(),
        rm.enum_by_name.len(),
        "enums/enum_by_name length mismatch"
    );
    assert_eq!(
        rm.traits.len(),
        rm.trait_by_name.len(),
        "traits/trait_by_name length mismatch"
    );
    let method_index_count: usize = rm.method_index.values().map(|m| m.len()).sum();
    assert_eq!(
        rm.user_methods.len(),
        method_index_count,
        "user_methods/method_index length mismatch"
    );
    assert_eq!(
        rm.user_method_offset as usize,
        rm.functions.len(),
        "user_method_offset must equal functions.len()"
    );

    // call_type_args invariant (per the field's documented contract):
    // no entry contains Type::Error.  Run only in debug because the
    // payload is unbounded and the cost is O(N·M); a release build
    // that violates the invariant would surface as a panic during
    // monomorphization.
    debug_assert!(
        rm.call_type_args
            .values()
            .flat_map(|v| v.iter())
            .all(|t| !matches!(t, Type::Error)),
        "call_type_args contains Type::Error — would break monomorphization"
    );
}

/// Allocate the next [`EnumId`] for a freshly-pushed entry, panicking
/// with a clear message if the program declares more enums than fit
/// in a `u32`.  The cast can only truncate on a `usize` of more than
/// 32 bits when `len > u32::MAX`, which would itself indicate a
/// degenerate program.
fn next_enum_id(len: usize) -> EnumId {
    EnumId(u32::try_from(len).expect("more than u32::MAX enums in program"))
}

/// Allocate the next [`StructId`].  See [`next_enum_id`] for the
/// `usize → u32` overflow guard rationale.
fn next_struct_id(len: usize) -> StructId {
    StructId(u32::try_from(len).expect("more than u32::MAX structs in program"))
}

/// Allocate the next [`TraitId`].  See [`next_enum_id`] for the
/// `usize → u32` overflow guard rationale.
fn next_trait_id(len: usize) -> TraitId {
    TraitId(u32::try_from(len).expect("more than u32::MAX traits in program"))
}
