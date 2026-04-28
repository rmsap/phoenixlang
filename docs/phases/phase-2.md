# Phase 2: Compilation

**Status: 2.2 complete; 2.6 active**

Move from interpretation to native code generation. This is what makes Phoenix a real language rather than a scripting tool.

## 2.1 Intermediate Representation (IR)

**Status: Complete.** The `phoenix-ir` crate implements an SSA-style IR with basic blocks, typed instructions, and explicit control flow. The lowering pass converts the type-checked AST into IR for all major language features (arithmetic, control flow, structs, enums, match, closures, method calls, collections, try operator, string interpolation). Use `phoenix ir <file.phx>` to inspect the output. The `phoenix-ir-interp` crate provides an IR interpreter for round-trip verification — use `phoenix run-ir <file.phx>` to execute via the IR and compare output with `phoenix run`. Round-trip tests cover all lowered features including the try operator; see `crates/phoenix-ir-interp/tests/` for the full suite.

- Lower the type-checked AST to a flat, SSA-style IR
- Basic blocks, typed instructions, explicit control flow
- This decouples semantic analysis from code generation
- Makes it possible to target multiple backends (native, WASM)

## 2.2 Native Compilation (Cranelift)

**Status: ✅ Complete (2026-04-27).** The `phoenix-cranelift` crate translates Phoenix IR to Cranelift IR and produces native executables via `cranelift-object` + system linker. The `phoenix-runtime` crate provides a small static library linked into every compiled binary. Use `phoenix build <file.phx>` to compile. Supported features:

- Value types (Int, Float, Bool), strings, structs (including String fields)
- Enums (including String variant fields), pattern matching
- `for x in list` iteration over `List<T>` collections
- Closures (including String captures), direct and indirect function calls
- All string methods (including `split`)
- `List<T>` with all functional methods (map, filter, reduce, find, any, all, flatMap, sortBy, first, last, contains, take, drop). Note: `sortBy` uses O(n^2) insertion sort — acceptable for small lists, merge sort planned.
- `Map<K, V>` with all methods (get, set, contains, remove, keys, values, length)
- `Option<T>` with combinators (unwrap, unwrapOr, isSome, isNone, map, andThen, orElse, filter, okOr, unwrapOrElse).
- `Result<T, E>` with combinators (unwrap, unwrapOr, isOk, isErr, map, andThen, orElse, mapErr, unwrapOrElse)

All memory is currently leaked (no GC); compiled binaries are not suitable for long-running processes. Next step: garbage collection, then WebAssembly target.

- Translate Phoenix IR to Cranelift IR
- Produce native executables via `cranelift-object` + system linker
- Start with debug builds only (no optimization)
- Keep the interpreter available as a fast-feedback mode (`phoenix run` = interpret, `phoenix build` = compile)
- **Why Cranelift over LLVM:** pure Rust dependency, fast compile times, built-in WASM support. Add LLVM as an optional optimizing backend later.

### Design decisions locked in this phase

These decisions pin the ABI / calling convention and must land before 2.2 wraps — retrofitting after user code ships is strictly worse. See [design-decisions.md](../design-decisions.md):

