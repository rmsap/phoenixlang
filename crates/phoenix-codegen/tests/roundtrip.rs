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

/// Small schema for the dedicated `DateTime` wire round-trip (separate from the
/// contract-driven `gen_api` round-trip above). It exercises `DateTime` in a body
/// (required / `Option` / `List` / `Map`), as a query param, and as both a
/// required and an optional response header — the positions whose cross-language
/// wire format (RFC 3339) the bespoke `datetime/*` drivers assert actually
/// round-trips. The `Map<String, DateTime>` (`phases`) covers the value-revival
/// shape (TS `Object.entries(...)` rebuild) end-to-end, not just compile-lint.
/// `Task`/`echoTask` adds a body whose only `DateTime` is reached *through* a
/// nested struct (`Reminder`), with no direct/generic `DateTime` field of its own
/// — the case that regressed when the Python client's `model_dump(mode="json")`
/// gate was non-transitive (it left a raw nested `datetime` that httpx's
/// `json.dumps` rejects). `getEvent`'s optional `expiresAt` response header guards
/// the optional response-header read paths (`time.Parse` into `&t` / `fromisoformat
/// if raw else None` / `!== null ? new Date(...) : undefined`), which the required
/// `servedAt` doesn't reach. `echoInstant`/`echoInstants`/`echoInstantMap` cover
/// BARE scalar/`List`/`Map` `DateTime` responses (not wrapped in a struct) — the
/// case the Python client decoded with the object-only `Type(**response.json())`
/// form, which crashed at runtime on a scalar (`datetime(**"…")`); now decoded
/// by type (`datetime.fromisoformat(...)` / comprehensions). Kept inline (not a
/// fixture) because only these drivers consume it. See
/// `docs/design-decisions.md` (DateTime & UUID scalar types).
const DATETIME_RT_SCHEMA: &str = r#"
struct Event {
    id: Int
    name: String
    startsAt: DateTime
    endsAt: Option<DateTime>
    checkpoints: List<DateTime>
    phases: Map<String, DateTime>
}

struct Reminder {
    note: String
    remindAt: DateTime
}

struct Task {
    id: Int
    reminder: Reminder
}

endpoint echoEvent: POST "/events" {
    body Event
    response Event
}

endpoint echoTask: POST "/tasks" {
    body Task
    response Task
}

endpoint getEvent: GET "/events/{id}" {
    query {
        since: DateTime
    }
    response Event headers {
        servedAt: DateTime
        expiresAt: Option<DateTime>
    }
}

endpoint echoInstant: GET "/instant" {
    query {
        at: DateTime
    }
    response DateTime
}

endpoint echoInstants: GET "/instants" {
    query {
        at: DateTime
    }
    response List<DateTime>
}

endpoint echoInstantMap: GET "/instant-map" {
    query {
        at: DateTime
    }
    response Map<String, DateTime>
}
"#;

/// Absolute path to `tests/roundtrip/datetime/` (the dedicated DateTime drivers).
fn datetime_dir() -> PathBuf {
    roundtrip_dir().join("datetime")
}

/// Small schema for the dedicated `Uuid` wire round-trip. Exercises `Uuid` in a
/// body (required / `Option` / `List` / `Map`), as a query param, a required
/// response header, and a BARE scalar response — the positions whose RFC 4122
/// wire format the bespoke `uuid/*` drivers assert round-trips. The validating
/// decode paths (Python `UUID(...)`, TS `parseUuid`, Go `Validate()`'s `uuidRe`)
/// accept the valid values sent here. See `docs/design-decisions.md`.
const UUID_RT_SCHEMA: &str = r#"
struct Account {
    id: Uuid
    ownerId: Option<Uuid>
    members: List<Uuid>
    index: Map<String, Uuid>
}

endpoint echoAccount: POST "/accounts" {
    body Account
    response Account
}

endpoint getAccount: GET "/accounts/{id}" {
    query {
        ref: Uuid
    }
    response Account headers {
        requestId: Uuid
    }
}

endpoint newId: GET "/id" {
    response Uuid
}
"#;

/// Absolute path to `tests/roundtrip/uuid/` (the dedicated Uuid drivers).
fn uuid_dir() -> PathBuf {
    roundtrip_dir().join("uuid")
}

/// Small schema for the dedicated `Decimal` wire round-trip. Exercises `Decimal`
/// in a body (required / `Option` / `List` / `Map`), as a query param, a required
/// response header, and a BARE scalar response. The validating decode paths
/// (Python `Decimal(...)`, TS `parseDecimal`, Go `Validate()`'s `decimalRe`)
/// accept the valid values sent here; the drivers also assert a malformed body
/// decimal is rejected. See `docs/design-decisions.md` (Decimal scalar type).
const DECIMAL_RT_SCHEMA: &str = r#"
struct Invoice {
    id: Int
    subtotal: Decimal
    discount: Option<Decimal>
    lineTotals: List<Decimal>
    rates: Map<String, Decimal>
}

endpoint echoInvoice: POST "/invoices" {
    body Invoice
    response Invoice
}

endpoint getQuote: GET "/quote/{id}" {
    query {
        minAmount: Decimal
    }
    response Invoice headers {
        computedTax: Decimal
    }
}

endpoint exchangeRate: GET "/rate" {
    response Decimal
}
"#;

/// Absolute path to `tests/roundtrip/decimal/` (the dedicated Decimal drivers).
fn decimal_dir() -> PathBuf {
    roundtrip_dir().join("decimal")
}

/// Small schema for the dedicated `Money` wire round-trip. Exercises the composite
/// `Money` in a body (required / `Option` / nested in a `List` element / as a
/// direct `List<Money>` element / as a `Map<String, Money>` value) and as a bare
/// response. The validating decode paths (Python pydantic model + currency
/// validator, TS `reviveMoney`, Go `Invoice.Validate()` recursing into
/// `Money.Validate()`) accept valid values; the drivers also assert a bad amount
/// and an unknown currency are rejected. `Money` is composite, so it never appears
/// in query/header position. See `docs/design-decisions.md` (Money type).
const MONEY_RT_SCHEMA: &str = r#"
struct LineItem {
    label: String
    price: Money
}

struct Invoice {
    id: Int
    total: Money
    tip: Option<Money>
    items: List<LineItem>
    charges: List<Money>
    byCategory: Map<String, Money>
}

endpoint echoInvoice: POST "/invoices" {
    body Invoice
    response Invoice
}

endpoint getBalance: GET "/balance" {
    response Money
}
"#;

/// Absolute path to `tests/roundtrip/money/` (the dedicated Money drivers).
fn money_dir() -> PathBuf {
    roundtrip_dir().join("money")
}

/// Small schema for the dedicated enum query/header round-trip. `pickItem`
/// exercises a required enum query param (`color`), a defaulted enum query param
/// (`size = Medium`), a required enum request header (`preferred`) and an
/// `Option<enum>` header (`fallback`), plus a required (`chosen`) and
/// `Option<enum>` (`alt`) response header — the handler echoes the inputs so each
/// path can be asserted to survive the wire (the bare variant string), AND so the
/// drivers can drive an UNKNOWN variant through the query/header to prove the
/// server rejects it (TS `parse<Enum>`→400, Go `Valid()`→400, Python FastAPI→422).
/// Kept inline (not a fixture) because only these drivers consume it. See
/// `docs/design-decisions.md` (enum query/header params).
const ENUM_RT_SCHEMA: &str = r#"
enum Color { Red  Green  Blue }
enum Size { Small  Medium  Large }

struct Item {
    name: String
    color: Color
    size: Size
}

endpoint pickItem: GET "/pick" {
    headers {
        preferred: Color as "X-Preferred"
        fallback: Option<Color> as "X-Fallback"
    }
    query {
        color: Color
        size: Size = Medium
    }
    response Item headers {
        chosen: Color as "X-Chosen"
        alt: Option<Color> as "X-Alt"
    }
}
"#;

