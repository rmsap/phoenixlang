# Phoenix

**A strict, statically typed programming language for full-stack web development.**

[![CI](https://github.com/rmsap/phoenixlang/actions/workflows/ci.yml/badge.svg)](https://github.com/rmsap/phoenixlang/actions/workflows/ci.yml)
[![Tests](https://img.shields.io/badge/tests-2%2C200%2B-brightgreen)](https://github.com/rmsap/phoenixlang/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Phoenix combines functional and object-oriented programming with a clean, familiar syntax and a focus on safe concurrency, async-first design, and developer productivity. Programs run on a tree-walk interpreter (`phoenix run`) or compile to native binaries via Cranelift (`phoenix build`), and API schemas can be code-generated to TypeScript, Python, Go, or OpenAPI.

> When searching online, use **phoenixlang** to distinguish this project from the [Phoenix Framework](https://www.phoenixframework.org/) for Elixir.

---

## Highlights

- **Two execution modes** — tree-walk interpreter for fast iteration, Cranelift-backed native compilation for production
- **SSA-style intermediate representation** with a round-trip IR interpreter for verification
- **Multi-target API codegen** — generate TypeScript, Python, Go, or OpenAPI 3.1 clients and servers from `.phx` schemas
- **Full Language Server Protocol** — diagnostics, hover, autocomplete, go-to-definition, find-references, rename
- **Modern type system** — generics with trait bounds, algebraic data types, pattern matching, closures, first-class functions
- **First-class error handling** — built-in `Option<T>`, `Result<T, E>`, the `?` operator, and a rich functional-collection standard library

---

## Compilation Pipeline

```
 .phx source
     │
     ▼
┌─────────┐   ┌──────────┐   ┌─────────┐   ┌────────┐
│  Lexer  │──▶│  Parser  │──▶│  Sema   │──▶│   IR   │
└─────────┘   └──────────┘   └─────────┘   └────┬───┘
                                                │
         ┌───────────────────┬──────────────────┼──────────────────┐
         ▼                   ▼                  ▼                  ▼
   tree-walk interp    IR round-trip    Cranelift native       Codegen
    (phoenix run)         interp        (phoenix build)    TS · Py · Go · OpenAPI
                                                              (phoenix gen)
```

---

## Quick Start

Install (adds `phoenix` and `phoenix-lsp` to `/usr/local/bin`):

```bash
curl -fsSL https://raw.githubusercontent.com/rmsap/phoenixlang/main/install.sh | sudo sh
```

Or grab binaries directly from [GitHub Releases](https://github.com/rmsap/phoenixlang/releases).

Build from source (requires [Rust](https://www.rust-lang.org/tools/install) stable):

```bash
git clone https://github.com/rmsap/phoenixlang.git
cd phoenixlang
cargo build --release
./target/release/phoenix run path/to/file.phx
```

---

## Examples

### Hello World

```phoenix
function main() {
  print("Hello, World!")
}
```

### Traits, generics, and pattern matching

```phoenix
trait Display {
  function toString(self) -> String
}

enum Shape {
  Circle(Float)
  Rect(Float, Float)

  impl Display {
    function toString(self) -> String {
      match self {
        Circle(r) -> "circle(r={toString(r)})"
        Rect(w, h) -> "rect({toString(w)}x{toString(h)})"
      }
    }
  }

  function area(self) -> Float {
    match self {
      Circle(r) -> 3.14159 * r * r
      Rect(w, h) -> w * h
    }
  }
}

function describe<T: Display>(item: T) -> String {
  item.toString()
}

function main() {
  let shapes: List<Shape> = [Circle(5.0), Rect(3.0, 4.0)]
  let areas: List<Float> = shapes.map(function(s: Shape) -> Float { s.area() })
  let total: Float = areas.reduce(0.0, function(a: Float, b: Float) -> Float { a + b })
  print("total area: {toString(total)}")
  match shapes.first() {
    Some(s) -> print(describe(s))
    None -> print("no shapes")
  }
}
```

### Error handling with `Result` and `?`

```phoenix
function safeDivide(a: Int, b: Int) -> Result<Int, String> {
  if b == 0 {
    Err("cannot divide by zero")
  } else {
    Ok(a / b)
  }
}

function computeRatio(a: Int, b: Int) -> Result<Int, String> {
  let q: Int = safeDivide(a, b)?
  Ok(q * 2)
}

function main() {
  match computeRatio(42, 3) {
    Ok(v) -> print("got {toString(v)}")
    Err(msg) -> print("error: {msg}")
  }
}
```

See [`tests/fixtures/`](tests/fixtures/) and [`crates/phoenix-bench/benches/fixtures/large.phx`](crates/phoenix-bench/benches/fixtures/large.phx) for more.

---

## Phoenix Gen

Write an API schema once in `.phx`; generate typed clients and servers across multiple languages. Field-level constraints (`where`), path parameters, query parameters, response types, and error variants are all type-checked and carried through every target.

```phoenix
struct Post {
  Int id
  String title where self.length > 0 && self.length <= 200
  String body where self.length > 0
  List<String> tags
}

endpoint createPost: POST "/api/posts" {
  body Post omit { id }
  response Post
  error {
    ValidationError(400)
    Unauthorized(401)
  }
}
```

```bash
phoenix gen api.phx                      # TypeScript (types, client, handlers, server)
phoenix gen api.phx --target python      # Python (Pydantic, FastAPI, httpx)
phoenix gen api.phx --target go          # Go (structs, net/http, client)
phoenix gen api.phx --target openapi     # OpenAPI 3.1 JSON spec
phoenix gen api.phx --client             # Types + client SDK only
phoenix gen api.phx --server             # Types + handlers + router only
phoenix gen api.phx --watch              # Regenerate on change
```

See **[docs/phoenix-gen.md](docs/phoenix-gen.md)** for the full guide, or [`tests/fixtures/gen_api.phx`](tests/fixtures/gen_api.phx) for a realistic blog-platform schema.

---

## Language Features

**Syntax & Types**
- `let` / `let mut` with type inference or explicit annotations, compound assignment (`+=`, `-=`, `*=`, `/=`, `%=`)
- Functions with typed, named, and default parameters
- Structs with inline methods, field assignment, and `where` field constraints (`String name where self.length > 0`)
- Enums/ADTs with variant destructuring, wildcards, literals, and inline methods & trait impls
- Generics on functions, structs, and enums with trait bounds (`<T: Display>`)
- Traits with `trait` declarations and `impl Trait for Type`
- Type aliases, recursive types, destructuring, implicit returns
- Pipe operator (`|>`), string interpolation, block and line comments

**Collections & Errors**
- `List<T>` literals with `map`, `filter`, `reduce`, `find`, `any`, `all`, `flatMap`, `sortBy`, `first`, `last`, `contains`, `take`, `drop`
- `Map<K, V>` literals with `get`, `set`, `contains`, `remove`, `keys`, `values`
- Built-in `Option<T>` and `Result<T, E>` with full combinator sets (`map`, `andThen`, `orElse`, `mapErr`, `filter`, `okOr`, `unwrapOrElse`, …)
- `?` operator for error propagation
- Rich `String` method suite with ordering comparisons

**Tooling & Codegen**
- Tree-walk interpreter and Cranelift-backed native compilation
- SSA-style IR with a standalone round-trip interpreter
- Full LSP server and [VS Code extension](https://marketplace.visualstudio.com/items?itemName=rmsap.phoenixlang)
- Phoenix Gen targets: TypeScript, Python (Pydantic/FastAPI/httpx), Go (net/http), OpenAPI 3.1 — see [docs/phoenix-gen.md](docs/phoenix-gen.md)
- Endpoint declarations for typed API schemas
- CI pipeline enforcing `cargo fmt`, `clippy`, and `cargo test`

---

## CLI

```bash
phoenix run file.phx             # Execute via the tree-walk interpreter
phoenix build file.phx           # Compile to a native executable via Cranelift
phoenix check file.phx           # Type-check without running
phoenix gen file.phx             # Generate API clients/servers (see Phoenix Gen above)
phoenix lex | parse | ir file.phx   # Inspect internal compiler stages
```

`phoenix build` requires a C compiler (gcc or clang) for linking. Run `phoenix --help` for the full command list.

---

## Editor Support

A [VS Code extension](https://marketplace.visualstudio.com/items?itemName=rmsap.phoenixlang) provides syntax highlighting, inline diagnostics, hover type info, autocomplete, go-to-definition, find-references, and rename — powered by the `phoenix-lsp` binary.

---

## Architecture

Phoenix is implemented in Rust as a Cargo workspace of 13 crates, each representing one stage of the compiler pipeline or an independent tool.

| Crate | Purpose |
|-------|---------|
| `phoenix-common` | Shared types (spans, diagnostics, source maps) |
| `phoenix-lexer` | Tokenization |
| `phoenix-parser` | Recursive-descent parser and AST |
| `phoenix-sema` | Semantic analysis (name resolution and type checking) |
| `phoenix-interp` | Tree-walk interpreter |
| `phoenix-ir` | SSA-style intermediate representation and AST-to-IR lowering |
| `phoenix-ir-interp` | IR interpreter for round-trip verification |
| `phoenix-cranelift` | Cranelift-based native code generation |
| `phoenix-runtime` | Runtime library linked into compiled Phoenix binaries |
| `phoenix-codegen` | Code generation backends (TypeScript, Python, Go, OpenAPI) |
| `phoenix-lsp` | Language Server Protocol server |
| `phoenix-driver` | CLI binary |
| `phoenix-bench` | Benchmarks for the compiler pipeline (Criterion) |

---

## Roadmap & Vision

Phase 1 (core language) is complete; Phase 2 (native compilation via Cranelift) is largely done, with garbage collection and a WebAssembly backend next. Planned beyond that: async/await with structured concurrency, typed database queries, refinement types, and first-class reactivity for a full-stack web language.

See **[Roadmap](docs/roadmap.md)** for the implementation timeline and **[Language Vision](docs/vision.md)** for designs of planned features.

---

## Contributing

Phoenix is in early development and contributions are welcome.

1. Check the [roadmap](docs/roadmap.md) for current priorities
2. Look at [known issues](docs/known-issues.md) for things that need fixing, or [design decisions](docs/design-decisions.md) for open questions about the language
3. Open an issue to discuss before starting large changes
4. All PRs should pass `cargo fmt`, `cargo clippy`, and `cargo test` — git hooks (pre-commit: fmt + clippy; pre-push: tests) install automatically via [cargo-husky](https://github.com/rhysd/cargo-husky) on first `cargo test`

---

## License

MIT
