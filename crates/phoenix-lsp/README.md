# phoenix-lsp

Language Server Protocol implementation for [Phoenix](https://github.com/rmsap/phoenixlang). Provides IDE features for `.phx` files: diagnostics, hover, autocomplete, go-to-definition, find references, and rename.

The server speaks LSP over stdio and is editor-agnostic — any LSP client can talk to it.

## Usage

Build and install the binary:

```sh
cargo install --path crates/phoenix-lsp
# or, from a checkout:
cargo build -p phoenix-lsp --release
```

The binary is named `phoenix-lsp`. Configure your editor to launch it as the language server for `*.phx` files.

A ready-made VS Code extension lives in [`editors/vscode`](../../editors/vscode). For other editors, point the LSP client at the `phoenix-lsp` executable on your `PATH` and register the `phoenix` language id for the `phx` file extension.

## License

MIT — see [LICENSE](../../LICENSE).