/// Absolute path to `tests/roundtrip/enum/` (the dedicated enum drivers).
fn enum_dir() -> PathBuf {
    roundtrip_dir().join("enum")
}

/// Small schema for the dedicated inline-response-projection round-trip.
/// `getProfile` returns a bare projected struct (`response User pick { … }`),
/// `listProfiles` a `List<User pick { … }>`, `getSummary` a `partial`
/// projection (`pick … partial` — every projected field optional), and
/// `getContact` an `omit` projection (the complementary selector — proving the
/// `omit` field set round-trips the wire, not just `pick`). Each projection
/// includes a `Uuid` and a `DateTime` field so the drivers can assert the generated
/// `<Endpoint>Response` (and, for the list, each element) round-trips the wire AND
/// that the TS client's revival of the GENERATED projected struct turns `createdAt`
/// back into a real `Date` (the runtime behavior compile-lint can't prove) — the
/// `partial` case additionally exercises the reviver's OPTIONAL-field wrapping path
/// (the projected `createdAt` becomes `Option<DateTime>`, still revived when
/// present). Kept inline (not a fixture) because only these drivers consume it. See
/// `docs/design-decisions.md` (inline response projection).
const PROJECTION_RT_SCHEMA: &str = r#"
struct User {
    id: Uuid
    displayName: String
    email: String
    passwordHash: String
    createdAt: DateTime
}

endpoint getProfile: GET "/users/{id}/profile" {
    response User pick { id, displayName, createdAt }
}

endpoint listProfiles: GET "/profiles" {
    response List<User pick { id, displayName, createdAt }>
}

endpoint getSummary: GET "/users/{id}/summary" {
    response User pick { id, displayName, createdAt } partial
}

endpoint getContact: GET "/users/{id}/contact" {
    response User omit { passwordHash }
}
"#;

/// Absolute path to `tests/roundtrip/projection/` (the dedicated projection drivers).
fn projection_dir() -> PathBuf {
    roundtrip_dir().join("projection")
}

/// Small schema for the dedicated list-valued-param round-trip. `search` takes
/// query params and request headers each covering EVERY permitted element type —
/// `String`/`Int`/`Uuid`/`Status` (a simple enum)/`Float`/`Bool`/`DateTime`/
/// `Decimal` — and echoes all of them into the response so the drivers can assert
/// multiple values, and the empty list, survive the wire in both directions. The
/// query `List<Uuid>` exercises a branded/format-checked element coercion and
/// `List<Status>` the per-element enum validation (whose unknown variant the
/// drivers also drive through the reject path). Both positions carry every element
/// type because the query and header paths DIVERGE per target: Go/TS share their
/// encode/coerce helpers across positions, but Python's query path uses FastAPI's
/// native `list[T]` parsing plus the `py_list_query_value` client encoders, whereas
/// its header path coerces each element manually in the route body
/// (`int(...)`/`float(...)`/`UUID(...)`/`Decimal(...)`/`datetime.fromisoformat(...)`/
/// `Status(...)`/`== "true"`) — so each path needs its own typed element per type
/// or those branches go untested. Kept inline (not a fixture); only these drivers
/// consume it. See `docs/design-decisions.md` (list-valued query/header params).
const LIST_RT_SCHEMA: &str = r#"
enum Status { Active  Inactive  Pending }

struct Echo {
    ids: List<String>
    counts: List<Int>
    uuids: List<Uuid>
    statuses: List<Status>
    qFloats: List<Float>
    qFlags: List<Bool>
    qTimes: List<DateTime>
    qAmounts: List<Decimal>
    roles: List<String>
    limits: List<Int>
    keys: List<Uuid>
    ratios: List<Float>
    flags: List<Bool>
    times: List<DateTime>
    amounts: List<Decimal>
    tags: List<Status>
}

endpoint search: GET "/search" {
    headers {
        roles: List<String> as "X-Role"
        limits: List<Int> as "X-Limit"
        keys: List<Uuid> as "X-Key"
        ratios: List<Float> as "X-Ratio"
        flags: List<Bool> as "X-Flag"
        times: List<DateTime> as "X-Time"
        amounts: List<Decimal> as "X-Amount"
        tags: List<Status> as "X-Tag"
    }
    query {
        ids: List<String>
        counts: List<Int>
        uuids: List<Uuid>
        statuses: List<Status>
        qFloats: List<Float>
        qFlags: List<Bool>
        qTimes: List<DateTime>
        qAmounts: List<Decimal>
    }
    response Echo
}
"#;

/// Absolute path to `tests/roundtrip/list/` (the dedicated list-param drivers).
fn list_dir() -> PathBuf {
    roundtrip_dir().join("list")
}

/// Drives the `Url` (branded validated string, exact pass-through wire) and
/// `Bytes` (first-class binary, base64 wire) scalars end to end. The body
/// `Payload` carries `Url` and `Bytes` each as a required field, an `Option`
/// (present and absent across the two calls), and a `List`, plus a
/// `Map<String, Bytes>` — so the drivers assert the base64-to-binary
/// encode/decode (TS `encodeBytes`/`bytesFromBase64`, Go `[]byte` auto-base64,
/// pydantic `Bytes` alias) preserves NON-UTF-8 bytes through every combinator
/// (the `Map` exercises the TS `encodeBytes` deep-walk over a `Record` and the
/// `Object.fromEntries` revival, Go `map[string][]byte`, and the dict-valued
/// pydantic alias), and that a `Url` survives byte-for-byte (validated, never
/// normalized, so query string, fragment, and scheme case are all preserved). The
/// `Url` query, the `List<Url>` query, and the `Url` header exercise the
/// server-side `parseUrl`/`urlRe`/`BeforeValidator` validation, echoed into the
/// response `Echo` so the round-trip is observable. A malformed `Url` query value
/// drives the reject path (TS/Go 400, Py 422). The `replace` MULTI-STATUS endpoint
/// (`response { 200: Payload … }`) round-trips a `Bytes`-bearing shared body
/// through the response envelope — behaviorally exercising the `encodeBytes` wrap
/// on the server's `result.body` branch and the client's revival of the envelope
/// body (compile-lint alone cannot catch a missing wrap/revival there). Kept inline
/// (not a fixture); only these drivers consume it. See `docs/design-decisions.md`
/// (URL & bytes types).
const URL_BYTES_RT_SCHEMA: &str = r#"
struct Payload {
    source: Url
    mirror: Option<Url>
    thumbnails: List<Url>
    checksum: Bytes
    signature: Option<Bytes>
    chunks: List<Bytes>
    tags: Map<String, Bytes>
}

struct Echo {
    source: Url
    mirror: Option<Url>
    thumbnails: List<Url>
    checksum: Bytes
    signature: Option<Bytes>
    chunks: List<Bytes>
    tags: Map<String, Bytes>
    origin: Url
    mirrors: List<Url>
    referer: Url
}

endpoint upload: POST "/assets" {
    headers { referer: Url as "X-Referer" }
    query { origin: Url  mirrors: List<Url> }
    body Payload
    response Echo
}

endpoint replace: PUT "/assets/{id}" {
    body Payload
    response { 200: Payload  201: Payload  204 }
}
"#;

/// Absolute path to `tests/roundtrip/url_bytes/` (the dedicated Url/Bytes drivers).
fn url_bytes_dir() -> PathBuf {
    roundtrip_dir().join("url_bytes")
}

/// The cross-LANGUAGE wire-conformance schema. Unlike every other round-trip —
/// which pairs a target's generated client with its OWN server (same-language) —
/// this one checks each target against a single committed golden wire
/// (`cross_lang/wire.json`). Each target's driver constructs the matching typed
/// values, drives its generated client against its generated server, captures the
/// actual HTTP bytes at the client transport (Go `RoundTripper`, Python httpx event
/// hooks, TS `fetch` wrapper), and asserts the request/response wire equals the
/// golden. Conformance of all three to ONE wire ⟹ any client interoperates with any
/// server (transitivity), so no cross-process pairing is needed. The schema
/// concentrates the cross-target divergence surfaces in one shot: camelCase field
/// names + a nested struct, enum *values* (`Role`), the scalar zoo
/// (`Uuid`/`DateTime`/`Decimal`/`Url`/`Bytes`/`Money`), an `Option` (absent → null),
/// `List<String>`, a multi-word path param (`{accountId}`), a camelCase query param,
/// a `List<enum>` repeated-key query, an aliased request header, and the pagination
/// envelope (`totalCount`). See `docs/design-decisions.md` (Python camelCase wire).
const CROSS_LANG_SCHEMA: &str = r#"
enum Role { admin  guest }

