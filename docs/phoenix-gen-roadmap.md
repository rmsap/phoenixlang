# Phoenix Gen: Production-Ready Plan

This document defines what shipping Phoenix Gen as a v1.0 product looks like — what "production-ready" means concretely, what gaps need to close, and what the critical path is.

The decision frame: **Phoenix Gen ships from the phoenixlang repo but is branded and distributed as its own product.** The full Phoenix language and Phoenix Gen share the lexer, parser, and sema pipeline, so a split repo would either duplicate that pipeline or build a fragile cross-repo dependency. Co-residence is structural, not just convenient.

A user adopting Phoenix Gen should never need to install the full Phoenix toolchain or learn the language to ship code with it. Conversely, anyone who installs Phoenix the language gets `phoenix gen` for free. The `.phx` files Gen consumes are still Phoenix syntax, so a power user *can* dig into the language; the goal is that they never *have* to.

For the current state of Phoenix Gen as a feature, see [phoenix-gen.md](phoenix-gen.md). For the full language roadmap, see [roadmap.md](roadmap.md).

---

## 1. Goal — what "v1.0" means

Phoenix Gen v1.0 is the version a developer can put on their team's roadmap. It commits to:

- **A frozen schema language** for the v1.x line. Schema files written against v1.0 keep parsing and generating equivalent code through every v1.x release.
- **Stable generated output.** Regenerating against the same schema produces byte-identical output across patch versions; minor versions may add but not break output shape.
- **Idiomatic generated code per target.** Output passes the target language's standard linters and formatters with no manual cleanup. The bar is "code a senior engineer in that language would accept in a PR."
- **An installable artifact for non-Rust users.** A TypeScript dev should `npm i -g ...` (or equivalent) and not need cargo.
- **Documented, testable contract for every target.** Each target has a published support matrix: which schema features generate, which are partially supported, which are unsupported with a clear error.

Out of scope for v1.0 — explicitly deferred:
- Auth modeling in the schema (continues to be framework-middleware-shaped, see §4)
- Streaming responses, WebSockets, SSE
- Multi-version API support (`/v1`, `/v2` coexisting)
- Custom target plugins (third-party codegen backends)

---

## 2. Distribution & branding

### Binaries

- **`phoenix-gen`** — standalone binary. Same Cargo workspace, separate `[[bin]]` target in `phoenix-driver` (or a thin new crate `phoenix-gen-cli` that depends on `phoenix-codegen`). Behaves as if `phoenix gen` were the only subcommand: `phoenix-gen schema.phx --target typescript`.
- **`phoenix gen`** — unchanged subcommand of the main `phoenix` binary. Same code path internally.

The two binaries must accept identical CLI surface. A test in CI should diff `phoenix gen --help` against `phoenix-gen --help` (modulo the program name) so they cannot drift.

### Install paths

Each path produces only the `phoenix-gen` binary. The Phoenix language install (`install.sh`) ships `phoenix` which includes `gen` as a subcommand.

