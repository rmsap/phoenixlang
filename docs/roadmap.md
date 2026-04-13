# Phoenix Roadmap

This document outlines the path from the current tree-walk interpreter to a production-ready full-stack web language.

**Current state (updated 2026-04-13):** A working tree-walk interpreter with 1,600+ tests across 45,000+ LOC (9 crates). Phoenix Gen Phases 1–3 and 5 fully complete with TypeScript, Python, Go, and OpenAPI code generation, VS Code extension with full LSP (hover, autocomplete, go-to-definition, find-references, rename), watch mode, and `where` constraint validation. Phase 1 is fully complete. Phase 2.1 (IR) is in progress — the `phoenix-ir` crate implements SSA-style IR lowering from the typed AST. `#![warn(missing_docs)]` enforced on all crates. All tests pass, clippy is clean, formatting is clean, CI is green. **Next up: Phase 2 (Compilation) — continuing with IR interpreter, then Cranelift integration.**

---

## Phases

| Phase | Name | Status | Description |
|-------|------|--------|-------------|
| [1](phases/phase-1.md) | Core Language | ✅ Complete | Variables, functions, control flow, structs, enums, generics, traits, closures, collections, error handling, and all ergonomic features |
| [2](phases/phase-2.md) | Compilation | In Progress | IR design (started), Cranelift native compilation, runtime library (GC), WebAssembly target, JS interop, module system and visibility |
| [3](phases/phase-3.md) | Tooling | Planned | Package manager, LSP, formatter, test framework, error message quality |
| [4](phases/phase-4.md) | Standard Library | Planned | Core types (tuples, Date/Time, Regex, iterators), config, async runtime, HTTP/WebSocket/SSE, typed routing, annotations, JSON serialization, database access, logging |
| [5](phases/phase-5.md) | Differentiating Features | Planned | Built-in serialization, refinement types, reactivity, typed endpoints, comptime, auto-generated API docs, built-in observability |
| [6](phases/phase-6.md) | Ecosystem & Adoption | Planned | Documentation site, package registry, starter templates, community, 1.0 release |

See also: [Known Issues & Design Decisions](known-issues.md) | [Phoenix Gen](phoenix-gen.md) (parallel track)

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

> Package manager, LSP, formatter, test framework. A developer can realistically start a project.

**M4 — "Web-capable"** (Phases 4.1–4.8)

> Tuples, Date/Time, Regex, iterators, type-safe config, async with background jobs, HTTP/WebSocket/SSE, struct update syntax, annotations, JSON, compile-time typed database queries (with schema-references-struct and auto-generated migrations), and error context chaining.

**M5 — "Differentiated"** (Phase 5)

> Built-in serialization, refinement types, reactivity, typed endpoints, auto-generated API docs (OpenAPI from typed endpoints), and built-in observability (automatic tracing from structured concurrency). Phoenix does something no other language does.

**M6 — "Ecosystem"** (Phase 6)

> Package registry, documentation, starter templates, community. Developers can discover, learn, and build with Phoenix independently.

---

## Implementation Priority

| # | Feature | Depends On | Effort | Status |
|---|---------|-----------|--------|--------|
| 1–29 | Phase 1 (all items) | — | — | ✅ Done |
| 30 | IR + Cranelift compilation | — | High | |
| 31 | WebAssembly target | Cranelift | Medium | |
| 32 | Module system and visibility (`public`/private) | Semantic analysis | High | |
| 33 | JavaScript interop | WASM, Package manager | High | |
| 34 | Package manager | Compilation, modules | Medium | |
| 35 | LSP + VS Code extension | Modules | High | |
| 36 | Formatter | Parser | Small | |
| 37 | Test framework | — | Medium | |
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
| 60 | Documentation site | All of the above | Medium | |
| 61 | Package registry | Package manager | Medium | |
