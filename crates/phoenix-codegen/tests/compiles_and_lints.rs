//! End-to-end "the generated code actually compiles and lints" harness.
//!
//! Unlike the snapshot/string tests, this runs the real toolchain for each
//! target against generated output:
//! - Go: `go build ./...`, `gofmt -l` (empty), strict `golangci-lint run` (see [`GOLANGCI_CONFIG`]).
//! - TypeScript: `tsc --noEmit`, `eslint` (strict + strict-type-checked), `prettier --check`.
//! - Python: `black --check`, `ruff check` (broad select), `mypy` (strict).
//! - OpenAPI: `redocly lint`.
//!
//! Each target is run over several schemas covering different feature
//! combinations: `SCHEMA` (the full blog API), `MINIMAL_SCHEMA` (no query
//! params / errors / constrained body — proves feature-gated imports stay
//! absent), `WIDE_SCHEMA` / `WRAP_SCHEMA` (long identifiers that force the
//! prettier/black line-wrapping paths), and `FEATURE_SCHEMA` (maps, float
//! constraints, multi-path-param routes). `TAGGED_ENUM_SCHEMA` runs through
//! TypeScript only — Go and Python skip complex enums.
//!
//! Toolchain gating: if a required tool is missing from `PATH`, each target test
//! SKIPS with a printed message — UNLESS `PHOENIX_GEN_E2E=1` is set, in which
//! case a missing toolchain is a hard failure so CI cannot silently skip.
//!
//! ── Scope / known gap: behavioral (round-trip) testing ──────────────────────
//!
//! This harness proves the generated code *compiles, type-checks, lints, and is
//! formatted* — NOT that it *behaves correctly* at runtime. A client that builds
//! a wrong URL, a server that mis-coerces a query param, an inverted constraint,
//! or an off-by-one in path substitution would all still pass here. Nothing is
//! executed, and the client/server pair is never checked for mutual
//! consistency.
//!
//! Closing that gap is a separate, larger effort (a round-trip suite: spin up
//! the generated server, call it with the generated client over a set of
//! fixtures, and assert request/response shapes — per target, or cross-target
//! against a reference server). Until then, "green here" means "valid + clean,"
//! not "correct."

use std::path::Path;

mod common;
use common::{e2e_required, gate, missing_tools, parse_and_check, run, tool_available};

/// The representative schema exercised end-to-end. Mirrors
/// `tests/fixtures/gen_api.phx` (structs with constraints, an enum, optional
/// query params, omit/pick/partial bodies, error mappings, void responses).
const SCHEMA: &str = include_str!("../../../tests/fixtures/gen_api.phx");

/// A deliberately minimal schema: one struct, one endpoint with NO query params
/// and NO declared errors. It exercises the import paths the main `SCHEMA` can't
/// — generators must not emit imports (`Any` in the Python client, `HTTPException`
/// in the Python server, etc.) that go unused when those features are absent, or
/// the strict linters flag them.
const MINIMAL_SCHEMA: &str = r#"
struct Item {
    id: Int
    name: String
}

endpoint getItem: GET "/api/items/{id}" {
    response Item
}
"#;

/// A schema whose constrained field has a deliberately long name, so the
/// generated `if (!(...))` validation guard overflows the print width and must
/// be broken across lines (the condition, the `!(...)` operands, and the long
/// throw message). Exercises the wrapping path `MINIMAL_SCHEMA`/`SCHEMA` don't
/// reach, and locks the output to what `prettier`/`black` actually accept.
const WIDE_SCHEMA: &str = r#"
struct Profile {
    id: Int
    thisIsAnIntentionallyLongFieldNameToForceGuardWrapping: String where self.length > 0 && self.length <= 100
}

endpoint createProfile: POST "/api/profiles" {
    body Profile omit { id }
    response Profile
}
"#;

/// A schema that drives the *other* wrapping branches `WIDE_SCHEMA` doesn't
/// reach, each of which encodes a guess about how `prettier` lays a construct
/// out and so must be locked to the real formatter:
///   * long enum / error-variant names → the union type alias wraps onto one
///     leading-`|` member per line (`emit_union_type_alias`);
///   * long query-param names → the server's query-coercion object properties
///     wrap, including the ternary-arm break (`emit_object_property`,
///     `split_ternary`), and an `Option<String>` param exercises the
///     non-ternary value-on-its-own-line branch;
///   * a long static path → the client `fetch(...)` call breaks across lines
///     (`emit_fetch_call`).
const WRAP_SCHEMA: &str = r#"
enum AccountSubscriptionTierLevel {
    ComplimentaryStarterIntroductoryPlan
    ProfessionalMonthlyBillingPlan
    EnterpriseAnnualContractPlan
}