struct Profile {
    displayName: String
    // INVARIANT: `avatarUrl` must remain the ONLY null-valued field in the golden
    // wire (it is null in `cross_lang/wire.json`). The comparator's absent≡null rule
    // means a *renamed* null field reads as null==null and slips through; every other
    // field is non-null so a snake_case rename is caught. Adding a second nullable
    // field widens that blind spot. See docs/design-decisions.md (cross-language wire).
    avatarUrl: Option<String>
}

struct Account {
    id: Uuid
    createdAt: DateTime
    balance: Decimal
    homepage: Url
    avatar: Bytes
    wallet: Money
    role: Role
    profile: Profile
    tags: List<String>
    active: Bool
}

endpoint createAccount: POST "/accounts" {
    body Account
    response Account
    // `BadRequest`, not `ValidationError`: the latter collides with a generated
    // helper type in some targets (a documented LOUD residual in known-issues.md),
    // and a permanent fixture should not sit on that edge — the error path is never
    // exercised here anyway, so the name only matters at codegen/compile time.
    error { BadRequest(400) }
}

endpoint getAccount: GET "/accounts/{accountId}" {
    headers { requestId: String as "X-Request-Id" }
    query { includeArchived: Bool  roles: List<Role> }
    response Account
    error { NotFound(404) }
}

endpoint listAccounts: GET "/accounts" {
    query { page: Int = 1 }
    response List<Account>
    pagination { offset }
    error { NotFound(404) }
}
"#;

/// Absolute path to `tests/roundtrip/cross_lang/` (the golden wire + Go driver).
fn cross_lang_dir() -> PathBuf {
    roundtrip_dir().join("cross_lang")
}

// ── Toolchain gating + subprocess runner live in `common` (shared with
//    compiles_and_lints.rs), as does the schema → AST + analysis pipeline. ──

/// Absolute path to `tests/roundtrip/` (where the committed drivers live).
fn roundtrip_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("roundtrip")
}

/// Gates a TypeScript round-trip on its committed npm project being installed.
/// Returns `true` if `<driver_dir>/node_modules` exists. Otherwise SKIPs (returns
/// `false`) with an install hint — UNLESS `PHOENIX_GEN_E2E=1`, where a missing
/// dependency tree is a hard failure so CI cannot silently skip. The caller is
/// responsible for `return`ing on `false`.
fn require_node_modules(driver_dir: &Path) -> bool {
    if driver_dir.join("node_modules").is_dir() {
        return true;
    }
    let msg = format!(
        "TypeScript round-trip driver has no node_modules; run `npm ci` in {}",
        driver_dir.display()
    );
    if e2e_required() {
        panic!("PHOENIX_GEN_E2E=1 but {msg}");
    }
    eprintln!("SKIP (set PHOENIX_GEN_E2E=1 to enforce): {msg}");
    false
}

