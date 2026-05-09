# phoenix-driver

The `phoenix` command-line driver for the [Phoenix](https://github.com/rmsap/phoenixlang) programming language. This crate wires the lexer, parser, semantic checker, IR builder, tree-walk interpreter, Cranelift backend, and codegen crates into a single binary.

## Subcommands

| Command  | Description |
|----------|-------------|
| `check`  | Type-check a `.phx` file and report errors |
| `run`    | Execute a Phoenix program via the tree-walk interpreter |
| `build`  | Compile a Phoenix program to a native executable via Cranelift |
| `gen`    | Generate typed client/server code or OpenAPI specs from a schema (`--watch` supported) |
| `run-ir` | Run a Phoenix program via the IR interpreter |
| `lex` / `parse` / `ir` | Dump intermediate representations for debugging |

## Usage

```sh
# Build the binary
cargo build -p phoenix-driver --release

# Type-check, run, and compile
phoenix check examples/hello.phx
phoenix run examples/hello.phx
phoenix build examples/hello.phx -o ./hello

# Generate a TypeScript client + server from a schema
phoenix gen api.phx --target typescript --out ./generated

# Watch a schema and re-generate on save
phoenix gen api.phx --target typescript --out ./generated --watch
```

Run `phoenix --help` or `phoenix <subcommand> --help` for the full flag list. Codegen targets and output paths can also be set in a `phoenix.toml` at the project root — see [`phoenix.toml.example`](../../phoenix.toml.example).

## License

MIT — see [LICENSE](../../LICENSE).