struct Widget {
    id: Int
    label: String
    tier: AccountSubscriptionTierLevel
}

endpoint listWidgets: GET "/api/widgets" {
    query {
        pageNumberOffsetForResults: Int = 1
        optionalSearchKeywordFilter: Option<String>
    }
    response List<Widget>
}

endpoint getNestedWidgetResource: GET "/api/organizations/teams/projects/widgets/configurations/details" {
    response Widget
}

endpoint createWidget: POST "/api/widgets" {
    body Widget omit { id }
    response Widget
    error {
        ResourceCouldNotBeLocatedError(404)
        RequestPayloadValidationError(400)
    }
}
"#;

/// Exercises feature dimensions the other schemas miss, all uniformly supported
/// across the Go / Python / TypeScript / OpenAPI generators:
///   * a `Map<String, String>` field (→ `map[string]string` / `dict[str, str]`
///     / `Record<string, string>` / `additionalProperties`);
///   * a `Float` field with a constraint using float literals (`0.0`/`1.0`),
///     which exercises float-literal rendering inside both the struct- and
///     body-validation paths;
///   * routes carrying *two* path params (`{regionId}` + `{configId}`), which
///     the single-param schemas never produce — notably TypeScript's
///     `Request<{ regionId: string; configId: string }>` request typing.
const FEATURE_SCHEMA: &str = r#"
struct ServerConfig {
    id: Int
    settings: Map<String, String>
    load: Float where self >= 0.0 && self <= 1.0
}

endpoint getRegionConfig: GET "/api/regions/{regionId}/configs/{configId}" {
    response ServerConfig
}

endpoint updateRegionConfig: PUT "/api/regions/{regionId}/configs/{configId}" {
    body ServerConfig omit { id }
    response ServerConfig
}
"#;

/// A schema with a *tagged-union* (payload-carrying) enum. Only the TypeScript
/// generator emits these — it lowers them to a discriminated union — so this
/// schema is run through the TypeScript target ONLY.
///
/// The Python and Go generators deliberately skip complex enums (see their
/// `emit_enum`: `if !all_unit { return; }`, "Skip complex enums for now"), so a
/// tagged-union enum used as a field would leave a dangling type reference and
/// fail to compile there. That is a known generator limitation, not something
/// this harness can lock down until those targets implement complex enums.
const TAGGED_ENUM_SCHEMA: &str = r#"
enum Shape {
    Circle(Float)
    Rect(Float, Float)
    Point
}

struct Drawing {
    id: Int
    shape: Shape
}

endpoint getDrawing: GET "/api/drawings/{id}" {
    response Drawing
}
"#;

/// Targets two generator edge cases the other schemas never hit, both run
/// through the Go and Python targets (the two affected generators):
///
///   * a **constrained `Option<T>` field carried into a body** (`displayName`):
///     the source type is already optional, so Go renders it as a pointer even
///     though no `partial` modifier applied. The body's `Validate()` must
///     nil-guard and dereference it (`if s.DisplayName != nil && !(...)`), the
///     same as the source struct's own `Validate()` — a regression guard for the
///     body-validation pointer detection.
///
///   * **required query params whose camelCase name forces a `Query(alias=...)`
///     ahead of a required plain param** (`maxResults` before `page`): a required
///     aliased param renders a syntactic default, so it must sort AFTER the
///     non-defaulted `page` or Python raises "non-default argument follows
///     default argument". The main `SCHEMA`'s `searchPosts` has required aliased
///     params too, but only in a *safe* order (no plain required param mixed in),
///     so this schema is what uniquely guards the reordering hazard.
const EDGE_SCHEMA: &str = r#"
struct Account {
    id: Int
    handle: String where self.length > 0 && self.length <= 30
    displayName: Option<String> where self.length <= 60
}

endpoint searchAccounts: GET "/api/accounts" {
    query {
        maxResults: Int
        page: Int
    }
    response List<Account>
}

endpoint updateAccount: PATCH "/api/accounts/{id}" {
    body Account omit { id }
    response Account
}
"#;

