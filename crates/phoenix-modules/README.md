# phoenix-modules

Module resolver for the [Phoenix](https://github.com/rmsap/phoenixlang) programming language. Given an entry `.phx` file, it produces a deterministic, topologically-ordered list of parsed modules reachable through the transitive `import` graph.

Discovery is **lazy**: only files reachable from the entry are parsed. The output order is BFS-from-entry with a lexical-path tiebreak, and is load-bearing — downstream `FuncId`/`StructId` allocators iterate it and depend on the order being stable across runs.

## Usage

```rust
use phoenix_common::source::SourceMap;
use phoenix_modules::resolve;

let mut source_map = SourceMap::new();
let modules = resolve(std::path::Path::new("src/main.phx"), &mut source_map)?;

for m in &modules {
    println!("{} ({})", m.module_path, m.file_path.display());
}
```

`resolve_with_overlay` is the same entry point but consults an in-memory map of unsaved buffer contents before reading from disk — that's the path the LSP uses so live edits flow through the resolver without writing to disk.

Errors (missing modules, ambiguous `foo.phx` vs `foo/mod.phx`, import cycles) are surfaced as `ResolveError` variants with rich span information for diagnostics.

## Documentation

```
cargo doc -p phoenix-modules --open
```

For the design rationale (root selection, `mod.phx` rules, entry-point handling), see [`docs/design-decisions.md`](../../docs/design-decisions.md) under "Module system".

## License

MIT — see [LICENSE](../../LICENSE).
