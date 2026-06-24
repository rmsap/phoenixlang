# Phase 3: Tooling

**Status: Not started**

Developers will not adopt a language without good tooling. Phase 3 (tooling) and [Phase 4](./phase-4.md) (the standard library) are **independent tracks that run in parallel** — nothing in Phase 3 depends on Phase 4. Every Phase 3 item rests only on foundations that already shipped in Phase 2: the package manager (3.1) on the module system (2.6), the LSP gap-closing (3.2) on the 2.6 LSP pipeline, the formatter (3.3) on the parser, and error-message quality (3.5) on the 2.6 diagnostic builder.

Annotations ([4.5](./phase-4.md#45-annotation-system)) are the keystone for the **stdlib** track, not for tooling — they unblock JSON serialization, config loading, database hints, and the test framework, so 4.5 is the first item on the Phase 4 track. The only cross-track touch-point is the formatter (3.3), which will need to format `@annotation` syntax once 4.5 lands (see 3.3).

## Recommended order

- **In parallel from the start:** 3.1 Package Manager, 3.2 LSP gap-closing, 3.3 Formatter — all independent of each other and of Phase 4. The shared constraint is intra-Phase-3, not cross-phase: the formatter and LSP both consume whatever grammar the parser produces, so any new language surface (e.g. 4.5 annotations) lands before, or is followed up in, those tools.
- **Continuous:** 3.5 Error Message quality — the diagnostic-builder foundation already landed in 2.6, so this is an ongoing investment that improves with every release rather than a discrete milestone.
- **On the stdlib track (Phase 4):** 4.5 Annotations goes first; see [Phase 4](./phase-4.md#recommended-order).

## 3.1 Package Manager

**Status: scoped, not started.** This is the active item on the tooling track. **Depends on:** Module system and visibility (2.6, complete) — cross-package imports build on intra-project modules. Independent of all Phase 4 work (see [Parallel-track note](#31-parallel-track-note) below).

### Goal

A `phoenix.toml`-driven package manager: declare a package and its dependencies, resolve them (semver), fetch git-based dependencies into a cache, pin them in a lockfile, and let `import` reach across package boundaries. Make `phoenix build` / `run` / `check` dependency-aware so a multi-package project builds from a clean checkout.

### Current state (what exists today)

- `crates/phoenix-driver/src/config.rs` — `PhoenixConfig` parses `phoenix.toml` with **only** a `[gen]` section (`#[serde(deny_unknown_fields)]`); `find_and_load(start_dir)` walks up the tree for the manifest. TOML plumbing (`toml` 0.8), `serde`, and a tempdir CLI test pattern (`crates/phoenix-driver/tests/`) already exist.
- `crates/phoenix-modules/src/lib.rs` — `resolve()` / `resolve_with_overlay()` compute the project root as the entry file's parent dir; `resolve_module_path(root, root_canon, target, span)` maps `a.b.c` → `<root>/a/b/c.phx` or `…/c/mod.phx`; `ensure_under_root()` enforces the `EscapesRoot` safety check. **The cross-package seam is `resolve_module_path` + the import loop** (it currently knows only the single project root).
- `crates/phoenix-driver/src/main.rs` — clap `Commands` enum dispatches to `lib.rs` handlers (`run_gen` pattern). **`phoenix build` already exists** (`src/build.rs`); `init`, `add`, and `test` do not.
- Workspace `Cargo.toml` already has `serde` / `serde_json` / `toml` / `clap` / `tempfile`. **Newly added deps:** `semver` and a git client (`git2` or `gix`).

### Design decisions to lock (record in design-decisions.md when implemented)

- **Manifest schema.** `[package]` = `name`, `version` (semver), optional `description` / `authors` / `license`. `[dependencies]` accepts **git** (`dep = { git = "url", tag|rev|branch = "…" }`) and **local path** (`dep = { path = "../foo" }`) sources. Path deps are invaluable for local dev, monorepos, and testing the resolver itself. A bare-string semver value (`dep = "^1.2"`) is **reserved for the future registry** and, until then, is a clear "no registry configured" error rather than a silent failure.
- **Resolution + lockfile.** Resolve transitively, solve semver with the `semver` crate, and write `phoenix.lock` pinning each dependency to a resolved commit SHA. A present lockfile is authoritative (reproducible builds); `--locked` fails if the manifest and lock disagree.
- **Dependency cache.** Fetch into `$PHOENIX_HOME/cache` (default `~/.phoenix/cache`), keyed by URL + SHA; never inside the project tree.
- **Package root + cross-package imports.** A dependency's root is the directory containing **its** `phoenix.toml`; its modules resolve under that root with the same rules (and the same `EscapesRoot` check) as a local project. An `import`'s **first path segment** is matched against declared dependency names first; a match resolves in that package's root, otherwise it's local. A local module colliding with a dependency name is an error, not silent precedence.
- **Visibility across packages.** Only `public` declarations are importable across a package boundary (the 2.6 rule, now enforced at the package edge too).

### Scope boundaries (carved out, with forward pointers)

- **`phoenix test` is NOT in 3.1.** It belongs to the test framework ([Phase 4.9](./phase-4.md#49-test-framework)), which depends on annotations/async/HTTP/db. 3.1 ships `init` and `add`; `build`/`run`/`check` become dependency-aware. (The earlier `phoenix.toml (name, version, dependencies)` bullet listing `phoenix test` was aspirational; this supersedes it.)
- **Registry + `phoenix publish` are deferred.** 3.1 is git-first; a central registry, search, and publishing ride a later phase (see [Phase 6.2](./phase-6.md)).
- **The npm / `js-dependencies` slice is a carved-out follow-up, not a 3.1 close gate.** Phase 2.5 decision J deferred `import js "pkg"` string-source imports + `[js-dependencies]` to "Phase 3.1," but that slice (npm fetch, typings, bundling) is orthogonal to the Phoenix-package core and far larger. It rides a dedicated **3.1-js** follow-up once the core package manager works; the core close criteria below do not depend on it. The `extern js` import-section machinery (Phase 2.5) is the seam it will extend.

### PR sequence

1. **Manifest.** Extend `PhoenixConfig` with `[package]` + `[dependencies]` (keep `deny_unknown_fields`; `[gen]` keeps working). Unit tests for parse/validation; update `phoenix.toml.example`.
2. **Resolver semver core.** Pull in `semver`; model the dependency graph + constraint solving + conflict diagnostics (no fetching yet — operate over a test-injected set of manifests).
3. **Dependency fetch + lockfile.** Git sources clone into the cache (`git2`/`gix`); local `path` sources resolve in place (no fetch, not SHA-pinned); transitive resolution; write/read `phoenix.lock`; `--locked`.
4. **Cross-package imports.** Thread a `dependency_roots: HashMap<String, PathBuf>` through `resolve_module_path`; first-segment dispatch; per-package `EscapesRoot`; collision + missing-dependency diagnostics. Make `build`/`run`/`check` resolve+fetch before compiling.
5. **CLI.** `phoenix init [--name]` scaffolds `phoenix.toml` + an entry `.phx`; `phoenix add <name> (--git <url> [--tag|--rev|--branch] | --path <dir>)` edits the manifest and refreshes the lockfile. Tempdir + local-git-repo integration tests.
6. **Close.** Exit criteria below; design-decisions.md writeup; known-issues entries for the carve-outs.

### Exit criteria for declaring Phase 3.1 complete

- [ ] `[package]` + `[dependencies]` parse from `phoenix.toml`; `[gen]` still parses; malformed manifests give clear diagnostics. Unit tests cover the schema.
- [ ] Semver resolution solves a transitive graph and reports conflicts legibly; covered by tests over injected manifests.
- [ ] Git dependencies fetch into the cache and local `path` dependencies resolve in place; `phoenix.lock` is generated, respected, and makes git-backed builds reproducible from a clean checkout; `--locked` detects drift.
- [ ] `import dep.module { ... }` resolves to the fetched package (public-only), with per-package `EscapesRoot` preserved and collision/missing-dep diagnostics. Multi-package integration fixture builds and runs.
- [ ] `phoenix build` / `run` / `check` resolve + fetch dependencies first; `phoenix init` and `phoenix add` work, with tempdir + local-git-repo integration tests.
- [ ] Workspace `cargo test` / `clippy --all-targets` / `fmt --check` clean; CI green.
- [ ] `phoenix.toml.example` updated; design-decisions.md records the locked decisions; known-issues opened for the registry and npm/js carve-outs.

### 3.1 Parallel-track note

3.1 lives entirely in **`phoenix-driver`** (config, CLI, a new resolver/lockfile module or a `phoenix-package` crate) and **`phoenix-modules`** (the resolver seam), plus new workspace deps. It does **not** touch the lexer, parser, sema, IR, runtime, or codegen — so it is disjoint from [Phase 4.6](./phase-4.md#46-json-and-serialization) and any other stdlib work. The only files both tracks might touch are the workspace `Cargo.toml` `[workspace.dependencies]` table (each appends distinct entries), `tests/fixtures/` (additive new files), and the docs (different sections). Rebase those few touch-points frequently; everything else is in separate crates.

## 3.2 Language Server Protocol (LSP)

A multi-module foundation landed in Phase 2.6 — diagnostics, hover, completion, goto-def, find-references, and rename all work cross-file for functions / structs / enums / methods / fields / enum variants, with the rich diagnostic shape (notes routed to the right file URI). 3.2 closes the remaining symbol-coverage gaps and adds the standard LSP features the editor experience expects.

### Core requests

- Go-to-definition, hover for type info, find references
- Rename (cross-file `WorkspaceEdit` — already implemented; pin coverage with the symbol-kind expansion below)
- Real-time error diagnostics (run the type checker on every keystroke)
- Auto-completion for fields, methods, and function parameters
- VS Code extension as the first-class IDE integration

### Symbol-kind coverage

The current `SymbolKind` taxonomy (Function / Struct / Enum / Field / Method / EnumVariant / Variable) leaves several Phoenix surfaces invisible to the LSP. Goto-def, references, and rename should work for all of them:

- **Local variables** — today `SymbolKind::Variable` returns `None` from `find_definition_span` because variable definitions aren't recorded in `ResolvedModule`. Lift `VarInfo` into the resolved schema (or a sidecar map) so let-bindings, parameters, and pattern bindings round-trip.
- **Imports** — sema doesn't emit `symbol_references` for the names inside `import lib { foo }` or for the module path `lib`. Goto-def at the import site should jump to the source declaration; goto-def on the module path should jump to the source file.
- **Traits as standalone symbols** — add a `SymbolKind::Trait` so goto-def on a trait name in `dyn Trait`, `impl Trait for Type`, and `<T: Trait>` bound positions resolves.
- **Type aliases** — `Analysis::type_aliases` is populated but the LSP doesn't surface aliases in completion or expose goto-def on the alias name.
- **Builtin types (generic and scalar)** — neither the generic builtins (`List`, `Map`, `Option`, `Result`, `ListBuilder`, `MapBuilder` — the Phase 2.7 decision F additions) nor the scalar builtins beyond the originals (`File`, `DateTime`, `Uuid`, `Decimal`, `Money`, `Url`, `Bytes`, `JsValue` — added across the Gen type-system work and Phase 2.5) are surfaced in completion or hover today. The LSP's completion sources are user-defined symbols + lexer keywords; builtins are resolved by name in `phoenix-sema` (`Type::from_name` for scalars, `check_types::resolve_type_expr` for generics) and have no entries in `module_scopes` / `struct_by_name` / `enum_by_name`. Add them so type annotations like `let xs: List<…>` or `let d: DateTime` autocomplete, so `b.` on a `ListBuilder<T>` receiver suggests `push` / `freeze`, and so hovering on a builtin type-name token (e.g. `List` in `List.builder()`, or `Money` in a field annotation) shows the type's kind.

### Standard LSP features beyond the core

Not yet implemented in any form. All are independent of the symbol-coverage work above and can land in any order.

- **Signature help** — parameter info popup as the user types a call (LSP `textDocument/signatureHelp`)
- **Document symbols** — outline view per file (LSP `textDocument/documentSymbol`)
- **Workspace symbols** — fuzzy symbol search across the project (LSP `workspace/symbol`)
- **Code actions / quick fixes** — surface diagnostic suggestions as one-click edits (LSP `textDocument/codeAction`); maps onto the diagnostic builder's `suggestion` field
- **Semantic tokens** — richer syntax highlighting driven by the type checker (LSP `textDocument/semanticTokens`); colors module-qualified names, trait bounds, and `dyn` differently from local idents
- **Format-on-save** — wires `phoenix fmt` (3.3) into the LSP via `textDocument/formatting`

### Why critical

Developers evaluate a language by opening a file in their editor. If there's no autocomplete or inline errors, the language feels broken.

### Prerequisite

The [diagnostic builder](../design-decisions.md#diagnostic-builder-pattern) (landed in Phase 2.6) is in place. LSP clients render rich diagnostics — secondary spans, notes, quick-fix suggestions — and those already map onto the builder's fields. The 2.6 multi-module rewrite also wired the LSP to the resolver + `check_modules` pipeline with a shared `SourceMap` and `SourceId → Url` plumbing, so 3.2 doesn't need to retrofit cross-file infrastructure.

## 3.3 Formatter

- `phoenix fmt` — opinionated code formatter
- One canonical style (no configuration bikeshedding)
- Format-on-save in the LSP
- **Grammar dependency:** the formatter prints every AST node, so it must keep pace with new language surface. In particular, when [Annotations (4.5)](./phase-4.md#45-annotation-system) land on the stdlib track, the formatter must format `@name` / `@name(args)` on declarations and fields (one canonical placement — typically each annotation on its own line above the declaration). If the formatter ships before 4.5, annotation formatting is a small follow-up; if 4.5 ships first, the formatter handles it from day one.

## 3.4 Test Framework — moved to 4.9

*Moved to [Phase 4.9](./phase-4.md#49-test-framework). The test framework depends on annotations (4.5), async runtime (4.3), HTTP (4.4), and database (4.7), so it sequences after those land. The numbering slot is preserved here to avoid breaking cross-references.*

## 3.5 Error Messages

- Invest heavily in error message quality
- Every error should say what went wrong, where, and suggest a fix
- Use source-annotated diagnostics (like Rust's or Elm's error messages)
- This is not a feature — it is a continuous effort that should improve with every release
- **Foundation:** the [diagnostic builder](../design-decisions.md#diagnostic-builder-pattern) (Phase 2.6) is the construction API this phase builds on; notes, secondary spans, and suggestions are already wired through by the time 3.5 begins. This phase is about *populating* those fields with high-quality messages, not about infrastructure.
