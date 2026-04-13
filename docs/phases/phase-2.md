# Phase 2: Compilation

**Status: In progress (2.1 started)**

Move from interpretation to native code generation. This is what makes Phoenix a real language rather than a scripting tool.

## 2.1 Intermediate Representation (IR)

**Status: In progress.** The `phoenix-ir` crate implements an SSA-style IR with basic blocks, typed instructions, and explicit control flow. The lowering pass converts the type-checked AST into IR for all major language features (arithmetic, control flow, structs, enums, match, closures, method calls, collections, try operator, string interpolation). Use `phoenix ir <file.phx>` to inspect the output. The `phoenix-ir-interp` crate provides an IR interpreter for round-trip verification — use `phoenix run-ir <file.phx>` to execute via the IR and compare output with `phoenix run`. Round-trip tests cover all lowered features including the try operator; see `crates/phoenix-ir-interp/tests/` for the full suite. Next step: Cranelift integration.

- Lower the type-checked AST to a flat, SSA-style IR
- Basic blocks, typed instructions, explicit control flow
- This decouples semantic analysis from code generation
- Makes it possible to target multiple backends (native, WASM)

## 2.2 Native Compilation (Cranelift)

- Translate Phoenix IR to Cranelift IR
- Produce native executables via `cranelift-object` + system linker
- Start with debug builds only (no optimization)
- Keep the interpreter available as a fast-feedback mode (`phoenix run` = interpret, `phoenix build` = compile)
- **Why Cranelift over LLVM:** pure Rust dependency, fast compile times, built-in WASM support. Add LLVM as an optional optimizing backend later.

## 2.3 Runtime Library

- A small Rust library linked into every compiled Phoenix binary
- Garbage collector (tracing GC or reference-counted — TBD during compiler development)
- String implementation (UTF-8, immutable by default)
- Panic/abort handler
- Built-in function implementations (`print`, `toString`)

## 2.4 WebAssembly Target

- Add WASM output via Cranelift's `wasm32` support
- Slim runtime for the browser
- Bridge to browser APIs via imports (DOM manipulation, fetch, etc.)
- Shared types between backend and frontend targets

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

## 2.7 Benchmark Suite

- Add a benchmark suite (e.g. `criterion`) early in Phase 2 to measure IR lowering and codegen performance
- Track compile times for representative Phoenix programs across changes
- Establish baseline metrics before optimization work begins
- **Why:** Phase 2 introduces compilation where performance becomes user-visible. Without benchmarks, regressions go unnoticed and optimization work has no measurable target.
- **Complexity:** Low — `criterion` integrates directly with Cargo; start with a handful of representative programs.
- **Depends on:** IR (2.1)
