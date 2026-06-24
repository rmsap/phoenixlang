# Phoenix

**Type-safe from database to DOM. One language, no drift.**

[![CI](https://github.com/rmsap/phoenixlang/actions/workflows/ci.yml/badge.svg)](https://github.com/rmsap/phoenixlang/actions/workflows/ci.yml)
[![Tests](https://img.shields.io/badge/tests-3%2C700%2B-brightgreen)](https://github.com/rmsap/phoenixlang/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Phoenix is a statically typed language for full-stack web development where one sound type system spans your whole app — database query, API endpoint, and browser — and the compiler checks every boundary between them.** Write your data model once and the compiler keeps the SQL, the serialization, the API client, and the server handler in agreement: if the two ends drift, the build fails, not production. It feels like the TypeScript and Python you already write — garbage-collected, familiar syntax — and it's safe in a way neither of them is.

*Today:* a compiled language (native via Cranelift, a WebAssembly target, GC, full type system, JavaScript interop, LSP) plus **Phoenix Gen**, a codegen tool that brings the type-safety story to TypeScript / Python / Go teams right now. *Coming:* async/await, typed endpoints, compile-time SQL, a WASM frontend, and refinement types — the full DB-to-DOM vision. See the [roadmap](docs/roadmap.md) and [vision](docs/vision.md).

> When searching online, use **phoenixlang** to distinguish this project from the [Phoenix Framework](https://www.phoenixframework.org/) for Elixir.

---

## Two ways in

This repository ships **two products** that share one compiler front-end, so they cannot drift apart:

- **🚀 [Phoenix Gen](#phoenix-gen--typed-apis-across-languages-today) — usable today.** A multi-language API codegen tool: turn one `.phx` schema into typed clients and servers for **TypeScript, Python, and Go**, plus an **OpenAPI 3.1** spec. No need to learn the Phoenix language — write a schema, generate code, ship it. **Start here if you want type-safe APIs now.**
- **🔭 [The Phoenix language](#the-phoenix-language) — the full vision.** A strict, statically-typed language with its own lexer, parser, type checker, interpreter, and native + WebAssembly backends. The end goal: one type system from the database to the DOM. **Start here if you want to follow or hack on the language.**

---

## Install

Adds `phoenix` and `phoenix-lsp` to `/usr/local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/rmsap/phoenixlang/main/install.sh | sudo sh
```

Or grab binaries directly from [GitHub Releases](https://github.com/rmsap/phoenixlang/releases), or [build from source](CONTRIBUTING.md#building-from-source).

The same `phoenix` binary runs your code (`phoenix run`/`build`) and generates API code (`phoenix gen`). Teams who only want codegen can use the standalone `phoenix-gen` binary instead — it shares the exact same implementation.

---

## Phoenix Gen — typed APIs across languages, today

Write an API schema once in `.phx`; generate typed clients and servers across multiple languages. Field-level constraints (`where`), path/query parameters, response types, and error variants are all type-checked and carried through every target.

```phoenix
struct Post {
  id: Int
  title: String where self.length > 0 && self.length <= 200
  body: String where self.length > 0
  tags: List<String>
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

### No drift — the mismatch shows up in your build, not in production

The schema is the single source of truth for the client *and* the server, so they physically cannot disagree. Generate a TypeScript client and a Go server from the schema above, then rename `title` to `headline` and regenerate:

- the TypeScript frontend still constructing `{ title }` **fails `tsc`**,
- the Go handler still returning `Title` **stops compiling**.

A TypeScript client and a Go server, generated from one schema, can't fall out of sync — the kind of drift that normally surfaces as a 4 PM production bug becomes a compile error. And the `where` constraints aren't documentation: they compile to **real runtime validators** on the server (a too-long `title` is rejected with the error variant you declared, in every target).

See **[docs/phoenix-gen.md](docs/phoenix-gen.md)** for the full guide, or [`tests/fixtures/gen_api.phx`](tests/fixtures/gen_api.phx) for a realistic blog-platform schema.

---

## The Phoenix language

A strict, statically-typed, garbage-collected language that compiles to native code (via Cranelift) and to WebAssembly. The bet: a single sound type system that eventually spans the database query, the API endpoint, and the browser, with the compiler checking every boundary in between.

- **One sound type system, checked at every boundary** — the core bet of the project
- **Two execution modes** — tree-walk interpreter for fast iteration, Cranelift-backed native compilation for production
- **WebAssembly target** — two backends (linear-memory embed-and-merge and inline WASM-GC), byte-identical to native across the fixture matrix, with JavaScript/DOM interop via `extern js`
- **Modern type system** — generics with trait bounds, `dyn Trait` dynamic dispatch, algebraic data types, pattern matching, closures, first-class functions
- **First-class error handling** — built-in `Option<T>`, `Result<T, E>`, the `?` operator, and a rich functional-collection standard library (`map`/`filter`/`reduce`/…), plus `ListBuilder` / `MapBuilder` transient-mutable accumulators for O(n) bulk construction
- **Multi-file modules with `public`/private visibility** — `import a.b.c { Foo }` syntax, lazy import-driven discovery, cross-module visibility enforcement with rich diagnostics
- **Full Language Server Protocol** — diagnostics, hover, autocomplete, go-to-definition, find-references, rename — via a [VS Code extension](https://marketplace.visualstudio.com/items?itemName=rmsap.phoenixlang)

### Examples

#### Hello World

```phoenix
function main() {
  print("Hello, World!")
}
```

#### Traits, generics, and pattern matching

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

#### Static and dynamic dispatch

`<T: Trait>` gives static dispatch (monomorphized). `dyn Trait` gives runtime dispatch through a vtable — use it when you need one function to accept multiple concrete types behind a trait without a generic type parameter. See **[docs/dyn-trait.md](docs/dyn-trait.md)** for the full guide. Both examples below reuse the `Display` trait defined in the previous snippet.

```phoenix
function describeStatic<T: Display>(item: T) -> String { item.toString() }
function describeDyn(item: dyn Display) -> String      { item.toString() }
```

#### Error handling with `Result` and `?`

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

#### Modules and visibility

Each `.phx` file is a module. Declarations are private by default; mark them `public` to export. `import a.b.c { Item }` brings names into scope, with `as` aliases and `{ * }` wildcards. Discovery is lazy (only files reachable via imports are parsed), and the project root is the directory of the entry file.

```phoenix
// models/user.phx
public struct User {
  public name: String
  passwordHash: Int            // private — set via the constructor, not readable from outside
}

public function createUser(name: String) -> User {
  User(name, hash(""))
}

function hash(input: String) -> String { input }   // private helper; importers can't see it
```

```phoenix
// main.phx
import models.user { User, createUser }

function main() {
  let alice: User = createUser("alice")
  print(alice.name)
}
```

See [`tests/fixtures/`](tests/fixtures/) and [`tests/fixtures/multi/`](tests/fixtures/multi/) for more, plus [`crates/phoenix-bench/benches/fixtures/large.phx`](crates/phoenix-bench/benches/fixtures/large.phx).

### CLI

```bash
phoenix run file.phx                       # Execute via the tree-walk interpreter
phoenix run-ir file.phx                    # Execute via the IR interpreter (round-trip verification)
phoenix build file.phx                     # Compile to a native executable via Cranelift
phoenix check file.phx                     # Type-check without running
phoenix gen file.phx                       # Generate API clients/servers (see Phoenix Gen above)
phoenix lex | parse | ir file.phx          # Inspect internal compiler stages
```

`phoenix build` requires a C compiler (gcc or clang) for linking. Run `phoenix --help` for the full command list.

### Editor support

A [VS Code extension](https://marketplace.visualstudio.com/items?itemName=rmsap.phoenixlang) provides syntax highlighting, inline diagnostics, hover type info, autocomplete, go-to-definition, find-references, and rename — powered by the `phoenix-lsp` binary.

---

## Roadmap & Vision

Phase 1 (core language) and all of Phase 2 are complete: IR (2.1), native compilation via Cranelift (2.2), the module system and visibility (2.6), tracing GC + runtime + `defer` syntax (2.3), the benchmark suite + `ListBuilder` / `MapBuilder` transient-mutable accumulators (2.7), the WebAssembly target (2.4 — `wasm32-linear` embed-and-merge plus inline `wasm32-gc`, both byte-identical to native across the full fixture matrix), and JavaScript interop (2.5 — `extern js` as a uniform host-FFI boundary across all five backends). The active phase is 3.1 (package manager). Async/await with structured concurrency, typed database queries, refinement types, and first-class reactivity for a full-stack web language follow.

See **[Roadmap](docs/roadmap.md)** for the implementation timeline and **[Language Vision](docs/vision.md)** for designs of planned features. Phoenix Gen tracks its own [feature set](docs/phoenix-gen.md), [roadmap](docs/phoenix-gen-roadmap.md), and [design decisions](docs/phoenix-gen-design-decisions.md).

---

## Contributing

Phoenix is in early development and contributions are welcome. See **[CONTRIBUTING.md](CONTRIBUTING.md)** for how to get started, build from source, the compilation pipeline, and the crate-by-crate architecture. In short: check the [roadmap](docs/roadmap.md) for priorities, open an issue before large changes, and make sure PRs pass `cargo fmt`, `cargo clippy`, and `cargo test`.

---

## License

MIT — see [LICENSE](LICENSE). Ports of MIT-licensed third-party code
carry their upstream notices in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
