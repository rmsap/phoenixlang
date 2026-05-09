# phoenix-codegen

Code generation backends for [Phoenix](https://github.com/rmsap/phoenixlang). Given a parsed and type-checked Phoenix program containing `endpoint` declarations, this crate produces typed client/server code for:

- **TypeScript** — interfaces, derived types, a `fetch`-based client SDK, server handler interfaces, and an Express router
- **Python** — Pydantic models, a typed `httpx` client, a handler `Protocol` class, and a FastAPI router
- **Go** — structs with JSON tags, an HTTP client, a `Handlers` interface, and a `net/http` router
- **OpenAPI** — an OpenAPI 3.1 JSON specification

## Usage

Each backend takes a `&Program` (from `phoenix-parser`) and an `&Analysis` (from `phoenix-sema`) and returns the generated source as strings:

```rust
use phoenix_codegen::generate_typescript;

let files = generate_typescript(&program, &analysis);
std::fs::write("api/types.ts", files.types)?;
std::fs::write("api/client.ts", files.client)?;
std::fs::write("api/server.ts", files.server)?;
```

Use `GenMode` to request only the client or only the server subset. The other backends (`generate_python`, `generate_go`, `generate_openapi`) follow the same shape.

## Documentation

See the crate-level rustdoc for the full API. From the workspace root:

```
cargo doc -p phoenix-codegen --open
```

## License

MIT — see [LICENSE](../../LICENSE).
