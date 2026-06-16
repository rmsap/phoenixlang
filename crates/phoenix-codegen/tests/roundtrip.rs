//! Behavioral round-trip harness for Phoenix Gen.
//!
//! Sibling to [`compiles_and_lints`](./compiles_and_lints.rs), which proves the
//! generated code is *valid + clean* but explicitly NOT *behaviorally correct*.
//! This harness closes that gap: for each target it generates a client AND a
//! server from the same schema, runs the generated client against the generated
//! server over a shared set of fixtures, and asserts they round-trip data,
//! error-variant→status mappings, and constraint violations correctly.
//!
//! See `docs/phoenix-gen-roundtrip-design.md` for the design and
//! `tests/roundtrip/README.md` for the `contract.json` schema that every target
//! driver consumes.
//!
//! ── Layout ──────────────────────────────────────────────────────────────────
//!
//! ```text
//! tests/roundtrip/
//!   contract.json        # language-agnostic interaction cases (shared)
//!   README.md            # contract.json schema docs
//!   go/
//!     roundtrip_test.go  # committed Go driver (package roundtrip_test)
//!     go.mod.template    # module template assembled at test time
//!   typescript/          # committed TS driver (driver.ts), mirroring go/
//!   python/              # committed Python driver (driver.py), mirroring go/
//! ```
//!
//! ── Toolchain gating ────────────────────────────────────────────────────────
//!
//! Identical to `compiles_and_lints.rs`: if a required tool is missing from
//! `PATH`, the target test SKIPS with a printed message — UNLESS
//! `PHOENIX_GEN_E2E=1` is set, in which case a missing toolchain is a hard
//! failure so CI cannot silently skip.

use std::path::{Path, PathBuf};

mod common;
use common::{
    chi_module_at_version, chi_require_from_scaffold, chi_scaffold_dir, e2e_required, gate,
    go_module_cached, missing_tools, parse_and_check, run,
};

/// The schema every target round-trips against. Same fixture the
/// compile-and-lint harness uses, so the two suites stay in lock-step.
const SCHEMA: &str = include_str!("../../../tests/fixtures/gen_api.phx");

/// The shared, language-agnostic contract every target driver consumes.
const CONTRACT_JSON: &str = include_str!("roundtrip/contract.json");

// ── Toolchain gating + subprocess runner live in `common` (shared with
//    compiles_and_lints.rs), as does the schema → AST + analysis pipeline. ──

/// Absolute path to `tests/roundtrip/` (where the committed drivers live).
fn roundtrip_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("roundtrip")
}

// ── Go target ───────────────────────────────────────────────────────────────

fn generate_go_files(schema: &str) -> phoenix_codegen::GoFiles {
    let (program, result) = parse_and_check(schema);
    phoenix_codegen::generate_go(&program, &result)
}

/// Like [`generate_go_files`] but emits the chi `server.go` variant. Only the
/// router wiring in `server.go` differs (chi handlers are ordinary
/// `http.HandlerFunc`s); the driver mounts `NewRouter(stub)` as an `http.Handler`
/// either way, so the round-trip driver is shared unchanged.
fn generate_go_chi_files(schema: &str) -> phoenix_codegen::GoFiles {
    let (program, result) = parse_and_check(schema);
    phoenix_codegen::generate_go_with(&program, &result, phoenix_codegen::GoServerFramework::Chi)
}