- **[Generic function monomorphization](../design-decisions.md#generic-function-monomorphization-strategy)** — user generics get one specialized copy per concrete instantiation. **✅ Implemented 2026-04-20** as `monomorphize` pass in `phoenix-ir/src/monomorphize.rs` (BFS worklist; symbol-safe specialization names `orig__i64__str`; templates kept as inert stubs; covers generic methods on user-defined types). Concrete type args are embedded directly in `Op::Call` so the IR is self-describing.
- **[Dynamic dispatch via `dyn Trait`](../design-decisions.md#dynamic-dispatch-via-dyn-trait)** — vtable ABI `(data_ptr, vtable_ptr)`; static dispatch stays the default. **✅ Implemented 2026-04-20** (MVP scope: function params, returns, `let` annotations, struct fields, single-trait-bound). `IrType::DynRef`, `Op::DynAlloc`/`Op::DynCall(slot_idx)`, rodata vtables per `(concrete_type, trait_name)` pair (`phoenix-cranelift/src/translate/dyn_trait.rs`). Object-safety gated at trait-declaration time (see design-decisions.md for the full rule list); non-object-safe traits remain usable as generic bounds (`<T: Trait>`). IR invariants are enforced by the verifier. Heterogeneous list literals ([see known-issues.md](../known-issues.md#listdyn-trait-literal-initialization-in-compiled-mode)) are deferred beyond 2.2.
- **[Centralized `Layout` trait](../design-decisions.md#centralized-layout-for-reference-types)** — single source of truth for reference-type slot count, alignment, load/store codegen. **✅ Implemented 2026-04-19** as `TypeLayout` in `phoenix-cranelift/src/translate/layout/`.
- **[Numeric error semantics](../design-decisions.md#numeric-error-semantics-division-overflow-integer-edge-cases)** — Int operators panic on overflow / divide-by-zero / `i64::MIN` negation (ratifies current behavior); Float follows IEEE 754. Stdlib `Int.checked*` family lands in Phase 4.1.
- **[Post-sema ownership: `ResolvedModule`](../design-decisions.md#post-sema-ownership-resolvedmodule-as-the-semair-handoff)** — **✅ Implemented 2026-04-24.** Sema returns `Analysis` from `check()`, which wraps a `ResolvedModule` (the IR-facing schema: callables, types, per-span maps) plus auxiliary outputs (diagnostics, endpoints, symbol_references, trait_impls, type_aliases). IR lowering and the IR interpreter take `&ResolvedModule`; codegen / LSP / driver / bench take `&Analysis`. `Vec<FunctionInfo>` (free functions, FuncId `0..N`) and `Vec<MethodInfo>` (user methods, FuncId `N..N+M`) sit alongside `Vec<StructInfo>`/`Vec<EnumInfo>`/`Vec<TraitInfo>` and `*_by_name` lookup tables; built-in stdlib methods live in `builtin_methods` (no `FuncId`, since Cranelift inlines them). Stable ids (`FuncId`/`StructId`/`EnumId`/`TraitId`) live in `phoenix_common::ids` and are allocated by `Checker`'s registration pass. **IR lowering does not re-walk the AST to register declarations** — `register_declarations` iterates `resolved.functions_with_names()` and `resolved.user_methods_with_names()` directly, so `IrModule.functions[id.index()]` agrees with the sema entry at the same id by construction (no walk-order contract to maintain). `IrModule.user_method_offset` and `IrModule.synthesized_start` mirror sema's boundaries so consumers can distinguish user-declared from synthesized callables. The Phase 2.6 follow-up that originally would have come later (FuncId unification of user methods into the function space) landed in the same diff. Sema↔IR id alignment is now pinned by `crates/phoenix-ir/tests/sema_ir_id_alignment.rs`, which compares both id spaces post-lowering. Full test suite passes (2,573 tests, plus 6 new id-alignment / stability tests).

### Bugs closed in this phase

- **Generic user-defined structs in compiled mode** — **✅ Fixed 2026-04-21.** `struct Container<T>` now compiles end-to-end under `phoenix build`, with full method support and correct `dyn Trait` interaction. Fix landed as a second-stage monomorphization pass in `phoenix-ir/src/monomorphize.rs::monomorphize_structs`: every `StructRef(name, non_empty_args)` in a concrete function is rewritten to `StructRef(mangled_name, [])` where `mangled_name = "Container__i64"` (shared grammar with generic-function mangling), specialized struct layouts are registered under mangled names, methods on generic structs are cloned and specialized alongside (with type-var substitution throughout the body), and `Op::DynAlloc` concrete-type strings plus `dyn_vtables` keys are rekeyed in the same pass so `Container<Int>: Trait` vs. `Container<String>: Trait` don't collide. Fixed-point worklist handles recursive generics (`Node<T>`). Removes the `register_method` struct-side panic; enum-side gate is untouched (separate known-issues entry, Phase 4 target).
- **`<T: Trait>` method calls in compiled mode** — **✅ Fixed 2026-04-21.** `function show<T: Display>(x: T) { x.toString() }` compiles and runs under `phoenix build`; previously it failed with `builtin '.method' not yet supported`. IR lowering emits `Op::BuiltinCall(".method", [recv, ...])` with an empty type-name prefix for trait-bound method calls on TypeVar receivers; a new `resolve_trait_bound_builtin_calls` helper in function-monomorphization's body-cloning step rewrites the marker to a direct `Op::Call` using `method_index[(substituted_type, method)]`. Cooperates with struct-monomorphization's `rewrite_method_calls` when the receiver is a generic struct — function-mono lands a template FuncId, struct-mono promotes it to the mangled specialization.
- **`<T: Trait>` → `dyn Trait` coercion in compiled mode** — **✅ Fixed 2026-04-24.** `function f<T: Drawable>(x: T) { let d: dyn Drawable = x }` now compiles; previously it tripped an `unreachable!` in `coerce_to_expected`. Same shape as the method-call fix: IR lowering emits an `Op::UnresolvedDynAlloc` placeholder; monomorphization's Pass B substitutes and rewrites to a concrete `Op::DynAlloc`. See `phoenix-ir/src/monomorphize/function_mono.rs::resolve_unresolved_dyn_allocs`.
- **Default argument values in compiled mode** — **✅ Fixed 2026-04-24.** `function f(x: Int = 1)` with a call `f()` now runs under `phoenix build`; previously IR lowering trapped on unfilled positional slots. See [design-decisions.md: *Default-argument lowering strategy*](../design-decisions.md#default-argument-lowering-strategy) for the caller-site materialization decision and its tradeoffs.
- **Default arguments on method calls** — **✅ Fixed 2026-04-24.** `impl Counter { function bump(self, by: Int = 1) -> Int { ... } }` with a call `c.bump()` now compiles and runs under all three backends; previously sema rejected the arity mismatch before lowering even saw the call. Same caller-site materialization rule as the free-function fix, extended to `lower_method_call`'s inherent-impl branch — both merge paths now route through a shared `assemble_call_args` core in `phoenix-ir/src/lower_expr.rs` (free-function and method wrappers differ only in lookup source and named-arg handling). `MethodInfo.default_param_exprs` is populated by `register_impl`, and pass-1 default validation is unified across free functions and methods via a shared `Checker::check_param_defaults` helper. Trait-method defaults remain out of scope — see [known-issues.md](../known-issues.md) for the Phase 3 follow-up. Method-arg coercion (the pre-existing gap where `lower_method_call` skips `coerce_call_args`) is also out of scope; this fix deliberately does not add coercion to the method branch.

### Exit criteria for declaring Phase 2.2 complete

These are the bars that have to clear before Phase 2.2 is closed.  An item with an unchecked box is a real outstanding follow-up, not a stylistic note — every checked-off item below has been verified against the codebase.

- [x] **All Phase-2.2 design decisions implemented.** Every `**[…]**` bullet in §2.2 above carries a `✅ Implemented YYYY-MM-DD` marker (monomorphization, `dyn Trait`, centralized layout, numeric semantics, `ResolvedModule`).
- [x] **All Phase-2.2 bug-closure entries have regression tests.** Each of the five entries under "Bugs closed in this phase" maps to a topic-specific test file under `crates/phoenix-cranelift/tests/` (e.g. `compile_generic_structs.rs`, `compile_trait_bounds.rs`, `compile_default_args.rs`).  Reverting the fix would fail those suites.
- [x] **No `known-issues.md` entry is targeted at Phase 2.2.** Outstanding issues are explicitly re-targeted to Phase 3 / 4 or carry no phase tag at all (i.e., they describe pre-existing gaps not committed to 2.2's scope).
- [x] **Workspace test/clippy/fmt clean on the 2.2 branch.** `cargo test --workspace` green (2,617 tests, up from 2,579 at the start of the close-out: +9 verifier negative tests, +11 backend-matrix tests, +18 from cargo doctest accounting and ambient additions); `cargo clippy --workspace --tests` zero warnings; `cargo fmt --all -- --check` clean.
- [x] **Three-backend roundtrip matrix on every `tests/fixtures/*.phx`.** **✅ Completed 2026-04-27.** `crates/phoenix-driver/tests/three_backend_matrix.rs` walks every runnable `tests/fixtures/*.phx` (11 fixtures: the original `hello`/`fibonacci`/`fizzbuzz`/`features` plus 7 added to cover the surface that landed in 2.2 — `generics`, `traits_static`, `traits_dyn`, `collections`, `option_result`, `defaults`, `closures`), runs each under `phoenix run`, `phoenix run-ir`, and `phoenix build` + execute, and asserts byte-identical stdout. `gen_*.phx` fixtures are excluded — they exist as inputs to `phoenix gen` and aren't worth exercising through the matrix (they are valid Phoenix programs, just not the surface this gate is for). One `#[test]` per fixture so a divergence names the offending fixture in `cargo test` output. Building the matrix surfaced one pre-existing parser bug ([interpolated-expression spans collide across functions](../known-issues.md#interpolated-expression-spans-are-zero-based-colliding-across-functions)); the affected pattern is excluded from the fixtures with a comment pointing at the issue.
- [x] **No `unreachable!()` reachable from a well-typed program in the Cranelift backend.** **✅ Completed 2026-04-27.** All 16 `unreachable!()` sites under `crates/phoenix-cranelift/src/translate/` (arith.rs ×4, calls.rs ×4, data.rs ×3, dyn_trait.rs, layout/type_layout.rs, list_methods.rs, map_methods.rs, mutable.rs) are replaced with `ice!(...)` invocations carrying a `"internal compiler error in cranelift backend: <dispatcher> <expected>"` prefix. The `ice!` macro lives at the crate root in `crates/phoenix-cranelift/src/lib.rs` and is in scope crate-wide via textual ordering. Every remaining panic is a compiler-bug indicator, not a user-error path — user-error paths return `CompileError` (see `calls.rs:118` for the existing pattern).
  - **Soundness of the "compiler-bug" claim:** holds unconditionally for the dispatchers that match on `Op` *category* (e.g. `translate_int_arith`, `translate_float_arith`, `translate_string`, `translate_struct`, `translate_enum`) — the outer router has already classified the op, so the wildcard arm is structurally unreachable.
  - **Maintenance hazard** for the inner-match arms in `translate_string_method` (`calls.rs:213`, `calls.rs:224`): the outer `matches!(method, "trim" | "toLowerCase" | …)` guard and the inner `match method` arm list must stay in lockstep. A rename in one place and not the other turns the inner `ice!` reachable from a well-typed program. If/when more methods are added, prefer a single match that returns the runtime function pointer directly over the current outer-guard / inner-match pair.
- [x] **`phoenix-ir` verifier has one negative test per invariant.** **✅ Completed 2026-04-27.** `crates/phoenix-ir/src/verify.rs::structural_verifier_tests` adds 9 hand-written-IR negative tests covering the structural invariants checked by `verify_function` and `verify_value_types_index`: missing terminator (`Terminator::None`), terminator references invalid block target, terminator-arg-count mismatch, instruction operand uses `VOID_SENTINEL`, instruction operand is undefined, terminator operand uses `VOID_SENTINEL`, terminator operand is undefined, value_types index out of sync (via a `#[cfg(test)]`-only `IrFunction::debug_desync_value_types` helper, since the desync invariant is structurally unbreakable from the public API), plus a positive sanity test. Combined with the existing `unresolved_placeholder_op_tests` (4 tests) and `dyn_ops::dyn_verifier_tests` (7 tests), every verifier invariant now has dedicated coverage.

When every box above is ticked, Phase 2.2 closes and Phase 2.6 (modules and visibility) becomes the active phase. Phase 2.3 (GC + runtime), 2.4 (WebAssembly target), and 2.5 (JavaScript interop) remain on the roadmap but are sequenced after the module system: the language-level scaffolding for cross-file code organization is a prerequisite for the package manager (3.1) and LSP (3.2), and `Value::Closure → IR blocks` (one of the 2.6-bundled refactors) settles closure representation in a way that simplifies later runtime work. Compiled binaries continue to leak heap allocations until 2.3 lands — see [Memory leaks (no GC yet)](../known-issues.md#memory-leaks-no-gc-yet).

## 2.3 Runtime and Memory Management

**Status: queued (active phase is 2.6).**

A minimal runtime already exists as the [`phoenix-runtime`](../../crates/phoenix-runtime/) crate (static library linked into compiled binaries). It currently provides `print` (all value types + strings), `toString`, string comparison and concatenation, all string methods, heap allocation (`phx_alloc` via `malloc`, no GC), panic/abort, `List<T>` data structures (alloc, get, push, contains, take, drop), `String.split` (returns `List<String>`), and `Map<K, V>` data structures (alloc, get, set, remove, contains, keys, values). This section covers extending it into a full runtime.

- Garbage collector — **tracing GC, mark-and-sweep baseline** (decided 2026-04-19; see [GC strategy](../design-decisions.md#gc-strategy)). Leave room to evolve to generational later without ABI changes. Implied commitment: `defer` / `using` / `with` syntax becomes required since tracing GC has no deterministic-destruction story for resource cleanup (see [`defer` for resource cleanup](../design-decisions.md#defer-for-resource-cleanup), still open on syntax).
- String implementation (UTF-8, immutable by default) — basic ops already in `phoenix-runtime`
- Panic/abort handler — already in `phoenix-runtime`
- Built-in function implementations (`print`, `toString`) — already in `phoenix-runtime`
- Collection runtime support (List, Map data structures with dynamic resizing) — **basic implementation complete** (`list_methods.rs`, `map_methods.rs`); map lookup is currently O(n) linear scan — hash-based implementation planned
- Builtin method implementations (String.*, List.*, Map.*, Option.*, Result.*) — **complete** in compiled mode; closure-based list methods (map, filter, reduce, etc.) are compiled inline as Cranelift loops (`list_methods_closure.rs` for single-loop methods — map, filter, find, any, all, reduce; `list_methods_complex.rs` for nested-loop methods — flatMap, sortBy)

### Bugs to be closed in this phase

See [known-issues.md](../known-issues.md):

- **[O(n) map key lookup](../known-issues.md#on-map-key-lookup)** — replace the flat-array linear scan with a hash-based implementation.
- **[O(n²) `List.sortBy` insertion sort](../known-issues.md#on²-listsortby-insertion-sort)** — replace with merge sort. Both backends currently share the O(n²) algorithm; the fix lands in the runtime and the Cranelift inline codegen together.

## 2.4 WebAssembly Target

- Add WASM output via Cranelift's `wasm32` support
- Slim runtime for the browser
- Bridge to browser APIs via imports (DOM manipulation, fetch, etc.)
- Shared types between backend and frontend targets
- **Target the WASM GC proposal** (standardized, shipping in all major browsers). The [tracing GC decision](../design-decisions.md#gc-strategy) was made in part to align with WASM GC — Phoenix's object model maps onto WASM GC's struct/reference types cleanly, so the browser VM does the collection and binaries stay small. Linear-memory WASM remains a fallback option for runtimes without WASM GC support.

## 2.5 JavaScript Interop

- Import and call JavaScript/npm packages from Phoenix frontend code compiled to WASM
- `extern js` declarations for typing JS functions and objects without Phoenix implementations
- Automatic marshalling of Phoenix types to/from JS values across the WASM boundary
- Access to the full npm ecosystem: UI libraries, utility packages, browser APIs
- `phoenix.toml` supports `[js-dependencies]` for declaring npm packages

```phoenix
// Declare external JS functions available at runtime
extern js {
  function alert(message: String)
  function setTimeout(callback: (Void) -> Void, ms: Int)
}

// Import an npm module (resolved at build time)
import js "lodash" { function debounce(f: (Void) -> Void, ms: Int) -> (Void) -> Void }

async function main() {
  let greet: (Void) -> Void = debounce(function() { alert("Hello from Phoenix!") }, 300)
  greet()
}
```

- **Why:** Phoenix targets full-stack web development. The frontend ecosystem is dominated by JavaScript — ignoring npm would force developers to rewrite existing solutions. Interop lets Phoenix leverage the JS ecosystem while providing a better authoring experience.
- **Complexity:** High — requires a JS glue code generator, type marshalling layer, and integration with a JS bundler (e.g. esbuild or Vite) for npm resolution. The WASM component model (or wasm-bindgen-style approach) provides a proven foundation.
- **Depends on:** WebAssembly target (2.4), Package manager (3.1)

## 2.6 Module System and Visibility

**Status: active (since 2026-04-27, when 2.2 closed).** Sequenced ahead of 2.3–2.5 because the language-level scaffolding for cross-file code organization unblocks the package manager (3.1) and LSP (3.2), and the bundled `Value::Closure → IR blocks` refactor stabilizes closure representation in a way that simplifies later runtime work.

Phoenix needs a module system before packages (Phase 3.1) can work. This is the language-level mechanism for organizing code across multiple files, controlling what is exposed, and importing declarations from other modules.

### Modules

Each `.phx` file is a module. The module name is derived from the file path relative to the project root. Directories can contain a `mod.phx` file to define a parent module.

```
src/
  main.phx           // root module
  models/
    mod.phx           // models module
    user.phx          // models.user module
    post.phx          // models.post module
  handlers/
    mod.phx           // handlers module
    auth.phx          // handlers.auth module
```

### Imports

```phoenix
// Import specific items from a module
import models.user { User, createUser }

// Import everything from a module
import models.user { * }

// Import with alias
import models.user { User as UserModel }
```

### Visibility

All declarations are **private by default**. Use the `public` keyword to export from a module:

```phoenix
// models/user.phx

// Visible to other modules
public struct User {
    public String name       // field accessible from outside
    public String email
    Int passwordHash        // private — only accessible within this module
}

public function createUser(name: String, email: String) -> User {
    User(name, email, hash(""))
}

// Not visible to other modules
function hash(input: String) -> String {
    // internal implementation
}
```

**Visibility rules:**

- `public struct` — the struct type is visible to importers
- `public` on a struct field — the field is readable (and writable if `mut`) from outside the module
- Private fields can be set via the constructor but cannot be accessed by name from outside
- `public function` — callable from outside the module
- `public enum` — the enum and all its variants are visible
- `public trait` — the trait is visible and implementable from outside
- `public` on an inline method (in a `struct` / `enum` body or inherent `impl` block) — the method is callable from outside the module. Default is private; a public method on a private type is a sema error. Methods inside `impl Trait for Type` blocks take their visibility from the trait. See [design-decisions.md: *Per-method `public` / private on inline struct/enum methods*](../design-decisions.md#per-method-public--private-on-inline-structenum-methods).
- Functions, structs, enums, traits, and methods without `public` are module-private

### Design principles

- **Private by default:** Forces authors to think about their public API. Anything not marked `public` is an implementation detail that can change freely.
- **No `protected` or `internal`:** Two levels (public/private) keep the system simple. If a more granular system is needed later, it can be added without breaking existing code.
- **Struct fields and methods have independent visibility:** A struct can be `public` (importable) while some fields and methods are private (encapsulated). This supports the common pattern of exposing a type while hiding its internals. The inverse — a `public` method (or field) on a private type — is a sema error, since the receiver cannot be named from outside.

- **Why before packages:** The package manager (3.1) needs modules to exist. You cannot have cross-package imports without intra-project imports. Module resolution is also needed by the LSP (3.2) for go-to-definition and auto-imports.
- **Complexity:** High — requires a module resolver (file system → module tree), import resolution, visibility checking across module boundaries, and changes to name resolution in the semantic checker. The two-pass registration design already handles forward references within a file; extending it to cross-file references adds significant complexity.
- **Depends on:** Semantic analysis (Phase 1, complete)

### Refactors bundled into this phase

Three codebase-hygiene refactors land alongside the module-system work. The diagnostic-builder pattern is paid for by the module-system scope itself (multi-file diagnostics) and must be complete before Phase 3.2 (LSP). The remaining two are IR-shape refactors on the `IrFunction` / `IrModule` surface — bundling them amortizes the disruption rather than ripping the IR open twice. See [design-decisions.md](../design-decisions.md) and [known-issues.md](../known-issues.md).

#### Scope change: `Value::Closure` refactor dropped (2026-04-27)

A fourth refactor originally listed here — *Interpreter-parser coupling via `Value::Closure`* — was dropped from this batch. `phoenix-interp` is meant to remain a fast AST tree-walking interpreter for debugging (`phoenix run`), kept deliberately separate from `phoenix-ir-interp` (`phoenix run-ir`); a tree-walker walking AST closure bodies is the correct shape for that role. The bundled closure-capture-ambiguity bug that motivated the original coupling is addressed independently as an IR + Cranelift ABI change — see [Bugs closed in this phase](#bugs-closed-in-this-phase) below.

#### Refactors

- **[Diagnostic builder pattern](../design-decisions.md#diagnostic-builder-pattern)** — replace inline `self.error(msg, span)` with a fluent `Diagnostic::error(...).with_note(...).with_suggestion(...)` API. Module-system diagnostics are a natural first consumer (multi-span "symbol X is private, defined here: [other file]" errors). Hard deadline: before Phase 3.2. **🟡 Foundation landed 2026-04-27**:
    - **Data model:** `phoenix-common::diagnostics::{Diagnostic, Note}` carries `notes: Vec<Note>` + `suggestion: Option<String>`, with `with_note` / `with_suggestion` builder methods.
    - **Rendering:** consolidated in `Diagnostic::display_with(&SourceMap)` so every span (primary + each note) resolves against its own `SourceId`; the bare `Display` impl shares a private `fmt_suffix` helper so the two paths can't drift.
    - **Driver:** `phoenix-driver::report_diagnostics` calls `display_with` directly.
    - **LSP:** `phoenix-lsp::to_lsp_diagnostic` appends hint and suggestion to the LSP message and forwards notes as `related_information` (single-document URI for now — a `SourceId → Url` lookup is the follow-up once cross-file notes surface).
    - **Tests:** 20+ unit tests in `diagnostics.rs` plus dedicated LSP tests pin the shape.
    - **Remaining:** migration of existing `self.error(...)` sites to the rich form where it's needed — most call sites are already shape-compatible and can stay as-is.
- **Generic-template stubs typed split** — **✅ Implemented 2026-04-27**. `IrModule.functions: Vec<FunctionSlot>` where [`FunctionSlot`](../../crates/phoenix-ir/src/module.rs) is a tagged enum (`Concrete(IrFunction) | Template(IrFunction)`); the old `IrFunction.is_generic_template: bool` field is gone. Iteration helpers (`concrete_functions`, `templates`, `lookup`, `get_concrete`) make the dispatch type-system-enforced — a backend that walks `module.functions` directly now sees `&FunctionSlot` and must either pattern-match or use the high-level accessor; it cannot accidentally treat a template body as concrete.
- **`ValueId` allocator typed split** — **✅ Implemented 2026-04-27** as [`ValueIdAllocator`](../../crates/phoenix-ir/src/value_alloc.rs) on `IrFunction`. Owns both the counter and the per-value type vector; the only public mint path is `alloc(ty)`, which atomically appends the type. The historical `IrFunction.next_value_id` / `value_types` parallel-index pair is gone, the verifier's `verify_value_types_index` length-mismatch check is gone, and the `debug_desync_value_types` test helper is gone (the desync invariant is now structurally unreachable from any public API).

### Bugs closed in this phase

- **Closure capture type ambiguity with indirect calls** — **✅ Fixed 2026-04-27.** Closure functions now use an env-pointer calling convention: each closure function takes its environment pointer (the closure heap object) as the first arg and unpacks captures from it via the new `Op::ClosureLoadCapture(env_vid, capture_idx)`. `Op::CallIndirect` passes the closure value verbatim as the env arg — capture types never cross the indirect-call boundary, so two closures with identical user signatures but different captures unify cleanly through any phi/block parameter. The Cranelift heuristic capture-type scanner (`find_closure_capture_types` and the `closure_func_map`) is deleted. Regression: `tests/fixtures/closures_ambiguous_captures.phx` (registered in the three-backend matrix). IR + Cranelift + IR-interp change only; `phoenix-interp` is unchanged.

### Bugs bundled into this phase

- **[Default-expression visibility across module boundaries](../known-issues.md#default-expression-visibility-across-module-boundaries-phase-26-tripwire)** — cannot defer past 2.6. Default arguments are inlined at the caller's site today; once modules land, a public function with a default that references a private symbol must compile via the wrapper-synthesis pass without leaking the private symbol to importers.

### Exit criteria for declaring Phase 2.6 complete

These are the bars that have to clear before Phase 2.6 is closed.  An item with an unchecked box is a real outstanding follow-up, not a stylistic note.  Mirror of [§2.2's exit criteria](#exit-criteria-for-declaring-phase-22-complete) — the same shape (design-decision markers + regression tests + matrix + workspace clean) plus 2.6-specific gates for the module-system surface.

- [ ] **All Phase-2.6 design decisions implemented.** Each `**[…]**` bullet in §2.6's "Refactors bundled into this phase" carries a `✅ Implemented YYYY-MM-DD` marker. **Status: 2 of 3 done; checkbox stays unchecked until (a) lands.**
    - [x] **(a-foundation) Diagnostic builder foundation** — ✅ 2026-04-27. (Foundation only; the criterion-level box below stays open until module-system diagnostics actually exercise `with_note` for at least one cross-file error path — see the dedicated criterion further down.)
    - [x] **(b) Generic-template typed split** — ✅ 2026-04-27. `IrFunction.is_generic_template: bool` is gone (`rg 'is_generic_template' crates/` returns nothing); template / concrete iteration goes through the typed [`FunctionSlot`](../../crates/phoenix-ir/src/module.rs) enum.
    - [x] **(c) `ValueId` allocator typed split** — ✅ 2026-04-27. `ValueId` allocation and per-value type recording are a single operation via [`ValueIdAllocator::alloc`](../../crates/phoenix-ir/src/value_alloc.rs); no public API for "allocate a `ValueId` without assigning a type", and the verifier's old `verify_value_types_index` length-mismatch check is gone (structurally unreachable).

    The `Value::Closure → IR blocks` refactor that originally appeared in this list was dropped from the batch on 2026-04-27 — `phoenix-interp` is intended to remain a fast AST tree-walker for debugging, kept deliberately separate from `phoenix-ir-interp`. The closure-capture-ambiguity bug that was bundled with it is being addressed independently via the env-pointer ABI fix tracked under "Bugs bundled into this phase" below.
- [ ] **All Phase-2.6 bug-closure entries have regression tests.** Each entry under "Bugs closed in this phase" / "Bugs bundled into this phase" maps to a topic-specific test that fails when the fix is reverted. Closure-capture ambiguity is **✅ Closed 2026-04-27**: `tests/fixtures/closures_ambiguous_captures.phx` (registered in the three-backend matrix as `matrix_closures_ambiguous_captures`) compiles and runs identically under `phoenix run`, `phoenix run-ir`, and `phoenix build`; reverting the env-pointer ABI fix would resurface the original "ambiguous indirect call" `CompileError` in compiled mode. Default-expression visibility (a multi-file fixture where module A defines `public function f(x: Int = privateHelper())` and module B imports `f` — calling `f()` from B must work, and `privateHelper` must remain unimportable from B) remains open pending the module system.
- [ ] **No `known-issues.md` entry targeted at Phase 2.6.** Outstanding issues are either closed by 2.6 work, re-targeted to Phase 3 / 4 with a written rationale, or carry no phase tag (i.e., they describe pre-existing gaps not committed to 2.6's scope). Today the entries explicitly tagged 2.6 are: the two bug-closures listed above (closure-capture ambiguity, default-expression visibility) plus the two IR-shape refactors bundled with `Value::Closure` (generic-template stubs typed split, `ValueId` allocator typed split). All four "Phase 2.6 or Phase 3" hedges have been hard-picked to 2.6 (2026-04-27).
- [ ] **Workspace test/clippy/fmt clean on the 2.6 branch.** `cargo test --workspace` green; `cargo clippy --workspace --tests` zero warnings; `cargo fmt --all -- --check` clean. Baseline at 2.6 start: 2,624 tests passing.
- [ ] **Three-backend roundtrip matrix on multi-file fixtures.** §2.2's `crates/phoenix-driver/tests/three_backend_matrix.rs` covers single-file fixtures; 2.6 extends this to a sibling test driver (likely `multi_module_matrix.rs`) that walks every multi-file project under a new `tests/fixtures/multi/<name>/` tree (each project has at minimum a `main.phx` plus 1-2 imported modules), runs each under `phoenix run`, `phoenix run-ir`, and `phoenix build`, and asserts byte-identical stdout. The fixture set must include at least one project per visibility rule (see next criterion). Codegen `gen_*.phx` schemas remain excluded.
- [ ] **Visibility rule coverage.** For every entry in the §2.6 "Visibility rules" list (`public struct`, `public` field, `public function`, `public enum`, `public trait`, default-private for each), at least one *positive* test (importer reaches the public form) and one *negative* test (importer attempting the private form receives a single, well-shaped diagnostic — no panic, no `unreachable!()`, no infinite loop in the resolver). Positive tests live in the multi-file matrix above; negative tests can live in a dedicated `crates/phoenix-sema/tests/visibility.rs`.
- [ ] **Module-resolver error paths report cleanly, never panic.** Each of these inputs produces a single non-zero exit with a structured diagnostic, not a panic or unbounded recursion: missing imported module; ambiguous module name; cyclic imports (`a.phx` imports `b.phx` imports `a.phx`); malformed `mod.phx`; an import path that escapes the project root (`../foo` style). Each has a regression test under `crates/phoenix-driver/tests/` or `crates/phoenix-sema/tests/`.
- [ ] **Module-system diagnostics exercise the rich diagnostic shape.** At least one cross-module error renders a primary span plus a `note: <other_file>:<line>:<col>: defined here` line via `Diagnostic::error(...).with_note(decl_span, "...")`. The natural first consumer is "symbol X is private", which already has design-doc backing. Pins the diagnostic builder's rich shape to a real call site so it can't bit-rot before Phase 3.2 (LSP).

When every box above is ticked, Phase 2.6 closes and Phase 2.3 (GC + runtime) becomes the active phase.

## 2.7 Benchmark Suite

- Add a benchmark suite (e.g. `criterion`) early in Phase 2 to measure IR lowering and codegen performance
- Track compile times for representative Phoenix programs across changes
- Establish baseline metrics before optimization work begins
- **Why:** Phase 2 introduces compilation where performance becomes user-visible. Without benchmarks, regressions go unnoticed and optimization work has no measurable target.
- **Complexity:** Low — `criterion` integrates directly with Cargo; start with a handful of representative programs.
- **Depends on:** IR (2.1)
