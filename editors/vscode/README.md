# Phoenix for VS Code

Full IDE support for the [Phoenix](https://github.com/rmsap/phoenixlang) programming language via the Language Server Protocol.

## Features

- **Syntax highlighting** for `.phx` files — keywords, types, strings, numbers, comments, operators, endpoint declarations, `where` constraints
- **Inline diagnostics** — errors and warnings from the type checker
- **Hover** — shows the resolved type at the cursor position
- **Autocomplete** — struct, enum, and function names, plus keywords
- **Go-to-definition** — jump to the declaration of a symbol
- **Find references** — locate all uses of a symbol
- **Rename** — rename a symbol across all references

## Requirements

The `phoenix-lsp` binary must be installed and available on your `PATH` (or configure the path in settings).

```bash
# Build from source
git clone https://github.com/rmsap/phoenixlang.git
cd phoenixlang
cargo build --release -p phoenix-lsp
# Add target/release/phoenix-lsp to your PATH
```

## Extension Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `phoenix.lspPath` | `"phoenix-lsp"` | Path to the phoenix-lsp language server binary |

## Development

```bash
cd editors/vscode
npm install
npm run compile
```

Then press **F5** in VS Code to launch the Extension Development Host.
