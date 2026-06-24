# Contributing to Phoenix

Phoenix is in early development and contributions are welcome.

1. Check the [roadmap](docs/roadmap.md) for current priorities.
2. Look at [known issues](docs/known-issues.md) for things that need fixing, or [design decisions](docs/design-decisions.md) for open questions about the language (Phoenix Gen has its own [design-decisions doc](docs/phoenix-gen-design-decisions.md)).
3. Open an issue to discuss before starting large changes.
4. All PRs should pass `cargo fmt`, `cargo clippy`, and `cargo test` — git hooks (pre-commit: fmt + clippy; pre-push: tests) install automatically via [cargo-husky](https://github.com/rhysd/cargo-husky) on first `cargo test`.

---

## Building from source

Requires [Rust](https://www.rust-lang.org/tools/install) stable.

```bash
git clone https://github.com/rmsap/phoenixlang.git
cd phoenixlang
cargo build --release
./target/release/phoenix run path/to/file.phx
```

`phoenix build` (native compilation) requires a C compiler (gcc or clang) for linking.

---

## Compilation pipeline

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
   tree-walk interp    IR round-trip    native + WASM          Codegen
    (phoenix run)         interp        (phoenix build)    TS · Py · Go · OpenAPI
                                                              (phoenix gen)
```

---

## Architecture

Phoenix is implemented in Rust as a Cargo workspace of 15 crates, each representing one stage of the compiler pipeline or an independent tool.

| Crate | Purpose |
|-------|---------|
| `phoenix-common` | Shared types (spans, diagnostics, source maps, module paths) |
| `phoenix-lexer` | Tokenization |
| `phoenix-parser` | Recursive-descent parser and AST |
| `phoenix-modules` | Module resolver: maps an entry `.phx` file to a deterministic, lazy import-driven set of parsed modules |
| `phoenix-sema` | Semantic analysis (name resolution, visibility, and type checking) |
| `phoenix-interp` | Tree-walk interpreter |
| `phoenix-ir` | SSA-style intermediate representation and AST-to-IR lowering |
| `phoenix-ir-interp` | IR interpreter for round-trip verification |
| `phoenix-cranelift` | Cranelift-based native code generation (and the WebAssembly backends) |
| `phoenix-runtime` | Runtime library linked into compiled Phoenix binaries |
| `phoenix-codegen` | Code generation backends (TypeScript, Python, Go, OpenAPI) |
| `phoenix-lsp` | Language Server Protocol server |
| `phoenix-driver` | CLI binary |
| `phoenix-bench` | Benchmarks for the compiler pipeline (Criterion) |
| `phoenix-bench-diff` | Post-merge regression detector: diffs criterion output against committed `docs/perf-baselines/` |