| Channel | Package | Audience |
|---|---|---|
| npm | `<npm-scope>/cli` (postinstall downloads platform binary) | TypeScript / JS devs |
| pip | `phoenix-gen` | Python devs |
| Homebrew tap | `<brew-tap>` | macOS / Linux devs |
| Scoop / winget | `phoenix-gen` | Windows devs |
| `go install` | `<github-org>/phoenixlang/cmd/phoenix-gen` (shim) — `<github-org>` is a placeholder (TBD — see [§9 open decisions](#9-open-decisions)), not a commitment to any specific org | Go devs — but binaries still come from the Rust workspace |
| GitHub Releases | prebuilt binaries per platform | curl-based installs, CI |
| `cargo install phoenix-gen` | from crates.io | Rust devs |

The npm/pip postinstall pattern (download a prebuilt binary at install time) avoids forcing non-Rust users to install a Rust toolchain. Esbuild, swc, and Biome all ship this way successfully.

### Docs site

- **Domain:** dedicated, separate from the language.
- **Front door content:** lead with the codegen pitch. The full language is mentioned only in a footer "Background" link. A user who wants tRPC-without-the-TS-lockin should never bounce because the landing page reads as "future full-stack language."
- **Sections:** Quick start, Schema reference, Per-target guides (TS / Python / Go / OpenAPI), Comparison with tRPC / OpenAPI Generator / TypeSpec / Smithy, Migration guides, Recipes (auth, file uploads, pagination), Stability policy, Changelog.

### Repo positioning

- Top-level `README.md` gets a "This repo ships two products" section near the top, with Phoenix Gen co-equal to the language.
- Issue templates: separate templates for `gen:` issues vs. language compiler issues.
- Labels: `area:gen`, `area:lang` for triage.
- Release notes: separate `CHANGELOG-gen.md` from the language changelog. Independent versioning.

---

## 3. Versioning & stability

- **Semver.** Phoenix Gen versions independently of the Phoenix language. v1.0.0 is the first stable release; the language can be at any pre-1.0 version when Gen ships 1.0.
- **Schema language stability:** any v1.x schema file parses and generates equivalent output on any v1.x release. Adding a new schema construct is a minor version bump. Removing or changing existing semantics requires a major version bump.
- **Output stability:** regenerating against the same schema produces byte-identical output within a patch version. Within a minor version, output may gain new content (e.g., a new helper function) but not change the shape of existing content.
- **Deprecation policy:** schema syntax can be deprecated in a minor release with a compiler warning. Removal requires a major version bump and at least one minor release of warning lead time.
- **Generated code stability is target-versioned too.** A user can pin "TypeScript output v1.2 shape" in `phoenix.toml` if they want to upgrade the binary without regenerating.

The current doc already commits to "regenerating without schema changes produces byte-identical output" — that needs to become a CI invariant: a snapshot test in `phoenix-codegen/tests` that fails if any generator's output for the fixture suite changes unexpectedly.

---

## 4. Schema language: v1.0 scope

The current schema language handles structs, enums (simple and ADT), endpoints with path/query/body/response/error, type derivation (`omit` / `pick` / `partial`), `where` constraints, and doc comments. To be production-credible across the SaaS / API space, v1.0 must also handle:

### Must-add for v1.0

- **Headers.** Both request (`headers { authorization: String }`) and response. Auth tokens, idempotency keys, content negotiation, custom request IDs all live here.
- **File uploads / multipart.** `body multipart { avatar: File, caption: String }`. Generated code wires up the framework's multipart handling (multer, fastapi UploadFile, Go `multipart.Reader`).
- **Pagination patterns.** First-class support for cursor and offset pagination, since this is the single most common API shape and every team reinvents it. Probably a `paginated` modifier on response types.
- **Multiple content types per response.** `response { 200: User, 200 text: String }` — content negotiation by Accept header. *Update 2026-06: the multi-status half shipped (`response { 200: User, 201: User, 204 }`, JSON-only); the content-negotiation half is deferred indefinitely — see `design-decisions.md` (multi-status responses design).*
- **API versioning prefix.** `api version "v1" { ... endpoints ... }` so `/v1` doesn't have to be repeated on every path.

### Decisions punted to post-v1.0 (with documented rationale)

- **Auth as a first-class schema concept.** Stay with the current "wire it through middleware" position. Reasoning: every framework's auth model is different, and modeling it generically tends to produce a lowest-common-denominator that's worse than what each ecosystem's middleware already does. Document this position prominently in the docs site.
- **WebSockets / SSE / streaming responses.** Real demand exists but the abstractions are not stable across target frameworks. Defer to v2.0.
- **Custom validators beyond `where`.** A `validate` hook for arbitrary user logic. Defer until real users hit a `where` limit.

### Schema language gaps to close (already on the list, finish for v1.0)

- Multi-bound generic parameters (`<T: Foo + Bar>`) — currently parser-rejected per known-issues. **Reclassified 2026-06-07: this is Phase-3 *language* work, NOT a Gen-facing schema feature — deferred from the Gen track.** Trait bounds are a runtime/monomorphization construct (they let generic *code* call trait methods on `T`); a Gen `.phx` schema describes data shapes and HTTP contracts and never declares bounded generic parameters — the generics Gen consumes (`List<Post>`, `Option<String>`, `Map<K,V>`) are unbounded, so the number-of-bounds question never arises in Gen's domain. Implementing it means threading bounds through parser → sema → monomorphization → IR → codegen (the execution pipeline Gen deliberately never touches), and it pairs naturally with the bidirectional-inference / trait-bound items already queued in `known-issues.md`. Owned by the core-language Phase-3 effort; no Gen user exercises it. The parser already rejects it with a clear diagnostic, so there is no miscompile risk in the interim.
- Better error messages for malformed `where` clauses
- Uniform handling of `Option<T>` everywhere it appears

**Gen-facing schema scope — status.** The five Gen-shaped "must-add for v1.0" schema
features are all implemented and proven by both harnesses (compile-and-lint +
round-trip): **headers**, **multipart/file upload + binary download**, **API
versioning prefix**, **pagination** (offset + cursor), and **multi-status
responses** (shared body + typeless; content negotiation deferred — see
design-decisions.md). The remaining §4 items are either the reclassified language
work above or smaller polish (`where`-clause error messages, uniform `Option<T>`).
The next Gen priorities are therefore non-schema: Block C (code-gen quality — e.g.
a second TS server framework), the §6 fixture library, and Block F distribution.

---

## 5. Code generation quality bar

For each target, "production-ready" output means:

"Lint pass" means the listed linter exits clean against the generated code; "Format pass" means the listed formatter run in check mode reports zero diffs.

| Target | Lint pass | Format pass | Framework choice | Runtime deps |
|---|---|---|---|---|
| TypeScript | `eslint` + `@typescript-eslint` strict, no warnings | `prettier --check` clean | client: fetch (no deps); server: Express **and** Fastify | none for client; one peer dep for server |
| Python | `ruff check` strict, no findings | `black --check` clean | client: httpx; server: FastAPI | pydantic v2 |
| Go | `golangci-lint run` default config, no findings | `gofmt -l` empty | server: `net/http` + `chi` router | none |
| OpenAPI | `redocly lint` clean | — | — | — |

Each target gets a `compiles_and_lints.rs` integration test in `phoenix-codegen/tests` that:
1. Generates output against the fixture suite
2. Invokes the target's compiler (tsc / mypy / `go build`) on the output
3. Invokes the target's linter
4. Asserts both pass

This catches regressions from "the generator produces strings that look right" — a real failure mode for any codegen tool.

### Internal code health: generator test layout

Each generator's `#[cfg(test)]` module lives in a sibling file
(`go_tests.rs`, `python_tests.rs`, `typescript_tests.rs`), declared from the
generator via `#[path]` so the module path — and therefore every insta
snapshot name — matches the original inline layout. New generator tests go in
the sibling file, not back inline; `openapi.rs` keeps its inline module (it is
a fraction of the size and emits a single file).

### Server framework strategy

The current state generates one server framework per language. v1.0 needs at least two for TypeScript (Express is legacy but ubiquitous; Fastify is modern best-practice). A `framework = "express" | "fastify"` flag in `phoenix.toml`. Same eventually for Python (FastAPI, then Starlette / Litestar) and Go (net/http+chi, then echo). Lock the v1.0 list now — every additional framework is a maintenance cost.

### Runtime helpers

Generated code needs runtime helpers (validators, error mapping, request building). Two options:

- **Vendored:** helpers are emitted alongside generated types. Pro: zero runtime dependency. Con: helpers can drift between projects, harder to fix bugs centrally.
- **Imported from a runtime package:** `import { validate } from '<npm-scope>/runtime'`. Pro: bug fixes ship via package update. Con: introduces a dependency.

**Recommendation: vendored, with the helpers behind a `// generated by phoenix-gen v1.x.x` header so users know not to edit.** Matches sqlx, prost, and other codegen tools where users own the output. The runtime-package model couples user upgrade timing to ours, which is worse for adoption.

---

## 6. Real-world validation

The current fixture suite (`tests/fixtures/gen_api.phx`) is one schema. v1.0 needs a fixture library that exercises the realistic shapes API teams actually write:

| Fixture | Shapes exercised |
|---|---|
| `blog.phx` (existing) | basic CRUD, pagination, doc comments |
| `payments.phx` (new) | idempotency keys, webhooks, refund flows, money type, decimal precision |
| `multitenant_saas.phx` (new) | tenant scoping, role-based responses, audit fields, soft-delete |
| `webhooks.phx` (new) | inbound webhook receivers, signature headers, retry semantics |
| `file_storage.phx` (new) | multipart upload, range requests, content-disposition |
| `social.phx` (new) | nested resources, deep `omit` / `pick`, fan-out queries |
| `internal_admin.phx` (new) | wide types with many fields and constraints, enum-heavy state machines |

Every fixture runs through every target in CI and the generated code compiles and lints cleanly. Adding a fixture often surfaces a missing schema feature — that's the point.

---

## 7. Documentation deliverables

### Site IA

- **Landing page** — pitch, animated diagram, "Quick start" CTA. Compares Phoenix Gen to the closest alternative (tRPC for TS folks, OpenAPI Generator for the rest).
- **Quick start** — write a schema, generate, run, in under 5 minutes.
- **Schema reference** — every construct with examples and per-target output.
- **Target guides** (one per language) — how to wire generated code into a real project, framework-specific recipes.
- **Migration guides** — "I'm using tRPC", "I'm using OpenAPI Generator", "I'm using TypeSpec". Each shows side-by-side schema, the migration steps, and the gotchas.
- **Recipes** — auth, file uploads, pagination, error handling patterns, multi-environment config.
- **Stability policy** — versioning rules, what's promised, what's not.
- **Comparison page** — feature matrix vs. tRPC, OpenAPI Generator, TypeSpec, Smithy, Hono RPC, Encore.
- **Changelog** — separate from language changelog.

### Migration guides matter most

Most users adopting Phoenix Gen will have an existing tRPC or OpenAPI setup. Migration content is the highest-leverage doc work — it converts "interested" to "trying." Each guide is a real codebase shape, not a toy example.

---

## 8. Release & operational

### CI matrix (per release)

- Build `phoenix-gen` and `phoenix` binaries for: linux-x64, linux-arm64, macos-x64, macos-arm64, windows-x64
- For each target language: generate fixtures → compile → lint → run a smoke test against the generated client/server. The compile + lint half of this chain is the same `compiles_and_lints.rs` integration test described in [§5](#5-code-generation-quality-bar) — that test runs on every `cargo test`, and the release CI matrix layers the cross-platform build + smoke-test on top.
- Snapshot tests for byte-identical output stability
- `--help` parity check between `phoenix-gen` and `phoenix gen`

### Release process

- Phoenix Gen has its own git tag prefix: `gen-v1.2.3`. Language tags stay `v0.x.x`.
- Tagging triggers: build all platform binaries → publish to GitHub Releases → publish to npm / pip / Homebrew tap → publish to crates.io → invalidate docs site CDN.
- Release notes auto-generated from `area:gen`-labeled PRs since the last `gen-v*` tag.

---

## 9. Open decisions

These need answers before the plan executes. Each blocks at least one downstream task.

| Decision | Blocks | Recommendation |
|---|---|---|
| GitHub org / canonical repo path (current personal namespace vs. dedicated org — name redacted, see private scratch `<github-org>`) | `go install` shim path, `cargo install` source URL, all install docs | Move to a dedicated org before any v1.0 install path is published — renaming after release is permanently disruptive |
| Domain name (candidates redacted — see private scratch `<docs-domain>`) | Docs site | Short, no-hyphen `.dev` preferred — register before any public docs link |
| npm package name (candidates redacted — see private scratch `<npm-scope>`) | npm publishing | Scoped name (`<npm-scope>/cli`) — scoped names avoid squatting risk; claim the scope before publishing |
| Standalone binary: separate crate (`phoenix-gen-cli`) or extra `[[bin]]` in `phoenix-driver` | Binary build | Extra `[[bin]]` first — minimal change. Split later if `phoenix-driver` becomes too entangled |
| Server framework list per language (full v1.0 lock-in) | Code generation work | TS: Express + Fastify; Python: FastAPI; Go: net/http+chi. Lock down before beta |
| Runtime helpers: vendored vs. imported package | Generator architecture | Vendored — see §5 |
| Pagination shape: cursor-only, offset-only, or both | Schema language work | Both, with an explicit annotation on the response type |
| Telemetry collected: opt-in confirmed; what fields exactly | Telemetry implementation | Schema construct counts, target language, error types only — no schema content, no file paths |
| Beta user recruitment plan | Beta program | Identify 5 candidates from existing network before v1.0-rc1 |

---

## 10. Critical path to v1.0

Ordered by dependency. Items in the same block are independent and can parallelize.

**Block A — branding & distribution (no code changes to codegen)**
1. Pick domain, register, set up DNS
2. Stand up docs site scaffold (Astro / Starlight or similar)
3. Add `phoenix-gen` `[[bin]]` to `phoenix-driver`
4. Resolve open decisions (§9)
5. Reorganize top-level repo README to give Phoenix Gen co-equal billing

**Block B — schema language gaps**
6. Headers
7. Multipart / file upload
8. Pagination annotation
9. API versioning prefix
10. Multi-content-type responses
11. Multi-bound generic parameters fix

**Block C — code generation quality**
12. `compiles_and_lints` integration tests for every target
13. Second TS server framework (Fastify)
14. Format-pass / lint-pass cleanup for current generators
15. Vendored runtime helpers refactor

**Block D — fixture library**
16. Write the six new fixtures (§6)
17. Wire each into the cross-target CI matrix
18. Fix every gap each fixture surfaces

**Block E — docs**
19. Schema reference
20. Per-target guides (4)
21. Migration guides (3 — tRPC, OpenAPI Generator, TypeSpec)
22. Comparison page
23. Stability policy doc

**Block F — distribution channels**
24. npm package with binary postinstall
25. pip package with binary postinstall
26. Homebrew tap
27. Scoop / winget manifests
28. crates.io publish

**Block G — beta**
29. Recruit 5 beta users
30. 4-week beta with weekly feedback cycles
31. Address every blocker found
32. v1.0.0 release

Block A is short and unblocks everything. Blocks B / C / D run in parallel and converge at "internal feature complete." Block E can start in parallel with B/C/D as features stabilize. Block F is small per-channel but wide. Block G must be after the others.

The honest estimate is that Blocks B–E together are the main work — each item is days to a couple of weeks. Beta (G) is calendar time, not effort time. Do not skip the beta to ship faster — the whole point is finding what you don't know.

---

## 11. Non-goals

Worth stating explicitly so they don't accrete into the plan:

- **A web UI for editing schemas.** The .phx file is the source of truth; editing happens in the user's editor with the existing VS Code extension.
- **A schema linter beyond compiler errors.** Out of scope for v1.0.
- **A runtime / hosted service.** Phoenix Gen is a build-time tool. Encore is the hosted-service model; we are not.
- **Plugin support for third-party targets.** Post-v1.0. v1.0 ships exactly four targets and no extension API.
- **Folding Phoenix Gen into the language's typed-endpoint surface.** When the full language ships, `.phx` schema files become importable modules — but Phoenix Gen does not disappear into the language. It continues to exist for its multi-language codegen use case; the two are complementary, not staged.
