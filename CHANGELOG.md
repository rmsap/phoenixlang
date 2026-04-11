# Changelog

All notable changes to the Phoenix language will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [0.1.0] - 2026-04-11

Initial release. Tree-walk interpreter with full Phase 1 feature set.

### Language Features
- Variables (`let` and `let mut`) with explicit types or type inference
- Compound assignment operators (`+=`, `-=`, `*=`, `/=`, `%=`)
- Functions with typed parameters, return types, named/default parameters
- Control flow: `if`/`else if`/`else`, `while`, `for` (range and collection), `break`/`continue`
- Loop `else` clauses (runs when loop completes without `break`)
- Structs with fields, methods (`impl` blocks and inline methods), field assignment
- Enums/ADTs with `match` (variant destructuring, wildcards, literals), inline methods and trait impls
- Generics on functions, structs, and enums (`<T, U>` syntax with type inference)
- Closures and first-class functions (by-reference capture, higher-order functions)
- `List<T>` with literals and functional methods (`map`, `filter`, `reduce`, `find`, etc.)
- `Map<K, V>` with literals and methods (`get`, `set`, `contains`, `remove`, `keys`, `values`)
- `Option<T>` and `Result<T, E>` (built-in) with `unwrap`, `unwrapOr`, combinators
- `?` operator for error propagation on `Result` and `Option`
- Traits with `trait` declarations, `impl Trait for Type`, and trait bounds (`<T: Display>`)
- String interpolation, string methods, ordering comparisons
- Type aliases (`type UserId = Int`, `type StringResult<T> = Result<T, String>`)
- Implicit return (last expression in block is the return value)
- Pipe operator (`data |> parse() |> validate()`)
- Destructuring (`let Point { x, y } = getPoint()`)
- Recursive types (self-referential enums)
- Logical operators: `&&`, `||`, `!`
- `where` constraints on struct fields for validation
- `//` and `/* */` comments (nestable)
- Built-in `print()` and `toString()`

### Phoenix Gen (Code Generation)
- Endpoint declarations for API schema definition
- `--client` and `--server` flags for generating only client SDK or server handlers
- `phoenix.toml` configuration file for default settings and multi-target config
- TypeScript generation (types, client SDK, handler interfaces, Express router, validation)
- Python generation (Pydantic models, FastAPI handlers, httpx client)
- Go generation (structs with JSON tags, net/http handlers, typed client)
- OpenAPI 3.1 JSON spec generation
- Watch mode (`--watch`) for automatic re-generation on file changes

### Tooling
- CLI with `lex`, `parse`, `check`, `run`, and `gen` subcommands
- VS Code extension with LSP support (diagnostics, hover, autocomplete, go-to-definition, find-references, rename)
- CI pipeline with `cargo fmt`, `clippy`, and `cargo test`
- Cross-platform release builds (Linux, macOS, Windows)
- Install script (`install.sh`)
