# Phase 6: Ecosystem and Adoption

**Status: Not started**

A language lives or dies by its ecosystem. These are not language features — they are the things that make people actually use the language.

## 6.1 Documentation Site

- Language tutorial (learn Phoenix in 30 minutes)
- Standard library API reference (auto-generated from doc comments)
- Cookbook (common patterns: "how do I make an HTTP server?", "how do I parse JSON?")
- Playground (run Phoenix in the browser via WASM)

## 6.2 Package Registry

- Central registry for community packages (like crates.io or npm)
- `phoenix publish` and `phoenix install`
- Documentation hosting for published packages

## 6.3 Starter Templates

- `phoenix new web-server` — HTTP server with routing, JSON, database
- `phoenix new web-app` — full-stack app with backend + WASM frontend
- `phoenix new library` — reusable library with tests and CI

## 6.4 Community Building

- Open-source the compiler on GitHub with a permissive license (MIT or Apache 2.0)
- Write blog posts explaining the language design decisions
- Build one real, non-trivial application in Phoenix (a blog engine, a task manager, a chat app) to prove it works end-to-end
- Engage with the PL community (r/ProgrammingLanguages, Hacker News, Discord)

## 6.5 Production Readiness

- Comprehensive test suite for the compiler itself (fuzz testing, property-based testing)
- Benchmark suite comparing Phoenix to Rust, Go, TypeScript for web workloads
- Security audit of the GC/memory model
- Stable release (1.0) with backward compatibility guarantees