/// Gates a Python round-trip on its committed `.venv` being present, returning the
/// path to its `python` interpreter when it is. Otherwise SKIPs (returns `None`)
/// with an install hint — UNLESS `PHOENIX_GEN_E2E=1`, where a missing `.venv` is a
/// hard failure so CI cannot silently skip. The caller is responsible for
/// `return`ing on `None`.
fn require_venv(driver_dir: &Path) -> Option<PathBuf> {
    let venv_python = driver_dir.join(".venv").join("bin").join("python");
    if venv_python.is_file() {
        return Some(venv_python);
    }
    let msg = format!(
        "Python round-trip driver has no .venv; run `python3 -m venv .venv && \
         .venv/bin/pip install -r requirements.txt` in {}",
        driver_dir.display()
    );
    if e2e_required() {
        panic!("PHOENIX_GEN_E2E=1 but {msg}");
    }
    eprintln!("SKIP (set PHOENIX_GEN_E2E=1 to enforce): {msg}");
    None
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

/// Dedicated `DateTime` wire round-trip for Go: generates the `api` package from
/// [`DATETIME_RT_SCHEMA`], assembles a tempdir module with the bespoke
/// `datetime/go` driver (which reuses the main driver's `go.mod.template`), and
/// runs `go test`. Asserts body/query/response-header `DateTime`s survive the
/// RFC 3339 wire trip in both directions.
#[test]
fn datetime_go_roundtrip() {
    if gate(&missing_tools(&["go"])) {
        return;
    }

    let files = generate_go_files(DATETIME_RT_SCHEMA);

    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    let api_dir = root.join("api");
    std::fs::create_dir(&api_dir).expect("create api dir");
    std::fs::write(api_dir.join("types.go"), &files.types).expect("write types.go");
    std::fs::write(api_dir.join("client.go"), &files.client).expect("write client.go");
    std::fs::write(api_dir.join("handlers.go"), &files.handlers).expect("write handlers.go");
    std::fs::write(api_dir.join("server.go"), &files.server).expect("write server.go");

    // The module template is shared with the main Go driver (module `roundtrip`).
    let go_mod = std::fs::read_to_string(roundtrip_dir().join("go").join("go.mod.template"))
        .expect("read go.mod.template");
    std::fs::write(root.join("go.mod"), go_mod).expect("write go.mod");

    let driver = datetime_dir().join("go").join("datetime_roundtrip_test.go");
    let contents = std::fs::read_to_string(&driver)
        .unwrap_or_else(|e| panic!("read {}: {e}", driver.display()));
    std::fs::write(root.join("datetime_roundtrip_test.go"), contents).expect("write driver");

    let (ok, out) = run(root, "go", &["test", "./..."]);
    assert!(ok, "go DateTime round-trip test failed:\n{out}");
}

/// Dedicated `Uuid` wire round-trip for Go: generates the `api` package from
/// [`UUID_RT_SCHEMA`], assembles a tempdir module with the bespoke `uuid/go`
/// driver, and runs `go test`. Asserts body/query/response-header `Uuid` strings
/// survive the wire and that the generated `Validate()` accepts valid input.
#[test]
fn uuid_go_roundtrip() {
    if gate(&missing_tools(&["go"])) {
        return;
    }

    let files = generate_go_files(UUID_RT_SCHEMA);

    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    let api_dir = root.join("api");
    std::fs::create_dir(&api_dir).expect("create api dir");
    std::fs::write(api_dir.join("types.go"), &files.types).expect("write types.go");
    std::fs::write(api_dir.join("client.go"), &files.client).expect("write client.go");
    std::fs::write(api_dir.join("handlers.go"), &files.handlers).expect("write handlers.go");
    std::fs::write(api_dir.join("server.go"), &files.server).expect("write server.go");

    let go_mod = std::fs::read_to_string(roundtrip_dir().join("go").join("go.mod.template"))
        .expect("read go.mod.template");
    std::fs::write(root.join("go.mod"), go_mod).expect("write go.mod");

    let driver = uuid_dir().join("go").join("uuid_roundtrip_test.go");
    let contents = std::fs::read_to_string(&driver)
        .unwrap_or_else(|e| panic!("read {}: {e}", driver.display()));
    std::fs::write(root.join("uuid_roundtrip_test.go"), contents).expect("write driver");

    let (ok, out) = run(root, "go", &["test", "./..."]);
    assert!(ok, "go UUID round-trip test failed:\n{out}");
}

/// Dedicated `Decimal` wire round-trip for Go: generates the `api` package from
/// [`DECIMAL_RT_SCHEMA`], assembles a tempdir module with the bespoke
/// `decimal/go` driver, and runs `go test`. Asserts body/query/response-header
/// `Decimal` strings survive the wire and that `Validate()` accepts valid input
/// and rejects a malformed body decimal.
#[test]
fn decimal_go_roundtrip() {
    if gate(&missing_tools(&["go"])) {
        return;
    }

    let files = generate_go_files(DECIMAL_RT_SCHEMA);

    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    let api_dir = root.join("api");
    std::fs::create_dir(&api_dir).expect("create api dir");
    std::fs::write(api_dir.join("types.go"), &files.types).expect("write types.go");
    std::fs::write(api_dir.join("client.go"), &files.client).expect("write client.go");
    std::fs::write(api_dir.join("handlers.go"), &files.handlers).expect("write handlers.go");
    std::fs::write(api_dir.join("server.go"), &files.server).expect("write server.go");

    let go_mod = std::fs::read_to_string(roundtrip_dir().join("go").join("go.mod.template"))
        .expect("read go.mod.template");
    std::fs::write(root.join("go.mod"), go_mod).expect("write go.mod");

    let driver = decimal_dir().join("go").join("decimal_roundtrip_test.go");
    let contents = std::fs::read_to_string(&driver)
        .unwrap_or_else(|e| panic!("read {}: {e}", driver.display()));
    std::fs::write(root.join("decimal_roundtrip_test.go"), contents).expect("write driver");

    let (ok, out) = run(root, "go", &["test", "./..."]);
    assert!(ok, "go Decimal round-trip test failed:\n{out}");
}

/// Dedicated `Money` wire round-trip for Go: generates the `api` package from
/// [`MONEY_RT_SCHEMA`], assembles a tempdir module with the bespoke `money/go`
/// driver, and runs `go test`. Asserts the composite `Money` survives the wire in
/// body/bare-response positions and that the recursive `Validate()` accepts valid
/// input and rejects a bad amount and an unknown currency.
#[test]
fn money_go_roundtrip() {
    if gate(&missing_tools(&["go"])) {
        return;
    }

    let files = generate_go_files(MONEY_RT_SCHEMA);

    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    let api_dir = root.join("api");
    std::fs::create_dir(&api_dir).expect("create api dir");
    std::fs::write(api_dir.join("types.go"), &files.types).expect("write types.go");
    std::fs::write(api_dir.join("client.go"), &files.client).expect("write client.go");
    std::fs::write(api_dir.join("handlers.go"), &files.handlers).expect("write handlers.go");
    std::fs::write(api_dir.join("server.go"), &files.server).expect("write server.go");

    let go_mod = std::fs::read_to_string(roundtrip_dir().join("go").join("go.mod.template"))
        .expect("read go.mod.template");
    std::fs::write(root.join("go.mod"), go_mod).expect("write go.mod");

    let driver = money_dir().join("go").join("money_roundtrip_test.go");
    let contents = std::fs::read_to_string(&driver)
        .unwrap_or_else(|e| panic!("read {}: {e}", driver.display()));
    std::fs::write(root.join("money_roundtrip_test.go"), contents).expect("write driver");

    let (ok, out) = run(root, "go", &["test", "./..."]);
    assert!(ok, "go Money round-trip test failed:\n{out}");
}

/// Dedicated enum query/header wire round-trip for Go: generates the `api` package
/// from [`ENUM_RT_SCHEMA`], assembles a tempdir module with the bespoke `enum/go`
/// driver, and runs `go test`. Asserts enum query/header values survive the wire
/// as the bare variant string and that the server's generated `Valid()` check
/// rejects an unknown query/header variant (a Go `Color` is a plain string, so the
/// driver can hand the client an out-of-range value to drive the reject path).
#[test]
fn enum_go_roundtrip() {
    if gate(&missing_tools(&["go"])) {
        return;
    }

    let files = generate_go_files(ENUM_RT_SCHEMA);

    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    let api_dir = root.join("api");
    std::fs::create_dir(&api_dir).expect("create api dir");
    std::fs::write(api_dir.join("types.go"), &files.types).expect("write types.go");
    std::fs::write(api_dir.join("client.go"), &files.client).expect("write client.go");
    std::fs::write(api_dir.join("handlers.go"), &files.handlers).expect("write handlers.go");
    std::fs::write(api_dir.join("server.go"), &files.server).expect("write server.go");

    let go_mod = std::fs::read_to_string(roundtrip_dir().join("go").join("go.mod.template"))
        .expect("read go.mod.template");
    std::fs::write(root.join("go.mod"), go_mod).expect("write go.mod");

    let driver = enum_dir().join("go").join("enum_roundtrip_test.go");
    let contents = std::fs::read_to_string(&driver)
        .unwrap_or_else(|e| panic!("read {}: {e}", driver.display()));
    std::fs::write(root.join("enum_roundtrip_test.go"), contents).expect("write driver");

    let (ok, out) = run(root, "go", &["test", "./..."]);
    assert!(ok, "go enum round-trip test failed:\n{out}");
}

/// Dedicated inline-response-projection wire round-trip for Go: generates the
/// `api` package from [`PROJECTION_RT_SCHEMA`], assembles a tempdir module with the
/// bespoke `projection/go` driver, and runs `go test`. Asserts a bare projected
/// `<Endpoint>Response` and a `List<…>` of them round-trip the wire (incl. the
/// `Uuid`/`DateTime` projected fields).
#[test]
fn projection_go_roundtrip() {
    if gate(&missing_tools(&["go"])) {
        return;
    }

    let files = generate_go_files(PROJECTION_RT_SCHEMA);

    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    let api_dir = root.join("api");
    std::fs::create_dir(&api_dir).expect("create api dir");
    std::fs::write(api_dir.join("types.go"), &files.types).expect("write types.go");
    std::fs::write(api_dir.join("client.go"), &files.client).expect("write client.go");
    std::fs::write(api_dir.join("handlers.go"), &files.handlers).expect("write handlers.go");
    std::fs::write(api_dir.join("server.go"), &files.server).expect("write server.go");

    let go_mod = std::fs::read_to_string(roundtrip_dir().join("go").join("go.mod.template"))
        .expect("read go.mod.template");
    std::fs::write(root.join("go.mod"), go_mod).expect("write go.mod");

    let driver = projection_dir()
        .join("go")
        .join("projection_roundtrip_test.go");
    let contents = std::fs::read_to_string(&driver)
        .unwrap_or_else(|e| panic!("read {}: {e}", driver.display()));
    std::fs::write(root.join("projection_roundtrip_test.go"), contents).expect("write driver");

    let (ok, out) = run(root, "go", &["test", "./..."]);
    assert!(ok, "go projection round-trip test failed:\n{out}");
}

/// Dedicated list-valued-param wire round-trip for Go: generates the `api` package
/// from [`LIST_RT_SCHEMA`], assembles a tempdir module with the bespoke `list/go`
/// driver, and runs `go test`. Asserts query params (repeated keys) and request
/// headers (comma-separated), both covering every element type, round-trip
/// including the empty list, plus the per-element reject path (unknown enum /
/// malformed `Uuid` query element → 400).
#[test]
fn list_go_roundtrip() {
    if gate(&missing_tools(&["go"])) {
        return;
    }

    let files = generate_go_files(LIST_RT_SCHEMA);

    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    let api_dir = root.join("api");
    std::fs::create_dir(&api_dir).expect("create api dir");
    std::fs::write(api_dir.join("types.go"), &files.types).expect("write types.go");
    std::fs::write(api_dir.join("client.go"), &files.client).expect("write client.go");
    std::fs::write(api_dir.join("handlers.go"), &files.handlers).expect("write handlers.go");
    std::fs::write(api_dir.join("server.go"), &files.server).expect("write server.go");

    let go_mod = std::fs::read_to_string(roundtrip_dir().join("go").join("go.mod.template"))
        .expect("read go.mod.template");
    std::fs::write(root.join("go.mod"), go_mod).expect("write go.mod");

    let driver = list_dir().join("go").join("list_roundtrip_test.go");
    let contents = std::fs::read_to_string(&driver)
        .unwrap_or_else(|e| panic!("read {}: {e}", driver.display()));
    std::fs::write(root.join("list_roundtrip_test.go"), contents).expect("write driver");

    let (ok, out) = run(root, "go", &["test", "./..."]);
    assert!(ok, "go list round-trip test failed:\n{out}");
}

/// Dedicated `Url`/`Bytes` wire round-trip for Go: generates the `api` package
/// from [`URL_BYTES_RT_SCHEMA`], assembles a tempdir module with the bespoke
/// `url_bytes/go` driver, and runs `go test`. Asserts `[]byte` survives the base64
/// wire as raw binary (non-UTF-8 bytes intact) in body/`Option`/`List`/response,
/// and a `Url` round-trips byte-for-byte through body, query, `List` query, and a
/// request header — plus the malformed-`Url` query reject path (`urlRe` → 400).
#[test]
fn url_bytes_go_roundtrip() {
    if gate(&missing_tools(&["go"])) {
        return;
    }

    let files = generate_go_files(URL_BYTES_RT_SCHEMA);

    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    let api_dir = root.join("api");
    std::fs::create_dir(&api_dir).expect("create api dir");
    std::fs::write(api_dir.join("types.go"), &files.types).expect("write types.go");
    std::fs::write(api_dir.join("client.go"), &files.client).expect("write client.go");
    std::fs::write(api_dir.join("handlers.go"), &files.handlers).expect("write handlers.go");
    std::fs::write(api_dir.join("server.go"), &files.server).expect("write server.go");

    let go_mod = std::fs::read_to_string(roundtrip_dir().join("go").join("go.mod.template"))
        .expect("read go.mod.template");
    std::fs::write(root.join("go.mod"), go_mod).expect("write go.mod");

    let driver = url_bytes_dir()
        .join("go")
        .join("url_bytes_roundtrip_test.go");
    let contents = std::fs::read_to_string(&driver)
        .unwrap_or_else(|e| panic!("read {}: {e}", driver.display()));
    std::fs::write(root.join("url_bytes_roundtrip_test.go"), contents).expect("write driver");

    let (ok, out) = run(root, "go", &["test", "./..."]);
    assert!(ok, "go url/bytes round-trip test failed:\n{out}");
}

/// Cross-language wire conformance for the Go target: generates the `api` package
/// from [`CROSS_LANG_SCHEMA`], assembles a tempdir module with the `cross_lang/go`
/// driver plus the shared golden `wire.json`, and runs `go test`. The driver drives
/// the generated client against the generated server through a recording
/// `http.RoundTripper`, then asserts the captured request/response bytes equal the
/// golden — proving Go speaks the same wire every other target is checked against.
#[test]
fn cross_lang_go_conformance() {
    // Unlike the TS/Python conformance tests there is no `e2e_required()` escalation
    // here: Go has no pre-installed dependency dir (`node_modules`/`.venv`) to gate on
    // — `go test` resolves the module itself — so the only gate is toolchain presence,
    // matching every other Go round-trip in this file.
    if gate(&missing_tools(&["go"])) {
        return;
    }

    let files = generate_go_files(CROSS_LANG_SCHEMA);

    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    let api_dir = root.join("api");
    std::fs::create_dir(&api_dir).expect("create api dir");
    std::fs::write(api_dir.join("types.go"), &files.types).expect("write types.go");
    std::fs::write(api_dir.join("client.go"), &files.client).expect("write client.go");
    std::fs::write(api_dir.join("handlers.go"), &files.handlers).expect("write handlers.go");
    std::fs::write(api_dir.join("server.go"), &files.server).expect("write server.go");

    let go_mod = std::fs::read_to_string(roundtrip_dir().join("go").join("go.mod.template"))
        .expect("read go.mod.template");
    std::fs::write(root.join("go.mod"), go_mod).expect("write go.mod");

    let driver = cross_lang_dir().join("go").join("cross_lang_test.go");
    let contents = std::fs::read_to_string(&driver)
        .unwrap_or_else(|e| panic!("read {}: {e}", driver.display()));
    std::fs::write(root.join("cross_lang_test.go"), contents).expect("write driver");

    // The golden wire lives beside the module root so the driver reads `./wire.json`.
    let wire = std::fs::read_to_string(cross_lang_dir().join("wire.json"))
        .expect("read cross_lang/wire.json");
    std::fs::write(root.join("wire.json"), wire).expect("write wire.json");

    let (ok, out) = run(root, "go", &["test", "./..."]);
    assert!(ok, "go cross-language wire conformance failed:\n{out}");
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
    if !require_node_modules(&driver_dir) {
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

/// Dedicated `DateTime` wire round-trip for TypeScript (Express). Reuses the main
/// TS driver's committed npm project (`node_modules` with express + tsx) but
/// writes the generated files into a SEPARATE `generated-datetime/` dir and runs
/// the bespoke `datetime-driver.ts`, so it never races `typescript_roundtrip`'s
/// `generated/`. Proves the generated `Date` revival pass and `.toISOString()`
/// (de)serialization round-trip RFC 3339 in both directions.
///
/// Express only: the body-revival path lives in `emit_route_prelude`, which is
/// shared verbatim by both dialects (only the request accessor vocabulary
/// differs), so the Fastify `DateTime` server output is deliberately covered by
/// compile-lint (`compiles_and_lints.rs` generates BOTH frameworks) rather than a
/// second behavioral round-trip here.
#[test]
fn datetime_typescript_roundtrip() {
    if gate(&missing_tools(&["node", "npm", "npx"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("typescript");
    if !require_node_modules(&driver_dir) {
        return;
    }

    let (program, result) = parse_and_check(DATETIME_RT_SCHEMA);
    let files = phoenix_codegen::generate_typescript(&program, &result);

    let generated = driver_dir.join("generated-datetime");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated-datetime dir");
    std::fs::write(generated.join("types.ts"), &files.types).expect("write types.ts");
    std::fs::write(generated.join("client.ts"), &files.client).expect("write client.ts");
    std::fs::write(generated.join("handlers.ts"), &files.handlers).expect("write handlers.ts");
    std::fs::write(generated.join("server.ts"), &files.server).expect("write server.ts");

    let (ok, out) = run(&driver_dir, "npx", &["tsx", "datetime-driver.ts"]);
    assert!(ok, "typescript DateTime round-trip test failed:\n{out}");
}

/// Dedicated `Uuid` wire round-trip for TypeScript (Express). Reuses the main TS
/// driver's npm project but writes to a SEPARATE `generated-uuid/` dir and runs
/// `uuid-driver.ts`. Proves the branded `Uuid` alias and the `parseUuid`
/// validate-on-decode pass round-trip RFC 4122 strings (body / query / header /
/// bare response), and that the server body reviver accepts valid input.
#[test]
fn uuid_typescript_roundtrip() {
    if gate(&missing_tools(&["node", "npm", "npx"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("typescript");
    if !require_node_modules(&driver_dir) {
        return;
    }

    let (program, result) = parse_and_check(UUID_RT_SCHEMA);
    let files = phoenix_codegen::generate_typescript(&program, &result);

    let generated = driver_dir.join("generated-uuid");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated-uuid dir");
    std::fs::write(generated.join("types.ts"), &files.types).expect("write types.ts");
    std::fs::write(generated.join("client.ts"), &files.client).expect("write client.ts");
    std::fs::write(generated.join("handlers.ts"), &files.handlers).expect("write handlers.ts");
    std::fs::write(generated.join("server.ts"), &files.server).expect("write server.ts");

    let (ok, out) = run(&driver_dir, "npx", &["tsx", "uuid-driver.ts"]);
    assert!(ok, "typescript UUID round-trip test failed:\n{out}");
}

/// Dedicated `Decimal` wire round-trip for TypeScript (Express). Reuses the main
/// TS driver's npm project but writes to a SEPARATE `generated-decimal/` dir and
/// runs `decimal-driver.ts`. Proves the branded `Decimal` alias and the
/// `parseDecimal` validate-on-decode pass round-trip exact decimal strings
/// (body / query / header / bare response), and that the server validates both
/// body and query decimals (rejecting malformed input).
#[test]
fn decimal_typescript_roundtrip() {
    if gate(&missing_tools(&["node", "npm", "npx"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("typescript");
    if !require_node_modules(&driver_dir) {
        return;
    }

    let (program, result) = parse_and_check(DECIMAL_RT_SCHEMA);
    let files = phoenix_codegen::generate_typescript(&program, &result);

    let generated = driver_dir.join("generated-decimal");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated-decimal dir");
    std::fs::write(generated.join("types.ts"), &files.types).expect("write types.ts");
    std::fs::write(generated.join("client.ts"), &files.client).expect("write client.ts");
    std::fs::write(generated.join("handlers.ts"), &files.handlers).expect("write handlers.ts");
    std::fs::write(generated.join("server.ts"), &files.server).expect("write server.ts");

    let (ok, out) = run(&driver_dir, "npx", &["tsx", "decimal-driver.ts"]);
    assert!(ok, "typescript Decimal round-trip test failed:\n{out}");
}

/// Dedicated `Money` wire round-trip for TypeScript (Express). Reuses the main TS
/// driver's npm project but writes to a SEPARATE `generated-money/` dir and runs
/// `money-driver.ts`. Proves the composite `Money` interface + `reviveMoney`
/// round-trip a `Money` (body / nested list element / bare response) and that the
/// server body reviver rejects a bad amount and an unknown currency.
#[test]
fn money_typescript_roundtrip() {
    if gate(&missing_tools(&["node", "npm", "npx"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("typescript");
    if !require_node_modules(&driver_dir) {
        return;
    }

    let (program, result) = parse_and_check(MONEY_RT_SCHEMA);
    let files = phoenix_codegen::generate_typescript(&program, &result);

    let generated = driver_dir.join("generated-money");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated-money dir");
    std::fs::write(generated.join("types.ts"), &files.types).expect("write types.ts");
    std::fs::write(generated.join("client.ts"), &files.client).expect("write client.ts");
    std::fs::write(generated.join("handlers.ts"), &files.handlers).expect("write handlers.ts");
    std::fs::write(generated.join("server.ts"), &files.server).expect("write server.ts");

    let (ok, out) = run(&driver_dir, "npx", &["tsx", "money-driver.ts"]);
    assert!(ok, "typescript Money round-trip test failed:\n{out}");
}

/// Dedicated enum query/header wire round-trip for TypeScript (Express). Reuses
/// the main TS driver's npm project but writes to a SEPARATE `generated-enum/` dir
/// and runs `enum-driver.ts`. Proves enum query/header values round-trip as the
/// bare variant string (required + defaulted query, required + Option request and
/// response headers) and that the server's `parse<Enum>` rejects an unknown
/// query/header variant (→ ValidationError → 400 → client throws).
#[test]
fn enum_typescript_roundtrip() {
    if gate(&missing_tools(&["node", "npm", "npx"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("typescript");
    if !require_node_modules(&driver_dir) {
        return;
    }

    let (program, result) = parse_and_check(ENUM_RT_SCHEMA);
    let files = phoenix_codegen::generate_typescript(&program, &result);

    let generated = driver_dir.join("generated-enum");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated-enum dir");
    std::fs::write(generated.join("types.ts"), &files.types).expect("write types.ts");
    std::fs::write(generated.join("client.ts"), &files.client).expect("write client.ts");
    std::fs::write(generated.join("handlers.ts"), &files.handlers).expect("write handlers.ts");
    std::fs::write(generated.join("server.ts"), &files.server).expect("write server.ts");

    let (ok, out) = run(&driver_dir, "npx", &["tsx", "enum-driver.ts"]);
    assert!(ok, "typescript enum round-trip test failed:\n{out}");
}

/// Dedicated inline-response-projection wire round-trip for TypeScript (Express).
/// Reuses the main TS driver's npm project but writes to a SEPARATE
/// `generated-projection/` dir and runs `projection-driver.ts`. Proves a bare
/// projected `<Endpoint>Response` and a `List<…>` of them round-trip, and that the
/// client's revival of the GENERATED projected struct turns `createdAt` back into a
/// `Date` (the runtime behavior compile-lint can't assert).
#[test]
fn projection_typescript_roundtrip() {
    if gate(&missing_tools(&["node", "npm", "npx"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("typescript");
    if !require_node_modules(&driver_dir) {
        return;
    }

    let (program, result) = parse_and_check(PROJECTION_RT_SCHEMA);
    let files = phoenix_codegen::generate_typescript(&program, &result);

    let generated = driver_dir.join("generated-projection");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated-projection dir");
    std::fs::write(generated.join("types.ts"), &files.types).expect("write types.ts");
    std::fs::write(generated.join("client.ts"), &files.client).expect("write client.ts");
    std::fs::write(generated.join("handlers.ts"), &files.handlers).expect("write handlers.ts");
    std::fs::write(generated.join("server.ts"), &files.server).expect("write server.ts");

    let (ok, out) = run(&driver_dir, "npx", &["tsx", "projection-driver.ts"]);
    assert!(ok, "typescript projection round-trip test failed:\n{out}");
}

/// Dedicated list-valued-param wire round-trip for TypeScript, run against BOTH
/// server frameworks. Reuses the main TS driver's npm project but writes to a
/// SEPARATE `generated-list/` dir. Generates the Express server, runs
/// `list-driver.ts`, then overwrites just `server.ts` with the Fastify variant
/// (types/client/handlers are framework-independent — asserted by
/// `generate_typescript_express_and_fastify`) and runs `list-driver-fastify.ts`
/// against the SAME dir (so both must stay in this one test — a separate `#[test]`
/// would race the shared dir). The Fastify pass is not redundant: list query params
/// arrive as a repeated-key array via a framework-specific query parser, so both
/// Express and Fastify must be driven to prove the array shape `toStringArray`
/// normalizes actually arrives. Proves query params (repeated keys via
/// `toStringArray`) and headers (comma-split), covering every element type, round-trip
/// in both directions including the empty list.
#[test]
fn list_typescript_roundtrip() {
    if gate(&missing_tools(&["node", "npm", "npx"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("typescript");
    if !require_node_modules(&driver_dir) {
        return;
    }

    let (express, fastify) = generate_typescript_express_and_fastify(LIST_RT_SCHEMA);

    let generated = driver_dir.join("generated-list");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated-list dir");
    std::fs::write(generated.join("types.ts"), &express.types).expect("write types.ts");
    std::fs::write(generated.join("client.ts"), &express.client).expect("write client.ts");
    std::fs::write(generated.join("handlers.ts"), &express.handlers).expect("write handlers.ts");
    std::fs::write(generated.join("server.ts"), &express.server).expect("write server.ts");

    let (ok, out) = run(&driver_dir, "npx", &["tsx", "list-driver.ts"]);
    assert!(
        ok,
        "typescript list round-trip test failed (express):\n{out}"
    );

    // Fastify server variant: only `server.ts` differs (asserted framework-independent
    // above), so overwrite just the server into the SAME `generated-list/` and drive
    // the Fastify plugin via `list-driver-fastify.ts`.
    std::fs::write(generated.join("server.ts"), &fastify.server).expect("write server.ts");

    let (ok, out) = run(&driver_dir, "npx", &["tsx", "list-driver-fastify.ts"]);
    assert!(
        ok,
        "typescript list round-trip test failed (fastify):\n{out}"
    );
}

/// Dedicated `Url`/`Bytes` wire round-trip for the TypeScript (Express) target:
/// generates the Express server from [`URL_BYTES_RT_SCHEMA`] into
/// `generated-url-bytes/` and runs `url-bytes-driver.ts` via tsx. Asserts a `Bytes`
/// field comes back as a `Uint8Array` with identical raw bytes (proving
/// `encodeBytes` on send + `bytesFromBase64` revival, NOT a UTF-8 string), across
/// body/`Option`/`List`/response, and that a `Url` round-trips byte-for-byte
/// through body, query, `List` query, and a request header — plus the
/// malformed-`Url` query reject path (`parseUrl` → 400).
#[test]
fn url_bytes_typescript_roundtrip() {
    if gate(&missing_tools(&["node", "npm", "npx"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("typescript");
    if !require_node_modules(&driver_dir) {
        return;
    }

    let (program, result) = parse_and_check(URL_BYTES_RT_SCHEMA);
    let files = phoenix_codegen::generate_typescript(&program, &result);

    let generated = driver_dir.join("generated-url-bytes");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated-url-bytes dir");
    std::fs::write(generated.join("types.ts"), &files.types).expect("write types.ts");
    std::fs::write(generated.join("client.ts"), &files.client).expect("write client.ts");
    std::fs::write(generated.join("handlers.ts"), &files.handlers).expect("write handlers.ts");
    std::fs::write(generated.join("server.ts"), &files.server).expect("write server.ts");

    let (ok, out) = run(&driver_dir, "npx", &["tsx", "url-bytes-driver.ts"]);
    assert!(ok, "typescript url/bytes round-trip test failed:\n{out}");
}

/// Cross-language wire conformance for the TypeScript target: generates the Express
/// server + client from [`CROSS_LANG_SCHEMA`] into `generated-cross-lang/` and runs
/// `cross-lang-driver.ts`, which drives the generated client against the generated
/// server through a `fetch` wrapper that records the wire, then asserts the captured
/// request/response bytes equal the shared golden `cross_lang/wire.json`.
#[test]
fn cross_lang_typescript_conformance() {
    if gate(&missing_tools(&["node", "npm", "npx"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("typescript");
    if !require_node_modules(&driver_dir) {
        return;
    }

    let (program, result) = parse_and_check(CROSS_LANG_SCHEMA);
    let files = phoenix_codegen::generate_typescript(&program, &result);

    let generated = driver_dir.join("generated-cross-lang");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated-cross-lang dir");
    std::fs::write(generated.join("types.ts"), &files.types).expect("write types.ts");
    std::fs::write(generated.join("client.ts"), &files.client).expect("write client.ts");
    std::fs::write(generated.join("handlers.ts"), &files.handlers).expect("write handlers.ts");
    std::fs::write(generated.join("server.ts"), &files.server).expect("write server.ts");

    let (ok, out) = run(&driver_dir, "npx", &["tsx", "cross-lang-driver.ts"]);
    assert!(
        ok,
        "typescript cross-language wire conformance failed:\n{out}"
    );
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
    let Some(venv_python) = require_venv(&driver_dir) else {
        return;
    };

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

/// Dedicated `DateTime` wire round-trip for Python. Reuses the main Python
/// driver's committed `.venv` but writes the generated package into a SEPARATE
/// `generated_dt/` dir and runs the bespoke `datetime_driver.py`, so it never
/// races `python_roundtrip`'s `generated/`. Proves body/query/response-header
/// `DateTime`s round-trip RFC 3339 (incl. the client's `model_dump(mode="json")`
/// body serialization and `.isoformat()`/`fromisoformat` header handling).
#[test]
fn datetime_python_roundtrip() {
    if gate(&missing_tools(&["python3"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("python");
    let Some(venv_python) = require_venv(&driver_dir) else {
        return;
    };

    let files = generate_python_files(DATETIME_RT_SCHEMA);

    let generated = driver_dir.join("generated_dt");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated_dt dir");
    std::fs::write(generated.join("__init__.py"), &files.init).expect("write __init__.py");
    std::fs::write(generated.join("models.py"), &files.models).expect("write models.py");
    std::fs::write(generated.join("client.py"), &files.client).expect("write client.py");
    std::fs::write(generated.join("handlers.py"), &files.handlers).expect("write handlers.py");
    std::fs::write(generated.join("server.py"), &files.server).expect("write server.py");

    let python = venv_python.to_string_lossy().into_owned();
    let (ok, out) = run(&driver_dir, &python, &["datetime_driver.py"]);
    assert!(ok, "python DateTime round-trip test failed:\n{out}");
}

/// Dedicated `Uuid` wire round-trip for Python. Reuses the main Python driver's
/// `.venv` but writes the generated package into a SEPARATE `generated_uuid/` dir
/// and runs the bespoke `uuid_driver.py`. Proves body/query/response-header/bare
/// `Uuid`s round-trip RFC 4122 (incl. `model_dump(mode="json")` body
/// serialization, `str()`/`UUID(...)` header handling, and pydantic's parse
/// validation accepting valid input).
#[test]
fn uuid_python_roundtrip() {
    if gate(&missing_tools(&["python3"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("python");
    let Some(venv_python) = require_venv(&driver_dir) else {
        return;
    };

    let files = generate_python_files(UUID_RT_SCHEMA);

    let generated = driver_dir.join("generated_uuid");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated_uuid dir");
    std::fs::write(generated.join("__init__.py"), &files.init).expect("write __init__.py");
    std::fs::write(generated.join("models.py"), &files.models).expect("write models.py");
    std::fs::write(generated.join("client.py"), &files.client).expect("write client.py");
    std::fs::write(generated.join("handlers.py"), &files.handlers).expect("write handlers.py");
    std::fs::write(generated.join("server.py"), &files.server).expect("write server.py");

    let python = venv_python.to_string_lossy().into_owned();
    let (ok, out) = run(&driver_dir, &python, &["uuid_driver.py"]);
    assert!(ok, "python UUID round-trip test failed:\n{out}");
}

/// Dedicated `Decimal` wire round-trip for Python. Reuses the main Python
/// driver's `.venv` but writes the generated package into a SEPARATE
/// `generated_decimal/` dir and runs `decimal_driver.py`. Proves
/// body/query/response-header/bare `Decimal`s round-trip exactly (incl.
/// `model_dump(mode="json")` body serialization, `str()`/`Decimal(...)` header
/// handling, and pydantic's parse validation accepting valid + rejecting
/// malformed input).
#[test]
fn decimal_python_roundtrip() {
    if gate(&missing_tools(&["python3"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("python");
    let Some(venv_python) = require_venv(&driver_dir) else {
        return;
    };

    let files = generate_python_files(DECIMAL_RT_SCHEMA);

    let generated = driver_dir.join("generated_decimal");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated_decimal dir");
    std::fs::write(generated.join("__init__.py"), &files.init).expect("write __init__.py");
    std::fs::write(generated.join("models.py"), &files.models).expect("write models.py");
    std::fs::write(generated.join("client.py"), &files.client).expect("write client.py");
    std::fs::write(generated.join("handlers.py"), &files.handlers).expect("write handlers.py");
    std::fs::write(generated.join("server.py"), &files.server).expect("write server.py");

    let python = venv_python.to_string_lossy().into_owned();
    let (ok, out) = run(&driver_dir, &python, &["decimal_driver.py"]);
    assert!(ok, "python Decimal round-trip test failed:\n{out}");
}

/// Dedicated `Money` wire round-trip for Python. Reuses the main Python driver's
/// `.venv` but writes the generated package into a SEPARATE `generated_money/` dir
/// and runs `money_driver.py`. Proves the composite `Money` pydantic model
/// round-trips (body / nested list element / bare response) and that the server
/// rejects a malformed amount and an unknown currency.
#[test]
fn money_python_roundtrip() {
    if gate(&missing_tools(&["python3"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("python");
    let Some(venv_python) = require_venv(&driver_dir) else {
        return;
    };

    let files = generate_python_files(MONEY_RT_SCHEMA);

    let generated = driver_dir.join("generated_money");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated_money dir");
    std::fs::write(generated.join("__init__.py"), &files.init).expect("write __init__.py");
    std::fs::write(generated.join("models.py"), &files.models).expect("write models.py");
    std::fs::write(generated.join("client.py"), &files.client).expect("write client.py");
    std::fs::write(generated.join("handlers.py"), &files.handlers).expect("write handlers.py");
    std::fs::write(generated.join("server.py"), &files.server).expect("write server.py");

    let python = venv_python.to_string_lossy().into_owned();
    let (ok, out) = run(&driver_dir, &python, &["money_driver.py"]);
    assert!(ok, "python Money round-trip test failed:\n{out}");
}

/// Dedicated enum query/header wire round-trip for Python. Reuses the main Python
/// driver's `.venv` but writes the generated package into a SEPARATE
/// `generated_enum/` dir and runs `enum_driver.py`. Proves enum query/header
/// values round-trip as the bare variant string (FastAPI coerces the wire string
/// into the enum on receive; the client sends `.value` and reconstructs on read),
/// that the server applies a defaulted query enum, and that FastAPI rejects an
/// unknown query/header variant (422).
#[test]
fn enum_python_roundtrip() {
    if gate(&missing_tools(&["python3"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("python");
    let Some(venv_python) = require_venv(&driver_dir) else {
        return;
    };

    let files = generate_python_files(ENUM_RT_SCHEMA);

    let generated = driver_dir.join("generated_enum");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated_enum dir");
    std::fs::write(generated.join("__init__.py"), &files.init).expect("write __init__.py");
    std::fs::write(generated.join("models.py"), &files.models).expect("write models.py");
    std::fs::write(generated.join("client.py"), &files.client).expect("write client.py");
    std::fs::write(generated.join("handlers.py"), &files.handlers).expect("write handlers.py");
    std::fs::write(generated.join("server.py"), &files.server).expect("write server.py");

    let python = venv_python.to_string_lossy().into_owned();
    let (ok, out) = run(&driver_dir, &python, &["enum_driver.py"]);
    assert!(ok, "python enum round-trip test failed:\n{out}");
}

/// Dedicated inline-response-projection wire round-trip for Python. Reuses the main
/// Python driver's `.venv` but writes the generated package into a SEPARATE
/// `generated_projection/` dir and runs `projection_driver.py`. Proves a bare
/// projected `<Endpoint>Response` and a `List<…>` of them round-trip, incl. pydantic
/// decode of the projected `Uuid`/`DateTime` fields.
#[test]
fn projection_python_roundtrip() {
    if gate(&missing_tools(&["python3"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("python");
    let Some(venv_python) = require_venv(&driver_dir) else {
        return;
    };

    let files = generate_python_files(PROJECTION_RT_SCHEMA);

    let generated = driver_dir.join("generated_projection");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated_projection dir");
    std::fs::write(generated.join("__init__.py"), &files.init).expect("write __init__.py");
    std::fs::write(generated.join("models.py"), &files.models).expect("write models.py");
    std::fs::write(generated.join("client.py"), &files.client).expect("write client.py");
    std::fs::write(generated.join("handlers.py"), &files.handlers).expect("write handlers.py");
    std::fs::write(generated.join("server.py"), &files.server).expect("write server.py");

    let python = venv_python.to_string_lossy().into_owned();
    let (ok, out) = run(&driver_dir, &python, &["projection_driver.py"]);
    assert!(ok, "python projection round-trip test failed:\n{out}");
}

/// Dedicated list-valued-param wire round-trip for Python. Reuses the main Python
/// driver's `.venv` but writes the generated package into a SEPARATE
/// `generated_list/` dir and runs `list_driver.py`. Proves query params (FastAPI
/// native `list[T]` parsing + `py_list_query_value` client encoders) and request
/// headers (route-body manual split→coerce), both covering every element type,
/// round-trip including the empty list — the two paths diverge in Python, so each
/// carries every type.
#[test]
fn list_python_roundtrip() {
    if gate(&missing_tools(&["python3"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("python");
    let Some(venv_python) = require_venv(&driver_dir) else {
        return;
    };

    let files = generate_python_files(LIST_RT_SCHEMA);

    let generated = driver_dir.join("generated_list");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated_list dir");
    std::fs::write(generated.join("__init__.py"), &files.init).expect("write __init__.py");
    std::fs::write(generated.join("models.py"), &files.models).expect("write models.py");
    std::fs::write(generated.join("client.py"), &files.client).expect("write client.py");
    std::fs::write(generated.join("handlers.py"), &files.handlers).expect("write handlers.py");
    std::fs::write(generated.join("server.py"), &files.server).expect("write server.py");

    let python = venv_python.to_string_lossy().into_owned();
    let (ok, out) = run(&driver_dir, &python, &["list_driver.py"]);
    assert!(ok, "python list round-trip test failed:\n{out}");
}

/// Dedicated `Url`/`Bytes` wire round-trip for the Python target: generates the
/// package from [`URL_BYTES_RT_SCHEMA`] into `generated_url_bytes/` and runs
/// `url_bytes_driver.py` with the committed `.venv`. Asserts a `Bytes` field comes
/// back as raw `bytes` with identical contents (the custom `Bytes` alias ↔ base64
/// wire, preserving non-UTF-8 bytes) across body/`Option`/`List`/response, and that
/// a `Url` round-trips byte-for-byte through body, query, `List` query, and a
/// request header — plus the malformed-`Url` query reject path (FastAPI runs the
/// `BeforeValidator` → 422).
#[test]
fn url_bytes_python_roundtrip() {
    if gate(&missing_tools(&["python3"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("python");
    let Some(venv_python) = require_venv(&driver_dir) else {
        return;
    };

    let files = generate_python_files(URL_BYTES_RT_SCHEMA);

    let generated = driver_dir.join("generated_url_bytes");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated_url_bytes dir");
    std::fs::write(generated.join("__init__.py"), &files.init).expect("write __init__.py");
    std::fs::write(generated.join("models.py"), &files.models).expect("write models.py");
    std::fs::write(generated.join("client.py"), &files.client).expect("write client.py");
    std::fs::write(generated.join("handlers.py"), &files.handlers).expect("write handlers.py");
    std::fs::write(generated.join("server.py"), &files.server).expect("write server.py");

    let python = venv_python.to_string_lossy().into_owned();
    let (ok, out) = run(&driver_dir, &python, &["url_bytes_driver.py"]);
    assert!(ok, "python url/bytes round-trip test failed:\n{out}");
}

/// Cross-language wire conformance for the Python target: generates the package
/// from [`CROSS_LANG_SCHEMA`] into `generated_cross_lang/` and runs
/// `cross_lang_driver.py`, which drives the generated httpx client against the
/// generated FastAPI server through an httpx event hook that records the wire, then
/// asserts the captured request/response bytes equal the shared golden
/// `cross_lang/wire.json`. This is the regression guard for the Python camelCase
/// wire fix — it proves Python's wire matches the contract Go/TS are held to.
#[test]
fn cross_lang_python_conformance() {
    if gate(&missing_tools(&["python3"])) {
        return;
    }

    let driver_dir = roundtrip_dir().join("python");
    let Some(venv_python) = require_venv(&driver_dir) else {
        return;
    };

    let files = generate_python_files(CROSS_LANG_SCHEMA);

    let generated = driver_dir.join("generated_cross_lang");
    let _ = std::fs::remove_dir_all(&generated);
    std::fs::create_dir_all(&generated).expect("create generated_cross_lang dir");
    std::fs::write(generated.join("__init__.py"), &files.init).expect("write __init__.py");
    std::fs::write(generated.join("models.py"), &files.models).expect("write models.py");
    std::fs::write(generated.join("client.py"), &files.client).expect("write client.py");
    std::fs::write(generated.join("handlers.py"), &files.handlers).expect("write handlers.py");
    std::fs::write(generated.join("server.py"), &files.server).expect("write server.py");

    let python = venv_python.to_string_lossy().into_owned();
    let (ok, out) = run(&driver_dir, &python, &["cross_lang_driver.py"]);
    assert!(ok, "python cross-language wire conformance failed:\n{out}");
}
