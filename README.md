# Phoenix

[![CI](https://github.com/rsaperstein/phoenixlang/actions/workflows/ci.yml/badge.svg)](https://github.com/rsaperstein/phoenixlang/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Phoenix** is a strict, statically typed programming language designed for web development. It combines functional and object-oriented programming in a clean, familiar syntax with a focus on safe concurrency, async-first design, and developer productivity.

When searching online, use **phoenixlang** to distinguish this project from the [Phoenix Framework](https://www.phoenixframework.org/) for Elixir.

---

## Current Status

Phoenix is in **active development**. The current implementation is a **tree-walk interpreter** written in **Rust** with **1,082 tests** across the following features:

- Variables (`let` and `let mut`) with explicit types or type inference
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
- **String interpolation** — `"hello {name}, you are {age} years old"`
- **String methods** — `length`, `contains`, `startsWith`, `endsWith`, `trim`, `split`, `replace`, `substring`, `indexOf`, `toLowerCase`, `toUpperCase`, plus **ordering comparisons**
- **Type aliases** — `type UserId = Int`, `type StringResult<T> = Result<T, String>`
- **Implicit return** — last expression in a function/closure/match-arm/if-else block is the return value
- **Pipe operator** — `data |> parse() |> validate()`
- **Destructuring** — `let Point { x, y } = getPoint()`
- **Recursive types** — self-referential enums (linked lists, trees)
- `//` line comments and `/* */` block comments (nestable)
- Built-in `print()` and `toString()`
- **CI pipeline** with `cargo fmt`, `clippy`, and `cargo test`

**Next up:** [Phase 2 — Compilation](docs/roadmap.md) (IR design, Cranelift native compilation, WebAssembly target).

---

## Getting Started

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (stable toolchain)

### Build

```bash
git clone https://github.com/rsaperstein/phoenix.git
cd phoenix
cargo build --release
```

### Run a program

Phoenix source files use the `.phx` extension.

```bash
# Run a Phoenix program
cargo run -- run path/to/file.phx

# Or use the built binary directly
./target/release/phoenix run path/to/file.phx
```

### Other commands

```bash
phoenix lex file.phx     # Tokenize and print the token stream
phoenix parse file.phx    # Parse and dump the AST as JSON
phoenix check file.phx    # Type-check without running
phoenix run file.phx      # Execute the program
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
sum = sum + 1

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
    total = total + i
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

## Project Structure

Phoenix is implemented in Rust as a Cargo workspace:

| Crate | Purpose |
|-------|---------|
| `phoenix-common` | Shared types (spans, diagnostics, source maps) |
| `phoenix-lexer` | Tokenization |
| `phoenix-parser` | Recursive-descent parser and AST |
| `phoenix-sema` | Semantic analysis (name resolution and type checking) |
| `phoenix-interp` | Tree-walk interpreter |
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