/// Generates the Go `api` package from `SCHEMA`, assembles a tempdir Go module
/// containing it plus the committed Go driver + `contract.json` + a `go.mod`
/// (from `go.mod.template`), then runs `go test ./...` and asserts exit 0.
///
/// Unlike the TS/Python tests — which run IN their committed driver dirs because
/// those need a pre-installed dependency tree (`node_modules` / `.venv`) that
/// can't live in a tempdir — the Go driver depends only on the stdlib plus the
/// generated `api` package, so a self-contained throwaway module is cleaner and
/// avoids mutating the committed tree.
///
/// Module layout (matches what `roundtrip_test.go` imports — `roundtrip/api`):
/// ```text
/// <tmp>/
///   go.mod                 # module roundtrip
///   contract.json
///   roundtrip_test.go      # the committed driver
///   api/                   # generated: package api
///     types.go client.go handlers.go server.go
/// ```
#[test]
fn go_roundtrip() {
    if gate(&missing_tools(&["go"])) {
        return;
    }

    let files = generate_go_files(SCHEMA);

    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    let api_dir = root.join("api");
    std::fs::create_dir(&api_dir).expect("create api dir");
    std::fs::write(api_dir.join("types.go"), &files.types).expect("write types.go");
    std::fs::write(api_dir.join("client.go"), &files.client).expect("write client.go");
    std::fs::write(api_dir.join("handlers.go"), &files.handlers).expect("write handlers.go");
    std::fs::write(api_dir.join("server.go"), &files.server).expect("write server.go");

    let driver_dir = roundtrip_dir().join("go");
    let go_mod =
        std::fs::read_to_string(driver_dir.join("go.mod.template")).expect("read go.mod.template");
    std::fs::write(root.join("go.mod"), go_mod).expect("write go.mod");
    // Copy every committed `*_test.go` driver file into the module: the
    // per-schema driver (`roundtrip_test.go`) plus the schema-agnostic boilerplate
    // split out into sibling `*_test.go` files (e.g. `harness_test.go`). All share
    // package `roundtrip_test`, so a new split file is picked up without touching
    // this. The `_test.go` suffix is required: only test files may declare the
    // `roundtrip_test` package, so a plain `*.go` here would fail to compile.
    let mut copied_driver = false;
    for entry in std::fs::read_dir(&driver_dir).expect("read go driver dir") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().expect("go file name");
        if name.to_str().is_some_and(|n| n.ends_with("_test.go")) {
            let contents = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            std::fs::write(root.join(name), contents)
                .unwrap_or_else(|e| panic!("write {name:?}: {e}"));
            if name == "roundtrip_test.go" {
                copied_driver = true;
            }
        }
    }
    // The per-schema driver carries the only `Test*` function; without it `go
    // test` would run zero tests and pass silently. Fail loudly if it's gone
    // (rename/bad merge) instead of reporting a false green.
    assert!(
        copied_driver,
        "no roundtrip_test.go found in {}; go module would have no tests to run",
        driver_dir.display()
    );
    std::fs::write(root.join("contract.json"), CONTRACT_JSON).expect("write contract.json");

    let (ok, out) = run(root, "go", &["test", "./..."]);
    assert!(ok, "go round-trip test failed:\n{out}");
}

/// Round-trips the generated **chi** server. Identical to [`go_roundtrip`] except
/// the chi `server.go` variant is generated and the module requires chi: the
/// driver is unchanged because it mounts `NewRouter(stub)` as an `http.Handler`,
/// and `chi.Router` is one. The chi `require` is added to the module and the
/// matching `go.sum` is copied from the committed `go-chi` scaffold (the single
/// pinned source of chi's hashes); `go test` then resolves chi from the module
/// cache (offline) or the proxy (network). If chi isn't cached and
/// `PHOENIX_GEN_E2E=1` isn't set (which permits the network), the test skips
/// rather than reaching for the proxy in a sandboxed/offline run.
///
/// One round-trip over `SCHEMA` (`gen_api.phx`) is full coverage of the chi
/// divergence: that schema exercises all five verbs (so every `router.MethodFunc`
/// registration runs), nine path-param endpoints across varied shapes (`{id}`,
/// `{postId}`, `{tag}`, nested) so `chi.URLParam` is driven on each, plus request
/// and response headers and multi-status responses — all through the real
/// decode→handle→respond path. The chi-vs-net/http delta lives entirely in
/// router wiring / registration / the path-param accessor, so re-running the
/// fixture library here would add no behavioral coverage the contract doesn't
/// already give.
#[test]
fn go_chi_roundtrip() {
    if gate(&missing_tools(&["go"])) {
        return;
    }

    // chi is resolved from the module cache (or the proxy under E2E), not
    // vendored. Skip cleanly if it isn't cached and the network isn't permitted,
    // so a sandboxed/offline run doesn't fail trying to download.
    let chi_scaffold = chi_scaffold_dir();
    // chi's pinned version comes from the scaffold's own `go.mod` (the single
    // source of truth, shared with the compile-lint scaffold): `module version`
    // form for the `require` directive, `module@version` for the cache gate.
    let chi_require = chi_require_from_scaffold(&chi_scaffold);
    let chi_at_version = chi_module_at_version(&chi_scaffold);
    if !e2e_required() && !go_module_cached(&chi_at_version) {
        eprintln!(
            "SKIP go chi round-trip (set PHOENIX_GEN_E2E=1 to enforce): \
             {chi_at_version} not in the Go module cache"
        );
        return;
    }

    let files = generate_go_chi_files(SCHEMA);

    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    let api_dir = root.join("api");
    std::fs::create_dir(&api_dir).expect("create api dir");
    std::fs::write(api_dir.join("types.go"), &files.types).expect("write types.go");
    std::fs::write(api_dir.join("client.go"), &files.client).expect("write client.go");
    std::fs::write(api_dir.join("handlers.go"), &files.handlers).expect("write handlers.go");
    std::fs::write(api_dir.join("server.go"), &files.server).expect("write server.go");

    let driver_dir = roundtrip_dir().join("go");
    // Base module + the chi require so the chi-importing server.go resolves.
    let base_mod =
        std::fs::read_to_string(driver_dir.join("go.mod.template")).expect("read go.mod.template");
    let go_mod = format!("{}\nrequire {}\n", base_mod.trim_end(), chi_require);
    std::fs::write(root.join("go.mod"), go_mod).expect("write go.mod");
    // Copy the scaffold's go.sum (the single pinned source of chi's hashes) so
    // `go test` verifies chi against it while resolving the module from the cache.
    let go_sum =
        std::fs::read_to_string(chi_scaffold.join("go.sum")).expect("read go-chi scaffold go.sum");
    std::fs::write(root.join("go.sum"), go_sum).expect("write go.sum");

    let mut copied_driver = false;
    for entry in std::fs::read_dir(&driver_dir).expect("read go driver dir") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().expect("go file name");
        if name.to_str().is_some_and(|n| n.ends_with("_test.go")) {
            let contents = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            std::fs::write(root.join(name), contents)
                .unwrap_or_else(|e| panic!("write {name:?}: {e}"));
            if name == "roundtrip_test.go" {
                copied_driver = true;
            }
        }
    }
    assert!(
        copied_driver,
        "no roundtrip_test.go found in {}; go module would have no tests to run",
        driver_dir.display()
    );
    std::fs::write(root.join("contract.json"), CONTRACT_JSON).expect("write contract.json");

    let (ok, out) = run(root, "go", &["test", "./..."]);
    assert!(ok, "go chi round-trip test failed:\n{out}");
}

