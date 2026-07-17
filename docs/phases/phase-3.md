# Phase 3: Tooling

**Status: In progress — 3.1 (package manager) complete (2026-07-01); 3.2 / 3.3 / 3.5 not started.**

Developers will not adopt a language without good tooling. Phase 3 (tooling) and [Phase 4](./phase-4.md) (the standard library) are **independent tracks that run in parallel** — nothing in Phase 3 depends on Phase 4. Every Phase 3 item rests only on foundations that already shipped in Phase 2: the package manager (3.1) on the module system (2.6), the LSP gap-closing (3.2) on the 2.6 LSP pipeline, the formatter (3.3) on the parser, and error-message quality (3.5) on the 2.6 diagnostic builder.

Annotations ([4.5](./phase-4.md#45-annotation-system)) are the keystone for the **stdlib** track, not for tooling — they unblock JSON serialization, config loading, database hints, and the test framework, so 4.5 is the first item on the Phase 4 track. The only cross-track touch-point is the formatter (3.3), which will need to format `@annotation` syntax once 4.5 lands (see 3.3).

## Recommended order

- **In parallel from the start:** 3.1 Package Manager, 3.2 LSP gap-closing, 3.3 Formatter — all independent of each other and of Phase 4. The shared constraint is intra-Phase-3, not cross-phase: the formatter and LSP both consume whatever grammar the parser produces, so any new language surface (e.g. 4.5 annotations) lands before, or is followed up in, those tools.
- **After 3.1:** [3.1.2 npm / JavaScript dependencies](#312-npm--javascript-package-dependencies) — the carved-out follow-up. Unlike the items above it is **not** independent of Phase 4: it touches compiler crates (sema, the WASM glue, the interpreters), so the [Phase 4.6 parallel-track hygiene](./phase-4.md#46-parallel-track-note) applies.
- **Continuous:** 3.5 Error Message quality — the diagnostic-builder foundation already landed in 2.6, so this is an ongoing investment that improves with every release rather than a discrete milestone.
- **On the stdlib track (Phase 4):** 4.5 Annotations goes first; see [Phase 4](./phase-4.md#recommended-order).

## 3.1 Package Manager

**Status: ✅ complete (2026-07-01).** A `phoenix.toml`-driven, git-first package manager: manifest schema, transitive semver resolution, git-into-cache fetching with a reproducible `phoenix.lock`, cross-package imports, dependency-aware `build`/`run`/`check`, and `phoenix init`/`add`. See the [closeout](#-phase-31-closed-2026-07-01) below. **Depends on:** Module system and visibility (2.6, complete) — cross-package imports build on intra-project modules. Independent of all Phase 4 work (see [Parallel-track note](#31-parallel-track-note) below).

### Goal

A `phoenix.toml`-driven package manager: declare a package and its dependencies, resolve them (semver), fetch git-based dependencies into a cache, pin them in a lockfile, and let `import` reach across package boundaries. Make `phoenix build` / `run` / `check` dependency-aware so a multi-package project builds from a clean checkout.

### Current state (what exists today)

- `crates/phoenix-driver/src/config.rs` — `PhoenixConfig` parses `phoenix.toml` with **only** a `[gen]` section (`#[serde(deny_unknown_fields)]`); `find_and_load(start_dir)` walks up the tree for the manifest. TOML plumbing (`toml` 0.8), `serde`, and a tempdir CLI test pattern (`crates/phoenix-driver/tests/`) already exist.
- `crates/phoenix-modules/src/lib.rs` — `resolve()` / `resolve_with_overlay()` compute the project root as the entry file's parent dir; `resolve_module_path(root, root_canon, target, span)` maps `a.b.c` → `<root>/a/b/c.phx` or `…/c/mod.phx`; `ensure_under_root()` enforces the `EscapesRoot` safety check. **The cross-package seam is `resolve_module_path` + the import loop** (it currently knows only the single project root).
- `crates/phoenix-driver/src/main.rs` — clap `Commands` enum dispatches to `lib.rs` handlers (`run_gen` pattern). **`phoenix build` already exists** (`src/build.rs`); `init`, `add`, and `test` do not.
- Workspace `Cargo.toml` already has `serde` / `serde_json` / `toml` / `clap` / `tempfile`. **Newly added deps:** `semver` and `gix` (the pure-Rust git client; see the git-client decision below).

### Design decisions to lock (recorded in [design-decisions.md §Phase 3.1](../design-decisions.md#phase-31-package-manager))

- **Manifest schema.** `[package]` = `name`, `version` (semver), optional `description` / `authors` / `license`. `[dependencies]` accepts **git** (`dep = { git = "url", tag|rev|branch = "…" }`) and **local path** (`dep = { path = "../foo" }`) sources. Path deps are invaluable for local dev, monorepos, and testing the resolver itself. A bare-string semver value (`dep = "^1.2"`) is **reserved for the future registry** and, until then, is a clear "no registry configured" error rather than a silent failure.
- **Resolution + lockfile.** Resolve transitively, solve semver with the `semver` crate, and write `phoenix.lock` pinning each dependency to a resolved commit SHA. A present lockfile is authoritative (reproducible builds); `--locked` fails if the manifest and lock disagree.
- **Dependency cache.** Fetch into `$PHOENIX_HOME/cache` (default `~/.phoenix/cache`), keyed by URL + SHA; never inside the project tree.
- **Git client = `gix` (pure-Rust git), not `git2`/libgit2.** libgit2 is GPL-2.0-with-linking-exception and, with a TLS backend, drags in **system** OpenSSL — leaving a runtime `libssl`/`libcrypto` shared-library dependency in distributed binaries and complicating the release build. `gix` plus a `rustls` HTTPS transport is permissively licensed (MIT/Apache-2.0/ISC — no copyleft), needs no libgit2, and replaces system OpenSSL so the shipped binary has no TLS shared-library runtime dependency and stays self-contained. **Honest caveat:** this is not "pure Rust all the way down" — rustls's default crypto provider through the reqwest transport is `aws-lc-rs`, whose `aws-lc-sys` is C (AWS-LC), vendored and statically linked at build time. So there is no *runtime* C/OpenSSL dependency, but a C compiler is required at *build* time, including for the `aarch64-unknown-linux-gnu` cross target (the release matrix installs a cross C toolchain for this). reqwest 0.13's rustls transport offers no `ring` provider, so the fully C-free path isn't reachable through this stack today; the licensing and no-system-OpenSSL wins are what justify the choice. The production fetcher uses `gix`; the package-manager integration tests build throwaway local repos with the `git` CLI (always present in dev/CI), which keeps fixture-building simple without adding a runtime `git` dependency to Phoenix itself.
- **Package root + cross-package imports.** A dependency's root is the directory containing **its** `phoenix.toml`; its modules resolve under that root with the same rules (and the same `EscapesRoot` check) as a local project. An `import`'s **first path segment** is matched against declared dependency names first; a match resolves in that package's root, otherwise it's local. A local module colliding with a dependency name is an error, not silent precedence.
- **Visibility across packages.** Only `public` declarations are importable across a package boundary (the 2.6 rule, now enforced at the package edge too).
- **Registry-readiness seams.** 3.1 is git-first (registry deferred — see scope boundaries), but the resolver/fetcher/lockfile must stay additively extensible to a registry source. Two concrete seams are fixed **as part of 3.1** so the eventual registry doesn't force a lockfile-format migration or a provider-trait break: (1) carry an **explicit source-kind** on `ResolvedPackage` / `LockedPackage` rather than inferring git-vs-other from "has a git rev" (a registry package has no rev but must still be locked), making `LockedPackage` an enum (or source-tagged) so a `name`/`version`/`checksum` entry is representable; (2) shape `ManifestProvider` now so a future **version-requirement solver** can be added without breaking the trait (e.g. an `available_versions(name)` capability alongside `fetch`; git/path return a one-element set because the ref *is* the version choice). The backtracking solver itself is out of scope — only the seam is. Full rationale and the "add a provider *and* a solver, not just a provider" framing live in [design-decisions.md §Phase 3.1](../design-decisions.md#phase-31-package-manager).

### Scope boundaries (carved out, with forward pointers)

- **`phoenix test` is NOT in 3.1.** It belongs to the test framework ([Phase 4.9](./phase-4.md#49-test-framework)), which depends on annotations/async/HTTP/db. 3.1 ships `init` and `add`; `build`/`run`/`check` become dependency-aware. (The earlier `phoenix.toml (name, version, dependencies)` bullet listing `phoenix test` was aspirational; this supersedes it.)
- **Registry + `phoenix publish` are deferred.** 3.1 is git-first; a central registry, search, and publishing ride a later phase (see [Phase 6.2](./phase-6.md)).
- **The npm / `js-dependencies` slice is a carved-out follow-up, not a 3.1 close gate.** Phase 2.5 decision J deferred `import js "pkg"` string-source imports + `[js-dependencies]` to "Phase 3.1," but that slice (npm fetch, typings, bundling) is orthogonal to the Phoenix-package core and far larger. It rides the dedicated [§3.1.2](#312-npm--javascript-package-dependencies) follow-up once the core package manager works; the core close criteria below do not depend on it. The `extern js` import-section machinery (Phase 2.5) is the seam it will extend.

### PR sequence

1. **Manifest.** Extend `PhoenixConfig` with `[package]` + `[dependencies]` (keep `deny_unknown_fields`; `[gen]` keeps working). Unit tests for parse/validation; update `phoenix.toml.example`.
2. **Resolver semver core.** Pull in `semver`; model the dependency graph + constraint solving + conflict diagnostics (no fetching yet — operate over a test-injected set of manifests).
3. **Dependency fetch + lockfile.** Git sources clone into the cache (via `gix`); local `path` sources resolve in place (no fetch, not SHA-pinned); transitive resolution; write/read `phoenix.lock`; `--locked`. Carry an **explicit source-kind** on resolved/locked packages (not rev-inferred) and shape `ManifestProvider` for a future version solver — the registry-readiness seams above.
4. **Cross-package imports.** Thread a `dependency_roots: HashMap<String, PathBuf>` through `resolve_module_path`; first-segment dispatch; per-package `EscapesRoot`; collision + missing-dependency diagnostics. Make `build`/`run`/`check` resolve+fetch before compiling.
5. **CLI.** `phoenix init [--name]` scaffolds `phoenix.toml` + an entry `.phx`; `phoenix add <name> (--git <url> [--tag|--rev|--branch] | --path <dir>)` edits the manifest and refreshes the lockfile. Tempdir + local-git-repo integration tests.
6. **Close.** Exit criteria below; design-decisions.md writeup; known-issues entries for the carve-outs.

### Exit criteria for declaring Phase 3.1 complete

- [x] `[package]` + `[dependencies]` parse from `phoenix.toml`; `[gen]` still parses; malformed manifests give clear diagnostics. Unit tests cover the schema. *(PR1)*
- [x] Semver resolution solves a transitive graph and reports conflicts legibly; covered by tests over injected manifests. *(PR2)*
- [x] Git dependencies fetch into the cache and local `path` dependencies resolve in place; `phoenix.lock` is generated, respected, and makes git-backed builds reproducible from a clean checkout; `--locked` detects drift. *(PR3)*
- [x] Registry-readiness seams are in place: resolved/locked packages carry an explicit source-kind (not inferred from "has a git rev"), and `ManifestProvider` is shaped so a future version solver can be added without a trait break. (No registry behavior is implemented — only the seams.) *(PR4.5)*
- [x] `import dep.module { ... }` resolves to the fetched package (public-only), with per-package `EscapesRoot` preserved and collision/missing-dep diagnostics. Multi-package integration fixture builds and runs. *(PR4)*
- [x] `phoenix build` / `run` / `check` resolve + fetch dependencies first; `phoenix init` and `phoenix add` work, with tempdir + local-git-repo integration tests. *(PR4, PR5)*
- [x] Workspace `cargo test` / `clippy --all-targets` / `fmt --check` clean; CI green.
- [x] `phoenix.toml.example` updated; design-decisions.md records the locked decisions; known-issues opened for the registry and npm/js carve-outs. *(PR6)*

### 3.1 Parallel-track note

3.1 lives entirely in **`phoenix-driver`** (config, CLI, a new resolver/lockfile module or a `phoenix-package` crate) and **`phoenix-modules`** (the resolver seam), plus new workspace deps. It does **not** touch the lexer, parser, sema, IR, runtime, or codegen — so it is disjoint from [Phase 4.6](./phase-4.md#46-json-and-serialization) and any other stdlib work. The only files both tracks might touch are the workspace `Cargo.toml` `[workspace.dependencies]` table (each appends distinct entries), `tests/fixtures/` (additive new files), and the docs (different sections). Rebase those few touch-points frequently; everything else is in separate crates.

### Dependency resolution across commands

Any command that compiles a file — `check`, `run`, `build`, `ir`, `run-ir`, `gen` — first walks up from the entry file to discover the nearest `phoenix.toml` and, if it declares `[dependencies]`, resolves + fetches them before compiling. This is project-relative discovery (cargo-style): running a file that lives under a project with git dependencies will fetch those dependencies even if the file itself imports nothing from them.

Two roots are in play and they are intentionally distinct: **manifest/dependency resolution** is rooted at the discovered `phoenix.toml`'s directory (path deps resolve relative to it, the lockfile is written there), while **local-module resolution** for the entry package is rooted at the *entry file's own directory* (a bare `import util` resolves to `util.phx` beside the entry file, not beside the manifest). For the common layout — entry file and `phoenix.toml` in the same directory — they coincide. They diverge only when the entry file sits in a subdirectory below its manifest; that case is unchanged from pre-PR4 single-package behavior and is not (yet) something the package manager re-roots.

`--locked` (refuse to update `phoenix.lock`, error on drift) is accepted only on `check`, `run`, and `build` — the commands that produce a runnable/shippable artifact and therefore need reproducibility. `ir`, `run-ir`, and `gen` are diagnostic/codegen paths: they still resolve dependencies (so cross-package imports work) and may *write* `phoenix.lock` if it is out of date, but they are not `--locked`-gated. The asymmetry is deliberate — reproducibility gating belongs on the build/run surface, not on IR inspection or schema codegen.

### ✅ Phase 3.1 closed (2026-07-01)

Shipped a git-first, `phoenix.toml`-driven package manager end-to-end, in six reviewed PRs:

- **PR1 — Manifest.** `[package]` (name, version, optional description/authors/license) + `[dependencies]` (git `{ git, tag|rev|branch }` and local `{ path }`); a bare-string semver value is reserved for the registry with a clear "no registry configured" error; `[gen]` still parses; `deny_unknown_fields` preserved.
- **PR2 — Resolver core.** Transitive dependency graph + semver reconciliation (one source per name, caret-compatible, highest wins) + legible source/version/cyclic conflict diagnostics, over an injectable `ManifestProvider` (no fetching).
- **PR3 — Fetch + lockfile.** `gix` clones git sources into `$PHOENIX_HOME/cache` (default `~/.phoenix/cache`, keyed by URL + SHA); path sources resolve in place; `phoenix.lock` (name-keyed tables, git-only, requested-ref recorded) is generated, respected, and reproducible from a clean checkout; `--locked` detects drift (including a manifest ref bump).
- **PR4 — Cross-package imports.** First-segment dispatch against declared dependency names; package-qualified module identity so a dependency's internal module never collides with a local one; per-package `EscapesRoot`; collision + missing-dependency diagnostics; `build`/`run`/`check` resolve+fetch before compiling. The multi-package fixture (`tests/fixtures/multi_package/`, with a deliberately colliding `util` module) checks, runs, and native-compiles.
- **PR4.5 — Registry-readiness seams.** Explicit source-kind on `ResolvedPackage`/`LockedPackage` (a `PackageSource` enum and an untagged `LockedPackage` enum — no rev-inference, lockfile format unchanged) + an `available_versions` capability on `ManifestProvider` (default one-element set), so a registry + version solver is additive.
- **PR5 — CLI.** `phoenix init [--name]` scaffolds a `[package]` manifest + a runnable root `main.phx`; `phoenix add <name> (--git … | --path …)` validates the source via the manifest schema, format-preservingly edits `phoenix.toml`, and atomically refreshes the lockfile (rolling the edit back on any resolution failure).

**Design decisions** are recorded in [design-decisions.md §Phase 3.1](../design-decisions.md#phase-31-package-manager) (A–F). **Carve-outs** — a central registry, and the npm / `import js "pkg"` / `[js-dependencies]` slice (now [§3.1.2](#312-npm--javascript-package-dependencies) below) — are open in [known-issues.md](../known-issues.md). Verified by `cargo test --workspace` / `clippy --all-targets` / `fmt --check` clean.

### Bugs closed in this phase (post-close review)

A review of the merged implementation surfaced three defects, all now fixed with regression tests:

- **Module identity could silently collide (miscompile).** Cross-package identity was a flat module path whose first segment was the bare dependency alias, so a dependency's root module (`import greet` → path `greet`) was indistinguishable from an entry-package top-level module `greet.phx`, and a **transitive** dependency aliased the same as an entry top-level module (e.g. both `util`) collapsed to one identity — the BFS dedup silently dropped one, and a dependency's `import util` could bind to the *entry's* `util`. Fixed by giving the package dimension a reserved, un-forgeable marker (`ModulePath::in_package`), realizing genuine `(package, module path)` identity ([design-decisions §Phase 3.1 E](../design-decisions.md#e-cross-package-identity-sema-is-package-aware-dependency-asts-stay-verbatim)); the marker is stripped for display so diagnostics are unchanged.
- **A transitive git dependency behind a `path` dep fetched into the project tree.** The cache-vs-project-dir decision was made from the project's *direct* dependencies only, so an all-`path` project whose path dep transitively declared a git source cloned it under `<project>/git/…` instead of `$PHOENIX_HOME/cache`, violating the "never inside the project tree" invariant. Fixed by resolving the cache root **lazily** — only when a git source is actually reached, at any depth — which also stops a genuinely git-free project from needing `$PHOENIX_HOME`.
- **`phoenix.lock` was written non-atomically.** A truncate-then-write could leave a corrupt lockfile on an interrupted write (and undermined `phoenix add`'s rollback guarantee). Fixed with an atomic temp-file + rename, so a failed write always leaves the previous lockfile intact.

## 3.1.2 npm / JavaScript Package Dependencies

**Status: COMPLETE (2026-07-17).** The carved-out follow-up to [§3.1](#31-package-manager) (Phase 2.5 [decision J](../design-decisions.md#j-npm-package-slice-deferred-to-phase-31)): let a Phoenix program depend on npm packages. **Depends on:** the §3.1 package manager (complete) and the Phase 2.5 `extern js` interop (complete). This item **does** touch the compiler backends (sema, cranelift glue, the interpreters), unlike §3.1 — so the [Phase 4.6 parallel-track hygiene](./phase-4.md#46-parallel-track-note) applies again to those crates.

### Goal

Let a `.phx` program bind to an npm package's exports — `extern js "left-pad" { function leftPad(s: String, n: Int) -> String }` — with the package declared in a `[js-dependencies]` manifest section. Phoenix wires the WASM glue to that module and emits a `package.json`; the developer's existing JS toolchain (`npm install` + Node/bundler) supplies the actual code. **Phoenix fetches and bundles nothing** (the BYO model — see the toolchain decision below).

### The seam it extends (what existed before this slice)

The Phase 2.5 `extern js` machinery is already module-agnostic at the IR level, so this is *extending a prepared seam*, not new machinery:

- `Op::ExternCall(module, name, args)` already carries the module; IR lowering and the native C-ABI symbol (`phx_extern_<module>__<name>`) already namespace by it.
- The only hardcoding is (a) sema registers every extern under the ambient module `"js"` (`FunctionInfo.extern_js = Some(("js", name))` in `check_register.rs`), and (b) the WASM glue binds hosts *flatly* as `host.<name>` (`crates/phoenix-cranelift/src/wasm/glue.rs`).
- Two code comments already anticipate this work: `wasm/glue.rs` ("if npm-package modules land (Phase 3.1), … this must become `host.<module>.<name>`") and the interpreter's `interpreter/mod.rs` ("npm-package modules arrive with the import-js grammar later").
- `[dependencies]` parsing (`manifest.rs`) and the `deny_unknown_fields` `PhoenixConfig` (`config.rs`) are where `[js-dependencies]` hooks in. The Gen carve-out already rejects `extern js` / `JsValue` in schemas.

### Design decisions (recorded in [design-decisions.md §Phase 3.1.2](../design-decisions.md#phase-312-npm--javascript-dependencies), A–D)

- **Toolchain: BYO host modules — Phoenix fetches/bundles nothing.** `[js-dependencies]` records the intended packages/versions; the glue emits namespaced `host.<pkg>.<name>` bindings that `import` the package specifier; the *embedder's* `node_modules` + bundler/Node runtime resolves it. Phoenix emits a `package.json` from `[js-dependencies]` so `npm install` is one command. **Why:** preserves the self-contained, no-external-toolchain posture the [gix decision](#31-package-manager) protects (no required Node/npm/esbuild), and matches Phase 2.5 [decision K](../design-decisions.md#k-extern-declarations-are-signature-only-the-host-is-supplied-separately-no-inline-js-bodies) (host supplied by the embedder). Rejected: shelling out to npm+esbuild (hard Node toolchain dependency); a pure-Rust npm resolver + JS bundler (a multi-phase subsystem disproportionate to this slice). A future **opt-in** `phoenix build --bundle` (shell out to a bundler *if present*, never required) can layer on top later — out of scope here.
- **Syntax: `extern js "specifier" { … }`.** Extend the existing `extern js` block with an optional module-specifier string; `extern js { … }` (no string) stays the ambient `js` host. It is the same construct (host-external signatures), just naming the module — the smallest, most localized grammar change. (Supersedes decision J's `import js "pkg"` sketch, which would require the `import` grammar to carry a string source *and* a signature block.) **Signature-only** per decision K — no inline JS bodies; generating signatures from a package's `@types` is a future follow-up, not v1.
- **Backend scope: uniform mechanism-parity (A0).** `extern js "pkg"` compiles on all five backends; a *call* where no host is registered raises the existing [A0](../design-decisions.md#a0-parity-model-extern-functions-are-a-uniform-host-ffi-boundary) "unbound host `(module.name)`" runtime error — identical to `extern js` today. npm-using programs stay portable source that only *does* something under a WASM+JS host; a stubbable extern rejoins the five-backend byte-identical matrix. (Rejected a compile-time gate on non-JS targets — it would break the "same source compiles everywhere" property `extern js` has.)
- **`package.json` write-if-absent; an undeclared module warns (decided 2026-07-15, during implementation).** A wasm build writes `package.json` beside the glue only when none is already there — a developer-owned file is never clobbered — and an entry-package `extern js "pkg"` naming a module absent from `[js-dependencies]` warns rather than errors, on wasm builds only. Full rationale in [design-decisions §Phase 3.1.2 D](../design-decisions.md#d-packagejson-is-generated-only-when-absent-an-undeclared-module-warns).

### Scope boundaries (carved out, with forward pointers)

- **Automatic npm fetch + bundling is out of scope** (the BYO decision). A future opt-in `--bundle` convenience layer, and/or a pure-Rust fetcher, ride a later follow-up.
- **`@types` → Phoenix-signature generation is out of scope.** Signatures are hand-declared (decision K). A `.d.ts`-to-`extern js` generator is a separate future tool.
- **No Phoenix-owned JS lockfile.** The developer's `package-lock.json` owns JS-dependency reproducibility; Phoenix does not duplicate it.

### PR sequence

1. **Grammar + AST + sema.** Extend the `extern js` block to carry an optional module-specifier string (absent ⇒ ambient `"js"`, unchanged); sema registers externs under `(specifier, name)` so the module flows to `Op::ExternCall`. Marshallability rules unchanged. Unit tests.
2. **Backend namespacing.** Make the wasm-linear + wasm-gc glue and the required-host guard namespace host bindings as `host.<module>.<name>` (the already-TODO'd change); route the interpreters' extern dispatch by module; native already namespaces. A backend-matrix fixture with a named non-`js`, stubbable module (rejoins the byte-identical matrix).
3. **`[js-dependencies]` + `package.json` emission.** Add the `[js-dependencies]` section to `PhoenixConfig` (npm name → version spec); on a wasm `phoenix build`, emit a `package.json` from it beside the glue; diagnose an `extern js "pkg"` whose module is not a declared js-dependency. Integration tests.
4. **Close.** design-decisions.md §3.1.2 writeup; flip the [known-issues](../known-issues.md) npm entry from "carved out" to "implemented (BYO); auto-fetch/bundle deferred"; exit criteria + closeout.

### Exit criteria for declaring Phase 3.1.2 complete

- [x] `extern js "specifier" { … }` parses (ambient `extern js { … }` unchanged); sema registers externs under the named module; marshallability diagnostics unchanged. Unit tests. (Plus cross-module coherence: declarations binding the same `(host module, name)` pair must agree on parameter/return types — the pair is one linkage downstream (one wasm import, one shim symbol, one glue thunk), so a mismatch would mis-marshal, silently where the flattened ABIs coincide. Identical re-declarations across modules stay legal and dedupe — the expected BYO pattern, pinned by the `npm_module_multi` Node-tier fixture.)
- [x] WASM glue (both sub-targets) binds hosts as `host.<module>.<name>` and the required-host guard is per-module; the interpreters dispatch externs by module; a named-module interop fixture runs on the backend matrix (stubbed hosts rejoin byte-identical parity). (Native routes via its shim symbol, the module half escaped to a C identifier — `left-pad` → `left_2dpad` — so npm specifiers stay definable from plain C and the `__` separator stays unambiguous.)
- [x] `[js-dependencies]` parses and validates; a wasm build emits a `package.json` from it; an `extern js "pkg"` naming an undeclared js-dependency is diagnosed. Integration tests (tempdir). (Emission is **write-if-absent** and the undeclared diagnostic is a **warning**, emitted on wasm builds only — the sole target where the npm binding matters — and scoped to the entry package's own externs — see [design-decisions §Phase 3.1.2 D](../design-decisions.md#d-packagejson-is-generated-only-when-absent-an-undeclared-module-warns).)
- [x] Calling an npm extern with no host registered gives the A0 "unbound host" runtime error on native / the interpreters (no silent no-op); no compile-time gate.
- [x] Workspace `cargo test` / `clippy --all-targets` / `fmt --check` clean; CI green.
- [x] design-decisions.md §3.1.2 records the locked decisions; the known-issues npm entry is updated; `phoenix.toml.example` documents `[js-dependencies]`.

### Closeout (2026-07-17)

Shipped npm dependencies on the BYO model, in four reviewed PRs:

- **PR1 — Grammar + AST + sema.** `extern js "specifier" { … }` carries an optional module specifier (absent ⇒ the ambient `"js"` host, unchanged; an empty string is a parse error); sema registers each extern under `(specifier, name)` so the module flows to `Op::ExternCall`. Marshallability rules untouched.
- **PR2 — Backend namespacing.** Both wasm sub-targets bind hosts as `host.<module>.<name>` with a per-module required-host guard; the interpreters dispatch externs by module; native routes through its shim symbol with the module half escaped to a C identifier (`left-pad` → `left_2dpad`), keeping the `__` separator unambiguous. Named-module fixtures (`npm_module`, `npm_module_multi`) run on the backend matrix — stubbed hosts rejoin byte-identical parity.
- **PR3 — `[js-dependencies]` + `package.json`.** The manifest section (npm name → verbatim version spec) parses and validates; a wasm build emits a `package.json` (`"type": "module"` + sorted dependencies) beside the glue when none is present; on a wasm build, an entry-package `extern js "pkg"` naming an undeclared module warns and the build proceeds (other targets bind extern hosts directly, not via npm, so they skip the check).
- **PR4 — Close.** This writeup.

**Design decisions** are recorded in [design-decisions.md §Phase 3.1.2](../design-decisions.md#phase-312-npm--javascript-dependencies) (A–D); B supersedes Phase 2.5 [decision J](../design-decisions.md#j-npm-package-slice-deferred-to-phase-31)'s `import js "pkg"` sketch. **Carve-outs** — automatic npm fetch/bundling (an opt-in `--bundle` is the future shape) and `@types` → signature generation — are open in [known-issues.md](../known-issues.md). Verified by `cargo test --workspace` / `clippy --all-targets` / `fmt --check` clean.

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
