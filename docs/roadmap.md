# Phoenix Roadmap

This document outlines the path from the current implementation to a production-ready full-stack web language.

**Current state (updated 2026-05-07):** 2,900+ tests across 14 crates. Phoenix Gen Phases 1–3 and 5 fully complete with TypeScript, Python, Go, and OpenAPI code generation, VS Code extension with full LSP (hover, autocomplete, go-to-definition, find-references, rename), watch mode, and `where` constraint validation. Phase 1 is fully complete. Phase 2.1 (IR) is complete — the `phoenix-ir` crate implements SSA-style IR lowering and `phoenix-ir-interp` provides a round-trip verification interpreter. Phase 2.2 (Cranelift native compilation) covers value types, strings, structs, enums, pattern matching, closures, all string methods, `List<T>` with all functional methods, `Map<K, V>` with all methods, and `Option<T>`/`Result<T, E>` with all combinators. Phase 2.6 (modules and visibility) is complete — multi-file projects with `import`, `public`/private, and lazy import-driven discovery. Phase 2.3 (runtime and memory management) is complete — tracing mark-and-sweep GC with precise stack roots, GC-managed strings, valgrind-verified leak-clean compiled binaries, `defer` syntax across all three backends, open-addressing hash maps, and bottom-up merge sort for `List.sortBy`. `#![warn(missing_docs)]` enforced on all crates. All tests pass, clippy is clean, formatting is clean, CI is green. **Active phase: 2.7 (benchmark suite) — measured GC and codegen baseline before the WebAssembly target lands a second `GcHeap` impl behind the same trait.**

---

## Phases

| Phase | Name | Status | Description |
|-------|------|--------|-------------|
| [1](phases/phase-1.md) | Core Language | ✅ Complete | Variables, functions, control flow, structs, enums, generics, traits, closures, collections, error handling, and all ergonomic features |
| [2](phases/phase-2.md) | Compilation | In Progress | IR (complete), Cranelift native compilation (complete), modules and visibility (complete), runtime + GC + `defer` (complete), benchmark suite (active), WebAssembly target, JS interop |
| [3](phases/phase-3.md) | Tooling | Planned | Package manager, LSP, formatter, error message quality |
| [4](phases/phase-4.md) | Standard Library | Planned | Core types (tuples, Date/Time, Regex, iterators), config, async runtime, HTTP/WebSocket/SSE, typed routing, annotations, JSON serialization, database access, logging, test framework |
| [5](phases/phase-5.md) | Differentiating Features | Planned | Built-in serialization, refinement types, reactivity, typed endpoints, comptime, auto-generated API docs, built-in observability, frontend framework |
| [6](phases/phase-6.md) | Ecosystem & Adoption | Planned | Documentation site, package registry, starter templates, community, 1.0 release |

See also: [Known Issues](known-issues.md) | [Design Decisions](design-decisions.md) | [Phoenix Gen](phoenix-gen.md) (parallel track)

---

## Parallel Track: Phoenix Gen

**[Phoenix Gen](phoenix-gen.md)** is a standalone code generation tool that uses Phoenix syntax to define API schemas and generates typed code for existing languages (TypeScript, Go, Rust, etc.). It runs alongside the main phases — buildable now with the existing parser and type checker — to bring Phoenix's type safety story to developers before the full compiler exists.

Phoenix Gen is a stepping stone to the full language: `.phx` schema files are valid Phoenix code that becomes importable modules when the compiler ships, and developers who adopt the tool become the first users of the full language.

---

## Milestones

**M1 — "Useful scripting language"** (Phases 1.1–1.13) ✅

> Generics, collections, closures, traits, GC memory model, proper error handling,
`?` operator, string interpolation, field assignment, type aliases, implicit return,
Map, string methods, functional collection methods, pipes, named/default parameters, destructuring, inline methods,
CI pipeline, type registries exposed, and expression types annotated.

**M2 — "Compiled language"** (Phase 2)

> Phoenix produces native binaries and WASM. Module system with `public`/private visibility enables multi-file projects. Performance becomes competitive with Go.

**M3 — "Developer-ready"** (Phase 3)

> Package manager, LSP, formatter. A developer can realistically start a project. (The test framework lives in Phase 4 because it depends on annotations, async, HTTP, and the database layer.)

**M4 — "Web-capable"** (Phases 4.1–4.9)

> Tuples, Date/Time, Regex, iterators, type-safe config, async with background jobs, HTTP/WebSocket/SSE, struct update syntax, annotations, JSON, compile-time typed database queries (with schema-references-struct and auto-generated migrations), error context chaining, and the built-in test framework.

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
| 30b | Benchmark suite (GC + codegen baseline) | GC, Cranelift | Medium | 🔧 In Progress |
| 31 | WebAssembly target | Cranelift, GC, Bench | Medium | |
| 32 | Module system and visibility (`public`/private) | Semantic analysis | High | ✅ Done |
| 33 | JavaScript interop | WASM, Package manager | High | |
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