// ── TypeScript target ─────────────────────────────────────────────────────

/// Generates both the Express and Fastify variants from a single parse/check.
/// Only `server.ts` differs between frameworks; `types.ts`/`client.ts`/
/// `handlers.ts` are framework-independent. We assert that invariant here so the
/// Fastify round-trip (which reuses the Express-generated non-server files in the
/// shared `generated/` dir) can't silently pass against stale files if the
/// generator ever diverges them.
fn generate_typescript_express_and_fastify(
    schema: &str,
) -> (
    phoenix_codegen::GeneratedFiles,
    phoenix_codegen::GeneratedFiles,
) {
    let (program, result) = parse_and_check(schema);
    let express = phoenix_codegen::generate_typescript_with(
        &program,
        &result,
        phoenix_codegen::TsServerFramework::Express,
    );
    let fastify = phoenix_codegen::generate_typescript_with(
        &program,
        &result,
        phoenix_codegen::TsServerFramework::Fastify,
    );
    assert_eq!(
        express.types, fastify.types,
        "types.ts must be framework-independent"
    );
    assert_eq!(
        express.client, fastify.client,
        "client.ts must be framework-independent"
    );
    assert_eq!(
        express.handlers, fastify.handlers,
        "handlers.ts must be framework-independent"
    );
    (express, fastify)
}

