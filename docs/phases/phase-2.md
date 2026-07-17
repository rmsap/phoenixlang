# Phase 2: Compilation

**Status: all of Phase 2 complete (2.1 + 2.2 + 2.3 + 2.4 + 2.5 + 2.6 + 2.7); Phase 3.1 (package manager) is next.** See [§2.5](#25-javascript-interop) for the most recently closed-out writeup (with the `### ✅ Phase 2.5 closed (2026-06-23)` subsection at the end). The 2.5 closeout shipped the `extern js` host-FFI bridge — a uniform `Op::ExternCall` bound by all five backends — with the stubbable interop family byte-identical across all five and the DOM family verified under jsdom + headless Chromium. The 2.7 closeout shipped the benchmark suite + regression detector + cross-language Phoenix-vs-Go corpus, and added `ListBuilder<T>` / `MapBuilder<K, V>` transient-mutable accumulators (decision F) which cut the published `sort_ints` / `hash_map_churn` ratios from 1900× / 6979× to 5.4× / 3.6× against Go.

Move from interpretation to native code generation. This is what makes Phoenix a real language rather than a scripting tool.

## 2.1 Intermediate Representation (IR)

**Status: Complete.** The `phoenix-ir` crate implements an SSA-style IR with basic blocks, typed instructions, and explicit control flow. The lowering pass converts the type-checked AST into IR for all major language features (arithmetic, control flow, structs, enums, match, closures, method calls, collections, try operator, string interpolation). Use `phoenix ir <file.phx>` to inspect the output. The `phoenix-ir-interp` crate provides an IR interpreter for round-trip verification — use `phoenix run-ir <file.phx>` to execute via the IR and compare output with `phoenix run`. Round-trip tests cover all lowered features including the try operator.

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

- **[Generic function monomorphization](../design-decisions.md#generic-function-monomorphization-strategy)** — user generics get one specialized copy per concrete instantiation (symbol-safe specialization names, templates kept as inert stubs, covers generic methods on user-defined types). Concrete type args are embedded in the call so the IR is self-describing. **✅ Implemented 2026-04-20.**
- **[Dynamic dispatch via `dyn Trait`](../design-decisions.md#dynamic-dispatch-via-dyn-trait)** — vtable ABI `(data_ptr, vtable_ptr)`; static dispatch stays the default. **✅ Implemented 2026-04-20** (MVP scope: function params, returns, `let` annotations, struct fields, single-trait-bound). Object-safety is gated at trait-declaration time (see design-decisions.md for the full rule list); non-object-safe traits remain usable as generic bounds (`<T: Trait>`). Heterogeneous list literals ([see known-issues.md](../known-issues.md#listdyn-trait-literal-initialization-in-compiled-mode)) are deferred beyond 2.2.
- **[Centralized `Layout` trait](../design-decisions.md#centralized-layout-for-reference-types)** — single source of truth for reference-type slot count, alignment, load/store codegen. **✅ Implemented 2026-04-19.**
- **[Numeric error semantics](../design-decisions.md#numeric-error-semantics-division-overflow-integer-edge-cases)** — Int operators panic on overflow / divide-by-zero / `i64::MIN` negation (ratifies current behavior); Float follows IEEE 754. Stdlib `Int.checked*` family lands in Phase 4.1.
- **[Post-sema ownership: `ResolvedModule`](../design-decisions.md#post-sema-ownership-resolvedmodule-as-the-semair-handoff)** — sema returns `Analysis` wrapping a `ResolvedModule` (the IR-facing schema: callables, types, per-span maps) plus auxiliary outputs, consumed downstream by stable id — see design-decisions.md for the why. The Phase 2.6 follow-up (FuncId unification of user methods into the function space) landed in the same diff. Sema↔IR id alignment is covered by regression tests. **✅ Implemented 2026-04-24.**

### Bugs closed in this phase

- **Generic user-defined structs in compiled mode** — **✅ Fixed 2026-04-21.** `struct Container<T>` now compiles end-to-end under `phoenix build`, with full method support and correct `dyn Trait` interaction. Resolved by a second-stage struct-monomorphization pass that specializes generic struct layouts and their methods under mangled names (and rekeys `dyn Trait` vtables so `Container<Int>: Trait` vs. `Container<String>: Trait` don't collide); a fixed-point worklist handles recursive generics. Enum-side gate is untouched (separate known-issues entry, Phase 4 target).
- **`<T: Trait>` method calls in compiled mode** — **✅ Fixed 2026-04-21.** `function show<T: Display>(x: T) { x.toString() }` compiles and runs under `phoenix build`; previously it failed with `builtin '.method' not yet supported`. Resolved by rewriting trait-bound method-call markers to direct calls during monomorphization, cooperating with struct-monomorphization when the receiver is a generic struct.
- **`<T: Trait>` → `dyn Trait` coercion in compiled mode** — **✅ Fixed 2026-04-24.** `function f<T: Drawable>(x: T) { let d: dyn Drawable = x }` now compiles; previously it tripped an internal `unreachable!`. Resolved with the same placeholder-then-substitute shape as the method-call fix.
- **Default argument values in compiled mode** — **✅ Fixed 2026-04-24.** `function f(x: Int = 1)` with a call `f()` now runs under `phoenix build`; previously IR lowering trapped on unfilled positional slots. See [design-decisions.md: *Default-argument lowering strategy*](../design-decisions.md#default-argument-lowering-strategy) for the caller-site materialization decision and its tradeoffs.
- **Default arguments on method calls** — **✅ Fixed 2026-04-24.** `impl Counter { function bump(self, by: Int = 1) -> Int { ... } }` with a call `c.bump()` now compiles and runs under all three backends; previously sema rejected the arity mismatch before lowering even saw the call. Same caller-site materialization rule as the free-function fix, extended to the method-call branch. Trait-method defaults remain out of scope — see [known-issues.md](../known-issues.md) for the Phase 3 follow-up. Method-arg coercion is also deliberately out of scope.

### Exit criteria for declaring Phase 2.2 complete

These are the bars that have to clear before Phase 2.2 is closed.  An item with an unchecked box is a real outstanding follow-up, not a stylistic note — every checked-off item below has been verified against the codebase.

- [x] **All Phase-2.2 design decisions implemented.** Every `**[…]**` bullet in §2.2 above carries a `✅ Implemented YYYY-MM-DD` marker (monomorphization, `dyn Trait`, centralized layout, numeric semantics, `ResolvedModule`).
- [x] **All Phase-2.2 bug-closure entries have regression tests.** Each of the five entries under "Bugs closed in this phase" is covered by a topic-specific regression test; reverting the fix would fail those suites.
- [x] **No `known-issues.md` entry is targeted at Phase 2.2.** Outstanding issues are explicitly re-targeted to Phase 3 / 4 or carry no phase tag at all (i.e., they describe pre-existing gaps not committed to 2.2's scope).
- [x] **Workspace test/clippy/fmt clean on the 2.2 branch.** `cargo test --workspace` green; `cargo clippy --workspace --tests` zero warnings; `cargo fmt --all -- --check` clean.
- [x] **Three-backend roundtrip matrix on every `tests/fixtures/*.phx`.** **✅ Completed 2026-04-27.** The backend matrix walks every runnable fixture (the originals plus those added to cover the surface that landed in 2.2 — generics, static/dyn traits, collections, option/result, defaults, closures), runs each under `phoenix run`, `phoenix run-ir`, and `phoenix build` + execute, and asserts byte-identical stdout. `gen_*.phx` fixtures are excluded (they are inputs to `phoenix gen`, not this gate's surface). One `#[test]` per fixture so a divergence names the offender. Building the matrix surfaced one pre-existing parser bug ([interpolated-expression spans collide across functions](../known-issues.md#interpolated-expression-spans-are-zero-based-colliding-across-functions)); the affected pattern is excluded with a pointer to the issue.
- [x] **No `unreachable!()` reachable from a well-typed program in the Cranelift backend.** **✅ Completed 2026-04-27.** Every reachable `unreachable!()` site in the backend's translate layer is replaced with an `ice!(...)` (internal-compiler-error) invocation; every remaining panic is a compiler-bug indicator, not a user-error path (user-error paths return `CompileError`). The claim holds unconditionally for dispatchers that match on `Op` category (the outer router has already classified the op). The residual maintenance hazard: the string-method inner-match arms must stay in lockstep with their outer guard, or a rename turns an inner `ice!` reachable.
- [x] **`phoenix-ir` verifier has one negative test per invariant.** **✅ Completed 2026-04-27.** Hand-written-IR negative tests cover every structural invariant checked by the verifier (missing/invalid terminator, arg-count mismatch, undefined/sentinel operands, value-types desync), combined with the existing placeholder-op and dyn-op verifier tests.

When every box above is ticked, Phase 2.2 closes and Phase 2.6 (modules and visibility) becomes the active phase. Phase 2.3 (GC + runtime), 2.4 (WebAssembly target), and 2.5 (JavaScript interop) remain on the roadmap but are sequenced after the module system: the language-level scaffolding for cross-file code organization is a prerequisite for the package manager (3.1) and LSP (3.2). Compiled binaries continued to leak heap allocations until Phase 2.3 closed (2026-05-04) — see [§2.3](#23-runtime-and-memory-management).

## 2.3 Runtime and Memory Management

**Status: ✅ Complete (2026-05-04).** GC core, `defer` syntax, perf fixes (hash-map, merge-sort), and the valgrind leak-verification gate all landed; every exit-criteria checkbox below is green. Per-decision implementation details live in [Design decisions locked in this phase](#design-decisions-locked-in-this-phase) below.

A minimal runtime already exists as the [`phoenix-runtime`](../../crates/phoenix-runtime/) crate (static library linked into compiled binaries). It currently provides `print` (all value types + strings), `toString`, string comparison and concatenation, all string methods, heap allocation (`phx_gc_alloc`, backed by the tracing GC — see below), panic/abort, `List<T>` data structures (alloc, get, push, contains, take, drop), `String.split` (returns `List<String>`), and `Map<K, V>` data structures (alloc, get, set, remove, contains, keys, values). This phase replaces the leak-everything allocator with a working tracing GC, closes the two perf bugs that were deferred here, and lands the `defer` syntax that the GC decision implied.

- Garbage collector — **tracing GC, mark-and-sweep baseline** (decided 2026-04-19; see [GC strategy](../design-decisions.md#gc-strategy)). Leave room to evolve to generational later without ABI changes. Implementation sub-decisions A–G are pinned in [GC implementation: subordinate decisions](../design-decisions.md#gc-implementation-subordinate-decisions).
- `defer` syntax — Go-style statement-level `defer expr;`, runs LIFO at end of enclosing scope on every exit path (return, panic, fall-through). Decision in [§G of subordinate decisions](../design-decisions.md#g-scope-bound-cleanup-syntax-go-style-statement-level-defer); implementation status pinned in the [Decision G entry](#design-decisions-locked-in-this-phase) below.
- String implementation (UTF-8, immutable by default) — basic ops already in `phoenix-runtime`. **All string allocations move onto the GC heap in this phase** (every `leak_string()` site goes away).
- Panic/abort handler — already in `phoenix-runtime`
- Built-in function implementations (`print`, `toString`) — already in `phoenix-runtime`
- Collection runtime support (List, Map data structures with dynamic resizing) — **basic implementation complete**; map lookup is replaced with a hash-based implementation in this phase (see bugs below).
- Builtin method implementations (String.*, List.*, Map.*, Option.*, Result.*) — **complete** in compiled mode; closure-based list methods (map, filter, reduce, etc.) are compiled inline as Cranelift loops.

### Design decisions locked in this phase

These are sub-decisions of the parent `### GC strategy` decision — they pin the GC ABI and runtime layout and must land before 2.3 wraps. See [design-decisions.md](../design-decisions.md#gc-implementation-subordinate-decisions):

- **[A. Root-finding: precise via shadow stack](../design-decisions.md#a-root-finding-precise-via-shadow-stack)** — push/pop/set-root primitives emitted by Cranelift on function entry/exit and at every ref-typed SSA assignment. **✅ Implemented 2026-05-04.**
- **[B. Heap layout](../design-decisions.md#b-heap-layout-segregated-free-lists-by-size-class-single-arena)** — single global allocator + per-allocation registry. Baseline is Rust's global allocator with a registry (see the *Adopted baseline* note in the design-decisions entry); segregated free lists are tracked as a Phase 2.7 perf-tuning target. **✅ Implemented 2026-05-04.**
- **[C. Object header: 8-byte per-object header](../design-decisions.md#c-object-header-8-byte-per-object-header)** — mark bit + type tag + reserved forwarding-pointer slot + payload size. **✅ Implemented 2026-05-04.**
- **[D. Safepoint placement: at allocation calls only](../design-decisions.md#d-safepoint-placement-at-allocation-calls-only)** — threshold-based (1 MB since last collection); gated behind `phx_gc_enable()`, called by the generated C `main` before `phx_main`. **✅ Implemented 2026-05-04.**
- **[E. `GcHeap` Rust trait abstraction](../design-decisions.md#e-allocator-abstraction-gcheap-rust-trait-single-impl-in-23)** — single `MarkSweepHeap` impl in 2.3. **✅ Implemented 2026-05-04**.
- **[F. Strings join the GC heap](../design-decisions.md#f-strings-join-the-gc-heap-no-interning-in-23)** — every string-allocating runtime entry now goes through the GC heap (tagged `String` so the GC skips the interior scan); no more `leak_string()`. **✅ Implemented 2026-05-04.**
- **[G. `defer` syntax: Go-style statement-level](../design-decisions.md#g-scope-bound-cleanup-syntax-go-style-statement-level-defer)** — **✅ Implemented 2026-05-06.** Plumbed lexer → parser → sema → AST interp → IR lowering; Cranelift and the C backend inherit via the shared IR. Behavioral contract:
  - **Exit paths.** Defers fire on every function-exit terminator: explicit `return`, fall-through return, *and* the `?` (try) early-return path. Closures get their own defer frame, so a `defer` inside a closure fires when the closure returns, not at the enclosing function's exit.
  - **Defer-error policy.** Go-style: every registered defer runs even if an earlier one errored, and the *first* error is propagated (subsequent dropped). Body-error wins over defer-error on the AST interp; on the IR-driven backends hardware traps (e.g. integer division by zero) abort before defers run — see [known-issues.md](../known-issues.md#defer-does-not-fire-on-hardware-trap-body-errors-in-compiled-binaries) for the divergence.
  - **Sema placement rule.** `defer` is permitted only at the outermost statement level of a function, method, or lambda body; anything deeper is a sema error. The rule sidesteps defers that reference bindings from a popped inner scope and defers in untaken branches that would still fire on later exit paths. A future relaxation can lift it once the IR side gains per-iteration dynamic registration.
  - **Lazy-capture semantics.** Free variables resolve at exit, not at the defer point — a deliberate divergence from Go (which evaluates call-expression arguments eagerly), applied uniformly across the AST interp and IR-driven backends.
  - **Fixtures.** Ten matrix fixtures pin each exit path and contract (basic LIFO, explicit-return, lazy-capture, method, GC-heap allocation, closure-defer, `?`-early-return, multiple-explicit-returns, shadowed-at-return, nested-function-frames).
  - **Future use.** Real consumers (file / socket / lock cleanup) come in Phase 4 when the stdlib introduces resources.

### Bugs to be closed in this phase

See [known-issues.md](../known-issues.md):

- **Memory leaks (no GC yet)** — root driver. **✅ Closed 2026-05-04.** A tracing mark-and-sweep GC tracks every allocation, collects unreachable objects when triggered, and respects the shadow stack as a precise root set. Compiled binaries allocate via the GC; the Cranelift backend emits shadow-stack push/set/pop around every function so the GC has precise stack roots; the heap collects on a 1-MB-since-last-collection threshold once the C `main` enables it. Strings are GC-managed (no more `mem::forget`), and process-exit cleanup runs at the end of the generated `main` so the old heap's `Drop` frees every tracked allocation. Verified leak-clean under valgrind (0 / 0 / 0 lost on the 100k-iteration `alloc_loop` fixture) and pinned end-to-end by GC integration tests plus an `RLIMIT_AS` regression test (the companion to the valgrind gate — one catches "GC stops reclaiming during execution," the other "GC reclaims during execution but leaks at termination").
- **O(n) map key lookup** — **✅ Closed 2026-05-04.** Replaced the flat-array linear scan with an open-addressing hash table (linear probing, FNV-1a, 70 % max load factor); lookups, inserts, and removes are now O(1) average. A parallel insertion-order array preserves user-visible iteration order — `Map.keys()` / `Map.values()` return entries in first-inserted order, matching the IR interpreter and the Python / JS / TS user expectation. String keys hash by content. Regression: `map_hash_many_keys.phx` (100 inserts across the grow path + 10 lookups) in the backend matrix.
- **O(n²) `List.sortBy` insertion sort** — **✅ Closed 2026-05-07.** Replaced the Phase 2.2 insertion sort with bottom-up iterative merge sort across all three backends; **O(n log n)** worst case, stable (left run kept first on ties). The non-obvious bit on the Cranelift side is GC rooting: the sort's intermediate buffers exist only as SSA values invisible to the function-level shadow frame, so it pushes its own dedicated shadow frame and roots them there. Both interpreters delegate to a shared `merge_sort_by` helper. Five matrix fixtures cover correctness, the GC-rooting contract (allocating comparator across the collection threshold), edge lengths, string elements, and equal-key stability.

### Exit criteria for declaring Phase 2.3 complete

Mirror of [§2.2's exit criteria](#exit-criteria-for-declaring-phase-22-complete) and [§2.6's exit criteria](#exit-criteria-for-declaring-phase-26-complete) — minimum gates only. Performance benchmarks and tuning targets land in Phase 2.7 (see [§2.7 Benchmark Suite](#27-benchmark-suite)) and do not block 2.3 close.

- [x] **GC subordinate decisions A–F implemented (2026-05-04).** Shadow stack, mark-sweep heap, object header, threshold-triggered safepoints, `GcHeap` trait, GC-managed strings — all landed and exercised by GC integration tests.
- [x] **Decision G (`defer` syntax) implementation (2026-05-04).** Lexer → parser → sema → IR → all three backends. Ten fixtures in the backend matrix; see the Decision G entry above for individual contracts.
- [x] **GC has regression tests.** GC integration tests and the `gc_*.phx` matrix entries cover the precise-roots invariants and the threshold-driven auto-collect path.
- [x] **Map and sortBy regression tests (2026-05-07).** The hash-table contract is pinned by `map_hash_many_keys.phx`; the merge-sort contract by five matrix fixtures (correctness, the GC-rooting contract across the collection threshold, edge lengths, string elements, equal-key stability). Any divergence between the three backends names the offending fixture.
- [x] **No `known-issues.md` entry targeted at Phase 2.3 (2026-05-04).** All three entries deleted as their resolutions landed (map-lookup, sortBy, "Memory leaks (no GC yet)"). Full closure descriptions live in the §2.3 bug-closure bullets above (single source of truth; no stub entries to bit-rot).
- [x] **Workspace test/clippy/fmt clean (post-GC).** `cargo test --workspace` green as of 2026-05-04.
- [x] **Three-backend roundtrip matrix on memory-stress fixtures.** `alloc_loop.phx`, `gc_keeps_alive.phx`, `gc_loop_carried_ref.phx`, and `defer_basic.phx` all pass `phoenix run`, `phoenix run-ir`, `phoenix build`+execute with byte-identical stdout.
- [x] **No leaks under valgrind on `alloc_loop.phx` compiled binary (2026-05-04).** Three fixtures run under `valgrind --leak-check=full`, asserting zero bytes *definitely / indirectly / possibly lost* and capping *still reachable* at 2 KiB (~2× the measured stdout-buffer + GC-init baseline); each reports 0 / 0 / 0 / ≤1024 bytes. **Linux-only gate**; soft-skips when `valgrind` is absent, hard-fails under `PHOENIX_REQUIRE_VALGRIND=1` so a misconfigured runner cannot silently bypass it. The companion `RLIMIT_AS` test catches "GC stops reclaiming during execution"; together they pin both ends of the GC's lifetime contract.

When every box above is ticked, Phase 2.3 closes and Phase 2.7 (Benchmark Suite) becomes the active phase — sequenced ahead of 2.4 (WebAssembly target) so the native GC has a measured baseline before a second `GcHeap` impl arrives behind the same trait.

### ✅ Phase 2.3 closed (2026-05-04)

Tracing GC + runtime shipped end-to-end: precise stack roots via a per-thread shadow stack, 8-byte object headers, threshold-triggered collection (1 MB since last collect), a `GcHeap` trait with a single `MarkSweepHeap` impl, GC-managed strings, and process-exit cleanup so compiled binaries terminate leak-clean under valgrind. `defer` syntax landed across lexer / parser / sema / IR / all three backends with Go-style statement-level placement, lazy-capture semantics, and ten matrix fixtures pinning each exit path. The `O(n)` map-key-lookup bug closed with an open-addressing hash table that preserves insertion order; the `O(n²)` `List.sortBy` insertion sort closed with bottom-up iterative merge sort across all three backends. Verified by the workspace test suite plus GC integration tests, the `RLIMIT_AS` regression test (catches "GC stops reclaiming during execution"), and the valgrind gate (catches "GC reclaims during execution but leaks at termination").

Phase 2.7 (Benchmark Suite) is the next active phase — sequenced ahead of 2.4 (WebAssembly target) so the native GC has a measured baseline before a second `GcHeap` impl arrives, and so the size-class-arena and typed-allocator follow-ups carried over from 2.3 have a quantitative gate.

## 2.4 WebAssembly Target

**Status: ✅ Complete (2026-06-17).** Two WASM backends (`wasm32-linear` embed-and-merge, `wasm32-gc` inline WASM-GC) run byte-identical to native across the full fixture matrix; every exit-criteria box below is green. See the [`### ✅ Phase 2.4 closed (2026-06-17)`](#-phase-24-closed-2026-06-17) subsection at the end of this section for the closeout writeup. Subordinate scope decisions A–D and their rationale live in [design-decisions.md §Phase 2.4 WebAssembly compilation](../design-decisions.md#phase-24-webassembly-compilation).

Add a second `phoenix build` target that emits WebAssembly. Primary target: the WASM GC proposal (standardized, shipping in all major browsers as of 2024 — Chrome 119 / Firefox 120 / Safari 18.2 / Node 21 / wasmtime 18). Linear-memory WASM ships as the fallback for runtimes without WASM GC support, behind the same `GcHeap` trait abstraction that [§2.3 decision E](../design-decisions.md#e-allocator-abstraction-gcheap-rust-trait-single-impl-in-23) was built to enable. The exit-criteria runtime is `wasmtime` CLI; the host-import surface is WASI preview1 only (Phoenix-defined imports are Phase 2.5).

- `--target` flag on `phoenix build` (variants: `native` (default), `wasm32-linear`, `wasm32-gc`).
- WASM emission goes through the Bytecode Alliance's [`wasm-encoder`](https://docs.rs/wasm-encoder) — **not** Cranelift, whose `wasm32` support is input-side only (Cranelift consumes WASM for wasmtime, it doesn't emit WASM). See [design-decisions.md §Phase 2.4 decision A0](../design-decisions.md#a0-wasm-emission-tool-wasm-encoder-not-cranelift). Phoenix's IR is translated directly to WASM ops; sema / lowering / verifier stay codegen-neutral.
- New `WasmLinearMarkSweepHeap` (linear-memory port of `MarkSweepHeap`, backed by a no-std-friendly global allocator like `dlmalloc`) and `WasmGcHeap` (no-op collector backed by WASM GC managed refs) impls of the existing `GcHeap` trait.
- WASI preview1 host imports (`fd_write` for stdout, `proc_exit` for panic/exit) replace native `phx_print_*` / `phx_panic` on wasm32 targets.
- Shadow-stack emission is reused as-is for `wasm32-linear`; for `wasm32-gc` it's bypassed entirely per [§2.3 decision A](../design-decisions.md#a-root-finding-precise-via-shadow-stack)'s explicit Phase 2.4 contract ("WASM GC's typed references replace native root-finding entirely").

The expanded back-bridge work (DOM access, fetch, browser API imports, `extern js` declarations, npm-package resolution) is **Phase 2.5 (JavaScript interop)**, not 2.4. 2.4 ships compilation only — a WASM module that runs under wasmtime is the gate, not a WASM module that runs in a browser.

### PR sequence

The phase ships in 10 PRs (numbered 1–7, with PR 3 split across 3a / 3b / 3c / 3d as each chunk's natural review boundary surfaced during implementation). Sequencing constraint: PR 1 unblocks everything; PR 2 unblocks PR 3a (the runtime port needs the WASM emission scaffold and the WASI import surface); PR 3a → 3b (IR-op expansion needs the real runtime in place) → 3c (the heap-aware surface needs control flow + arith + calls) → 3d (collection / closure / indirect-call lowering needs shadow-stack roots + sret machinery + GC alloc primitives); PR 4 (linear-memory matrix) closes before PR 5 (WASM GC) starts so the matrix stays coherent at every commit; PR 5 locks the WASM GC type-mapping design decision before code lands. The PR 3 split is recorded in [design-decisions.md §Phase 2.4 decisions E–H](../design-decisions.md#phase-24-webassembly-compilation), which also pin the target triple, runtime-delivery model, control-flow translation shape, and string-literal materialization strategy used across 3a–3d.

1. **PR 1** — Backend abstraction + `--target` plumbing. `Target` enum, CLI flag, signature threading. Default `Native`; no behavior change for native builds.
2. **PR 2** — WASM emission scaffolding (via `wasm-encoder`) + WASI host imports. `phoenix build --target wasm32-linear hello.phx` produces a `.wasm` that runs under wasmtime and prints. Minimal translator surface — enough for hello-world; the rest of the IR-op coverage lands in PR 3b alongside the GC port. Integration tests always do structural validation (`wasmparser::validate`) and additionally execute under `wasmtime` when present; CI sets **`PHOENIX_REQUIRE_WASMTIME=1`** to turn the execute-tier skip into a hard failure (same gating shape as the §2.3 `PHOENIX_REQUIRE_VALGRIND` gate).
3. **PR 3a** — Linear-memory runtime port + embed-and-merge infrastructure. Compile `phoenix-runtime` for `wasm32-wasip1` (decision E); discover the resulting runtime `.wasm` and splice its functions / data / globals into the wasm-encoder output with index fix-up (decision F), replacing PR 2's hand-synthesized helpers with the merged real-runtime symbols. `_start` wires GC enable + `phx_main` + shutdown. The deliverable is "the GC and runtime are wired in via the real Rust crate" rather than "more fixtures run" — only hello.phx exercises this PR.
4. **PR 3b** — Control-flow + integer-arith translator surface. Int arith, full integer / bool comparisons, multi-block control flow via the loop+switch dispatcher (decision G), value-returning return, direct user-function calls. `fibonacci.phx` and negative-integer programs run end-to-end through wasmtime. **Strings, the shadow-stack root-emission pass, structs / enums / lists / maps / closures, and `defer` defer to PR 3c** — a data-section / `__heap_base` collision on string lowering surfaced mid-implementation and grouped better with PR 3c's heap-aware surface.
5. **PR 3c** — Strings + heap primitives + shadow-stack roots. Multi-commit; status as of 2026-05-18.
   - ✅ **Landed**: string-literal materialization via data-section borrowed pointers ([decision H](../design-decisions.md#h-string-literal-materialization-data-section-borrowed-pointers)); *sret* call machinery; `toString` for Int / Bool (the Float arm wired but blocked on float-const support); `print(String)` via the merged runtime. Fixtures unlocked: `defer_basic`, `fizzbuzz`.
   - ⏳ **Pending**: string concat; mutable-variable load/store; float const + float arithmetic (gates the `toString(Float)` arm); the shadow-stack root-emission pass; struct and enum ops. Fixtures pending these: `defaults`, `features`, `alloc_loop`, most `defer_*` variants. `defer` exit-path correctness comes "for free" from the IR having already linearized defers into the function-exit boundary — the translator just needs the underlying ops working.
6. **PR 3d** — Collections + closures + indirect calls. List / map allocation + all functional methods; closure allocation + capture load; indirect call via the env-pointer ABI ([Phase 2.6 closure-capture-ambiguity fix](../design-decisions.md#interpreter-parser-coupling-via-valueclosure--reverted-2026-04-27)); the Phase 2.7 builder ops. Every fixture in `tests/fixtures/*.phx` compiles and executes leak-clean under wasmtime by the close of this PR.
7. **PR 4** — Four-backend matrix (add `wasm32-linear`). Exit gate for the linear-memory variant.
8. **PR 5** — WASM GC type-mapping design decision (added to [design-decisions.md §Phase 2.4](../design-decisions.md#phase-24-webassembly-compilation)) + `WasmGcHeap` skeleton + initial codegen. Shadow-stack emission suppressed.
9. **PR 6** — WASM GC across the matrix (five backends). Exit gate for the WASM GC variant.
10. **PR 7** — Phase close: refresh the perf doc with WASM-vs-native numbers across the four corpus workloads; tick every exit-criteria box below.

### Bugs closed in this phase

- **Parser hung (infinite loop) on malformed struct/query/headers fields** — **✅ Fixed 2026-06-10**, alongside the field-syntax unification (see [design-decisions.md §Field declarations use colon syntax](../design-decisions.md#field-declarations-use-name-type-colon-syntax)). The struct-field, query-block, and headers-block parse loops left the cursor untouched when a field failed to parse, re-peeking the same token forever — any unexpected token in those bodies spun the process at 100% CPU instead of producing a diagnostic (effectively a DoS for any tooling that parses in-progress user code). All three loops now `synchronize_stmt()` on the failure path so every iteration consumes at least one token, and the old type-first shape gets a targeted migration diagnostic. Covered by regression tests.

- **Closure functions inside generic templates are not cloned per specialization** — **✅ Fixed during PR 3d.** Monomorphization now clones closure functions per substitution of their enclosing generic, so each specialization gets concrete capture / return types instead of the shared `__generic` placeholder — fixing both a single-width `call_indirect` type mismatch on wasm32 and a cross-width heap mis-sizing on native. A secondary latent bug was fixed alongside: the type-var-erasure pass did not erase closure capture types, so a leftover unreferenced template-copy closure reached codegen with a residual `TypeVar` and ICE'd native codegen. **Architectural note — the GC-root placeholder guard is kept by design:** the backend's `is_tracked_ref` returns `false` for type-var / generic-placeholder types, because placeholder ref types still legitimately reach codegen on dead/unconstrained paths (the unreferenced original generic closures persist as dead concrete functions with `__generic` captures, and unannotated empty list/map literals leave `__generic` placeholders). Those inert paths are enumerated in the monomorphizer's "Placeholder paths (canonical reference)" docstring — the authoritative anchor `is_tracked_ref` cross-references — so the guard must NOT be tightened to a `debug_assert!`. Verified on all three backends by single- and cross-width generic-closure fixtures, wasm-execution tests under wasmtime, and a structural assertion that ref-typed sortBy emits its ad-hoc shadow frame (pinning that cloned capture types reach the GC-root emitter as concrete refs).

- **`Map<Bool, V>` lookups and `List<Bool>.contains` always missed on native** — **✅ Fixed 2026-06-14**, surfaced while adding Bool-key coverage for the wasm32-gc maps slice (K.9). `Bool` occupies a full 8-byte container slot but the layout's store emitted a 1-byte store, leaving the upper 7 bytes uninitialized; the type-erased list/map runtimes hash and byte-compare the whole slot width, so two equal `Bool` keys built in different stack slots hashed differently from their residual garbage. The interpreters compare values structurally, so this was a silent native-only divergence the matrix hadn't caught (no fixture used Bool keys). The fix zero-extends any sub-slot-width scalar to the full slot before the store, making the whole slot deterministic. Regression: `map_bool_keys.phx` (five-backend matrix) plus a wasm32-gc execution test.

### Exit criteria for declaring Phase 2.4 complete

Per-decision rationale lives in [design-decisions.md §Phase 2.4 WebAssembly compilation](../design-decisions.md#phase-24-webassembly-compilation).

- [x] **All Phase-2.4 design decisions implemented (2026-06-17).** Sub-decisions A–D are scope/process decisions (dual-backend split, wasmtime-CLI exit runtime, WASI-only host surface, bench-refresh scope) carrying `**Decided:** 2026-05-15` markers — evidenced by the other boxes below, not a single code artifact. Every substantive codegen decision carries a `✅ Implemented YYYY-MM-DD` marker: A0 (`wasm-encoder`), E–J (target triple, embed-and-merge runtime delivery, loop+switch control flow, string-literal materialization, MVP scope), and the WASM-GC type-mapping series **K.0–K.12**. The two algorithmic gaps the corpus surfaced during the close — `List.sortBy` O(n²)→O(n log n) merge sort on both wasm backends, and the wasm32-gc `Map` hash *index* (K.9) — landed with the same marker convention.
- [x] **Backend abstraction landed (PR 1).** `Target` enum (`Native` / `Wasm32Linear` / `Wasm32Gc`) dispatched in compile; `--target` CLI flag parsed in the driver. Default = `Native`; native fixtures stay byte-identical (the native column is the baseline every other column is compared against).
- [x] **`phoenix build --target wasm32-linear <file>` produces a `.wasm` that runs under `wasmtime` with byte-identical stdout** to `phoenix run` / `phoenix run-ir` / native `phoenix build` — pinned per-fixture by the `wasm32-linear` matrix column. The linear-memory `MarkSweepHeap` is the same `phoenix-runtime` crate compiled for `wasm32-wasip1` and embedded-and-merged into the output (decision F).
- [x] **`phoenix build --target wasm32-gc <file>` ditto under `wasmtime -W function-references=y,gc=y`.** Pinned by the `wasm32-gc` matrix column. Shadow-stack emission is **structurally absent** rather than suppressed-after-the-fact: the GC backend runs no runtime merge and synthesizes its helpers inline, so the push/set/pop primitives are never declared or called; the host VM traces reference-typed locals. A host-surface test additionally proves an extern-free module imports nothing outside `wasi_snapshot_preview1`.
- [x] **Four-backend matrix on every fixture** (`wasm32-linear` column). One `#[test]` per fixture; a divergence names the offender.
- [x] **Five-backend matrix on every fixture** (`wasm32-gc` column added to both the single- and multi-module matrices). The only fixture skipping the wasm32-gc column is `gc_loop_carried_ref.phx` — an explicit wasmtime-host-GC *throughput* carve-out (50k growing-string concats take minutes under the host VM's GC, output verified correct at reduced counts), not a codegen gap; the same rooting path stays covered by a smaller variant.
- [x] **WASI imports (`fd_write`, `proc_exit`) are the only host surface.** No Phoenix-defined custom imports (those are Phase 2.5). Enforced per backend by the `extern_free_program_imports_only_wasi_on_wasm32_{linear,gc}` tests (renamed from `only_wasi_imports_*` in Phase 2.5 once `extern js` began adding `js.*` imports), which walk the import section and assert every import lives in `wasi_snapshot_preview1`. The merged linear runtime pulls `fd_write` / `proc_exit` plus a few more from Rust's wasip1 std startup — all WASI; the GC backend imports only `fd_write`.
- [x] **Linear-memory leak-clean at exit (2026-06-17).** GC shutdown runs on `_start` exit (swaps in a fresh heap and drops the old one, freeing the whole allocation registry — the same teardown the native valgrind gate verifies). There is no valgrind under wasmtime, so the wasm-side gate borrows the native `RLIMIT_AS` strategy: the 100k-iteration `alloc_loop` fixture runs with wasmtime's pooling allocator capping each linear memory at 6 MB — above the working-GC ~3.9 MB high-water, below the ~9.8 MB leak-everything footprint — so a GC that stopped reclaiming fails `memory.grow` and traps. Verified to have teeth (a 2 MB override forces the trap). Hard-fails under `PHOENIX_REQUIRE_WASMTIME=1`.
- [x] **Phase-close bench refresh (2026-06-17).** [`docs/perf/phoenix-wasm-vs-native.md`](../perf/phoenix-wasm-vs-native.md) publishes `wasm32-linear` / `wasm32-gc` vs `native` across the four corpus workloads (fib 2.3×/2.9×, sort_ints 2.5×/1.3×, hash_map_churn 2.0×/1.3×, alloc_walk 2.0×/2.3×), regenerated by `bench-corpus/run-wasm.sh` (per [decision D](../design-decisions.md#d-phase-close-bench-refresh-scope-wasm-vs-native-phoenix-only)).
- [x] **No `known-issues.md` entry targeted at Phase 2.4 (2026-06-17).** The single mention of "Phase 2.4" in `known-issues.md` is backlog *framing* in the Phase-2.7 perf-opportunities survey, not an entry blocking or scoped to 2.4.
- [x] **Workspace test/clippy/fmt clean (2026-06-17).** `cargo test --workspace` green under `PHOENIX_REQUIRE_RUNTIME_LIB=1 PHOENIX_REQUIRE_RUNTIME_WASM=1 PHOENIX_REQUIRE_WASMTIME=1`; `cargo clippy --workspace --all-targets -- -D warnings` zero warnings; `cargo fmt --all -- --check` clean.
- [x] **CI integration matches the gating shape.** The `check` job builds the wasm runtime then runs the driver and cranelift test crates under `PHOENIX_REQUIRE_RUNTIME_WASM=1` + `PHOENIX_REQUIRE_WASMTIME=1`, exercising both wasm targets (wasm32-gc via the GC-flagged harness) and the new leak / host-surface gates with no silent skips.

When every box above is ticked, Phase 2.4 closes and Phase 2.5 (JavaScript interop) becomes the active phase.

### ✅ Phase 2.4 closed (2026-06-17)

Phoenix now compiles to WebAssembly on two backends, both byte-identical to native across the full fixture matrix. `wasm32-linear` embeds-and-merges the `phoenix-runtime` crate compiled for `wasm32-wasip1` (linear-memory `MarkSweepHeap`, explicit shadow-stack roots); `wasm32-gc` emits inline WASM-GC (`struct.new` / `array.*` / `call_ref` over a rec-group of nominal types), delegating tracing to the host VM with no runtime merge and no shadow stack. The host surface is WASI preview1 only — a module that runs under `wasmtime` is the gate; browser/JS interop is Phase 2.5.

The phase-close bench refresh ([`docs/perf/phoenix-wasm-vs-native.md`](../perf/phoenix-wasm-vs-native.md)) was the first thing to exercise the collections at 100k scale, and it surfaced three pre-existing gaps that were closed before the phase shut: (1) `ListBuilder<T>` / `MapBuilder<K,V>` were compiled-backend-only — they now have full five-backend parity (both wasm backends + both interpreters); (2) `List.sortBy` shipped an O(n²) insertion sort on both wasm backends — replaced with the bottom-up merge sort native already used; (3) the `wasm32-gc` `Map` was an ordered-array O(n) linear scan — augmented with an open-addressing hash index (K.9) so `get` / `contains` / construction are O(1). All three were verified five-backend byte-identical. Final WASM-vs-native ratios land at 2.0–2.9×, dominated by `wasmtime` startup + JIT on these ~50–100 ms workloads.

Leak-cleanness at exit is gated wasm-side by a bounded-memory trap (the valgrind analog has no wasmtime equivalent): the 100k-allocation `alloc_loop` runs under a 6 MB pooling-allocator cap that a non-reclaiming GC would blow. CI runs both wasm targets under `PHOENIX_REQUIRE_WASMTIME=1` with no silent skips.

**Bugs closed in this phase**

- **wasm32-gc `Map<K,V>` O(n) lookup** — the ordered-association representation linear-scanned on every `get` / `contains` / dedup, making `hash_map_churn` (200k lookups over a 100k-entry map) ~380× slower than native. Closed by an open-addressing hash *index* over the still-insertion-ordered arrays (K.9); output stays byte-identical, only lookup speed changed.
- **`List.sortBy` O(n²) on both wasm backends** — a deliberate insertion sort (to avoid merge sort's many-edged CFG) made `sort_ints` (100k elements) ~1000× slower than native. Closed by porting native's bottom-up iterative merge sort to both wasm backends, each with the rooting discipline its backend needs (an ad-hoc shadow frame on linear; host-VM auto-rooting on gc).
- **Builders missing on the wasm backends and both interpreters** — `List.builder()` / `Map.builder()` were Cranelift-native-only (a Phase 2.7 known limitation: builder programs ran under `phoenix build` but not `phoenix run` / `run-ir`). Now lowered on both wasm backends and both interpreters, with last-wins-first-position freeze dedup matching the compiled backends; pinned five-backend by `builders.phx`.

## 2.5 JavaScript Interop

**Status: ✅ Complete (2026-06-23).** `extern js` is a uniform host-FFI boundary bound by all five backends — the two interpreters (registered Rust host table), native (linked C-ABI shim with weak-default override), and both WASM targets (generated JS glue, copy-marshalled on `wasm32-linear` / `externref` on `wasm32-gc`) — with the stubbable interop fixture family running byte-identical across all five and the DOM family verified under jsdom + headless Chromium on both WASM targets. Every exit-criteria box below is green. See the [`### ✅ Phase 2.5 closed (2026-06-23)`](#-phase-25-closed-2026-06-23) subsection at the end of this section for the closeout writeup; subordinate design decisions are pinned in [design-decisions.md §Phase 2.5 JavaScript interop](../design-decisions.md#phase-25-javascript-interop).

Build the host-FFI bridge that lets Phoenix call hand-declared JavaScript / browser APIs, marshal values across the boundary, and pass Phoenix closures as JS callbacks. Per [decision A0](../design-decisions.md#phase-25-javascript-interop), `extern js` is a **uniform host-FFI boundary**: the grammar, the `JsValue` type, marshallability rules, and the generic `Op::ExternCall` host-call node are backend-neutral, and each execution backend binds that call — the interpreters via a registerable Rust host table, native via a C-ABI host shim, and both WASM targets via a generated JS glue layer. This is the language-level interop layer; the npm-ecosystem layer (package resolution + bundling) rides a later phase.

- `extern js` declarations for typing JS functions and objects that have no Phoenix implementation (bodyless signatures).
- Automatic marshalling of Phoenix scalar/string/`JsValue` values across the host boundary, with per-backend bindings (copy-marshalling on `wasm32-linear`, `externref` on `wasm32-gc`, direct `Value` dispatch in the interpreters, C-ABI in native).
- Phoenix closures pass to the host as callbacks (no `async`/`await` — see below); JS async APIs (`fetch`, `setTimeout`) are modeled callback-style.
- A generated JS **glue layer** emitted alongside the `.wasm` (Node + browser entry variants) that instantiates the module and satisfies the declared imports.
- **Mechanism parity, not effect parity:** stubbable interop fixtures run byte-identical on all five backends; only genuinely DOM-touching fixtures are browser-tier. Tested under **Node.js** (always-on gate) and a **headless browser** (DOM-verification tier), plus the existing native + interpreter matrix columns.

```phoenix
// Declare external JS functions available at runtime (hand-declared host API).
extern js {
  function alert(message: String)
  function setTimeout(callback: () -> Void, ms: Int)
}

function main() {
  // Phoenix closures cross the boundary as JS callbacks — no async/await needed.
  setTimeout(function() { alert("Hello from Phoenix!") }, 300)
}
```

### Scope boundaries (carved out, with forward pointers)

- **npm package resolution is deferred to Phase 3.1.** `import js "pkg" { ... }` string-source imports, `[js-dependencies]` in `phoenix.toml`, and bundler/npm integration depend on a package manager that does not exist yet (today `phoenix.toml` carries only `[gen]`, and the module resolver is filesystem-only). 2.5 ships **hand-declared** `extern js` host/browser APIs only; the npm slice rides with / after the package manager. See [design-decisions.md §Phase 2.5 decision J](../design-decisions.md#j-npm-package-slice-deferred-to-phase-31).
- **async / await / `Promise` are deferred to Phase 4.3** (the async runtime). The language has no async support today; modeling it here would commit an async-shaped surface ahead of the runtime it must reconcile with. JS async APIs are callback-style in 2.5. The roadmap's earlier `async function main()` interop sketch is revised to the callbacks-only form above.
- **Aggregate marshalling (struct / enum / `List` / `Map`) across the boundary is future work.** Only `Int` / `Float` / `Bool` / `String` / `JsValue` / `Void` and marshallable-closure types cross in 2.5.

### PR sequence

The phase ships in ~19 PRs. Per [decision A0](../design-decisions.md#phase-25-javascript-interop), `extern js` is a **uniform host-FFI boundary**: the user-facing surface and the IR — `extern js` grammar, the `JsValue` type, marshallability rules, and the generic `Op::ExternCall` host-call node — are **backend-neutral and designed once** (PRs 1–3), and each execution backend gets a **host binding** for that call. The two interpreters, native, and the linear backend land their bindings first (giving a four-column interop matrix); the WASM-GC binding is then a purely additive `externref` layer (mirroring §2.4's "linear reaches the matrix gate before WASM GC starts" discipline). Mechanism parity, not effect parity — interop fixtures whose host functions are stubbable rejoin the five-backend byte-identical matrix; DOM-only fixtures stay browser-tier.

**Shared front-end + IR (backend-neutral):**

1. **`extern js` grammar.** `extern` keyword + bodyless function signatures (AST) + parser, with `synchronize_stmt()` on the failure path (the §2.4 anti-hang discipline). A body on an extern signature is a targeted parse error. Front-end plumbing only.
2. **Sema: `JsValue` + extern registration + marshalling rules.** Add an opaque `JsValue` type (pre-registered like `Option` / `Result`); register each extern signature as an extern-flagged callable with `(module, name)` linkage. Type-check rule: every extern param and return type must be *marshallable* (`Int`/`Float`/`Bool`/`String`/`JsValue`/`Void`, or a function of marshallable parts) — non-marshallable boundary types produce a diagnostic, not a silent pass. Sema also rejects `extern js` in a Gen-consumed schema (it is an executable-language feature, not a schema feature — the Gen backends would otherwise drop it silently).
3. **IR: generic `Op::ExternCall` host-call node.** New op carrying `(module, name)` + arg ids; `IrType::JsValue` mirrors the sema type. The op is backend-neutral — it names a host call, and each backend's binding (PRs 4 / 5–8 / 9 / 12–15) decides how it executes.

**Interpreter host binding (lands early — pure Rust, unblocks `phoenix run` / `run-ir` interop + their matrix columns):**

4. **Interpreter host-FFI table (AST + IR interp).** A registerable Rust host-function table keyed by `(module, name)`; an extern call dispatches to the registered closure with `Value` args. Unbound is a clear runtime error naming the missing `(module, name)`, never a silent no-op. The test harness registers Rust stub closures.

**Linear WASM binding (ships + gates before WASM-GC):**

5. **wasm32-linear custom-import emission.** Declare one WASM function import per distinct extern, with index fix-up through the runtime merge; lower the extern call to `call $import`. The `only_wasi_imports_*` structural test relaxes to "imports outside WASI are exactly the declared externs."
6. **Generated JS glue + paired `.wasm` + `.js` artifact.** `phoenix build --target wasm32-linear` emits a sidecar `.js` glue module (Node + browser entry variants over a shared marshalling core) that instantiates the module and wires WASI + the declared externs as JS thunks. Driven by the same extern table the import section was built from, so names/signatures cannot drift. **Starts after PR 5 closes** so import section and glue stay coherent at every commit.
7. **Linear value-marshalling helpers.** Encode/decode for each scalar + `String` + `JsValue` across linear memory. Strings are **copied** at the boundary (out via `TextDecoder` over `(ptr, len)`; in via the GC string allocator). `JsValue` is an opaque `i32` handle into a JS-owned handle table. Co-lands with PR 6 as the glue's core, split as its own review unit.
8. **Linear closures-as-callbacks.** A closure crosses as its env-pointer pair; the module exports an invoke trampoline; the glue wraps the pair in a JS callable and retains it; the Phoenix side roots the retained closure. Freeing is **explicit** (a drop extern / `FinalizationRegistry` tie-in) — callbacks-only async has no `Promise` to anchor lifetime. The host-never-released path is the linear-only known issue.

**Native host binding:**

9. **Native C-ABI host shim.** Each distinct extern lowers to a call of a C-ABI symbol with the native value ABI, resolved from a linked host shim; closures cross via an exported invoke entry point the shim calls back through. Default when no shim symbol is linked: a weak shim that aborts with a clear message naming the extern (loud, never silent). The test harness links a host shim providing stub bodies so native interop fixtures produce identical stdout.

**Test tiers:**

10. **Node test harness (always-on gate).** Per fixture: build → load the glue under Node → assert captured output against a baseline. Mirrors the wasmtime soft-skip shape: skip with a visible warning when `node` is absent, hard-fail under a new **`PHOENIX_REQUIRE_NODE=1`** gate. Node provides deterministic synchronous host stubs so output is byte-stable. Built generic over the WASM target so the WASM-GC binding (PR 15) joins without restructuring.
11. **Browser / DOM verification tier (gated soft-skip).** A headless-browser tier loads the *browser* glue variant in a real page and verifies DOM-mutating externs actually mutate the DOM and a closure-registered event handler fires. Gated by **`PHOENIX_REQUIRE_BROWSER=1`**; soft-skip otherwise. DOM coverage is a curated hand-declared subset.

**WASM-GC binding (additive — introduces `externref`):**

12. **wasm32-gc `externref` + custom-import emission.** Introduce `externref` into the WASM-GC backend (`JsValue` → `(ref null extern)`); declare custom imports; lower the extern call to `call $import`. Relax `only_wasi_imports_on_wasm32_gc` as PR 5 did for linear.
13. **WASM-GC value-marshalling helpers.** `JsValue` is an `externref` passed **directly** — no handle table, the host VM owns and traces it. Scalars as before; strings copied out/in via the GC backend's small linear-memory scratch region.
14. **WASM-GC closures-as-callbacks.** The closure crosses as a managed ref; the glue holds it via `externref` / `funcref` so the **host VM GC traces a host-retained callback automatically — no manual rooting, no explicit-free leak** (the WASM-GC win over the linear binding).
15. **WASM-GC glue variant.** The externref-aware GC entry variant over the PR-6 shared core; `phoenix build --target wasm32-gc` emits its paired `.js`, and the Node/browser tiers add the WASM-GC column.

**Five-backend integration + close:**

16. **Five-backend interop matrix + inventory.** The canonical interop fixture family (string out, scalar round-trip, `JsValue` pass-through, closure-as-callback, string in) runs on **all five backends** asserting byte-identical stdout. Registered as full matrix members — **not excluded**. DOM-only fixtures remain browser-tier (the effect they assert exists only in a browser); that carve-out is the *only* exception and is asserted, not silent.
17. **CI wiring.** Promote Node to the always-on job under `PHOENIX_REQUIRE_NODE=1`, exercising both WASM targets with no silent skips; native interop runs with the test host shim linked; the browser tier runs as a gated job. The existing wasmtime steps remain green.
18. **Interop boundary-cost bench.** A micro-bench of per-call marshalling overhead (a string round-trip vs. a pure call) across the bindings under Node + native, regenerated by `bench-corpus/run-interop.sh`; published to `docs/perf/`.
19. **Phase close.** Write the §2.5 closeout + "Bugs closed in this phase," tick every exit-criteria box, record the design decisions, open the deferred-item known-issues, and confirm the `async` example revision.

**Sequencing constraint:** PR 1 unblocks everything; 1 → 2 → 3 is the backend-neutral front-end → IR spine. PR 4 (interpreter binding) lands early and cheaply, bringing up the `run` / `run-ir` interop columns first. The linear WASM binding runs 5 (import declaration) → **5 closes before 6 starts** → 6/7 (glue + marshalling co-land) → 8 (callbacks). PR 9 (native shim) and PRs 10–11 (Node + browser tiers) can land in parallel once the linear binding exists, giving a four-backend interop matrix before WASM-GC starts. The WASM-GC binding (12–15) is additive on the shared front-end + glue core + harness. PR 16 then locks all five columns of the interop matrix (it cannot go green until every binding — 4, 9, 8, 15 — has landed), 17 wires CI, 18 benches the boundary, 19 closes.

### Design decisions locked in this phase

Full rationale and rejected alternatives in [design-decisions.md §Phase 2.5 JavaScript interop](../design-decisions.md#phase-25-javascript-interop):

- **[A0. Parity model: uniform host-FFI](../design-decisions.md#phase-25-javascript-interop)** — `extern js` is a uniform host-FFI boundary; the generic `Op::ExternCall` is bound per backend (WASM→JS glue, interpreters→registerable Rust host table, native→C-ABI host shim). Mechanism parity, not effect parity: stubbable interop fixtures rejoin the five-backend byte-identical matrix; unbound externs error loudly. DOM-only fixtures stay browser-tier.
- **[A. Host set & gating](../design-decisions.md#a-host-set--gating-node-always-on-browser-gated)** — Node is the always-on CI gate (`PHOENIX_REQUIRE_NODE=1`, mirroring `PHOENIX_REQUIRE_WASMTIME`); headless browser is a gated DOM-verification tier (`PHOENIX_REQUIRE_BROWSER=1`).
- **[B. Both WASM bindings ship: `wasm32-linear` and `wasm32-gc`](../design-decisions.md#b-wasm-host-bindings-both-wasm32-linear-and-wasm32-gc-ship)** — the two WASM host bindings of A0; linear uses copy-marshalling, WASM-GC introduces `externref`. Linear ships and gates first; WASM-GC is additive.
- **[C. Glue-artifact shape](../design-decisions.md#c-glue-artifact-shape-paired-sidecar-js)** — paired sidecar `.js` (Node + browser variants) over a shared marshalling core with per-backend encode/decode plugins, driven by the extern table.
- **[D. `JsValue` representation](../design-decisions.md#d-jsvalue-representation-per-backend-same-user-facing-type)** — per-backend (same user-facing type): an `i32` handle into a JS-owned table on linear; an `externref` passed directly on WASM-GC.
- **[E. Extern-call ABI](../design-decisions.md#e-extern-call-abi-per-backend-marshalled-signatures)** — per-backend marshalled signatures; one custom import per distinct `(module, name)`.
- **[F. String ownership across the boundary](../design-decisions.md#f-string-ownership-across-the-boundary-copied-never-shared)** — copied on both backends, never shared/borrowed.
- **[G. Closures-as-callbacks lifetime](../design-decisions.md#g-closures-as-callbacks-lifetime-per-backend)** — linear: trampoline + retention table + GC root + explicit free; WASM-GC: host-VM-traced, no manual lifetime.
- **[H. Async = callbacks-only](../design-decisions.md#h-async-model-callbacks-only)** · **[I. DOM coverage = curated hand-declared subset](../design-decisions.md#i-dom-type-coverage-curated-hand-declared-subset)** · **[J. npm slice deferred to Phase 3.1](../design-decisions.md#j-npm-package-slice-deferred-to-phase-31)** · **[K. Extern declarations are signature-only — no inline JS bodies](../design-decisions.md#k-extern-declarations-are-signature-only-the-host-is-supplied-separately-no-inline-js-bodies)**.

### Bugs closed in this phase

**None.** Phase 2.5 is greenfield — `extern js` is a new language surface, so there were no pre-existing `known-issues.md` entries about interop to close. Rather than close bugs, the phase **opened** five documented *forward-deferred* limitations (all linear- or platform-specific residuals of the marshalling model, none blocking 2.5): the [retained-callback pin leak](../known-issues.md#a-retained-extern-js-callback-is-pinned-for-the-programs-life-on-wasm32-linear) and [per-invocation `JsValue` handle interning](../known-issues.md#a-jsvalue-argument-to-an-extern-js-callback-interns-a-handle-per-invocation-on-wasm32-linear) on wasm32-linear, the [`JsValue`-as-field/capture gap](../known-issues.md#jsvalue-cannot-be-stored-as-a-struct-field-or-closure-capture-on-wasm32-linear), the [wasm32-gc 4095-byte extern-string cap](../known-issues.md#wasm32-gc-extern-js-strings-are-capped-at-4095-bytes), and [native interop being ELF/Mach-O-only](../known-issues.md#native-extern-js-interop-is-elfmach-o-only-no-windowscoff-weak-override). Each carries a "demand-triggered / forward deferral" target and a sketched fix. (The `fix(gen): …` commits in this calendar window belong to the separate **Phoenix Gen** track, not Phase 2.5.)

### Exit criteria for declaring Phase 2.5 complete

Mirror of [§2.4's exit criteria](#exit-criteria-for-declaring-phase-24-complete) — minimum gates, not aspirational targets.

- [x] **All Phase-2.5 design decisions implemented.** The binding/codegen decisions **C–G** (glue-artifact shape, `JsValue` representation, extern-call ABI, string ownership, closures-as-callbacks lifetime — each spanning the WASM, interpreter, and native bindings of A0) each carry an `✅ Implemented YYYY-MM-DD` marker per binding; **A0** carries an `✅ Implemented 2026-06-23 (PR 16)` marker (the five-backend matrix); the scope/process decisions **A, B, H, I, J, K** carry `**Decided:**` markers — their realization is evidenced by the other boxes below, not a single code artifact (mirroring §2.4's A–D split above).
- [x] **`extern js` parses, type-checks, and lowers.** A bodyless `extern js { ... }` block registers marshallable signatures; a non-marshallable boundary type produces a diagnostic, and a body on an extern signature is a targeted parse error. Pinned by parser unit tests + sema negative tests; lowering to the generic `Op::ExternCall` is exercised by every interop test below.
- [x] **Paired `.wasm` + `.js` glue on both WASM targets.** `phoenix build --target wasm32-linear` and `--target wasm32-gc` each emit a glue sidecar; the import section declares exactly the WASI imports plus the program's declared externs, enforced per backend by the `extern_free_program_imports_only_wasi_on_wasm32_{linear,gc}` tests plus interop compile tests that assert the declared `js.*` imports appear.
- [x] **`extern js` executes on every backend via its host binding.** `phoenix run` / `run-ir` dispatch to a registered Rust host table; native resolves through a linked C-ABI host shim; both WASM targets through the JS glue under Node. An unbound extern fails loudly — a clear runtime error in the interpreters, an aborting weak default in native — never silently. Pinned per binding.
- [x] **Node interop tier byte-identical to baselines across both WASM targets**, under the always-on `PHOENIX_REQUIRE_NODE=1` gate (6 fixtures × 2 targets): scalar round-trip, string in + out, multi-byte UTF-8 round-trip, `JsValue` handle/externref pass-through, closures-as-callbacks, and host-side-effect ordering.
- [x] **Browser/DOM tier verifies real DOM mutation** for the curated DOM subset and a closure-registered event handler (whose 200k-allocation GC churn proves the host-retained closure is pinned/traced), on both WASM targets, via jsdom (always-on) + headless Chromium (gated by `PHOENIX_REQUIRE_BROWSER=1`); soft-skips with a visible warning where no browser is provisioned.
- [x] **Marshalling round-trips leak-clean.** Linear: retained callbacks are pinned while held and unpinned on explicit `release()` / `FinalizationRegistry` (the host-never-released path documented as a *forward-deferred* known issue). WASM-GC: callbacks are held by an `externref`/`funcref` the host VM traces — no manual rooting, no explicit-free leak.
- [x] **Stubbable interop fixtures run on all five backends, byte-identical.** The canonical interop family (scalar round-trip, string in + out, multi-byte UTF-8, `JsValue` pass-through, closures-as-callbacks) runs on all five backends asserting identical output. The browser-tier `dom/*` and glue-ordering `host_effect` fixtures are the glue-tier carve-outs, **asserted** by `carve_outs_are_glue_tier_only` (the test fails if any other interop fixture skips the five columns), not silently skipped.
- [x] **All Phase-2.5 bug-closure entries have regression tests.** Vacuously satisfied — the phase closed no pre-existing bugs (see "Bugs closed in this phase" above: greenfield interop opened five forward-deferred limitations instead).
- [x] **No `known-issues.md` entry targeted at / blocking Phase 2.5.** The five new entries (linear retained-callback leak, linear per-invocation `JsValue` interning, linear `JsValue`-as-field/capture gap, wasm32-gc 4095-byte extern-string cap, native ELF/Mach-O-only) are *forward* deferrals (demand-triggered / platform-scoped), each with a sketched fix — not open 2.5 blockers. The npm / async-await / aggregate-marshalling deferrals are scope carve-outs pinned to future phases (decisions J / H + scope boundaries above), not bugs.
- [x] **Workspace test/clippy/fmt clean.** `cargo test --workspace` green under `PHOENIX_REQUIRE_RUNTIME_LIB=1 PHOENIX_REQUIRE_RUNTIME_WASM=1 PHOENIX_REQUIRE_WASMTIME=1 PHOENIX_REQUIRE_NODE=1 PHOENIX_REQUIRE_WASM_GC=1`; `cargo clippy --workspace --all-targets -- -D warnings` zero warnings; `cargo fmt --all -- --check` clean.
- [x] **CI integration matches the gating shape (PR 17).** CI runs Node in the always-on `check` job under `PHOENIX_REQUIRE_NODE=1` + `PHOENIX_REQUIRE_WASM_GC=1`, exercising both WASM targets with no silent skips, plus native interop (the matrix's C-shim column); a dedicated `interop-browser` job provisions npm deps + headless Chromium and runs the DOM tier hard on both targets; the existing wasmtime steps stay green.
- [x] **Phase-close bench refresh (PR 18).** The interop boundary-crossing cost (string round-trip vs. pure-wasm call) is published for both WASM backends in [`docs/perf/phoenix-interop-boundary.md`](../perf/phoenix-interop-boundary.md) and regenerated by `bench-corpus/run-interop.sh`.

When every box above is ticked, Phase 2.5 closes and Phase 3.1 (package manager — which carries the deferred `import js "pkg"` / `[js-dependencies]` / npm-resolution slice; delivered in [Phase 3.1.2](phase-3.md#312-npm--javascript-package-dependencies) as `extern js "pkg"`) becomes the natural next active phase.

- **Why:** Phoenix targets full-stack web development. The frontend ecosystem is dominated by JavaScript — the interop bridge lets Phoenix code in the browser reach hand-declared host and DOM APIs (and, once Phase 3.1 lands the package manager, the npm ecosystem) instead of reimplementing them. It is the prerequisite for the Phase 5 reactivity / frontend-framework work.
- **Complexity:** High — a JS glue-code generator, a value-marshalling layer across two WASM ABIs (copy-marshalling on linear, `externref` on WASM-GC), and dual Node + browser test harnesses. The wasm-bindgen-style approach is the proven foundation on the linear side; `externref` is the native model on the WASM-GC side.
- **Depends on:** WebAssembly target (2.4, complete). The npm-package slice additionally depends on the Package manager (3.1) and is deferred to ride with it.

### ✅ Phase 2.5 closed (2026-06-23)

Phoenix can now call hand-declared JavaScript / host APIs. A bodyless `extern js { ... }` block declares marshallable signatures; sema registers each as an extern `FunctionInfo` and rejects non-marshallable boundary types; lowering rewrites the call to a backend-neutral `Op::ExternCall` ([decision A0](../design-decisions.md#a0-parity-model-extern-functions-are-a-uniform-host-ffi-boundary)). That one host-call node is then **bound by every backend** — the architectural spine of the phase:

- **Interpreters (AST + IR)** — a registered Rust host table keyed by `(module, name)` (the shared `phoenix_common::host::HostRegistry`); an unbound extern is a clear runtime error.
- **Native (Cranelift)** — each extern lowers to a C-ABI symbol `phx_extern_<m>__<n>` with a **weak** aborting default a linked host shim overrides (strong-beats-weak; ELF/Mach-O).
- **`wasm32-linear`** — one custom WASM import per extern; copy-marshalling across linear memory (`(ptr,len)` strings, an `i32` handle table for `JsValue`, a single `i32` env pointer for closures invoked through an exported `__phoenix_invoke_closure_<sig>` trampoline); retained callbacks pinned (`phx_gc_pin`) + released via `FinalizationRegistry`.
- **`wasm32-gc`** — the same surface with `externref` as the native model: `JsValue` crosses directly (no handle table), strings copy through a linear-memory scratch region, closures cross as managed refs the host VM traces (no pin, no explicit-free leak) — the additive `externref` layer over the shared front-end + glue core.

Both WASM targets emit a paired `.js` glue sidecar from a **shared core + per-backend plugin** ([decision C](../design-decisions.md#c-glue-artifact-shape-paired-sidecar-js)): one generated module per target, correct under both Node and a browser. The same trampoline-naming scheme on both backends means one glue shape serves either.

**Test posture.** The payoff of mechanism parity ([A0](../design-decisions.md#a0-parity-model-extern-functions-are-a-uniform-host-ffi-boundary)) is that the *stubbable* interop family runs **byte-identical on all five backends** — the same program yielding the same output through an i32 handle table *and* an `externref`, copy-marshalled *and* scratch-region strings, pinned *and* host-traced closures. The always-on Node tier (both WASM targets) and the jsdom + headless-Chromium DOM tier cover the glue-tier effects the matrix carves out — and that carve-out is itself asserted, never silent. CI runs the Node + native interop in the always-on `check` job (no silent skips) and the browser tier in a dedicated `interop-browser` job.

**Boundary cost.** The phase-close bench ([`docs/perf/phoenix-interop-boundary.md`](../perf/phoenix-interop-boundary.md), regenerated by `bench-corpus/run-interop.sh`) measures the crossing per-call by subtraction: the bare boundary crossing is ~2–3 ns, while a `String` round-trip is ~600–840 ns/call — dominated by allocating a fresh copied Phoenix string ([decision F](../design-decisions.md#f-string-ownership-across-the-boundary-copied-never-shared)), with `wasm32-gc` marshalling strings ~28% faster than linear. A real program crosses the boundary a handful of times per frame, not a million times in a loop, so this is awareness data, not a current bottleneck.

**Deliberately deferred** (forward pointers, not gaps): npm package resolution → Phase 3.1 ([decision J](../design-decisions.md#j-npm-package-slice-deferred-to-phase-31)); async / await / `Promise` → Phase 4.3 ([decision H](../design-decisions.md#h-async-model-callbacks-only), JS async modeled callback-style); aggregate (struct / enum / `List` / `Map`) marshalling → future. The five backend-specific residuals (wasm32-linear retained-callback pin leak, wasm32-linear per-invocation `JsValue` interning, `JsValue`-as-field/capture, the wasm32-gc 4095-byte string cap, native ELF/Mach-O-only) are filed in `known-issues.md` as demand-triggered deferrals — `wasm32-gc` sidesteps the first two by construction (no handle table, host-traced callbacks).

With Phase 2.5 closed, **Phase 3.1 (package manager)** — which carries the deferred `import js "pkg"` / `[js-dependencies]` / npm-resolution slice, since delivered in [Phase 3.1.2](phase-3.md#312-npm--javascript-package-dependencies) as `extern js "pkg"` — becomes the natural next active phase.

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
    public name: String  // field accessible from outside
    public email: String
    passwordHash: Int  // private — only accessible within this module
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

- **[Diagnostic builder pattern](../design-decisions.md#diagnostic-builder-pattern)** — replace inline `self.error(msg, span)` with a fluent `Diagnostic::error(...).with_note(...).with_suggestion(...)` API. Module-system diagnostics are the natural first consumer (multi-span "symbol X is private, defined here: [other file]" errors). Hard deadline: before Phase 3.2. **✅ Implemented 2026-04-30.** Diagnostics carry notes + an optional suggestion; rendering resolves every span (primary + each note) against its own `SourceId`; the driver and LSP both consume the rich shape, and the cross-module privacy errors in sema are pinned to it as a real call site so the API can't bit-rot. Covered by diagnostics unit tests, LSP tests, and an end-to-end multi-module negative test.
- **Generic-template stubs typed split** — **✅ Implemented 2026-04-27.** `IrModule.functions` is now a `Vec<FunctionSlot>` (a tagged `Concrete | Template` enum); the old `is_generic_template: bool` field is gone. Iteration goes through typed helpers, so a backend walking `module.functions` cannot accidentally treat a template body as concrete.
- **`ValueId` allocator typed split** — **✅ Implemented 2026-04-27** as a `ValueIdAllocator` on `IrFunction` that owns both the counter and the per-value type vector; the only public mint path appends the type atomically. The historical parallel-index pair, the verifier's length-mismatch check, and the desync test helper are all gone (the desync invariant is now structurally unreachable from any public API).

### Bugs closed in this phase

- **Closure capture type ambiguity with indirect calls** — **✅ Fixed 2026-04-27.** Closure functions now use an env-pointer calling convention: each takes its environment pointer as the first arg and unpacks captures from it; the indirect call passes the closure value verbatim as the env arg, so capture types never cross the indirect-call boundary and two closures with identical user signatures but different captures unify cleanly through any phi/block parameter. The old heuristic capture-type scanner is deleted. Regression: `closures_ambiguous_captures.phx` (in the backend matrix). IR + Cranelift + IR-interp change only; the AST interpreter is unchanged.
- **Default-expression visibility across module boundaries** — **✅ Fixed 2026-04-30.** Before the fix, default-argument expressions were lowered at the *caller's* call site (see [design-decisions.md: *Default-argument lowering strategy*](../design-decisions.md#default-argument-lowering-strategy)). For a public `f(x: Int = privateHelper())` in module A imported from module B, calling `f()` from B would inline `privateHelper()` into B's output — a privacy leak (B references A's private symbol), a contract leak (renaming the private helper silently breaks every caller), and a shape sema couldn't detect (defaults type-check in the callee's module). **Resolution:** sema flags every non-pure-literal default; an IR pass synthesizes a zero-arg wrapper in the *callee's* module and records `(callee, slot) → wrapper`, and caller-site lowering calls the wrapper instead of inlining the AST default. Pure-literal defaults stay on the inline path (cheaper, privacy-safe by construction). Generic callees are gated off until per-specialization wrapper cloning lands as a follow-on. **Small accepted semantic shift:** defaults referencing private state now evaluate in the callee's scope rather than the caller's. Covered by six wrapper-synthesis regression tests (synthesis, pure-literal skip, chained wrappers, method form, closure-valued default, cross-module routing).

### Exit criteria for declaring Phase 2.6 complete

These are the bars that have to clear before Phase 2.6 is closed.  An item with an unchecked box is a real outstanding follow-up, not a stylistic note.  Mirror of [§2.2's exit criteria](#exit-criteria-for-declaring-phase-22-complete) — the same shape (design-decision markers + regression tests + matrix + workspace clean) plus 2.6-specific gates for the module-system surface.

- [x] **All Phase-2.6 design decisions implemented.** Each `**[…]**` bullet in §2.6's "Refactors bundled into this phase" carries a `✅ Implemented YYYY-MM-DD` marker.
    - [x] **(a-foundation) Diagnostic builder foundation** — ✅ 2026-04-27.
    - [x] **(a-consumer) Cross-module privacy diagnostic uses the rich shape** — ✅ 2026-04-30. Sema emits a `with_note(...).with_suggestion(...)` diagnostic when an `import` resolves to a private symbol, and likewise for cross-module field access. Pinned by an end-to-end multi-module negative test (asserts message + note + suggestion + cross-file span).
    - [x] **(b) Generic-template typed split** — ✅ 2026-04-27. The `is_generic_template: bool` field is gone; template / concrete iteration goes through the typed `FunctionSlot` enum.
    - [x] **(c) `ValueId` allocator typed split** — ✅ 2026-04-27. `ValueId` allocation and per-value type recording are a single operation; no public API for "allocate without assigning a type," and the verifier's length-mismatch check is gone (structurally unreachable).

    The `Value::Closure → IR blocks` refactor that originally appeared in this list was dropped from the batch on 2026-04-27 — `phoenix-interp` is intended to remain a fast AST tree-walker for debugging, kept deliberately separate from `phoenix-ir-interp`. The closure-capture-ambiguity bug bundled with it is addressed independently via the env-pointer ABI fix tracked under "Bugs closed in this phase" above.
- [x] **All Phase-2.6 bug-closure entries have regression tests.** Each entry under "Bugs closed in this phase" maps to a test that fails when the fix is reverted. Closure-capture ambiguity is **✅ Closed 2026-04-27** (`closures_ambiguous_captures.phx` in the backend matrix; reverting the env-pointer ABI fix resurfaces the original "ambiguous indirect call" error in compiled mode). Default-expression visibility is **✅ Closed 2026-04-30** (the six wrapper-synthesis tests plus the end-to-end multi-module fixture cover synthesis, chained-wrapper ordering, the method-default form, the closure-valued-default corner, the call-site rewrite, and the cross-module privacy property).
- [x] **No `known-issues.md` entry targeted at Phase 2.6.** The two bug-closures and the three bundled refactors are all closed (2026-04-27 / 2026-04-30). The "closure functions inside generic templates are not cloned per specialization" entry was hedged "Phase 2.6 if a module-system fixture trips the gap"; the multi-module fixture set landed without tripping it, so the entry is re-targeted to Phase 3. Two new entries were *opened* by 2.6 work and are tagged Phase 3 or no-phase — neither is a 2.6 deliverable.
- [x] **Workspace test/clippy/fmt clean on the 2.6 branch.** `cargo test --workspace` green; `cargo clippy --workspace --tests` zero warnings; `cargo fmt --all -- --check` clean.
- [x] **Three-backend roundtrip matrix on multi-file fixtures.** **✅ Completed 2026-04-30.** The multi-module matrix walks every multi-file project under `tests/fixtures/multi/<name>/`, runs each under `phoenix run`, `phoenix run-ir`, and `phoenix build` + execute, and asserts byte-identical stdout *and* equality with the per-project `expected.txt` (so a coherent regression that broke all three backends the same way still trips). Coverage spans cross-module function calls, `as` aliasing, wildcard imports, directory-as-module, the default-wrapper tripwire, positive cross-module struct/enum construction, cross-module method dispatch, private-default helper resolution, enum payloads, and a generic-trait-bound import. One `#[test]` per fixture; `gen_*.phx` schemas remain excluded.
- [x] **Visibility rule coverage.** Positive tests for `public struct` + fields, `public enum` + variants, and `public function` + default-private all live in the matrix above. Sema-level coverage for every visibility rule (struct / field / enum / trait / type-alias) and negative tests for the privacy paths live in dedicated sema and multi-module-negative suites. Each negative produces a single non-panic diagnostic with the rich shape (note + suggestion) where applicable.
- [x] **Module-resolver error paths report cleanly, never panic.** **✅ Completed 2026-04-30.** Every required input has a regression test: missing module (lists both probe paths), ambiguous module (lists both candidates), cyclic imports (renders the cycle path), `main` in a non-entry module, and malformed `mod.phx` (parser diagnostics forwarded). An import path escaping the project root cannot today be expressed in the grammar (no `..` in dotted paths), so the box ticks on unrepresentability; the `EscapesRoot` defensive guard is in place but currently untested — tracked under [known-issues: *`phoenix-modules` `EscapesRoot` resolver guard is untested*](../known-issues.md#phoenix-modules-escapesroot-resolver-guard-is-untested).
- [x] **Module-system diagnostics exercise the rich diagnostic shape.** **✅ Completed 2026-04-30.** Sema emits `"<name> is private to module <module>"` with a "declared here" note and a "mark as `public`" suggestion for private imports (and likewise for cross-module field access); the note span resolves against its own `SourceId`, so the rendered diagnostic shows the cross-file `lib.phx` location even though the primary span is in `main.phx`. Pinned by the multi-module negative suite.

When every box above is ticked, Phase 2.6 closes and Phase 2.3 (GC + runtime) becomes the active phase.

### ✅ Phase 2.6 closed (2026-04-30)

Module system + visibility shipped end-to-end: file-as-module discovery (lazy, import-driven, root = the entry file's directory), `import a.b.c { Item, Item as Alias, * }` syntax, `public` visibility on functions / structs / fields / enums / traits / type aliases (private-by-default), per-module name mangling so two modules can declare the same name without collision, cross-module name resolution, visibility enforcement at every lookup site with rich note + suggestion diagnostics, default-expression wrapper synthesis (the original 2.6 tripwire), and `function main()` reserved for the entry module. The resolver crate handles BFS discovery + cycle detection + the five `ResolveError` variants (Missing/Ambiguous/Cyclic/Malformed/EscapesRoot). All three backends handle multi-module input. The diagnostic-builder foundation, the `FunctionSlot` and `ValueIdAllocator` IR-shape refactors, and the closure-capture-ambiguity bug fix all landed alongside.

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

- [x] **Allocation throughput bench landed** (PR 1, 2026-05-11). Runs the four size buckets × two tag modes; numbers in [`docs/perf-baselines/allocation.md`](../perf-baselines/allocation.md).
- [x] **GC pause distribution bench landed** (PR 1, 2026-05-11). P50 / P95 / P99 / max for 1k / 10k / 100k. Numbers in [`docs/perf-baselines/pause.md`](../perf-baselines/pause.md).
- [x] **Collections bench landed** (PR 2, 2026-05-11). Confirms `Map.get` stays flat (8.8 → 12.0 ns across 100 → 10k, 1.4× range) and `sort_by` grows n log n (587 ns → 96 µs across 100 → 10k, 163×). Numbers in [`docs/perf-baselines/collections.md`](../perf-baselines/collections.md).
- [x] **End-to-end compiled-binary timing** (PR 3, 2026-05-11). The pipeline bench caches per-fixture binaries and times subprocess spawn. Only `medium` populates today — `medium_large` and `large` exercise Cranelift gaps (`print(List)`, string-method codegen) that surface cleanly via a skip path. Numbers in [`docs/perf-baselines/pipeline.md`](../perf-baselines/pipeline.md).
- [x] **Cross-language comparison published** (PR 5, 2026-05-12). [`bench-corpus/`](../../bench-corpus/) ships the four locked workloads; [`bench-corpus/run.sh`](../../bench-corpus/run.sh) renders [`docs/perf/phoenix-vs-go.md`](../perf/phoenix-vs-go.md) with absolute numbers, ratios, and the "what's not benchmarked yet (HTTP / JSON / concurrency)" gap call-out.
- [x] **All five subordinate decisions (A–E) implemented** (PRs 1–5). Each carries an `✅ Implemented YYYY-MM-DD` marker in its [`docs/design-decisions.md`](../design-decisions.md#phase-27-benchmarking) entry.
- [x] **Baseline storage decision documented and applied** (PR 4, 2026-05-11). [`docs/perf-baselines/`](../perf-baselines/) populated; [`phoenix-bench-diff`](../../crates/phoenix-bench-diff/) updates and diffs the snapshot; bench source files link back to the baseline path so a maintainer cutting a regression knows where to look.
- [x] **CI integration matches the gating decision** (PR 4, 2026-05-11). [`.github/workflows/bench.yml`](../../.github/workflows/bench.yml) runs the benches on `push: main`, gated on `BENCH_ENFORCE` for noise-floor observation per decision B.
- [x] **Workspace test/clippy/fmt clean** (verified end of PR 6). `cargo test --workspace` green; `cargo clippy --workspace --tests -- -D warnings` zero warnings; `cargo fmt --all -- --check` clean.
- [x] **Each Phase-2.3 perf follow-up resolved one way or the other** (PR 6, 2026-05-12).
  - *Typed-allocator threading via `TypeTag`* — **landed.** The list, map, closure-env, and struct/enum allocators now thread their concrete tag through GC alloc. Before/after pause numbers in [`docs/perf-baselines/pause.md`](../perf-baselines/pause.md): the committed numbers dropped 45–77 % across all rooted-object scenarios, but the **mark phase still does conservative interior scanning** (this change provides the substrate for trace tables — GC subordinate decision C — without yet swapping in per-tag mark functions), and the pause bench's own allocations are still tagged `Unknown`, so the bulk of the headline delta is environmental quiescence on the rerun rather than a code-induced win. Per the cited note in `pause.md`: don't over-extrapolate. Trace tables themselves stay deferred until a real pause-distribution signal lands.
  - *Segregated free lists by size class* — **not pursued.** Per [GC subordinate decision B](../design-decisions.md#b-heap-layout-segregated-free-lists-by-size-class-single-arena)'s 2026-05-12 evaluation: `phx_gc_alloc` does carry overhead (~60–130 ns/call vs. `malloc`'s ~10–30 ns), but no program in the bench corpus has alloc throughput as its dominant cost. The cross-language gap that drove the scope conversation is dominated by O(n²) immutable-container builds — addressed by [Phase 2.7 decision F (`ListBuilder` / `MapBuilder`)](../design-decisions.md#f-mutable-builder-api-for-list--map-explicit-types-not-implicit-linearity), not by faster allocators. Reopens when an alloc-throughput-dominated workload (likely Phase 4 HTTP / JSON handlers) lands.

When every box above is ticked, Phase 2.7 closes and Phase 2.4 (WebAssembly target) becomes the active phase.

- **Complexity:** Low for the bench-and-baseline scaffolding (~250 LOC across two new bench files plus storage glue); medium for the size-class-arena follow-up if benches say it should land; medium-to-high for typed-allocator-tagging because it touches every allocator call site. The phase's *minimum* scope is just the scaffolding and the decisions; the follow-ups are conditional.
- **Depends on:** Phase 2.3 (closed) — benches measure GC and collection behavior, both of which only make sense post-GC.

### ✅ Phase 2.7 closed (2026-05-13)

Benchmark suite shipped end-to-end. Implementation scope: allocation throughput + GC pause distribution + Map/sort_by collections + end-to-end compile-and-run criterion benches; `docs/perf-baselines/` snapshot + `phoenix-bench-diff` regression tool + post-merge `bench.yml` workflow with `BENCH_ENFORCE` noise-floor gate; four-workload Phoenix-vs-Go cross-language corpus + hyperfine-driven `bench-corpus/run.sh` + published [`docs/perf/phoenix-vs-go.md`](../perf/phoenix-vs-go.md). The two Phase-2.3 perf follow-ups landed: typed-allocator threading via `TypeTag` is done end-to-end (codegen + runtime); segregated free lists are explicitly deferred per the cited bench output.

Two scope additions were made during the phase, both Phoenix-language-level rather than benchmark-tooling:

- **Decision F (`ListBuilder` / `MapBuilder`)** — added in response to the published cross-language ratios showing Phoenix at 1900× / 6900× slower than Go on `sort_ints` / `hash_map_churn`, dominated by O(n²) immutable-container builds. The builders are transient-mutable accumulators that freeze to immutable `List<T>` / `Map<K, V>`; total build cost drops to O(n). After the rewrite, the published ratios fell to **5.4×** / **3.6×** — a ~350× / ~1900× reduction. Use-after-freeze is runtime-checked (static enforcement is decision G). *Backend completion (post-close):* the bench refresh exposed that the builders were initially native-only; the two interpreters and the wasm-gc `MapBuilder` lowering were added afterward so all five backends share one last-wins / first-insertion-position dedup contract, pinned by `builders.phx` in the backend matrix.
- **Decision G (linearity / ownership types: deferred to Phase 4+)** — the linearity story that would let `xs = xs.push(v)` become in-place automatically is real but out of scope for Phase 2; the design exploration is queued for Phase 4 (stdlib pass) or a dedicated phase between 3 and 4. The deferral note exists so a future contributor proposing "add linearity to fix problem X" finds the prior deliberation.

Verified by the workspace test suite plus targeted bench-fixture integration tests, the bench-suite smoke-runs (`cargo bench ... -- --test`), and a full `bench-corpus/run.sh` execution against `go1.23.0` on the dev machine.

Phase 2.4 (WebAssembly target) becomes the next active phase, with the typed-allocator-threaded substrate from PR 6 ready for the WASM `GcHeap` impl to plug into.