/// Headers-focused schema covering the generator branches the main `SCHEMA`'s
/// `getPostMetered` (a *mix* of required + optional request headers) cannot
/// reach:
///
///   * **all request headers optional** → the TS client renders the `headers`
///     param as a `= {}` default (`headers: { … } = {}`, not `headers?:`), so it
///     is omittable yet never `undefined` and the per-header send guard reads it
///     via a plain access (`if (headers.x !== undefined) …`). This is the one
///     shape that exercises the all-optional bag in `emit_header_set` /
///     `format_signature`, and it must type-check under `tsc` strict AND lint
///     clean under `eslint` strict-type-checked (a `headers?.x` chain on the
///     non-nullable bag would trip `no-unnecessary-condition`). The Go/Python
///     equivalents (`*T` params, `| None` kwargs) ride the same all-optional path.
///   * **all response headers optional** → every `<Endpoint>Result` envelope field
///     is optional (`*T` / `| None` / `?`), and the client read maps an absent
///     header to nil/None/undefined for each.
const HEADER_SCHEMA: &str = r#"
struct Thing {
    id: Int
    name: String
}

endpoint listThings: GET "/api/things" {
    headers {
        traceId: Option<String>
        maxResults: Option<Int>
    }
    response List<Thing>
}

endpoint getThing: GET "/api/things/{id}" {
    response Thing headers {
        etag: Option<String>
        ratelimitRemaining: Option<Int>
    }
}
"#;

/// The realistic schema fixture library (workspace `tests/fixtures/`; see the
/// "type-system gaps" entry in docs/design-decisions.md). Parse/sema
/// cleanliness is guarded by `phoenix-driver`'s `gen_schema_fixtures.rs`; every
/// fixture here is also run through THIS harness — generated, compiled, linted,
/// and format-checked on all four targets — unconditionally (under the
/// `PHOENIX_GEN_E2E` gate shared with the inline schemas). It was once gated
/// behind a `PHOENIX_GEN_FIXTURE_LIB` env var while a handful of generator bugs
/// (surfaced by these dense fixtures) made it red; those are all fixed — Go
/// passes `go build`/`gofmt`/`golangci-lint`, TypeScript `tsc`/`eslint`/`prettier`,
/// Python `black`/`ruff`/`mypy`, and OpenAPI `redocly lint` — so the gate is gone.
///
/// This list and the per-fixture test list in `phoenix-driver`'s
/// `gen_schema_fixtures.rs` must name the same fixtures; the
/// `gen_schema_library_lists_match` test in `phoenix-driver`'s
/// `fixture_inventory.rs` fails if the two lists ever diverge, so a schema
/// added to one file but forgotten in the other can't silently skip
/// compile-and-lint (or `phoenix check`) coverage.
const FILE_FIXTURES: &[(&str, &str)] = &[
    (
        "payments.phx",
        include_str!("../../../tests/fixtures/payments.phx"),
    ),
    (
        "multitenant_saas.phx",
        include_str!("../../../tests/fixtures/multitenant_saas.phx"),
    ),
    (
        "webhooks.phx",
        include_str!("../../../tests/fixtures/webhooks.phx"),
    ),
    (
        "file_storage.phx",
        include_str!("../../../tests/fixtures/file_storage.phx"),
    ),
    (
        "social.phx",
        include_str!("../../../tests/fixtures/social.phx"),
    ),
    (
        "internal_admin.phx",
        include_str!("../../../tests/fixtures/internal_admin.phx"),
    ),
];

// ── Toolchain gating + subprocess runner live in `common` (shared with
//    roundtrip.rs), as does the schema → AST + analysis pipeline. ──

fn generate_go_files(schema: &str) -> phoenix_codegen::GoFiles {
    let (program, result) = parse_and_check(schema);
    phoenix_codegen::generate_go(&program, &result)
}