/// Generates the TypeScript client/server/types/handlers from `SCHEMA`, drops
/// them into the committed driver's gitignored `generated/` dir, copies in the
/// shared `contract.json`, then runs the committed `driver.ts` via `tsx` and
/// asserts exit 0.
///
/// Like the TypeScript compile-and-lint test, the toolchain is pinned via the
/// committed npm project at `tests/roundtrip/typescript/` (its own
/// `package-lock.json` + installed `node_modules` with express + tsx). We run IN
/// that committed dir so the driver resolves both its deps and the generated
/// `./generated/*` imports (Bundler resolution handles the extensionless
/// imports). The `generated/` and `contract.json` paths are gitignored and
/// recreated fresh each run.
///
/// Project layout the driver expects:
/// ```text
/// tests/roundtrip/typescript/
///   driver.ts            # committed driver (package roundtrip)
///   package.json         # express + tsx, pinned
///   package-lock.json
///   tsconfig.json
///   node_modules/        # gitignored; `npm ci` reproducible
///   contract.json        # gitignored; copied in at test time
///   generated/           # gitignored; the generated client/server/types/handlers
/// ```
#[test]
fn typescript_roundtrip() {
    if gate(&missing_tools(&["node", "npm", "npx"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("typescript");
    let node_modules = driver_dir.join("node_modules");
    if !node_modules.is_dir() {
        let msg = format!(
            "TypeScript round-trip driver has no node_modules; run `npm ci` in {}",
            driver_dir.display()
        );
        if e2e_required() {
            panic!("PHOENIX_GEN_E2E=1 but {msg}");
        }
        eprintln!("SKIP (set PHOENIX_GEN_E2E=1 to enforce): {msg}");
        return;
    }

    let (files, fastify) = generate_typescript_express_and_fastify(SCHEMA);

    let generated = driver_dir.join("generated");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated dir");
    std::fs::write(generated.join("types.ts"), &files.types).expect("write types.ts");
    std::fs::write(generated.join("client.ts"), &files.client).expect("write client.ts");
    std::fs::write(generated.join("handlers.ts"), &files.handlers).expect("write handlers.ts");
    std::fs::write(generated.join("server.ts"), &files.server).expect("write server.ts");

    std::fs::write(driver_dir.join("contract.json"), CONTRACT_JSON).expect("write contract.json");

    let (ok, out) = run(&driver_dir, "npx", &["tsx", "driver.ts"]);
    assert!(ok, "typescript round-trip test failed:\n{out}");

    // Fastify server variant: only `server.ts` differs (types/client/handlers
    // are framework-independent — asserted in `generate_typescript_express_and_fastify`
    // — so the ones already in `generated/` from the Express pass stand). Overwrite
    // just the server, into the SAME `generated/` (so this must stay in this one
    // test — a separate `#[test]` would race the shared dir), and drive the Fastify
    // plugin via `driver-fastify.ts` against the same contract.
    std::fs::write(generated.join("server.ts"), &fastify.server).expect("write server.ts");

    let (ok, out) = run(&driver_dir, "npx", &["tsx", "driver-fastify.ts"]);
    assert!(ok, "typescript fastify round-trip test failed:\n{out}");
}

// ── Python target ──────────────────────────────────────────────────────────

fn generate_python_files(schema: &str) -> phoenix_codegen::PythonFiles {
    let (program, result) = parse_and_check(schema);
    phoenix_codegen::generate_python(&program, &result)
}

/// Generates the Python package from `SCHEMA`, drops it into the committed
/// driver's gitignored `generated/` dir as an importable package
/// (`generated/{__init__,models,client,handlers,server}.py` — the generated
/// files import each other relatively, e.g. `from .models import ...`), copies
/// in the shared `contract.json`, then runs the committed `driver.py` with the
/// driver's local `.venv` python and asserts exit 0.
///
/// The toolchain is pinned via the committed project at
/// `tests/roundtrip/python/` (its own `requirements.txt` + a local `.venv` with
/// fastapi/httpx/pydantic). The driver is a plain script (no pytest): it mounts
/// `create_router(stub)` on a `FastAPI()` app and drives the generated httpx
/// client IN-PROCESS via `httpx.ASGITransport` (no real port). The `generated/`
/// and `contract.json` paths are gitignored and recreated fresh each run.
///
/// Project layout the driver expects:
/// ```text
/// tests/roundtrip/python/
///   driver.py            # committed driver (plain script, exits non-zero on failure)
///   requirements.txt     # fastapi + httpx + pydantic, pinned
///   .venv/               # gitignored; the analog of node_modules
///   contract.json        # gitignored; copied in at test time
///   generated/           # gitignored; the generated importable package
/// ```
#[test]
fn python_roundtrip() {
    if gate(&missing_tools(&["python3"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("python");
    let venv_python = driver_dir.join(".venv").join("bin").join("python");
    if !venv_python.is_file() {
        let msg = format!(
            "Python round-trip driver has no .venv; run `python3 -m venv .venv && \
             .venv/bin/pip install -r requirements.txt` in {}",
            driver_dir.display()
        );
        if e2e_required() {
            panic!("PHOENIX_GEN_E2E=1 but {msg}");
        }
        eprintln!("SKIP (set PHOENIX_GEN_E2E=1 to enforce): {msg}");
        return;
    }

    let files = generate_python_files(SCHEMA);

    let generated = driver_dir.join("generated");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated dir");
    std::fs::write(generated.join("__init__.py"), &files.init).expect("write __init__.py");
    std::fs::write(generated.join("models.py"), &files.models).expect("write models.py");
    std::fs::write(generated.join("client.py"), &files.client).expect("write client.py");
    std::fs::write(generated.join("handlers.py"), &files.handlers).expect("write handlers.py");
    std::fs::write(generated.join("server.py"), &files.server).expect("write server.py");

    std::fs::write(driver_dir.join("contract.json"), CONTRACT_JSON).expect("write contract.json");

    let python = venv_python.to_string_lossy().into_owned();
    let (ok, out) = run(&driver_dir, &python, &["driver.py"]);
    assert!(ok, "python round-trip test failed:\n{out}");
}
