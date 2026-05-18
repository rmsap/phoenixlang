# Changelog

All notable changes to the Phoenix language will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).
## [0.2.0] - 2026-05-18


### Changed

- Add VS Code Extension link to README and phoenix-gen doc
- Implement IR lowering
- Implement IR interpreter for testing
- Initial Cranelift implementation
- Update docs with frontend framework plan
- Implement builtin String method support in compiler
- Implement compilation for remaining builtin functions
- Fix error in CI: libphoenix_runtime.a must exist before cranelift tests run
- Add husky pre-commit hook to confirm formatting and clippy and pre-push hook to verify tests pass
- Implement benchmarking
- Refactor if from statement to expression
- Update README, Phoenix Gen documentation, and vision
- Update documentation and make design decisions
- Create a centralized Layout trait for reference types
- Add Result.ok() and Result.err() to Cranelift
- Implement generic monomorphization
- Fix bug where Option.okOr() type was not inferred correctly
- Initial dynamic dispatch implementation
- Implement proper compilation for generic structs
- Fix compilation issues for static traits
- Fix bugs with default arguments and implement <T: Trait> → dyn Trait coercion
- Implement compilation for default argument values on inherent-impl method calls
- Implement ResolvedModule to cleanly separate sema -> IR handoff
- Close out phase 2
- Add 2.6 exit criteria
- Implement foundation for diagnostic builder refactor
- Fix closure capture type ambiguity, complete generic-template stub typed split and ValueID allocator typed split
- Module-system foundation: Implement keywords in lexer, add visibility to parser, update documentation, create module crate with module path resolution and error variants
- Set up driver to handle multi-module resolution
- Implement per-module name mangling and scoped lookup
- Implement module name mangling in IR lowering
- Fix leak of private default expressions across modules
- Complete cross-module type resolution
- Documentation to close out phase 2.6
- Update LSP to support multi-module
- Update documentation to reflect phase changes
- Initial garbage collector implementation
- Implement defer syntax
- Fix hash map O(n) lookup
- Implement merge sort for List.sortBy
- Implement valgrind integration test to confirm no leaks, close phase 2.3
- Update documentation for phase 2.7
- Update LSP to parse all keywords correctly
- Update documentation
- Fix CI failure
- Fix CI again
- Add READMEs for external crates
- Update benchmarking to include allocation
- Implement collections benchmark criterion
- Implement benchmark harness
- Add CI for benchmarking and library to compare benchmark diffs
- Implement Go corpus for benchmarking
- Initial Phoenix vs. Go comparison
- Implemenent type threading through allocation
- Implement ListBuilder and MapBuilder
- Documentation updates
- Implement backend abstraction and wire target into driver
- Implement wasm-encoder to emit WASM; update documentation to reflect pivot due to Cranelift misunderstanding
- Implement compilation for phoenix-runtime to wasm32-wasip1

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