/// Scaffolds a fresh Go module in a tempdir, writes the generated `api/*.go`,
/// then runs `go build`, `gofmt -l`, and (when present) `golangci-lint`.
///
/// Unlike the prettier/black targets, `gofmt` does not wrap on line width, so
/// the additional schemas here are not about layout — they exercise distinct
/// *feature combinations* through Go's strict toolchain. In particular Go treats
/// an unused import as a compile error, so `MINIMAL_SCHEMA` (no query params,
/// errors, or constrained body) is what proves the generator's feature-gated
/// imports (`net/url`, `strconv`, `fmt`, …) stay absent when unneeded.
fn check_go_output(files: &phoenix_codegen::GoFiles) {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    let api_dir = root.join("api");
    std::fs::create_dir(&api_dir).expect("create api dir");
    std::fs::write(root.join("go.mod"), "module gencheck\n\ngo 1.23\n").expect("write go.mod");
    // A strict golangci-lint config so "Go lints clean" means as much as the
    // TypeScript (`strict` + `strictTypeChecked`) and Python (`mypy --strict`,
    // broad `ruff select`) bars. Without a config golangci-lint runs only its
    // default linters; this adds correctness-focused families on top.
    std::fs::write(root.join(".golangci.yml"), GOLANGCI_CONFIG).expect("write .golangci.yml");
    std::fs::write(api_dir.join("types.go"), &files.types).expect("write types.go");
    std::fs::write(api_dir.join("client.go"), &files.client).expect("write client.go");
    std::fs::write(api_dir.join("handlers.go"), &files.handlers).expect("write handlers.go");
    std::fs::write(api_dir.join("server.go"), &files.server).expect("write server.go");

    // 1. `go build ./...` must succeed (also catches unused imports).
    let (built, build_out) = run(root, "go", &["build", "./..."]);
    assert!(built, "go build failed:\n{build_out}");

    // 2. `gofmt -l` over the generated files must report NOTHING.
    let go_files = [
        api_dir.join("types.go"),
        api_dir.join("client.go"),
        api_dir.join("handlers.go"),
        api_dir.join("server.go"),
    ];
    let mut gofmt_args = vec!["-l".to_string()];
    gofmt_args.extend(go_files.iter().map(|p| p.to_string_lossy().into_owned()));
    let gofmt_arg_refs: Vec<&str> = gofmt_args.iter().map(String::as_str).collect();
    let (_, gofmt_out) = run(root, "gofmt", &gofmt_arg_refs);
    assert!(
        gofmt_out.trim().is_empty(),
        "gofmt -l reported files needing formatting:\n{gofmt_out}"
    );

    // 3. `golangci-lint run ./...` must exit 0 (skip only this step if absent).
    if tool_available("golangci-lint") {
        let (linted, lint_out) = run(root, "golangci-lint", &["run", "./..."]);
        assert!(linted, "golangci-lint failed:\n{lint_out}");
    } else if e2e_required() {
        panic!("PHOENIX_GEN_E2E=1 but golangci-lint not found on PATH");
    } else {
        eprintln!("SKIP golangci-lint step (not installed)");
    }
}

/// Strict golangci-lint configuration written into each Go scaffold. Enables
/// correctness/bug-oriented linters on top of the default set (which already
/// includes staticcheck, govet, errcheck, ineffassign, unused, gosimple). These
/// were chosen to mirror the spirit of the TypeScript/Python strict rulesets
/// while staying clean against the generator's output; purely stylistic linters
/// that demand doc comments on every symbol are intentionally left off.
const GOLANGCI_CONFIG: &str = r#"
linters:
  enable:
    - bodyclose
    - errorlint
    - noctx
    - unconvert
    - unparam
    - misspell
    - nilerr
    - usestdlibvars
"#;

// ── Go target ───────────────────────────────────────────────────────────

#[test]
fn go_output_compiles_and_lints() {
    if gate(&missing_tools(&["go", "gofmt"])) {
        return;
    }

    // Run the full schema, then the minimal one (no query params / errors /
    // constrained body) so the generator's feature-gated imports are proven
    // absent when unneeded — Go fails to compile on an unused import. The wide
    // and wrap schemas add further feature combinations (gofmt does not wrap on
    // width, so they are about coverage, not layout).
    check_go_output(&generate_go_files(SCHEMA));
    check_go_output(&generate_go_files(MINIMAL_SCHEMA));
    check_go_output(&generate_go_files(WIDE_SCHEMA));
    check_go_output(&generate_go_files(WRAP_SCHEMA));
    check_go_output(&generate_go_files(FEATURE_SCHEMA));
    // Constrained `Option<T>` body field — the body `Validate()` must nil-guard
    // and deref the pointer (regression guard for body-validation detection).
    check_go_output(&generate_go_files(EDGE_SCHEMA));
    // All-optional request + response headers (the `*T` param / nil-guarded
    // send / nil-able envelope-field paths).
    check_go_output(&generate_go_files(HEADER_SCHEMA));

    // Realistic schema fixture library (see FILE_FIXTURES).
    for (name, schema) in FILE_FIXTURES.iter().copied() {
        eprintln!("fixture library: {name}");
        check_go_output(&generate_go_files(schema));
    }
}

