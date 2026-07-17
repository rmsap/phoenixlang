# Phoenix Roadmap

This document outlines the path from the current implementation to a production-ready full-stack web language.

**Current state (updated 2026-07-17):** 3,700+ tests across 15 crates. Phoenix Gen Phases 1–3 and 5 fully complete with TypeScript, Python, Go, and OpenAPI code generation, VS Code extension with full LSP (hover, autocomplete, go-to-definition, find-references, rename), watch mode, and `where` constraint validation. Phase 1 is fully complete. Phase 2.1 (IR) is complete — the `phoenix-ir` crate implements SSA-style IR lowering and `phoenix-ir-interp` provides a round-trip verification interpreter. Phase 2.2 (Cranelift native compilation) covers value types, strings, structs, enums, pattern matching, closures, all string methods, `List<T>` with all functional methods, `Map<K, V>` with all methods, and `Option<T>`/`Result<T, E>` with all combinators. Phase 2.6 (modules and visibility) is complete — multi-file projects with `import`, `public`/private, and lazy import-driven discovery. Phase 2.3 (runtime and memory management) is complete — tracing mark-and-sweep GC with precise stack roots, GC-managed strings, valgrind-verified leak-clean compiled binaries, `defer` syntax across all three backends, open-addressing hash maps, and bottom-up merge sort for `List.sortBy`. Phase 2.7 (benchmark suite) is complete — runtime alloc / pause / collections / pipeline / `compile_and_run` criterion benches, committed `docs/perf-baselines/` snapshot with `phoenix-bench-diff` post-merge regression detection, paired Phoenix-vs-Go cross-language corpus published to `docs/perf/phoenix-vs-go.md`, typed-allocator threading through every codegen and runtime allocation site, and the `ListBuilder<T>` / `MapBuilder<K, V>` transient-mutable accumulators that took the cross-language `sort_ints` / `hash_map_churn` ratios from 1900× / 6979× down to 5.4× / 3.6× against Go. Phase 2.4 (WebAssembly target) is complete — two backends compile byte-identical to native across the full fixture matrix: `wasm32-linear` embeds-and-merges the `phoenix-runtime` crate built for `wasm32-wasip1` (linear-memory `MarkSweepHeap`, explicit shadow-stack roots), and `wasm32-gc` emits inline WASM-GC (`struct.new` / `array.*` / `call_ref` over a rec-group of nominal types) delegating tracing to the host VM with no runtime merge. WASI preview1 is the only host surface; the phase-close bench refresh ([`docs/perf/phoenix-wasm-vs-native.md`](perf/phoenix-wasm-vs-native.md)) lands WASM at 2.0–2.9× native and closed three gaps it surfaced (five-backend builder parity, O(n log n) `List.sortBy` on both wasm backends, an O(1) hash index for the wasm32-gc `Map`). Phase 2.5 (JavaScript interop) is complete — `extern js` is a uniform host-FFI boundary (the backend-neutral `Op::ExternCall`) bound by all five backends: the two interpreters via a registered Rust host table, native via a weak-override C-ABI host shim (ELF/Mach-O), and both Phase 2.4 WASM backends via a generated JS glue sidecar (copy-marshalling on `wasm32-linear`, `externref` on `wasm32-gc`). The stubbable interop family runs byte-identical across all five backends; the DOM family is verified under jsdom + headless Chromium on both WASM targets; the phase-close bench ([`docs/perf/phoenix-interop-boundary.md`](perf/phoenix-interop-boundary.md)) puts the bare boundary crossing at ~2–3 ns and a `String` round-trip at ~600–840 ns/call. Closures-as-callbacks model JS async (fetch/setTimeout) in the meantime. `#![warn(missing_docs)]` enforced on all crates. All tests pass, clippy is clean, formatting is clean, CI is green. **With all of Phase 2 complete, the active work is [Phase 3](phases/phase-3.md) (tooling) and [Phase 4](phases/phase-4.md) (standard library), run as two parallel tracks — nothing in Phase 3 depends on Phase 4. The tooling track (3.1 package manager and its 3.1.2 npm slice — `extern js "pkg"` + `[js-dependencies]` on the BYO model — both complete; plus 3.2 LSP gap-closing and 3.3 formatter) rests only on Phase 2 foundations. On the stdlib track, the keystone is 4.5 (the annotation system), which unblocks JSON serialization, config loading, database hints, and the test framework, so it comes first. async/await + `Promise` remain deferred to Phase 4.3 (async runtime). See [phase-2.md §2.5](phases/phase-2.md#25-javascript-interop).**

---

## Phases

| Phase | Name | Status | Description |
|-------|------|--------|-------------|
| [1](phases/phase-1.md) | Core Language | ✅ Complete | Variables, functions, control flow, structs, enums, generics, traits, closures, collections, error handling, and all ergonomic features |
| [2](phases/phase-2.md) | Compilation | ✅ Complete | IR, Cranelift native compilation, modules and visibility, runtime + GC + `defer`, benchmark suite + `ListBuilder` / `MapBuilder`, WebAssembly target, and JavaScript interop — all complete |
| [3](phases/phase-3.md) | Tooling | Planned | Package manager, LSP, formatter, error message quality |
| [4](phases/phase-4.md) | Standard Library | Planned | Core types (tuples, Date/Time, Regex, iterators), config, async runtime, HTTP/WebSocket/SSE, typed routing, annotations, JSON serialization, database access (typed SQL + a transparent data layer, no ORM), logging, test framework |
| [5](phases/phase-5.md) | Differentiating Features | Planned | Built-in serialization, refinement types, reactivity, typed endpoints, comptime, auto-generated API docs, built-in observability, frontend framework |
| [6](phases/phase-6.md) | Ecosystem & Adoption | Planned | Documentation site, package registry, starter templates, community, 1.0 release |

See also: [Known Issues](known-issues.md) | [Design Decisions](design-decisions.md) | [Phoenix Gen](phoenix-gen.md) (parallel track)

---

## Parallel Track: Phoenix Gen

**[Phoenix Gen](phoenix-gen.md)** is a standalone code generation tool that uses Phoenix syntax to define API schemas and generates typed code for existing languages (TypeScript, Python, and Go, plus an OpenAPI 3.1 spec). It runs alongside the main phases — buildable now with the existing parser and type checker — to bring Phoenix's type safety story to developers before the full compiler exists.

Phoenix Gen is a stepping stone to the full language: `.phx` schema files are valid Phoenix code that becomes importable modules when the compiler ships, and developers who adopt the tool become the first users of the full language.

Phoenix Gen tracks its design decisions separately in **[phoenix-gen-design-decisions.md](phoenix-gen-design-decisions.md)**; the feature set lives in [phoenix-gen.md](phoenix-gen.md) and the v1.0 plan in [phoenix-gen-roadmap.md](phoenix-gen-roadmap.md).

---

## Milestones

**M1 — "Useful scripting language"** (Phases 1.1–1.13) ✅

> Generics, collections, closures, traits, GC memory model, proper error handling,
`?` operator, string interpolation, field assignment, type aliases, implicit return,
Map, string methods, functional collection methods, pipes, named/default parameters, destructuring, inline methods,
CI pipeline, type registries exposed, and expression types annotated.

**M2 — "Compiled language"** (Phase 2) ✅

> Phoenix produces native binaries and WASM. Module system with `public`/private visibility enables multi-file projects. Performance becomes competitive with Go.

**M3 — "Developer-ready"** (Phase 3)

> Package manager, LSP, formatter. A developer can realistically start a project. (The test framework lives in Phase 4 because it depends on annotations, async, HTTP, and the database layer.)

**M4 — "Web-capable"** (Phases 4.1–4.9)

> Tuples, Date/Time, Regex, iterators, type-safe config, async with background jobs, HTTP/WebSocket/SSE, struct update syntax, annotations, JSON, compile-time typed database queries (schema-references-struct, auto-generated migrations, and a transparent data layer — typed CRUD and explicit relationship loading, no ORM), error context chaining, and the built-in test framework.

**M5 — "Differentiated"** (Phase 5)

> Built-in serialization, refinement types, reactivity, typed endpoints, auto-generated API docs (OpenAPI from typed endpoints), built-in observability (automatic tracing from structured concurrency), and a native frontend framework (components, routing, SSR). Phoenix does something no other language does.

**M6 — "Ecosystem"** (Phase 6)

> Package registry, documentation, starter templates, community. Developers can discover, learn, and build with Phoenix independently.

---

## Implementation Priority

| # | Feature | Depends On | Effort | Status |
|---|---------|-----------|--------|--------|
| 1–29 | Phase 1 (all items) | — | — | ✅ Done |
| 30 | IR + Cranelift compilation | — | High | ✅ Done |
| 30a | Tracing GC + `defer` + runtime | Cranelift | High | ✅ Done |
| 30b | Benchmark suite (GC + codegen baseline) | GC, Cranelift | Medium | ✅ Done |
| 31 | WebAssembly target | Cranelift, GC, Bench | Medium | ✅ Done |
| 32 | Module system and visibility (`public`/private) | Semantic analysis | High | ✅ Done |
| 33 | JavaScript interop | WASM | High | ✅ Done |
| 34 | Package manager | Compilation, modules | Medium | |
| 35 | LSP + VS Code extension | Modules | High | |
| 36 | Formatter | Parser | Small | |
| 37 | Test framework | Annotations, Async runtime, HTTP, Typed database queries | Medium | |
| 38 | Tuple types | — | Medium | |
| 39 | Date/Time types (`Instant`, `DateTime`, `Duration`) | — | Medium | |
| 40 | Regular expressions (`Regex`) | — | Medium | |
| 41 | Iterator protocol (lazy sequences) | Traits, associated types | Medium-High | |
| 42 | Error context and chaining (`.context()`, `Error` trait) | — | Small | |
| 43 | Struct update syntax (`Type { ...source, field: value }`) | — | Small-Medium | |
| 44 | Async runtime | Closures, compilation | High | |
| 45 | Background jobs and scheduled tasks (`Scheduler`) | Async runtime | Small-Medium | |
| 46 | HTTP client/server | Async, stdlib | Medium | |
| 47 | WebSockets and Server-Sent Events | HTTP, async | Medium | |
| 48 | Environment and configuration (`@config`) | Annotations, serialization | Small-Medium | |
| 49 | Annotation system | — | Small-Medium | |
| 50 | JSON serialization | Annotations, compilation | Medium | |
| 51 | Typed database queries (with schema-from-struct) | Async, compilation, serialization, annotations | Very High | |
| 51a | Derived typed CRUD helpers (`db.<table>.insert/find/update/delete`) | 51 | Medium | |
| 51b | Explicit relationship loading (`db.load`, never lazy) | 51 | Medium | |
| 51c | Optional typed query builder (dynamic queries) | 51 | High | |
| 52 | Auto-generated migrations | Schema declarations, compilation | High | |
| 53 | Built-in serialization | Generics, traits, compilation, annotations | High | |
| 54 | Refinement types | Type system | Very High | |
| 55 | First-class reactivity | WASM target, traits | Very High | |
| 56 | Typed endpoints | HTTP, serialization, WASM target | High | |
| 57 | Auto-generated API documentation (OpenAPI) | Typed endpoints, serialization | Small | |
| 58 | Built-in observability (structured tracing, metrics) | Async runtime, structured concurrency, HTTP | High | |
| 59 | Compile-time evaluation (`comptime`) | Compilation | Very High | |
| 60 | Frontend framework (native components, routing, SSR) | WASM, JS interop, reactivity, typed endpoints | Very High | |
| 61 | Documentation site | All of the above | Medium | |
| 62 | Package registry | Package manager | Medium | |
