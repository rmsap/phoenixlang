# Phoenix

[![CI](https://github.com/rmsap/phoenixlang/actions/workflows/ci.yml/badge.svg)](https://github.com/rmsap/phoenixlang/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Phoenix** is a strict, statically typed programming language designed for web development. It combines functional and object-oriented programming in a clean, familiar syntax with a focus on safe concurrency, async-first design, and developer productivity.

When searching online, use **phoenixlang** to distinguish this project from the [Phoenix Framework](https://www.phoenixframework.org/) for Elixir.

---

## Current Status

Phoenix is in **active development**, written in **Rust** with **1,700+ tests** across **12 crates**. Phoenix programs can be executed via a tree-walk interpreter (`phoenix run`) or compiled to native binaries via Cranelift (`phoenix build`). Current language features:

- Variables (`let` and `let mut`) with explicit types or type inference, **compound assignment** (`+=`, `-=`, `*=`, `/=`, `%=`)
- Functions with typed parameters, return types, **named/default parameters**
- `if`/`else if`/`else`, `while` loops, `for` loops (range-based and **collection-based**), `break`/`continue`
- **Loop `else` clauses** — `for/while ... {} else {}` (else runs when loop completes without `break`)
- Structs with fields, methods (`impl` blocks and **inline methods**), field access, and **field assignment**
- Enums/ADTs with `match` (variant destructuring, wildcards, literals), **inline methods and trait impls**
- **Generics** on functions, structs, and enums (`<T, U>` syntax with type inference)
- **Closures** and first-class functions (by-reference capture, higher-order functions)
- **`List<T>`** with `[1, 2, 3]` literals and **functional methods** (`map`, `filter`, `reduce`, `find`, `any`, `all`, `flatMap`, `sortBy`, `first`, `last`, `contains`, `take`, `drop`)
- **`Map<K, V>`** with `{"key": value}` literals, `get()`, `set()`, `contains()`, `remove()`, `keys()`, `values()`
- **`Option<T>`** and **`Result<T, E>`** (built-in) with `unwrap()`, `unwrapOr()`, `isSome()`/`isOk()`, and **combinators** (`map`, `andThen`, `orElse`, `filter`, `okOr`, `mapErr`, `unwrapOrElse`)
- **`?` operator** for concise error propagation on `Result` and `Option` values
- **Traits** with `trait` declarations, `impl Trait for Type`, and trait bounds on generics (`<T: Display>`)
- **String interpolation** — `"hello {name}, you are {age} years old"` (non-string values are automatically converted)
- **String methods** — `length`, `contains`, `startsWith`, `endsWith`, `trim`, `split`, `replace`, `substring`, `indexOf`, `toLowerCase`, `toUpperCase`, plus **ordering comparisons**
- **Type aliases** — `type UserId = Int`, `type StringResult<T> = Result<T, String>`
- **Implicit return** — last expression in a function/closure/match-arm/if-else block is the return value
- **Pipe operator** — `data |> parse() |> validate()`
- **Destructuring** — `let Point { x, y } = getPoint()`
- **Recursive types** — self-referential enums (linked lists, trees)
- `//` line comments and `/* */` block comments (nestable)
- Built-in `print()` and `toString()`
- **Endpoint declarations** for API schema definition ([Phoenix Gen](docs/phoenix-gen.md))
- **`where` constraints** on struct fields for validation (`String name where self.length > 0`, `Int age where self >= 0`)
- **CI pipeline** with `cargo fmt`, `clippy`, and `cargo test`

**In progress:** [Phase 2 — Compilation](docs/roadmap.md) (IR lowering complete, Cranelift native compilation working for value types, strings, structs, enums, closures, and function calls; Lists/Maps and builtin methods not yet supported in compiled mode — use `phoenix run` for full coverage; WebAssembly target next).

---

## Getting Started

### Quick install

```bash
curl -fsSL https://raw.githubusercontent.com/rmsap/phoenixlang/main/install.sh | sudo sh
```

This installs `phoenix` and `phoenix-lsp` to `/usr/local/bin`. To install without `sudo`, set a user-local directory:

```bash
curl -fsSL https://raw.githubusercontent.com/rmsap/phoenixlang/main/install.sh | PHOENIX_INSTALL_DIR=~/.local/bin sh
```

Or download binaries directly from [GitHub Releases](https://github.com/rmsap/phoenixlang/releases).

### Build from source

Requires [Rust](https://www.rust-lang.org/tools/install) (stable toolchain).

```bash
git clone https://github.com/rmsap/phoenixlang.git
cd phoenixlang
cargo build --release
```

### Run a program

Phoenix source files use the `.phx` extension.

```bash
# Run a Phoenix program
./target/release/phoenix run path/to/file.phx
```

### Other commands

```bash
phoenix lex file.phx     # Tokenize and print the token stream
phoenix parse file.phx    # Parse and dump the AST as JSON
phoenix check file.phx    # Type-check without running
phoenix ir file.phx       # Dump the SSA-style intermediate representation
phoenix run file.phx      # Execute via the tree-walk interpreter
phoenix run-ir file.phx   # Execute via the IR interpreter (round-trip verification)
phoenix build file.phx    # Compile to a native executable via Cranelift
phoenix build file.phx -o out  # Compile with a custom output name
# Note: `phoenix build` requires a C compiler (gcc or clang) for linking.
phoenix gen file.phx                      # Generate TypeScript (types, client, handlers, server)
phoenix gen file.phx --target python      # Generate Python (Pydantic, FastAPI, httpx)
phoenix gen file.phx --target go          # Generate Go (structs, net/http, client)
phoenix gen file.phx --target openapi     # Generate OpenAPI 3.1 JSON spec
phoenix gen file.phx --client             # Generate only types + client SDK
phoenix gen file.phx --server             # Generate only types + handlers + router
phoenix gen file.phx --watch              # Watch for changes and re-generate
phoenix gen                               # Use settings from phoenix.toml
```

---

## Code Examples

### Hello World

```phoenix
function main() {
  print("Hello, World!")
}
```

### Variables and Functions

```phoenix
// Explicit type annotations
let x: Int = 42
let greeting: String = "Hello"
let active: Bool = true
let pi: Float = 3.14159

// Type inference — the compiler infers the type from the initializer
let name = "Phoenix"        // String
let count = 10              // Int
let mut sum = 0             // Int, mutable
sum += 1

// Functions with typed parameters and return type
function add(a: Int, b: Int) -> Int {
  a + b
}

// Functions that return nothing omit the return type
function greet(name: String) {
  print("Hello, {name}!")
}
```

### Control Flow and Loops

```phoenix
function fizzbuzz(n: Int) -> String {
  if n % 15 == 0 { return "FizzBuzz" }
  if n % 3 == 0 { return "Fizz" }
  if n % 5 == 0 { return "Buzz" }
  toString(n)
}

// For loop with range (0..n is exclusive of n)
function sumTo(n: Int) -> Int {
  let mut total: Int = 0
  for i in 0..n {
    total += i
  }
  total
}
```

### Structs and Methods

```phoenix
struct User {
  String name
  String email
  Int age

  function display(self) -> String {
    "{self.name} <{self.email}>"
  }

  function isAdult(self) -> Bool {
    self.age >= 18
  }
}

function main() {
  let alice: User = User("Alice", "alice@example.com", 30)
  print(alice.display())     // Alice <alice@example.com>
  print(alice.isAdult())    // true
}
```

### Enums and Pattern Matching

```phoenix
enum Shape {
  Circle(Float)
  Rect(Float, Float)

  function area(self) -> Float {
    match self {
      Circle(r) -> 3.14159 * r * r
      Rect(w, h) -> w * h
    }
  }
}

function main() {
  let s: Shape = Circle(5.0)
  print(s.area())   // 78.53975
}
```

### Closures and First-Class Functions

```phoenix
// Functions are values
let doubler: (Int) -> Int = function(x: Int) -> Int { x * 2 }

// Closures capture variables by reference
function makeAdder(n: Int) -> (Int) -> Int {
  function(x: Int) -> Int { x + n }
}
let add5: (Int) -> Int = makeAdder(5)
print(add5(10))  // 15
```

### Generics and Traits

```phoenix
function identity<T>(x: T) -> T { x }

trait Display {
  function toString(self) -> String
}

struct Pair<A, B> {
  A first
  B second

  impl Display {
    function toString(self) -> String {
      "({toString(self.first)}, {self.second})"
    }
  }
}

function show<T: Display>(item: T) -> String {
  item.toString()
}
```

### Error Handling

```phoenix
// Result and Option are built-in — no declaration needed
let ok: Result<Int, String> = Ok(42)
let none: Option<Int> = None

// The ? operator propagates errors
function doubleParsed(s: String) -> Result<Int, String> {
  let value: Int = parse(s)?
  Ok(value * 2)
}

// Pattern matching on Result/Option
match ok {
  Ok(val) -> print("Success: {toString(val)}")
  Err(msg) -> print("Error: {msg}")
}
```

### Collections

```phoenix
// List literals and functional methods
let nums: List<Int> = [1, 2, 3, 4, 5]
let evens: List<Int> = nums.filter(function(n: Int) -> Bool { n % 2 == 0 })
let doubled: List<Int> = nums.map(function(n: Int) -> Int { n * 2 })

// Map literals
let scores: Map<String, Int> = {"alice": 95, "bob": 87}
print(scores.get("alice"))  // 95
```

### Pipes

```phoenix
// Left-to-right function chaining
let result: String = data |> parse() |> validate() |> format()
```

---

## Language Vision

Phoenix aims to be a **full-stack web language** that compiles to native code (backend) and WebAssembly (frontend), with built-in serialization, typed endpoints, typed database queries, refinement types, and first-class reactivity.

See **[Language Vision](docs/vision.md)** for detailed designs and code examples of planned features, and the **[Roadmap](docs/roadmap.md)** for implementation timeline and priorities.

---

## Editor Support

A **[VS Code extension](https://marketplace.visualstudio.com/items?itemName=rmsap.phoenixlang)** is available with full **Language Server Protocol** support via the `phoenix-lsp` binary:

- **Syntax highlighting** for `.phx` files (keywords, types, strings, numbers, comments, operators, endpoint declarations, `where` constraints)
- **Inline diagnostics** — errors and warnings as editor squiggles
- **Hover** — shows resolved type at cursor
- **Autocomplete** — struct/enum/function names, keywords
- **Go-to-definition** — jump to declarations from references
- **Find references** — locate all uses of a symbol
- **Rename** — rename a symbol across all references

To use: build `phoenix-lsp` with `cargo build -p phoenix-lsp`, then open `editors/vscode/` in VS Code, run `npm install && npm run compile`, and press **F5** to launch the Extension Development Host.

---

## Project Structure

Phoenix is implemented in Rust as a Cargo workspace:

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

---

## Contributing

Phoenix is in early development and contributions are welcome. If you're interested in contributing:

1. Check the [roadmap](docs/roadmap.md) for current priorities
2. Look at [known issues](docs/known-issues.md) for things that need fixing
3. Open an issue to discuss before starting large changes
4. All PRs should pass `cargo fmt`, `cargo clippy`, and `cargo test`

---

## Why "Phoenix"?

The phoenix is a mythical bird that rises from its own ashes, symbolizing rebirth, resilience, and renewal. Phoenix aims to reimagine web development with a language that is safe, expressive, and productive — rising above the complexity of existing ecosystems to let developers write full-stack code in a single language, with compile-time safety from client to database.

---

## License

MIT