// ── OpenAPI target ───────────────────────────────────────────────────────

/// The redocly config used to lint generated specs. It extends `recommended`
/// but disables rules that conflict with Phoenix Gen's documented design (auth
/// deferred, no license, optional 4xx). See the file for per-rule rationale.
const REDOCLY_CONFIG: &str = include_str!("scaffold/openapi/redocly.yaml");

fn generate_openapi_spec(schema: &str) -> String {
    let (program, result) = parse_and_check(schema);
    phoenix_codegen::generate_openapi(&program, &result)
}

/// Generates the OpenAPI spec for `schema` and lints it with `redocly`. `label`
/// identifies the schema in the failure message.
fn check_openapi_output(label: &str, schema: &str) {
    let spec = generate_openapi_spec(schema);

    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    std::fs::write(root.join("openapi.json"), &spec).expect("write openapi.json");
    // redocly auto-discovers `redocly.yaml` in the working directory.
    std::fs::write(root.join("redocly.yaml"), REDOCLY_CONFIG).expect("write redocly.yaml");

    let (linted, lint_out) = run(
        root,
        "npx",
        &["--yes", "@redocly/cli", "lint", "openapi.json"],
    );
    assert!(linted, "redocly lint failed for {label}:\n{lint_out}");
}

#[test]
fn openapi_output_lints() {
    // `npx` fetches `@redocly/cli` on first use; gate on `npx` being present.
    if gate(&missing_tools(&["npx"])) {
        return;
    }

    // Lint the spec for every schema the language targets exercise (except the
    // TypeScript-only tagged-enum one): each produces a distinct spec shape —
    // MINIMAL has no errors/query, FEATURE adds maps + multi-path-param +
    // float constraints, WIDE/WRAP add constrained/optional fields.
    check_openapi_output("SCHEMA", SCHEMA);
    check_openapi_output("MINIMAL_SCHEMA", MINIMAL_SCHEMA);
    check_openapi_output("WIDE_SCHEMA", WIDE_SCHEMA);
    check_openapi_output("WRAP_SCHEMA", WRAP_SCHEMA);
    check_openapi_output("FEATURE_SCHEMA", FEATURE_SCHEMA);
    check_openapi_output("HEADER_SCHEMA", HEADER_SCHEMA);

    // Realistic schema fixture library (see FILE_FIXTURES). NOTE: redocly's WASM
    // runtime needs a large address space; do not run this under a tight
    // `ulimit -v` (it OOMs under a 6 GB cap — a false failure unrelated to the
    // generated specs).
    for (name, schema) in FILE_FIXTURES.iter().copied() {
        check_openapi_output(name, schema);
    }
}

// ── TypeScript target ─────────────────────────────────────────────────────

fn generate_typescript_files(schema: &str) -> phoenix_codegen::GeneratedFiles {
    let (program, result) = parse_and_check(schema);
    phoenix_codegen::generate_typescript(&program, &result)
}

/// Writes the four generated `.ts` files into a fresh `generated/` dir under
/// `scaffold`, then runs `tsc`, `eslint`, and `prettier --check` against them.
///
/// NOTE: this mutates the committed scaffold's `generated/` dir in place (it is
/// recreated each call). All calls MUST stay funneled through the single
/// `typescript_output_compiles_and_lints` test so they run sequentially — cargo
/// runs separate `#[test]` fns in parallel, and two tests sharing this scaffold
/// would race on `generated/`. Add coverage as more calls in that one test, not
/// as new `#[test]` fns.
fn check_typescript_output(scaffold: &Path, files: &phoenix_codegen::GeneratedFiles) {
    let generated = scaffold.join("generated");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated dir");
    std::fs::write(generated.join("types.ts"), &files.types).expect("write types.ts");
    std::fs::write(generated.join("client.ts"), &files.client).expect("write client.ts");
    std::fs::write(generated.join("handlers.ts"), &files.handlers).expect("write handlers.ts");
    std::fs::write(generated.join("server.ts"), &files.server).expect("write server.ts");

    // 1. `tsc --noEmit` (strict via tsconfig.json) must pass.
    let (tsc_ok, tsc_out) = run(scaffold, "npx", &["tsc", "--noEmit"]);
    assert!(tsc_ok, "tsc --noEmit failed:\n{tsc_out}");

    // 2. `eslint generated/` (strict @typescript-eslint) must pass.
    let (eslint_ok, eslint_out) = run(scaffold, "npx", &["eslint", "generated/"]);
    assert!(eslint_ok, "eslint failed:\n{eslint_out}");

    // 3. `prettier --check generated/` must pass. We pass `--ignore-path` at an
    //    empty `.prettierignore` so Prettier does NOT fall back to `.gitignore`
    //    (which ignores `generated/`, silently checking nothing).
    let (prettier_ok, prettier_out) = run(
        scaffold,
        "npx",
        &[
            "prettier",
            "--check",
            "generated/",
            "--ignore-path",
            ".prettierignore",
        ],
    );
    assert!(prettier_ok, "prettier --check failed:\n{prettier_out}");
}

