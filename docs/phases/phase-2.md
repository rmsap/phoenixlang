# Phase 2: Compilation

**Status: In progress (2.2 started)**

Move from interpretation to native code generation. This is what makes Phoenix a real language rather than a scripting tool.

## 2.1 Intermediate Representation (IR)

**Status: Complete.** The `phoenix-ir` crate implements an SSA-style IR with basic blocks, typed instructions, and explicit control flow. The lowering pass converts the type-checked AST into IR for all major language features (arithmetic, control flow, structs, enums, match, closures, method calls, collections, try operator, string interpolation). Use `phoenix ir <file.phx>` to inspect the output. The `phoenix-ir-interp` crate provides an IR interpreter for round-trip verification — use `phoenix run-ir <file.phx>` to execute via the IR and compare output with `phoenix run`. Round-trip tests cover all lowered features including the try operator; see `crates/phoenix-ir-interp/tests/` for the full suite.

- Lower the type-checked AST to a flat, SSA-style IR
- Basic blocks, typed instructions, explicit control flow
- This decouples semantic analysis from code generation
- Makes it possible to target multiple backends (native, WASM)

## 2.2 Native Compilation (Cranelift)

**Status: In progress.** The `phoenix-cranelift` crate translates Phoenix IR to Cranelift IR and produces native executables via `cranelift-object` + system linker. The `phoenix-runtime` crate provides a small static library linked into every compiled binary. Use `phoenix build <file.phx>` to compile. Supported features:

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

- **[Generic function monomorphization](../design-decisions.md#generic-function-monomorphization-strategy)** — user generics get one specialized copy per concrete instantiation. **✅ Implemented 2026-04-19** as `monomorphize` pass in `phoenix-ir/src/monomorphize.rs` (BFS worklist; symbol-safe specialization names `orig__i64__str`; templates kept as inert stubs; covers generic methods on user-defined types). Concrete type args are embedded directly in `Op::Call` so the IR is self-describing.
- **[Dynamic dispatch via `dyn Trait`](../design-decisions.md#dynamic-dispatch-via-dyn-trait)** — vtable ABI `(data_ptr, vtable_ptr)`; static dispatch stays the default.
- **[Centralized `Layout` trait](../design-decisions.md#centralized-layout-for-reference-types)** — single source of truth for reference-type slot count, alignment, load/store codegen. **✅ Implemented 2026-04-19** as `TypeLayout` in `phoenix-cranelift/src/translate/layout/`.
- **[Numeric error semantics](../design-decisions.md#numeric-error-semantics-division-overflow-integer-edge-cases)** — Int operators panic on overflow / divide-by-zero / `i64::MIN` negation (ratifies current behavior); Float follows IEEE 754. Stdlib `Int.checked*` family lands in Phase 4.1.

## 2.3 Runtime and Memory Management

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
- Functions, structs, enums, and traits without `public` are module-private

### Design principles

- **Private by default:** Forces authors to think about their public API. Anything not marked `public` is an implementation detail that can change freely.
- **No `protected` or `internal`:** Two levels (public/private) keep the system simple. If a more granular system is needed later, it can be added without breaking existing code.
- **Struct fields have independent visibility:** A struct can be `public` (importable) while some fields are private (encapsulated). This supports the common pattern of exposing a type while hiding its internals.

- **Why before packages:** The package manager (3.1) needs modules to exist. You cannot have cross-package imports without intra-project imports. Module resolution is also needed by the LSP (3.2) for go-to-definition and auto-imports.
- **Complexity:** High — requires a module resolver (file system → module tree), import resolution, visibility checking across module boundaries, and changes to name resolution in the semantic checker. The two-pass registration design already handles forward references within a file; extending it to cross-file references adds significant complexity.
- **Depends on:** Semantic analysis (Phase 1, complete)

### Refactors bundled into this phase

Two codebase-hygiene refactors land alongside the module-system work — both paid for by the module-system scope (multi-file diagnostics, evolving parser AST) and both must be complete before Phase 3.2 (LSP). See [design-decisions.md](../design-decisions.md):

- **[Diagnostic builder pattern](../design-decisions.md#diagnostic-builder-pattern)** — replace inline `self.error(msg, span)` with a fluent `Diagnostic::error(span, msg).with_note(...).with_suggestion(...).emit()` API. Module-system diagnostics are a natural first consumer (multi-span "symbol X is private, defined here: [other file]" errors). Hard deadline: before Phase 3.2.
- **[Interpreter-parser coupling via `Value::Closure`](../design-decisions.md#interpreter-parser-coupling-via-valueclosure)** — switch closures to store IR blocks instead of parser AST blocks, so the interpreter consumes IR like the Cranelift backend does. IR stabilizes during Phase 2.2; doing this in 2.6 targets a settled IR.

### Bugs bundled into this phase

- **[Closure capture type ambiguity with indirect calls](../known-issues.md#closure-capture-type-ambiguity-with-indirect-calls)** — deferred from Phase 2.2. The proper fix requires capture metadata in the IR closure representation, which the `Value::Closure` refactor above already touches. Address as part of that refactor rather than independently.

## 2.7 Benchmark Suite

- Add a benchmark suite (e.g. `criterion`) early in Phase 2 to measure IR lowering and codegen performance
- Track compile times for representative Phoenix programs across changes
- Establish baseline metrics before optimization work begins
- **Why:** Phase 2 introduces compilation where performance becomes user-visible. Without benchmarks, regressions go unnoticed and optimization work has no measurable target.
- **Complexity:** Low — `criterion` integrates directly with Cargo; start with a handful of representative programs.
- **Depends on:** IR (2.1)
