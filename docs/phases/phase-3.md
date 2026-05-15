# Phase 3: Tooling

**Status: Not started**

Developers will not adopt a language without good tooling. Tooling and the stdlib are interleaved, not strictly sequenced. The keystone is [Annotations (4.5)](./phase-4.md#45-annotation-system) — small, with no dependencies — which unblocks JSON serialization, config loading, database hints, and the test framework. Most of phase 3 (package manager, LSP gap-closing, formatter) is independent of phase 4 and can land in parallel with stdlib work.

## Recommended order

- **First:** [4.5 Annotations](./phase-4.md#45-annotation-system) — keystone for the rest of the stdlib.
- **In parallel after 4.5:** 3.1 Package Manager, 3.2 LSP gap-closing, 3.3 Formatter, plus 4.1 / 4.3 / 4.6 on the stdlib side.
- **Continuous:** 3.5 Error Message quality — the diagnostic-builder foundation already landed in 2.6, so this is an ongoing investment that improves with every release rather than a discrete milestone.

## 3.1 Package Manager

- `phoenix.toml` manifest file (name, version, dependencies)
- Dependency resolution (semver)
- Start with git-based dependencies, add a registry later
- `phoenix init`, `phoenix add`, `phoenix build`, `phoenix test`
- **Depends on:** Module system and visibility (2.6) — cross-package imports require intra-project modules

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
- **Builtin generic types** — `List`, `Map`, `Option`, `Result`, `ListBuilder`, `MapBuilder` (the Phase 2.7 decision F additions) are not surfaced in completion or hover today. The LSP's completion sources are user-defined symbols + lexer keywords; builtins are hardcoded in `phoenix-sema::check_types::resolve_type_expr` and have no entries in `module_scopes` / `struct_by_name` / `enum_by_name`. Add them so type annotations like `let xs: List<…>` autocomplete, so `b.` on a `ListBuilder<T>` receiver suggests `push` / `freeze`, and so hovering on a builtin type-name token in static-method-call position (e.g. `List` in `List.builder()`) shows the type's kind.

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

## 3.4 Test Framework — moved to 4.9

*Moved to [Phase 4.9](./phase-4.md#49-test-framework). The test framework depends on annotations (4.5), async runtime (4.3), HTTP (4.4), and database (4.7), so it sequences after those land. The numbering slot is preserved here to avoid breaking cross-references.*

## 3.5 Error Messages

- Invest heavily in error message quality
- Every error should say what went wrong, where, and suggest a fix
- Use source-annotated diagnostics (like Rust's or Elm's error messages)
- This is not a feature — it is a continuous effort that should improve with every release
- **Foundation:** the [diagnostic builder](../design-decisions.md#diagnostic-builder-pattern) (Phase 2.6) is the construction API this phase builds on; notes, secondary spans, and suggestions are already wired through by the time 3.5 begins. This phase is about *populating* those fields with high-quality messages, not about infrastructure.