#[test]
fn typescript_output_compiles_and_lints() {
    // Unlike Go/OpenAPI (which scaffold into a fresh tempdir), the TypeScript
    // toolchain is pinned via a committed npm project at
    // `tests/scaffold/typescript/` with its own `package-lock.json`. We run the
    // checks IN that committed dir so they reuse its installed `node_modules`
    // (a tempdir copy would have none). Generated files go into the gitignored
    // `generated/` subdir, which we recreate fresh each run.
    if gate(&missing_tools(&["node", "npm", "npx"])) {
        return;
    }

    let scaffold = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scaffold")
        .join("typescript");
    let node_modules = scaffold.join("node_modules");
    if !node_modules.is_dir() {
        let msg = format!(
            "TypeScript scaffold has no node_modules; run `npm ci` in {}",
            scaffold.display()
        );
        if e2e_required() {
            panic!("PHOENIX_GEN_E2E=1 but {msg}");
        }
        eprintln!("SKIP (set PHOENIX_GEN_E2E=1 to enforce): {msg}");
        return;
    }

    // Check the full schema, then the minimal one (no query params / errors) so
    // feature-gated imports are exercised in both their present and absent forms,
    // then the wide schema so the overflowing-guard wrapping path is covered.
    check_typescript_output(&scaffold, &generate_typescript_files(SCHEMA));
    check_typescript_output(&scaffold, &generate_typescript_files(MINIMAL_SCHEMA));
    check_typescript_output(&scaffold, &generate_typescript_files(WIDE_SCHEMA));
    check_typescript_output(&scaffold, &generate_typescript_files(WRAP_SCHEMA));
    check_typescript_output(&scaffold, &generate_typescript_files(FEATURE_SCHEMA));
    // Tagged-union enums are a TypeScript-only feature (see TAGGED_ENUM_SCHEMA).
    check_typescript_output(&scaffold, &generate_typescript_files(TAGGED_ENUM_SCHEMA));
    // All-optional request headers force the nullable `headers?:` param and its
    // optional-chain send guard — the `emit_header_set` path the mixed-header
    // `getPostMetered` in SCHEMA never reaches.
    check_typescript_output(&scaffold, &generate_typescript_files(HEADER_SCHEMA));

    // Realistic schema fixture library (see FILE_FIXTURES).
    for (name, schema) in FILE_FIXTURES.iter().copied() {
        eprintln!("fixture library: {name}");
        check_typescript_output(&scaffold, &generate_typescript_files(schema));
    }
}

// ── Python target ──────────────────────────────────────────────────────────

fn generate_python_files(schema: &str) -> phoenix_codegen::PythonFiles {
    let (program, result) = parse_and_check(schema);
    phoenix_codegen::generate_python(&program, &result)
}

