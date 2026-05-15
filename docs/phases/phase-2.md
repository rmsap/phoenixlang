# Phase 2: Compilation

**Status: 2.1 + 2.2 + 2.3 + 2.6 + 2.7 complete; 2.4 (WebAssembly Target) active.** See [§2.4](#24-webassembly-target) for the active phase's scope; [§2.7](#27-benchmark-suite) holds the most recently closed-out writeup (with the `### ✅ Phase 2.7 closed (2026-05-13)` subsection at the end). The 2.7 closeout shipped the benchmark suite + `phoenix-bench-diff` regression detector + cross-language Phoenix-vs-Go corpus, threaded typed `TypeTag` values through every codegen and runtime allocation site, and added `ListBuilder<T>` / `MapBuilder<K, V>` transient-mutable accumulators (decision F) which cut the published `sort_ints` / `hash_map_churn` ratios from 1900× / 6979× to 5.4× / 3.6× against Go.

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
- `List<T>` with all functional methods (map, filter, reduce, find, any, all, flatMap, sortBy, first, last, contains, take, drop). `sortBy` uses bottom-up iterative merge sort (O(n log n) worst-case, stable) — see the §2.3 closure entry for the rooting contract on its intermediate buffers.
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

When every box above is ticked, Phase 2.2 closes and Phase 2.6 (modules and visibility) becomes the active phase. Phase 2.3 (GC + runtime), 2.4 (WebAssembly target), and 2.5 (JavaScript interop) remain on the roadmap but are sequenced after the module system: the language-level scaffolding for cross-file code organization is a prerequisite for the package manager (3.1) and LSP (3.2). Compiled binaries continued to leak heap allocations until Phase 2.3 closed (2026-05-04) — see [§2.3](#23-runtime-and-memory-management).

## 2.3 Runtime and Memory Management

**Status: ✅ Complete (2026-05-04).** GC core, `defer` syntax, perf fixes (hash-map, merge-sort), and the valgrind leak-verification gate all landed; every exit-criteria checkbox below is green. Per-decision implementation details live in [Design decisions locked in this phase](#design-decisions-locked-in-this-phase) below.

A minimal runtime already exists as the [`phoenix-runtime`](../../crates/phoenix-runtime/) crate (static library linked into compiled binaries). It currently provides `print` (all value types + strings), `toString`, string comparison and concatenation, all string methods, heap allocation (`phx_gc_alloc`, backed by the tracing GC — see below), panic/abort, `List<T>` data structures (alloc, get, push, contains, take, drop), `String.split` (returns `List<String>`), and `Map<K, V>` data structures (alloc, get, set, remove, contains, keys, values). This phase replaces the leak-everything allocator with a working tracing GC, closes the two perf bugs that were deferred here, and lands the `defer` syntax that the GC decision implied.

- Garbage collector — **tracing GC, mark-and-sweep baseline** (decided 2026-04-19; see [GC strategy](../design-decisions.md#gc-strategy)). Leave room to evolve to generational later without ABI changes. Implementation sub-decisions A–G are pinned in [GC implementation: subordinate decisions](../design-decisions.md#gc-implementation-subordinate-decisions).
- `defer` syntax — Go-style statement-level `defer expr;`, runs LIFO at end of enclosing scope on every exit path (return, panic, fall-through). Decision in [§G of subordinate decisions](../design-decisions.md#g-scope-bound-cleanup-syntax-go-style-statement-level-defer); implementation status pinned in the [Decision G entry](#design-decisions-locked-in-this-phase) below.
- String implementation (UTF-8, immutable by default) — basic ops already in `phoenix-runtime`. **All string allocations move onto the GC heap in this phase** (every `leak_string()` site goes away).
- Panic/abort handler — already in `phoenix-runtime`
- Built-in function implementations (`print`, `toString`) — already in `phoenix-runtime`
- Collection runtime support (List, Map data structures with dynamic resizing) — **basic implementation complete** (`list_methods.rs`, `map_methods.rs`); map lookup is replaced with hash-based implementation in this phase (see bugs below).
- Builtin method implementations (String.*, List.*, Map.*, Option.*, Result.*) — **complete** in compiled mode; closure-based list methods (map, filter, reduce, etc.) are compiled inline as Cranelift loops (`list_methods_closure.rs` for single-loop methods — map, filter, find, any, all, reduce; `list_methods_complex.rs` for nested-loop methods — flatMap, sortBy).

### Design decisions locked in this phase

These are sub-decisions of the parent `### GC strategy` decision — they pin the GC ABI and runtime layout and must land before 2.3 wraps. See [design-decisions.md](../design-decisions.md#gc-implementation-subordinate-decisions):

- **[A. Root-finding: precise via shadow stack](../design-decisions.md#a-root-finding-precise-via-shadow-stack)** — `phx_gc_push_frame` / `phx_gc_pop_frame` / `phx_gc_set_root`, emitted by Cranelift on function entry/exit and at every ref-typed SSA assignment. **✅ Implemented 2026-05-04** in `crates/phoenix-cranelift/src/translate/gc_roots.rs` and `crates/phoenix-runtime/src/gc/shadow_stack.rs`.
- **[B. Heap layout](../design-decisions.md#b-heap-layout-segregated-free-lists-by-size-class-single-arena)** — single global allocator + per-allocation registry tracked by `MarkSweepHeap`. **✅ Implemented 2026-05-04** in `crates/phoenix-runtime/src/gc/heap.rs`. Baseline is Rust's global allocator with a `HashSet` registry (see the *Adopted baseline* note in the design-decisions entry); segregated free lists are tracked as a Phase 2.7 perf-tuning target.
- **[C. Object header: 8-byte per-object header](../design-decisions.md#c-object-header-8-byte-per-object-header)** — mark bit + type tag + reserved forwarding-pointer slot + payload size. **✅ Implemented 2026-05-04** in `crates/phoenix-runtime/src/gc/mod.rs::ObjectHeader`.
- **[D. Safepoint placement: at allocation calls only](../design-decisions.md#d-safepoint-placement-at-allocation-calls-only)** — threshold-based (1 MB since last collection). **✅ Implemented 2026-05-04**; gated behind `phx_gc_enable()` which the Cranelift-generated C `main` calls before `phx_main`.
- **[E. `GcHeap` Rust trait abstraction](../design-decisions.md#e-allocator-abstraction-gcheap-rust-trait-single-impl-in-23)** — single `MarkSweepHeap` impl in 2.3. **✅ Implemented 2026-05-04**.
- **[F. Strings join the GC heap](../design-decisions.md#f-strings-join-the-gc-heap-no-interning-in-23)** — every `leak_string()` call site migrates. **✅ Implemented 2026-05-04**: `phx_str_concat`, `phx_i64_to_str`, `phx_f64_to_str`, `phx_bool_to_str`, and every `phx_str_*` transform in `string_methods.rs` now go through `phx_string_alloc` (tagged `TypeTag::String` so the GC skips the interior scan).
- **[G. `defer` syntax: Go-style statement-level](../design-decisions.md#g-scope-bound-cleanup-syntax-go-style-statement-level-defer)** — **✅ Implemented 2026-05-06.**
  - **Plumbing.** lexer keyword → parser `DeferStmt` → sema (placement check + reject `return`/`?` inside the deferred expression) → AST-interp `defer_stack` → IR-lowering `pending_defers` with `lower_defers_for_exit` emitting each in reverse before every `Terminator::Return`. Cranelift and the C backend inherit via the shared IR.
  - **Exit paths.** Defers fire on every function-exit terminator: the explicit `return` statement, the fall-through return at the end of a body, *and* the `?` (try) early-return path in `lower_try`. All three call `lower_defers_for_exit` before terminating. Closures get their own defer frame (saved/restored across lambda lowering on the IR side; pushed in `call_closure` on the AST side), so a `defer` inside a closure fires when the closure returns, not at the enclosing function's exit.
  - **Defer-error policy.** Go-style: every registered defer runs even if an earlier one errored, and the *first* error is propagated after the sequence completes (subsequent errors are dropped). On the AST interp, body errors run defers too but body-error wins over defer-error; on the IR-driven backends, hardware traps such as integer division by zero abort before defers run — see [known-issues.md](../known-issues.md#defer-does-not-fire-on-hardware-trap-body-errors-in-compiled-binaries) for the divergence.
  - **Sema placement rule.** `Statement::Defer` is permitted only at the outermost statement level of a function, method, or lambda body. Anything deeper (inside an `if`-arm, a loop body, a `match`-arm block, a call argument that contains a block expression, etc.) is a sema error: `` `defer` must appear at the function's outermost statement level``. The rule sidesteps two classes of defects in the static lowering — defers that reference bindings from a popped inner scope, and defers in untaken branches that would still fire on later exit paths because the IR has no active flag. Lambda bodies have their own outermost level (and their own defer frame on both interpreters), so a `defer` inside a lambda is fine — it is checked against the *lambda's* outermost level. A future relaxation can lift the restriction once the IR side gains per-iteration dynamic registration.
  - **Lazy-capture semantics.** Free variables resolve at exit, not at the defer point — assignments after the defer *do* affect what the deferred expression sees. This is a deliberate divergence from Go (which evaluates call-expression arguments eagerly at the defer point); it applies uniformly to the AST interp and the IR-driven backends.
  - **Fixtures (in the three-backend matrix):**
    - `tests/fixtures/defer_basic.phx` — fall-through with multiple defers; LIFO order observable via stdout.
    - `tests/fixtures/defer_explicit_return.phx` — defers fire on the explicit-`return` exit path.
    - `tests/fixtures/defer_lazy_capture.phx` — pins lazy-capture semantics (mutation after defer is observed).
    - `tests/fixtures/defer_method.phx` — defer inside a struct method (`call_method_body` path).
    - `tests/fixtures/defer_heap.phx` — defer that allocates on the GC heap (string concat) before `phx_gc_pop_frame` runs, so the allocation's roots stay reachable.
    - `tests/fixtures/defer_closure.phx` — defer inside a closure body fires on closure return, not at the enclosing function's exit.
    - `tests/fixtures/defer_try.phx` — defer fires on the `?` (try) early-return path, identically across the AST interp and IR-driven backends.
    - `tests/fixtures/defer_multiple_returns.phx` — defer fires on every explicit-return exit path, not just the first or the fall-through.
    - `tests/fixtures/defer_shadowed_at_return.phx` — defer at the function's outermost level fires from a `return` reached inside a block that shadows one of its free variables; resolves to the OUTER binding (pins the scope-masking in `lower_defers_for_exit`).
    - `tests/fixtures/defer_nested_function_frames.phx` — callee defers fire when the callee returns, not at the caller's exit; each call to the same callee starts with a fresh defer frame.
  - **Future use.** Real consumers (file / socket / lock cleanup) come in Phase 4 when the stdlib introduces resources.

### Bugs to be closed in this phase

See [known-issues.md](../known-issues.md):

- **Memory leaks (no GC yet)** — root driver. **✅ Closed 2026-05-04.** A tracing mark-and-sweep GC tracks every allocation, collects unreachable objects when triggered, and respects the shadow stack as a precise root set. Compiled binaries allocate via `phx_gc_alloc` (an earlier untyped `phx_alloc` shim was retired by PR 6 of phase 2.7 — see the [decision B 2026-05-12 update](../design-decisions.md#b-heap-layout-segregated-free-lists-by-size-class-single-arena)); the Cranelift backend emits shadow-stack push/set/pop around every function so the GC has precise stack roots; the heap collects on a 1-MB-since-last-collection threshold once the C `main` calls `phx_gc_enable`. Strings and all `phx_str_*` transforms are GC-managed (no more `mem::forget`). Process-exit cleanup is wired through `phx_gc_shutdown`, called from the generated C `main` after `phx_main` returns — it replaces the singleton `MarkSweepHeap` with a fresh empty one, letting the old heap's `Drop` impl deallocate every tracked header. Verified leak-clean by `crates/phoenix-driver/tests/gc_valgrind.rs::alloc_loop_terminates_leak_clean_under_valgrind`: 0 / 0 / 0 / 1024 bytes across the *definitely lost* / *indirectly lost* / *possibly lost* / *still reachable* categories on the `alloc_loop.phx` fixture (100k iterations, ~8 MB cumulative allocation, ~1024 bytes still-reachable from Rust's stdout buffer baseline). Pinned end-to-end by the integration tests in `crates/phoenix-runtime/tests/gc_collects.rs` (precise-roots + threshold-driven auto-collect) and the `RLIMIT_AS` regression test in `crates/phoenix-driver/tests/gc_bounded_memory.rs` (catches "GC stops reclaiming during execution" as the companion to the valgrind gate, which catches "GC reclaims during execution but leaks at termination").
- **O(n) map key lookup** — **✅ Closed 2026-05-04.** Replaced the flat-array linear scan with an open-addressing hash table (linear probing, FNV-1a 64-bit hash, 70 % max load factor). Lookups, inserts, and removes are now O(1) average. A parallel insertion-order array preserves user-visible iteration order — `Map.keys()` / `Map.values()` return entries in the order they were first inserted, matching `phoenix-ir-interp`'s `Vec<(K, V)>` and the user expectation set by Python / JavaScript / TypeScript. String keys hash by content (so two equal strings at different addresses land in the same bucket, consistent with `keys_equal`). Cranelift's map-literal codegen now writes pairs into a stack buffer and calls a single `phx_map_from_pairs` runtime entry point that hash-builds the table in one pass — no more inline pair-writes against a fixed offset. Full layout (header → tags → order → pairs) and the operation-by-operation contract live in the `phoenix-runtime/src/map_methods.rs` module header. Regression: `tests/fixtures/map_hash_many_keys.phx` (100 inserts crossing the 8 → 16 → 32 → 64 → 128 grow path, then 10 lookups) registered in the three-backend matrix.
- **O(n²) `List.sortBy` insertion sort** — **✅ Closed 2026-05-07.** Replaced the Phase 2.2 insertion sort with bottom-up iterative merge sort across all three backends; **O(n log n)** worst case, stable (`cmp ≤ 0` keeps the left run first). The non-obvious bit on the Cranelift side is GC rooting: `translate_list_sortby` allocates two intermediate buffers (`copy`, `aux`) that exist only as Cranelift SSA values, so the function-level shadow frame from `gc_roots.rs` can't see them. It therefore pushes its own dedicated 2-slot shadow frame on entry, roots `copy` and `aux` into it, and ping-pongs `src`/`dst` via two stack slots so each width pass merges directly into the previously-stale buffer (no copyback). Both interpreters delegate to the shared `phoenix_common::algorithms::merge_sort_by` helper. Fixtures (all five in the three-backend matrix): `list_sortby_merge.phx`, `list_sortby_alloc_comparator.phx` (pins the GC-rooting contract by allocating across the 1 MB collection threshold mid-sort), `list_sortby_edge_lengths.phx`, `list_sortby_strings.phx`, `list_sortby_stable.phx` (explicit equal-key stability — pins `cmp <= 0` favoring the left run). Algorithm-level error propagation is pinned by unit tests in `phoenix-common::algorithms` and per-interpreter in `eval_builtins`/`roundtrip_collections`. Full per-block CFG and rooting contract live in `translate_list_sortby`'s doc comment.

### Exit criteria for declaring Phase 2.3 complete

Mirror of [§2.2's exit criteria](#exit-criteria-for-declaring-phase-22-complete) and [§2.6's exit criteria](#exit-criteria-for-declaring-phase-26-complete) — minimum gates only. Performance benchmarks and tuning targets land in Phase 2.7 (see [Performance benchmarks](#performance-benchmarks)) and do not block 2.3 close.

- [x] **GC subordinate decisions A–F implemented (2026-05-04).** Shadow stack, mark-sweep heap, object header, threshold-triggered safepoints, `GcHeap` trait, GC-managed strings — all landed and exercised by `crates/phoenix-runtime/tests/gc_collects.rs`.
- [x] **Decision G (`defer` syntax) implementation (2026-05-04).** Lexer → parser → sema → IR → all three backends. Ten fixtures in the three-backend matrix (basic LIFO, explicit-return, lazy-capture, method, GC-heap allocation, closure-defer, `?`-early-return, multiple-explicit-returns, shadowed-at-return, nested-function-frames); see the Decision G entry above for individual contracts.
- [x] **GC has regression tests.** `crates/phoenix-runtime/tests/gc_collects.rs` and the `tests/fixtures/gc_*.phx` matrix entries cover the precise-roots invariants and the threshold-driven auto-collect path.
- [x] **Map and sortBy regression tests (2026-05-07).** `tests/fixtures/map_hash_many_keys.phx` (100-insert grow path + 10 lookups) pins the hash-table contract. The merge-sort contract is pinned by five matrix fixtures: `list_sortby_merge.phx` (50-element Int correctness fingerprint), `list_sortby_alloc_comparator.phx` (allocating comparator across the 1 MB GC threshold — pins the dedicated 2-slot shadow-stack frame in `translate_list_sortby`), `list_sortby_edge_lengths.phx` (lengths 0/1/2/3), `list_sortby_strings.phx` (fat-pointer element load/store), and `list_sortby_stable.phx` (explicit equal-key stability — pins `cmp <= 0` favoring the left run). Any divergence between AST interp / IR interp / Cranelift codegen names the offending fixture.
- [x] **No `known-issues.md` entry targeted at Phase 2.3 (2026-05-04).** All three entries deleted as their resolutions landed: map-lookup in commit `d34fb56` (hash-map rewrite), sortBy in commit `74667e5` (merge-sort), and "Memory leaks (no GC yet)" in the same change as the valgrind gate (this commit). Full closure descriptions live in the corresponding §2.3 bug-closure bullets above (single source of truth; no stub entries in `known-issues.md` to bit-rot).
- [x] **Workspace test/clippy/fmt clean (post-GC).** `cargo test --workspace` green as of 2026-05-04.
- [x] **Three-backend roundtrip matrix on memory-stress fixtures.** `alloc_loop.phx`, `gc_keeps_alive.phx`, `gc_loop_carried_ref.phx`, and `defer_basic.phx` all pass `phoenix run`, `phoenix run-ir`, `phoenix build`+execute with byte-identical stdout.
- [x] **No leaks under valgrind on `alloc_loop.phx` compiled binary (2026-05-04).** `crates/phoenix-driver/tests/gc_valgrind.rs` builds three fixtures (`alloc_loop.phx`, `gc_loop_carried_ref.phx`, `defer_basic.phx`) and runs each under `valgrind --leak-check=full`, asserting zero bytes in each of the *definitely lost*, *indirectly lost*, and *possibly lost* categories and capping *still reachable* at 2 KiB (~2× the measured baseline of Rust's stdout buffer + any GC `OnceLock` overhead — `PHOENIX_GC_VALGRIND_REACHABLE_CAP_BYTES` overrides if a future libc change shifts the floor). On the current main branch each fixture reports 0 / 0 / 0 / ≤1024 bytes (cap = 2 KiB). **Linux-only gate** (`#[cfg(target_os = "linux")]`, matching `gc_bounded_memory.rs`); CI runs Linux for this gate. The test skips with a `println!` when `valgrind` is not on `$PATH` so dev machines without it are not blocked; CI sets `PHOENIX_REQUIRE_VALGRIND=1` to turn that skip into a hard failure so a misconfigured runner cannot silently bypass the gate. The companion `RLIMIT_AS` test in `gc_bounded_memory.rs` catches "GC stops reclaiming during execution"; the valgrind tests catch "GC reclaims during execution but leaks at termination" — together they pin both ends of the GC's lifetime contract.

When every box above is ticked, Phase 2.3 closes and Phase 2.7 (Benchmark Suite) becomes the active phase — sequenced ahead of 2.4 (WebAssembly target) so the native GC has a measured baseline before a second `GcHeap` impl arrives behind the same trait.

### ✅ Phase 2.3 closed (2026-05-04)

Tracing GC + runtime shipped end-to-end. Implementation scope: precise stack roots via a per-thread shadow stack (`phx_gc_push_frame` / `phx_gc_set_root` / `phx_gc_pop_frame`, emitted by Cranelift on every function entry / ref-typed assignment / exit), 8-byte object headers (mark bit + type tag + reserved forwarding-pointer slot + payload size), threshold-triggered collection (1 MB since last collect), `GcHeap` Rust trait with a single `MarkSweepHeap` impl backed by Rust's global allocator + a per-allocation registry, GC-managed strings (no more `mem::forget`), and process-exit cleanup via `phx_gc_shutdown` so compiled binaries terminate leak-clean under valgrind. `defer` syntax landed across the lexer / parser / sema / IR / all three backends with Go-style statement-level placement, lazy-capture semantics, and ten matrix fixtures pinning each exit path. The `O(n)` map-key-lookup performance bug closed with an open-addressing hash table that preserves user-visible insertion order via a parallel order array; the `O(n²)` `List.sortBy` insertion sort closed with bottom-up iterative merge sort across all three backends. Verified by the workspace test suite plus the targeted integration tests in `crates/phoenix-runtime/tests/gc_collects.rs` (precise-roots + threshold-driven auto-collect), the `RLIMIT_AS` regression test in `crates/phoenix-driver/tests/gc_bounded_memory.rs` (catches "GC stops reclaiming during execution"), and the valgrind gate in `crates/phoenix-driver/tests/gc_valgrind.rs` (catches "GC reclaims during execution but leaks at termination").

Phase 2.7 (Benchmark Suite) is the next active phase — sequenced ahead of 2.4 (WebAssembly target) so the native GC has a measured baseline before a second `GcHeap` impl arrives, and so the size-class-arena and typed-allocator follow-ups carried over from 2.3 have a quantitative gate.

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

**Status: ✅ Complete (2026-04-30).** Sequenced ahead of 2.3–2.5 because the language-level scaffolding for cross-file code organization unblocks the package manager (3.1) and LSP (3.2). All exit criteria below tick green; see the closing notes after the criteria list.

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

- **[Diagnostic builder pattern](../design-decisions.md#diagnostic-builder-pattern)** — replace inline `self.error(msg, span)` with a fluent `Diagnostic::error(...).with_note(...).with_suggestion(...)` API. Module-system diagnostics are a natural first consumer (multi-span "symbol X is private, defined here: [other file]" errors). Hard deadline: before Phase 3.2. **✅ Implemented 2026-04-30**:
    - **Data model:** `phoenix-common::diagnostics::{Diagnostic, Note}` carries `notes: Vec<Note>` + `suggestion: Option<String>`, with `with_note` / `with_suggestion` builder methods.
    - **Rendering:** consolidated in `Diagnostic::display_with(&SourceMap)` so every span (primary + each note) resolves against its own `SourceId`; the bare `Display` impl shares a private `fmt_suffix` helper so the two paths can't drift.
    - **Driver:** `phoenix-driver::report_diagnostics` calls `display_with` directly.
    - **LSP:** `phoenix-lsp::to_lsp_diagnostic` appends hint and suggestion to the LSP message and forwards notes as `related_information` (single-document URI for now — a `SourceId → Url` lookup is the follow-up once cross-file notes surface).
    - **Consumer:** `phoenix-sema/src/import_resolve.rs` and `phoenix-sema/src/field_privacy.rs` emit the rich shape (`with_note` + `with_suggestion`) for cross-module privacy errors, pinning the API to a real call site so it can't bit-rot before Phase 3.2.
    - **Tests:** 20+ unit tests in `diagnostics.rs` plus dedicated LSP tests pin the shape; `negative_import_private_function` in `crates/phoenix-driver/tests/multi_module_negative.rs` pins the consumer end-to-end.
- **Generic-template stubs typed split** — **✅ Implemented 2026-04-27**. `IrModule.functions: Vec<FunctionSlot>` where [`FunctionSlot`](../../crates/phoenix-ir/src/module.rs) is a tagged enum (`Concrete(IrFunction) | Template(IrFunction)`); the old `IrFunction.is_generic_template: bool` field is gone. Iteration helpers (`concrete_functions`, `templates`, `lookup`, `get_concrete`) make the dispatch type-system-enforced — a backend that walks `module.functions` directly now sees `&FunctionSlot` and must either pattern-match or use the high-level accessor; it cannot accidentally treat a template body as concrete.
- **`ValueId` allocator typed split** — **✅ Implemented 2026-04-27** as [`ValueIdAllocator`](../../crates/phoenix-ir/src/value_alloc.rs) on `IrFunction`. Owns both the counter and the per-value type vector; the only public mint path is `alloc(ty)`, which atomically appends the type. The historical `IrFunction.next_value_id` / `value_types` parallel-index pair is gone, the verifier's `verify_value_types_index` length-mismatch check is gone, and the `debug_desync_value_types` test helper is gone (the desync invariant is now structurally unreachable from any public API).

### Bugs closed in this phase

- **Closure capture type ambiguity with indirect calls** — **✅ Fixed 2026-04-27.** Closure functions now use an env-pointer calling convention: each closure function takes its environment pointer (the closure heap object) as the first arg and unpacks captures from it via the new `Op::ClosureLoadCapture(env_vid, capture_idx)`. `Op::CallIndirect` passes the closure value verbatim as the env arg — capture types never cross the indirect-call boundary, so two closures with identical user signatures but different captures unify cleanly through any phi/block parameter. The Cranelift heuristic capture-type scanner (`find_closure_capture_types` and the `closure_func_map`) is deleted. Regression: `tests/fixtures/closures_ambiguous_captures.phx` (registered in the three-backend matrix). IR + Cranelift + IR-interp change only; `phoenix-interp` is unchanged.
- **Default-expression visibility across module boundaries** — **✅ Fixed 2026-04-30.** Before the fix, default-argument expressions were lowered at the *caller's* call site (see [design-decisions.md: *Default-argument lowering strategy*](../design-decisions.md#default-argument-lowering-strategy)). For a multi-file program where a public function `f(x: Int = privateHelper())` lives in module A and is imported from module B, calling `f()` from B would inline `privateHelper()` into B's compiled output — three failure modes: (1) privacy leak — B's binary references the private symbol directly, forcing implicit re-export; (2) contract leak — renaming `privateHelper` silently breaks every caller of `f`, even though A's author thought it was safely private; (3) sema couldn't detect the shape because defaults type-check in the callee's module with full access. **Resolution:** sema flags every non-pure-literal default with a per-slot `default_needs_wrapper` set ([`phoenix-sema/src/check_register.rs`](../../crates/phoenix-sema/src/check_register.rs)). IR's [`default_wrappers`](../../crates/phoenix-ir/src/default_wrappers.rs) pass synthesizes a zero-arg wrapper (`__default_fn{FID}_<name>_<slot>` / `__default_m{FID}_<Type>__<method>_<slot>`) in the callee's module and records `(callee, slot) → wrapper_id` in `IrModule::default_wrapper_index`. Caller-site lowering in `assemble_call_args` ([`phoenix-ir/src/lower_expr.rs`](../../crates/phoenix-ir/src/lower_expr.rs)) consults that index and emits `Op::Call(wrapper, [], [])` instead of inlining the AST default. Pure-literal defaults stay on the inline path — cheaper, privacy-safe by construction. Generic callees are gated off (sema's `default_ty.has_type_vars()` rejection ensures their defaults are closed and module-internal) until per-specialization wrapper cloning lands as a follow-on. **Small accepted semantic shift:** defaults referencing private state now evaluate in the callee's scope rather than the caller's. Regression: `default_wrapper_synthesized_for_non_literal_default`, `pure_literal_default_does_not_synthesize_wrapper`, `chained_default_wrappers_call_each_other_not_inlined`, `method_default_wrapper_synthesized`, `closure_default_wrapper_synthesizes_and_lowers_closure`, and `multi_module_default_wrapper_routes_through_wrapper` in `crates/phoenix-ir/src/tests.rs`.

### Exit criteria for declaring Phase 2.6 complete

These are the bars that have to clear before Phase 2.6 is closed.  An item with an unchecked box is a real outstanding follow-up, not a stylistic note.  Mirror of [§2.2's exit criteria](#exit-criteria-for-declaring-phase-22-complete) — the same shape (design-decision markers + regression tests + matrix + workspace clean) plus 2.6-specific gates for the module-system surface.

- [x] **All Phase-2.6 design decisions implemented.** Each `**[…]**` bullet in §2.6's "Refactors bundled into this phase" carries a `✅ Implemented YYYY-MM-DD` marker.
    - [x] **(a-foundation) Diagnostic builder foundation** — ✅ 2026-04-27.
    - [x] **(a-consumer) Cross-module privacy diagnostic uses the rich shape** — ✅ 2026-04-30. `crates/phoenix-sema/src/import_resolve.rs` emits `Diagnostic::error(...).with_note(decl_span, "declared here").with_suggestion("mark as `public`")` when an `import` resolves to a private symbol; `crates/phoenix-sema/src/field_privacy.rs` does the same for cross-module field access. Pinned by `negative_import_private_function` in `crates/phoenix-driver/tests/multi_module_negative.rs` (asserts message + note + suggestion + cross-file `lib.phx` span).
    - [x] **(b) Generic-template typed split** — ✅ 2026-04-27. `IrFunction.is_generic_template: bool` is gone (`rg 'is_generic_template' crates/` returns nothing); template / concrete iteration goes through the typed [`FunctionSlot`](../../crates/phoenix-ir/src/module.rs) enum.
    - [x] **(c) `ValueId` allocator typed split** — ✅ 2026-04-27. `ValueId` allocation and per-value type recording are a single operation via [`ValueIdAllocator::alloc`](../../crates/phoenix-ir/src/value_alloc.rs); no public API for "allocate a `ValueId` without assigning a type", and the verifier's old `verify_value_types_index` length-mismatch check is gone (structurally unreachable).

    The `Value::Closure → IR blocks` refactor that originally appeared in this list was dropped from the batch on 2026-04-27 — `phoenix-interp` is intended to remain a fast AST tree-walker for debugging, kept deliberately separate from `phoenix-ir-interp`. The closure-capture-ambiguity bug that was bundled with it is being addressed independently via the env-pointer ABI fix tracked under "Bugs closed in this phase" above.
- [x] **All Phase-2.6 bug-closure entries have regression tests.** Each entry under "Bugs closed in this phase" maps to a topic-specific test that fails when the fix is reverted. Closure-capture ambiguity is **✅ Closed 2026-04-27**: `tests/fixtures/closures_ambiguous_captures.phx` (registered in the three-backend matrix as `matrix_closures_ambiguous_captures`) compiles and runs identically under `phoenix run`, `phoenix run-ir`, and `phoenix build`; reverting the env-pointer ABI fix would resurface the original "ambiguous indirect call" `CompileError` in compiled mode. Default-expression visibility is **✅ Closed 2026-04-30**: the six wrapper-synthesis tests in `crates/phoenix-ir/src/tests.rs` (`default_wrapper_synthesized_for_non_literal_default`, `pure_literal_default_does_not_synthesize_wrapper`, `chained_default_wrappers_call_each_other_not_inlined`, `method_default_wrapper_synthesized`, `closure_default_wrapper_synthesizes_and_lowers_closure`, `multi_module_default_wrapper_routes_through_wrapper`) plus the end-to-end `tests/fixtures/multi/default_wrapper/` fixture in the multi-module three-backend matrix together cover the synthesis pass, the chained-wrapper ordering invariant, the method-default form, the closure-valued-default corner, the call-site rewrite path, and the cross-module privacy property; reverting the wrapper-synthesis pass would resurface the inline-default path and trip the multi-module test.
- [x] **No `known-issues.md` entry targeted at Phase 2.6.** The two bug-closures (closure-capture ambiguity, default-expression visibility) and the three bundled refactors (diagnostic builder foundation, generic-template typed split, `ValueId` allocator typed split) are all closed (2026-04-27 / 2026-04-30). The "closure functions inside generic templates are not cloned per specialization" entry was hedged "Phase 2.6 if a module-system fixture trips the gap"; the multi-module fixture set landed without tripping it, so the entry is re-targeted to Phase 3. Two new entries were *opened* by 2.6 work and are tagged Phase 3 ("Sema `Type::Named/Generic/Dyn` payload allocates on every construction") or no-phase (`drain_remaining_into` callback duplication) — neither is a 2.6 deliverable.
- [x] **Workspace test/clippy/fmt clean on the 2.6 branch.** `cargo test --workspace` green (2,828 tests, up from the 2,624-test 2.6-start baseline: +204 tests covering parser/lexer keywords + AST visibility, the resolver crate, multi-module sema scopes + import resolution + visibility enforcement, default-arg wrapper synthesis, the multi-module three-backend matrix, the negative diagnostic suite, and IR / interp cross-module name resolution); `cargo clippy --workspace --tests` zero warnings; `cargo fmt --all -- --check` clean.
- [x] **Three-backend roundtrip matrix on multi-file fixtures.** **✅ Completed 2026-04-30.** `crates/phoenix-driver/tests/multi_module_matrix.rs` walks every multi-file project under `tests/fixtures/multi/<name>/` and runs each under `phoenix run`, `phoenix run-ir`, and `phoenix build` + execute, asserting byte-identical stdout *and* equality with the per-project `expected.txt` (so a coherent regression that broke all three backends the same way still trips). The matrix covers `basic_import` (cross-module function call), `import_alias` (`as` aliasing), `import_wildcard` (`{ * }`), `nested_modules` (directory-as-module without `mod.phx`), `default_wrapper` (the §2.6 tripwire fixture), `visibility_struct_pub` + `visibility_enum_pub` (positive cross-module type construction), `struct_methods` (cross-module method dispatch), `method_default_helper` (private-default helper resolves through the callee's scope), `enum_with_fields` (cross-module enum with payloads), and a generic-trait-bound import case. One `#[test]` per fixture so a divergence names the offending project. Codegen `gen_*.phx` schemas remain excluded.
- [x] **Visibility rule coverage.** Positive tests for `public struct` + `public` fields (`tests/fixtures/multi/visibility_struct_pub/`), `public enum` + variants (`tests/fixtures/multi/visibility_enum_pub/`), and `public function` + default-private (`tests/fixtures/multi/basic_import/` and `default_wrapper/`) all live in the matrix above. Sema-level coverage for every visibility rule (struct / field / enum / trait / type-alias public + default-private) lives in `crates/phoenix-sema/tests/check_modules_imports.rs` and `check_modules_callable.rs`. Negative tests for the privacy paths live in `crates/phoenix-driver/tests/multi_module_negative.rs` (`negative_import_private_function`, `negative_unimported_function_not_in_scope`, `negative_import_nonexistent_name`) and `crates/phoenix-sema/tests/check_modules_imports.rs`. Each negative produces a single non-panic diagnostic with the rich shape (note + suggestion) where applicable.
- [x] **Module-resolver error paths report cleanly, never panic.** **✅ Completed 2026-04-30.** Every required input has a regression test in `crates/phoenix-driver/tests/multi_module_negative.rs`: `negative_missing_module` (lists both probe paths in stderr), `negative_ambiguous_module` (lists both candidate paths), `negative_cyclic_imports` (renders `a → b → a` in the cycle path), and `negative_main_in_non_entry_module`. Malformed `mod.phx` is handled by the resolver's `MalformedSourceFiles` variant (parser diagnostics forwarded). An import path escaping the project root cannot today be expressed in the import grammar (no `..` in dotted paths), so the box ticks on unrepresentability rather than a live test; the `EscapesRoot` defensive guard in `phoenix-modules` is in place for future symlink-via-`mod.phx` shenanigans but is currently untested — tracked under [known-issues: *`phoenix-modules` `EscapesRoot` resolver guard is untested*](../known-issues.md#phoenix-modules-escapesroot-resolver-guard-is-untested) for the follow-up unit test.
- [x] **Module-system diagnostics exercise the rich diagnostic shape.** **✅ Completed 2026-04-30.** `crates/phoenix-sema/src/import_resolve.rs::resolve_named_import` emits `Diagnostic::error(format!("`{name}` is private to module `{module}`"), use_span).with_note(definition_span, "declared here").with_suggestion(format!("mark `{name}` as `public` in `{module}` to export it"))` for private-import attempts; `crates/phoenix-sema/src/field_privacy.rs` does the same for cross-module field access (read and write). The note span resolves against its own `SourceId` via `Diagnostic::display_with(&SourceMap)`, so the rendered diagnostic shows `lib.phx:1:10: declared here` even though the primary span is in `main.phx`. Pinned by `negative_import_private_function` in the multi-module negative suite, which asserts message + suggestion + note + cross-file `lib.phx` span all appear in stderr.

When every box above is ticked, Phase 2.6 closes and Phase 2.3 (GC + runtime) becomes the active phase.

### ✅ Phase 2.6 closed (2026-04-30)

Module system + visibility shipped end-to-end. Implementation scope: file-as-module discovery (lazy, import-driven, root = `dirname(entry_file)`), `import a.b.c { Item, Item as Alias, * }` syntax, `public` visibility on functions / structs / fields / enums / traits / type aliases (private-by-default), per-module `module_qualify` mangling so two modules can declare the same name without collision, cross-module name resolution via `ResolvedModule::module_scopes`, visibility enforcement at every lookup site with rich `Diagnostic::error(...).with_note(...).with_suggestion(...)` diagnostics, default-expression wrapper synthesis (the original 2.6 tripwire), and `function main()` reserved for the entry module. The phoenix-modules resolver crate handles BFS discovery + cycle detection + the five `ResolveError` variants (Missing/Ambiguous/Cyclic/Malformed/EscapesRoot). All three backends (`phoenix run`, `phoenix run-ir`, `phoenix build`) handle multi-module input through `lower_modules` / `run_modules` / `parse_resolve_check`. The diagnostic-builder foundation, the `FunctionSlot` and `ValueIdAllocator` IR-shape refactors, and the closure-capture-ambiguity bug fix all landed alongside.

Phase 2.3 (GC + runtime) is the next active phase.

## 2.7 Benchmark Suite

**Status: active (since 2026-05-04, when 2.3 closed).** Sequenced ahead of 2.4 (WebAssembly target) — the native GC needs a measured baseline before a second `GcHeap` impl arrives behind the same trait, and the size-class-arena and typed-allocator follow-ups carried over from 2.3 need a quantitative gate before either is worth building.

A minimal benchmark already exists at [`crates/phoenix-bench/benches/pipeline.rs`](../../crates/phoenix-bench/benches/pipeline.rs): it uses [criterion](https://docs.rs/criterion) to time the **compile pipeline** (lex / parse / sema / IR-lower / Cranelift codegen / IR-interp / tree-walk) on five static fixtures (`empty`, `small`, `medium`, `medium_large`, `large`). It measures **how long the compiler takes**, not how fast the *compiled programs* run. This phase extends the suite with runtime-side benchmarks and establishes a baseline-storage policy.

### Motivation

Phase 2.3 shipped a tracing GC, an open-addressing hash table for maps, and bottom-up merge sort for `List.sortBy`. Each of those decisions has a perf footprint that's currently un-measured. Going forward:

- **Size-class arena** ([decision B](../design-decisions.md#b-heap-layout-segregated-free-lists-by-size-class-single-arena)) is queued behind the registry-on-global-allocator baseline. It only matters if alloc throughput is showing real cost — without a number, no one can decide whether the arena is worth building.
- **Typed allocator threading via `TypeTag`** replaces conservative interior scanning with per-tag trace tables. The win is in mark-phase pause time. Without pause-time numbers, "trace tables made the GC faster" is unverifiable.
- **WASM target (Phase 2.4)** plugs a second `GcHeap` impl in behind the same trait. Without a native baseline, "the WASM GC is X% faster / slower" has nothing to compare against.
- **General regression detection** — functional tests catch correctness, not "someone reverted the hash table to a linear scan and the test still passes because `n=3`".

### Scope

#### Phoenix-only benchmarks (the core deliverable)

- **Allocation throughput bench** — new `crates/phoenix-bench/benches/allocation.rs`. `phx_gc_alloc(size, tag)` in a tight loop, varying object size (16 / 64 / 256 / 1024 bytes) and tag (`Unknown` for conservative scan vs `String` for skip-scan). Drives the heap through repeated grow→sweep cycles so the bench reflects steady-state, not first-allocation cost.
- **GC pause distribution bench** — same file, separate function. Force `phx_gc_collect()` at controlled intervals while a known number of objects are rooted. Sample wall-clock per collection and report P50 / P95 / P99 / max across 1k / 10k / 100k live-object scenarios.
- **Collections bench** — new `crates/phoenix-bench/benches/collections.rs`. `Map.get` / `Map.set` / `Map.remove` at sizes 10 / 100 / 1k / 10k (must be flat — bench confirms hash table); `List.sortBy` at sizes 100 / 1k / 10k (must grow `n log n`, not `n²` — bench confirms the merge-sort shape).
- **End-to-end compiled-program throughput** — extend `pipeline.rs` (or add `compiled_programs.rs`) to time the *compiled binary's* runtime on the existing `medium` / `medium_large` / `large` fixtures, not just the compile pipeline. Catches cumulative regressions that span IR + runtime + codegen.

#### Cross-language comparison (Phoenix vs Go) — informational only

One comparator (Go 1.22+); paired Phoenix and Go programs in `bench-corpus/<workload>/{phoenix,go}/`; off-CI runner; results published to `docs/perf/phoenix-vs-go.md`. Refresh cadence: per-phase close (2.7, 2.4, 2.5 each refresh once). **Not a regression gate** — Phoenix-vs-Phoenix numbers stay the gating signal. Cross-language numbers are positioning awareness — they tell us where the absolute-perf gap is, not whether a PR should land.

- **Workloads:**
  - `sort_ints` — sort 100k random integers via `List.sortBy` (Phoenix) / `slices.Sort` (Go)
  - `hash_map_churn` — 100k inserts followed by 100k lookups, half hits / half misses
  - `alloc_walk_struct` — allocate 1M small structs, walk them once, drop. Exercises the GC, not just the allocator
  - `fib_recursive` — `fib(35)` recursive (no allocation; pure dispatch + arithmetic — isolates inlining / call overhead)
- **Not benchmarked yet** (out of Phoenix's current capability): HTTP servers, JSON parse/serialize, concurrent workloads. These are Phoenix's actual differentiators per the web-framework pitch — but we can't compare them until Phase 4 stdlib lands. The comparison page must document this gap so a reader doesn't conclude "Phoenix is just slower than Go" from compute-only workloads.
- **Why Go specifically.** Closest comparison Phoenix has: GC'd, compiled, statically typed, web-server-friendly. JVM has 25+ years of GC tuning ahead of us; .NET is comparable but less commonly the comp Phoenix users would be coming from; Rust has no GC; TypeScript/Node has a totally different perf model. One comparison that's most predictive of "would a user choose Phoenix over X" — adding more multiplies workload-authoring effort for diminishing positioning value. Decision E (below) explicitly forecloses adding a second language.

#### GC perf follow-ups carried over from 2.3 (conditional)

These land *if and only if* the benches above show they would help. If the numbers say no, the decision flips to "intentionally not pursued" with cited bench output.

- **Segregated free lists by size class** ([decision B in the GC subordinate decisions](../design-decisions.md#b-heap-layout-segregated-free-lists-by-size-class-single-arena)). 2.3 ships the registry-on-global-allocator baseline; 2.7 plugs the size-class arena in behind the same `GcHeap` trait *if and only if* the allocation bench shows it would help.
- **Thread typed allocators through `TypeTag`.** The runtime declares `TypeTag::{List, Map, Closure, Struct, Enum, Dyn}` but the only allocators that pass a non-`Unknown` tag today are the string allocators (`TypeTag::String`). Migrating `phx_list_alloc`, `phx_map_alloc`, the closure-env allocator, and struct/enum allocators to thread their concrete tag through means the GC can swap in trace tables (GC subordinate decision C) instead of conservatively scanning every payload. See the TODO in `crates/phoenix-runtime/src/gc/mod.rs` on `TypeTag` for the migration path. Conservative scan keeps it correct in the meantime; pause-distribution numbers are the gate for whether this lands.

### Design decisions locked in this phase

These pin the bench harness's contract before benches start producing numbers other code might rely on. Full rationale and rejected alternatives live in [design-decisions.md](../design-decisions.md#phase-27-benchmarking); the bullets below are the locked positions.

- **[A. Baseline storage strategy: manual snapshot in `docs/perf-baselines/`](../design-decisions.md#a-baseline-storage-strategy-manual-snapshot-in-docsperf-baselines)** — per-bench markdown table (`bench / parameters / mean / median / stddev / sample-size`), refreshed at phase close. Source files reference the baseline path so a maintainer who cuts a regression knows where to look. Rejected criterion's `--save-baseline` (per-CI-host so cross-host comparison is meaningless) and external services like bencher.dev (third-party dependency on accounts and out-of-repo state). **Decided 2026-05-04**.
- **[B. CI gating policy: post-merge on `main`, alerts but doesn't block](../design-decisions.md#b-ci-gating-policy-post-merge-on-main)** — per-PR gating with N% slack flakes too easily before we know how stable the numbers are; informational-only is too easy to ignore. Implementation: GitHub Actions workflow on `push: main` that runs `cargo bench`, parses criterion output, compares to the committed baseline, and opens an issue if any number regresses by more than 20%. **Decided 2026-05-04**.
- **[C. Calibration and runner constraints](../design-decisions.md#c-calibration-and-runner-constraints)** — pinned CPU governor (`performance`) when the runner permits; minimum 5-run aggregate per bench; criterion default sample size unless variance is unworkable; runner spec documented in the baseline file's header so readers can flag environmental drift; single-threaded runs only in 2.7 (multi-thread comes with Phase 4.3). **Decided 2026-05-04**.
- **[D. Aggregate choice](../design-decisions.md#d-aggregate-choice)** — throughput benchmarks: mean / median / stddev (criterion's defaults). Pause-time benchmarks: P50 / P95 / P99 / max (need the tail to catch worst-case GC stalls). Pick once and stick. **Decided 2026-05-04**.
- **[E. Cross-language comparison scope: Go 1.22+ only, informational, off-CI](../design-decisions.md#e-cross-language-comparison-scope-go-122-only)** — locked at one comparator, the four workloads listed above, off-CI runner, results published to `docs/perf/phoenix-vs-go.md`. Refresh cadence: per-phase-close. Explicitly forecloses adding a second comparator (Java / .NET / TypeScript / Rust were considered and declined) so a future contributor doesn't quietly add another language. **Decided 2026-05-04**.

### Exit criteria for declaring Phase 2.7 complete

Mirror of [§2.2's exit criteria](#exit-criteria-for-declaring-phase-22-complete) and [§2.3's exit criteria](#exit-criteria-for-declaring-phase-23-complete) — minimum gates, not aspirational targets. The size-class arena and typed-allocator-tagging follow-ups are scope items but not exit-criteria gates: they land *if the benches say they should*, otherwise their decision flips to "intentionally not done; baseline numbers in `<path>` showed the rewrite would not have helped."

- [x] **Allocation throughput bench landed** (PR 1, 2026-05-11). `crates/phoenix-bench/benches/allocation.rs` runs the four size buckets × two tag modes; numbers in [`docs/perf-baselines/allocation.md`](../perf-baselines/allocation.md).
- [x] **GC pause distribution bench landed** (PR 1, 2026-05-11). Same file; P50 / P95 / P99 / max for 1k / 10k / 100k via `iter_custom` + warmup-trim. Numbers in [`docs/perf-baselines/pause.md`](../perf-baselines/pause.md).
- [x] **Collections bench landed** (PR 2, 2026-05-11). `crates/phoenix-bench/benches/collections.rs` confirms `Map.get` stays flat (8.8 → 12.0 ns across 100 → 10k, 1.4× range) and `sort_by` grows n log n (587 ns → 96 µs across 100 → 10k, 163×). Numbers in [`docs/perf-baselines/collections.md`](../perf-baselines/collections.md).
- [x] **End-to-end compiled-binary timing** (PR 3, 2026-05-11). `pipeline.rs` adds a `compile_and_run` group; harness (`phoenix_bench::compile_and_link` + `time_run`) caches per-fixture binaries and times subprocess spawn. Only `medium` populates today — `medium_large` and `large` exercise Cranelift gaps (`print(List)`, string-method codegen) that surface cleanly via the `cranelift_ok` skip path. Numbers in [`docs/perf-baselines/pipeline.md`](../perf-baselines/pipeline.md).
- [x] **Cross-language comparison published** (PR 5, 2026-05-12). [`bench-corpus/`](../../bench-corpus/) ships the four locked workloads; [`bench-corpus/run.sh`](../../bench-corpus/run.sh) renders [`docs/perf/phoenix-vs-go.md`](../perf/phoenix-vs-go.md) with absolute numbers, ratios, and the "what's not benchmarked yet (HTTP / JSON / concurrency)" gap call-out.
- [x] **All five subordinate decisions (A–E) implemented** (PRs 1–5). Each carries an `✅ Implemented YYYY-MM-DD` marker in its [`docs/design-decisions.md`](../design-decisions.md#phase-27-benchmarking) entry.
- [x] **Baseline storage decision documented and applied** (PR 4, 2026-05-11). [`docs/perf-baselines/`](../perf-baselines/) populated; [`phoenix-bench-diff`](../../crates/phoenix-bench-diff/) updates and diffs the snapshot; bench source files link back to the baseline path so a maintainer cutting a regression knows where to look.
- [x] **CI integration matches the gating decision** (PR 4, 2026-05-11). [`.github/workflows/bench.yml`](../../.github/workflows/bench.yml) runs the benches on `push: main`, gated on `BENCH_ENFORCE` for noise-floor observation per decision B.
- [x] **Workspace test/clippy/fmt clean** (verified end of PR 6). `cargo test --workspace` green; `cargo clippy --workspace --tests -- -D warnings` zero warnings; `cargo fmt --all -- --check` clean.
- [x] **Each Phase-2.3 perf follow-up resolved one way or the other** (PR 6, 2026-05-12).
  - *Typed-allocator threading via `TypeTag`* — **landed.** `phx_list_alloc`, `phx_map_alloc`, the closure-env allocator, and struct/enum allocators now thread their concrete tag through `phx_gc_alloc`; codegen routes through a new `RuntimeFunctions::gc_alloc` declared `(size, tag) -> ptr`. Before/after pause numbers in [`docs/perf-baselines/pause.md`](../perf-baselines/pause.md). The committed pause numbers dropped 45–77 % across all rooted-object scenarios, but the **mark phase still does conservative interior scanning** (the change provides the substrate for trace tables — GC subordinate decision C — without yet swapping in per-tag mark functions), and the pause bench's own allocations are still tagged `Unknown`, so the bulk of the headline delta is environmental quiescence on the rerun rather than a code-induced win. Per the cited note in `pause.md`: don't over-extrapolate. Trace tables themselves stay deferred until a real pause-distribution signal lands.
  - *Segregated free lists by size class* — **not pursued.** Per [GC subordinate decision B](../design-decisions.md#b-heap-layout-segregated-free-lists-by-size-class-single-arena)'s 2026-05-12 evaluation: `phx_gc_alloc` does carry overhead (~60–130 ns/call vs. `malloc`'s ~10–30 ns), but no program in the bench corpus has alloc throughput as its dominant cost. The cross-language gap that drove the scope conversation is dominated by O(n²) immutable-container builds — addressed by [Phase 2.7 decision F (`ListBuilder` / `MapBuilder`)](../design-decisions.md#f-mutable-builder-api-for-list--map-explicit-types-not-implicit-linearity), not by faster allocators. Reopens when an alloc-throughput-dominated workload (likely Phase 4 HTTP / JSON handlers) lands.

When every box above is ticked, Phase 2.7 closes and Phase 2.4 (WebAssembly target) becomes the active phase.

- **Complexity:** Low for the bench-and-baseline scaffolding (~250 LOC across two new bench files plus storage glue); medium for the size-class-arena follow-up if benches say it should land; medium-to-high for typed-allocator-tagging because it touches every allocator call site. The phase's *minimum* scope is just the scaffolding and the decisions; the follow-ups are conditional.
- **Depends on:** Phase 2.3 (closed) — benches measure GC and collection behavior, both of which only make sense post-GC.

### ✅ Phase 2.7 closed (2026-05-13)

Benchmark suite shipped end-to-end. Implementation scope: allocation throughput + GC pause distribution + Map/sort_by collections + end-to-end compile-and-run criterion benches; `docs/perf-baselines/` snapshot + `phoenix-bench-diff` regression tool + post-merge `bench.yml` workflow with `BENCH_ENFORCE` noise-floor gate; four-workload Phoenix-vs-Go cross-language corpus + hyperfine-driven `bench-corpus/run.sh` + published [`docs/perf/phoenix-vs-go.md`](../perf/phoenix-vs-go.md). The two Phase-2.3 perf follow-ups landed: typed-allocator threading via `TypeTag` is done end-to-end (codegen + runtime); segregated free lists are explicitly deferred per the cited bench output.

Two scope additions were made during the phase, both Phoenix-language-level rather than benchmark-tooling:

- **Decision F (`ListBuilder` / `MapBuilder`)** — added in response to the published cross-language ratios showing Phoenix at 1900× / 6900× slower than Go on `sort_ints` / `hash_map_churn`, dominated by O(n²) immutable-container builds. The builders are transient-mutable accumulators that freeze to immutable `List<T>` / `Map<K, V>`; total build cost drops to O(n). After the rewrite, the published ratios fell to **5.4×** / **3.6×** — a ~350× / ~1900× reduction. Use-after-freeze is runtime-checked (static enforcement is decision G).
- **Decision G (linearity / ownership types: deferred to Phase 4+)** — the linearity story that would let `xs = xs.push(v)` become in-place automatically is real but out of scope for Phase 2; the design exploration is queued for Phase 4 (stdlib pass) or a dedicated phase between 3 and 4. The deferral note exists so a future contributor proposing "add linearity to fix problem X" finds the prior deliberation.

Verified by the workspace test suite plus the targeted integration tests in `crates/phoenix-bench/tests/fixture_validity.rs` (native builder fixtures alongside the existing tree-walk + IR-interp coverage), the bench-suite smoke-runs (`cargo bench --bench {allocation,collections,pipeline} -- --test`), and a full `bench-corpus/run.sh` execution against `go1.23.0` on the dev machine.

Phase 2.4 (WebAssembly target) becomes the next active phase, with the typed-allocator-threaded substrate from PR 6 ready for the WASM `GcHeap` impl to plug into.