/// Writes the generated package files into a fresh `generated/` dir under
/// `scaffold`, then runs `black --check`, `ruff check`, and `mypy` against them.
/// `venv_bin` is the scaffold's `.venv/bin` (where the pinned tools live).
///
/// NOTE: like `check_typescript_output`, this mutates the committed scaffold's
/// `generated/` dir in place. All calls MUST stay funneled through the single
/// `python_output_compiles_and_lints` test so they run sequentially — two
/// `#[test]` fns sharing this scaffold would race on `generated/` under cargo's
/// parallel runner. Add coverage as more calls in that one test.
fn check_python_output(scaffold: &Path, venv_bin: &Path, files: &phoenix_codegen::PythonFiles) {
    let generated = scaffold.join("generated");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated dir");
    std::fs::write(generated.join("__init__.py"), &files.init).expect("write __init__.py");
    std::fs::write(generated.join("models.py"), &files.models).expect("write models.py");
    std::fs::write(generated.join("client.py"), &files.client).expect("write client.py");
    std::fs::write(generated.join("handlers.py"), &files.handlers).expect("write handlers.py");
    std::fs::write(generated.join("server.py"), &files.server).expect("write server.py");

    // 1. `black --check`. Black's default exclude follows `.gitignore` (which
    //    lists `generated/`, silently skipping it), so we pass the files
    //    explicitly to force them to be checked.
    let black = venv_bin.join("black").to_string_lossy().into_owned();
    let (black_ok, black_out) = run(
        scaffold,
        &black,
        &[
            "--check",
            "generated/__init__.py",
            "generated/models.py",
            "generated/client.py",
            "generated/handlers.py",
            "generated/server.py",
        ],
    );
    assert!(black_ok, "black --check failed:\n{black_out}");

    // 2. `ruff check generated/` (strict ruleset via pyproject.toml).
    let ruff = venv_bin.join("ruff").to_string_lossy().into_owned();
    let (ruff_ok, ruff_out) = run(scaffold, &ruff, &["check", "generated/"]);
    assert!(ruff_ok, "ruff check failed:\n{ruff_out}");

    // 3. `mypy generated/` (strict mode via pyproject.toml).
    let mypy = venv_bin.join("mypy").to_string_lossy().into_owned();
    let (mypy_ok, mypy_out) = run(scaffold, &mypy, &["generated/"]);
    assert!(mypy_ok, "mypy failed:\n{mypy_out}");
}

#[test]
fn python_output_compiles_and_lints() {
    // Like the TypeScript target, the Python toolchain is pinned via a committed
    // project at `tests/scaffold/python/` with its own `requirements-dev.txt` and
    // a local `.venv/` (the analog of node_modules). We run the checks IN that
    // committed dir so they reuse its installed deps. Generated files go into the
    // gitignored `generated/` subdir, recreated fresh each run.
    if gate(&missing_tools(&["python3"])) {
        return;
    }

    let scaffold = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scaffold")
        .join("python");
    let venv = scaffold.join(".venv");
    let venv_bin = venv.join("bin");
    if !venv_bin.is_dir() {
        let msg = format!(
            "Python scaffold has no .venv; run `python3 -m venv .venv && \
             .venv/bin/pip install -r requirements-dev.txt` in {}",
            scaffold.display()
        );
        if e2e_required() {
            panic!("PHOENIX_GEN_E2E=1 but {msg}");
        }
        eprintln!("SKIP (set PHOENIX_GEN_E2E=1 to enforce): {msg}");
        return;
    }

    // Check the full schema, then the minimal one (no query params / errors) so
    // feature-gated imports are exercised in both their present and absent forms,
    // then the wide schema so a long constrained field is covered.
    check_python_output(&scaffold, &venv_bin, &generate_python_files(SCHEMA));
    check_python_output(&scaffold, &venv_bin, &generate_python_files(MINIMAL_SCHEMA));
    check_python_output(&scaffold, &venv_bin, &generate_python_files(WIDE_SCHEMA));
    check_python_output(&scaffold, &venv_bin, &generate_python_files(WRAP_SCHEMA));
    check_python_output(&scaffold, &venv_bin, &generate_python_files(FEATURE_SCHEMA));
    // Required aliased query param ordering: a required `Query(alias=...)` param
    // must sort after the required plain param, or the generated server is a
    // Python syntax error (non-default argument follows default argument).
    check_python_output(&scaffold, &venv_bin, &generate_python_files(EDGE_SCHEMA));
    // All-optional request + response headers (all-`| None` kwargs, guarded
    // sends, and an all-optional envelope).
    check_python_output(&scaffold, &venv_bin, &generate_python_files(HEADER_SCHEMA));

    // Realistic schema fixture library (see FILE_FIXTURES).
    for (name, schema) in FILE_FIXTURES.iter().copied() {
        eprintln!("fixture library: {name}");
        check_python_output(&scaffold, &venv_bin, &generate_python_files(schema));
    }
}
