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
use common::{
    chi_module_at_version, chi_scaffold_dir, e2e_required, gate, go_module_cached, missing_tools,
    parse_and_check, run, tool_available,
};

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

/// A multi-status endpoint with three *typed* statuses. The generated multi-status
/// guard `if ([200, 201, 202].includes(result.status) && result.body === undefined)`
/// crosses the print width once the route nests at Fastify's depth (one level
/// deeper than Express), so the generator must break it the way prettier does.
/// No other linted schema has a 3-typed-status block, so without this fixture a
/// regression in that wrapping would only surface as a `prettier --check`
/// failure on real user output. Linted under Fastify (the framework that
/// triggers the wrap).
const MULTI_STATUS_WRAP_SCHEMA: &str = r#"
struct Thing {
    id: Int
    name: String
}

endpoint upsertThing: PUT "/api/things/{id}" {
    body Thing
    response { 200: Thing  201: Thing  202: Thing  204 }
}
"#;

/// Exercises the `DateTime` scalar across every position and target-specific
/// path it touches: a struct field (`publishedAt`), an `Option<DateTime>` field
/// (`editedAt`), a `List<DateTime>` (`timeline`), a `Map<String, DateTime>`
/// (`milestones` — exercises the TS Map-revival codegen, the one revival shape
/// the round-trip schema can't easily assert), an `Option<Map<String, DateTime>>`
/// field (`archivedPhases` — exercises the TS `revive*` path where an optional
/// guard wraps a `Map` value-revival `for…of` loop, the only revival-layout
/// branch the required-collection fields don't reach), a nested Date-bearing
/// struct (`comments: List<Comment>`), a `DateTime` query param (`since`), a
/// request header (`requestedAt`), a response header (`servedAt`), a plain
/// response, a paginated response (items revival), and a body+response-headers
/// envelope. `replacePost` adds a MULTI-STATUS (`response { 200: Post … }`)
/// endpoint with a Date-bearing body, exercising the client's multi-status decode
/// branch (the `responseBody = revive…(JSON.parse(...))` path in TS, the parsed
/// `<Endpoint>Response` envelope in Python) — the one DateTime-touching client
/// branch the bare/paginated/header endpoints don't reach. It
/// also covers BARE scalar/collection `DateTime` responses (`response DateTime`,
/// `response List<DateTime>`, `response Map<String, DateTime>`) — the positions
/// that render a bare `datetime`/`time.Time`/`Date` as the return type, exercising
/// the per-file import detection AND the Python client's by-type body decode
/// (`datetime.fromisoformat(...)` rather than the object-only `Type(**...)` form).
/// `getPublishedAtMetered` pins a bare `DateTime` body on a response-header
/// endpoint with a NON-`DateTime` header (`etag: String`): the Python client still
/// decodes the body via `datetime.fromisoformat(...)` into the envelope's `body=`,
/// so it must import `datetime` even though no response header is a `DateTime` —
/// the case the import walker missed when it excluded response-header endpoints.
/// `Post.title`'s `where` constraint makes `createPost`'s body BOTH constrained and
/// Date-bearing, exercising the TS server's combined
/// `reviveCreatePostBody(validateCreatePostBody(...))` path and the single merged
/// value-import (`ValidationError`/`validate*`/`revive*` from `./types`).
/// This is the lint proof that all four generators emit valid output for
/// `DateTime`: Go `time.Time` (+`time` import, `time.Parse`/`.Format(time.RFC3339)`),
/// Python `datetime` (+import, `.isoformat()`/`fromisoformat`), TypeScript `Date`
/// (+the `revive*` pass and `.toISOString()`), and OpenAPI `format: date-time`. See
/// `docs/design-decisions.md` (DateTime & UUID scalar types).
const DATETIME_SCHEMA: &str = r#"
struct Comment {
    id: Int
    createdAt: DateTime
}

struct Post {
    id: Int
    title: String where self.length > 0
    publishedAt: DateTime
    editedAt: Option<DateTime>
    timeline: List<DateTime>
    milestones: Map<String, DateTime>
    archivedPhases: Option<Map<String, DateTime>>
    comments: List<Comment>
}

endpoint getPost: GET "/api/posts/{id}" {
    response Post
}

endpoint listPosts: GET "/api/posts" {
    query {
        since: Option<DateTime>
        limit: Int = 20
    }
    response List<Post>
    pagination { cursor }
}

endpoint createPost: POST "/api/posts" {
    body Post
    headers {
        requestedAt: Option<DateTime>
    }
    response Post headers {
        servedAt: Option<DateTime>
    }
}

endpoint getPublishedAt: GET "/api/posts/{id}/published" {
    response DateTime
}

endpoint listPublishDates: GET "/api/publish-dates" {
    response List<DateTime>
}

endpoint getMilestoneMap: GET "/api/milestones" {
    response Map<String, DateTime>
}

endpoint getPublishedAtMetered: GET "/api/posts/{id}/published-metered" {
    response DateTime headers {
        etag: String
    }
}

endpoint replacePost: PUT "/api/posts/{id}" {
    body Post
    response { 200: Post  201: Post  204 }
}
"#;

/// Exercises the `Uuid` scalar across every position and target path it touches:
/// struct fields (`id`), `Option<Uuid>` (`ownerId`), `List<Uuid>` (`members`),
/// `Map<String, Uuid>` (`index`), a nested Uuid-bearing struct (`owner: Profile`),
/// a `Uuid` query param (`ref`) + `Option<Uuid>` query param (`since`), required
/// request header (`idempotencyKey`) + `Option<Uuid>` request header (`ifMatch`),
/// required + optional response headers (`requestId`/`traceId`), a body, and
/// BARE scalar /
/// `List` / `Map` `Uuid` responses. The lint proof that all four generators emit
/// valid output: Go `string` + the `uuidRe` `Validate()` check (+`regexp`),
/// Python `uuid.UUID` (+import, `str()`/`UUID(...)`), TypeScript the branded
/// `Uuid` alias + `parseUuid` validate-on-decode pass, OpenAPI `format: uuid`.
const UUID_SCHEMA: &str = r#"
struct Profile {
    handle: String
    avatarId: Uuid
}

struct Account {
    id: Uuid
    ownerId: Option<Uuid>
    name: String
    members: List<Uuid>
    index: Map<String, Uuid>
    owner: Profile
}

endpoint getAccount: GET "/accounts/{id}" {
    query {
        ref: Uuid
        since: Option<Uuid>
    }
    headers {
        ifMatch: Option<Uuid>
    }
    response Account headers {
        requestId: Uuid
        traceId: Option<Uuid>
    }
}

endpoint createAccount: POST "/accounts" {
    body Account
    headers {
        idempotencyKey: Uuid
    }
    response Account
}

endpoint newId: GET "/id" {
    response Uuid
}

endpoint listIds: GET "/ids" {
    response List<Uuid>
}

endpoint idMap: GET "/id-map" {
    response Map<String, Uuid>
}
"#;

/// Exercises the `Decimal` scalar across every position and target path: struct
/// fields (`subtotal`), `Option<Decimal>` (`discount`), `List<Decimal>`
/// (`lineTotals`), `Map<String, Decimal>` (`rates`), a nested Decimal-bearing
/// struct (`tax: TaxLine`), a `Decimal` query param (`minAmount`) + `Option<Decimal>`
/// query param (`maxAmount`), request header (`budgetCap`) + `Option<Decimal>`
/// request header (`priceFloor`), required + optional response headers
/// (`computedTax`/`fxRate`), a
/// body, and BARE scalar / `List` / `Map` `Decimal` responses. The lint proof that
/// all four generators emit valid output: Go `string` + the `decimalRe`
/// `Validate()` check (+`regexp`), Python `decimal.Decimal` (+import,
/// `str()`/`Decimal(...)`), TypeScript the branded `Decimal` alias + `parseDecimal`
/// validate-on-decode pass, OpenAPI `format: decimal`.
const DECIMAL_SCHEMA: &str = r#"
struct TaxLine {
    label: String
    amount: Decimal
}

struct Invoice {
    id: Int
    subtotal: Decimal
    discount: Option<Decimal>
    lineTotals: List<Decimal>
    rates: Map<String, Decimal>
    tax: TaxLine
}

endpoint getInvoice: GET "/invoices/{id}" {
    query {
        minAmount: Decimal
        maxAmount: Option<Decimal>
    }
    headers {
        priceFloor: Option<Decimal>
    }
    response Invoice headers {
        computedTax: Decimal
        fxRate: Option<Decimal>
    }
}

endpoint createInvoice: POST "/invoices" {
    body Invoice
    headers {
        budgetCap: Decimal
    }
    response Invoice
}

endpoint exchangeRate: GET "/rate" {
    response Decimal
}

endpoint listRates: GET "/rates" {
    response List<Decimal>
}

endpoint rateMap: GET "/rate-map" {
    response Map<String, Decimal>
}
"#;

/// Exercises the composite `Money` built-in across every position + target path:
/// struct fields (`total`), `Option<Money>` (`tip`), `List<Money>` (`charges`),
/// `Map<String, Money>` (`byCategory`), a nested Money-bearing struct
/// (`items: List<LineItem>`), a body, BARE scalar / `List` / `Map` `Money`
/// responses, and a PAGINATED `List<Money>` (item type `Money`, exercising the
/// `pagination.item_type` arm of `schema_uses_money`). The lint proof that all four generators emit valid output for the
/// composite: Go `Money` struct + `Validate()` (decimal + ISO-4217 set, recursed
/// into by containing `Validate()`s), Python `Money` pydantic model + currency
/// `field_validator`, TypeScript the `Money` interface + `reviveMoney`, OpenAPI a
/// shared `Money` component with the currency `enum`. (No query/header `Money` —
/// a composite isn't URL/header-encodable.)
///
/// Also covers a MULTI-STATUS response carrying a Money-bearing struct
/// (`settleInvoice` → `Invoice`): a multi-status `response { }` block must carry a
/// named struct (sema rejects a bare `Money`/list/scalar there), so the
/// `<Endpoint>Response` envelope wraps an `Invoice` whose fields include `Money` —
/// exercising the multi-status envelope generation crossed with the `Money`
/// composite (Money emission stays gated on the struct-field scan).
///
/// NOTE: the `List<Money>`/`Map<String, Money>` *element* values here are validated
/// on decode by Python (pydantic) and TypeScript (`reviveMoney` runs through
/// list/map elements) but NOT by Go — Go's `Validate()` only recurses into direct
/// `Money`/`Option<Money>` fields (the documented "weak link" parallel to the
/// regex-scalar one; see `money_field_shape` in `go.rs` and `docs/known-issues.md`).
/// This fixture only proves the output lints; the divergence itself is NOT
/// behaviorally tested — the Money round-trip drivers place only valid currencies
/// in `List`/`Map` position, so no test asserts Go-accepts / Python+TS-reject.
const MONEY_SCHEMA: &str = r#"
struct LineItem {
    label: String
    price: Money
}

struct Invoice {
    id: Int
    total: Money
    tip: Option<Money>
    charges: List<Money>
    byCategory: Map<String, Money>
    items: List<LineItem>
}

endpoint getInvoice: GET "/invoices/{id}" {
    response Invoice
}

endpoint createInvoice: POST "/invoices" {
    body Invoice
    response Invoice
}

endpoint getBalance: GET "/balance" {
    response Money
}

endpoint listCharges: GET "/charges" {
    response List<Money>
}

endpoint chargeMap: GET "/charge-map" {
    response Map<String, Money>
}

endpoint pageCharges: GET "/charge-page" {
    response List<Money>
    pagination { cursor }
}

endpoint settleInvoice: POST "/invoices/{id}/settle" {
    response { 200: Invoice  201: Invoice }
}
"#;

/// A schema whose ONLY `Money` use is a bare response, with NO user structs. The
/// generated `Money` type definition is then the last thing in the file (Python's
/// `models.py` ends just after `class Money` — there is no user model after it),
/// which exercises the blank-line / trailing-newline tail of `emit_money_model`
/// (and the parallel Go/TS/OpenAPI emit) that `MONEY_SCHEMA` — always followed by
/// user structs — does not. Lint-only; behavior is covered by the round-trips.
const MONEY_ONLY_SCHEMA: &str = r#"
endpoint getBalance: GET "/balance" {
    response Money
}
"#;

/// Exercises simple enums in query/header param positions across every
/// target-specific path: an `Option<enum>` query param (`color`), an enum query
/// param WITH a default (`size = Medium` — the `is_optional`-with-default branch,
/// and the enum-default rendering: TS `"Medium"`, Go `SizeMedium`, Python
/// `Size.MEDIUM`, OpenAPI `default`), a required enum REQUEST header
/// (`preferredColor`) and an `Option<enum>` header (`fallbackColor`), plus a
/// required and `Option<enum>` RESPONSE header (`chosen`/`alt`). `listItems` has
/// an `error { }` block (so the catch maps both `ValidationError`→400 and the
/// declared error); `pickColor` has NONE (exercising the catch-binding path when
/// only the enum 400 guard is present). The server validates the wire string into
/// the typed enum on every target (TS `parse<Enum>`→`ValidationError`, Go
/// `Valid()`→400, Python FastAPI enum coercion→422), the headline soundness win
/// of the slice. `Tier` is used ONLY as a response header (`pickColor`'s `tier`)
/// and nowhere in a query/request header, so it is NOT a param-enum: it pins the
/// response-header-only path — the type is still imported and the client casts it
/// on read, but no `parse<Enum>`/`Valid()` is emitted for it.
const ENUM_PARAM_SCHEMA: &str = r#"
enum Color { Red  Green  Blue }
enum Size { Small  Medium  Large }
enum Tier { Free  Pro }

struct Item {
    id: Uuid
    color: Color
    size: Size
}

endpoint listItems: GET "/items" {
    headers {
        preferredColor: Color as "X-Preferred-Color"
        fallbackColor: Option<Color> as "X-Fallback-Color"
    }
    query {
        color: Option<Color>
        size: Size = Medium
    }
    response List<Item>
    error { Unauthorized(401) }
}

endpoint pickColor: GET "/pick" {
    query {
        color: Color = Red
    }
    response Item headers {
        chosen: Color as "X-Chosen-Color"
        alt: Option<Color> as "X-Alt-Color"
        tier: Tier as "X-Tier"
    }
}
"#;

/// Exercises inline response projection (`response Struct pick/omit/partial`)
/// across every position + target-specific path: a bare `pick` (`getProfile`), a
/// bare `omit` (`getUser`), `omit … partial` (`getSummary` — projected struct with
/// all-optional fields), a BARE `List<Struct pick…>` (`searchUsers`), a PAGINATED
/// `List<Struct pick…>` (`listUsers` — the `<Endpoint>Page` envelope over projected
/// items), a paginated projected list whose reviver name is long enough to push the
/// items-revival `.map((x) => …)` past the print width (`listActiveSubscribers` —
/// locks the multi-line `prettier` break of that line, vs `listUsers`'s one-line
/// form), and a projection WITH response headers (`getMetered` — the
/// `<Endpoint>Result` envelope wrapping the projected `<Endpoint>Response`). Every
/// projection includes a `DateTime`/`Uuid` field so the TS client's revival pass
/// over the generated `<Endpoint>Response` (and the paginated-items revival) is
/// exercised end-to-end, not just at compile-lint. The generated `<Endpoint>Response`
/// reuses the multi-status name slot (mutually exclusive). Auth header + `error`
/// blocks mirror the other schemas so the output is realistic.
const PROJECTION_SCHEMA: &str = r#"
struct User {
    id: Uuid
    displayName: String where self.length > 0
    email: String where self.contains("@")
    passwordHash: String where self.length > 0
    loginCount: Int where self >= 0
    createdAt: DateTime
}

endpoint getProfile: GET "/users/{id}/profile" {
    headers { authorization: String }
    response User pick { id, displayName, createdAt }
    error { Unauthorized(401)  NotFound(404) }
}

endpoint getUser: GET "/users/{id}" {
    headers { authorization: String }
    response User omit { passwordHash }
    error { Unauthorized(401)  NotFound(404) }
}

endpoint getSummary: GET "/users/{id}/summary" {
    headers { authorization: String }
    response User omit { passwordHash, email } partial
    error { Unauthorized(401) }
}

endpoint searchUsers: GET "/users/search" {
    headers { authorization: String }
    query { q: String }
    response List<User pick { id, displayName, createdAt }>
    error { Unauthorized(401) }
}

endpoint listUsers: GET "/users" {
    headers { authorization: String }
    response List<User pick { id, displayName }>
    pagination { offset }
    error { Unauthorized(401) }
}

endpoint listActiveSubscribers: GET "/subscribers/active" {
    headers { authorization: String }
    response List<User pick { id, displayName, createdAt }>
    pagination { offset }
    error { Unauthorized(401) }
}

endpoint getMetered: GET "/users/{id}/metered" {
    headers { authorization: String }
    response User pick { id, displayName, createdAt } headers {
        etag: String as "ETag"
        ratelimitRemaining: Int as "X-RateLimit-Remaining"
    }
    error { Unauthorized(401) }
}
"#;

/// Exercises list-valued params (`List<T>`, repeated query keys / comma-separated
/// headers) across every element type and both positions: `List<Uuid>`,
/// `List<String>`, `List<Int>`, `List<DateTime>`, `List<Bool>`, `List<Decimal>`,
/// and `List<enum>` query params (repeated key, FastAPI native `list[T]`; the enum
/// element is validated), plus a `List<String>` and a `List<enum>` REQUEST header
/// (comma-separated: client joins, server splits + trims + coerces each — the enum
/// header exercises the split→validate path). The `List<Bool>`/`List<Decimal>`
/// query params exist to compile-lint the query-only client encoders (the header
/// encoders cover the other element types). Auth header + `error` block mirror the
/// other schemas.
const LIST_PARAM_SCHEMA: &str = r#"
enum Tag { Red  Green  Blue }

struct Item {
    id: Uuid
    name: String
}

endpoint search: GET "/search" {
    headers {
        authorization: String
        roles: List<String> as "X-Role"
        tagFilter: List<Tag> as "X-Tag"
    }
    query {
        ids: List<Uuid>
        names: List<String>
        counts: List<Int>
        since: List<DateTime>
        flags: List<Bool>
        prices: List<Decimal>
        tags: List<Tag>
    }
    response List<Item>
    error { Unauthorized(401) }
}
"#;

/// Exercises the `Url` (branded validated string, `format: uri`) and `Bytes`
/// (first-class binary, base64 wire, `contentEncoding: base64`) scalars across positions:
/// `Url` as a struct field / `Option` / `List` / a query param / a `List` query
/// param / a request header, and `Bytes` as a struct field / `Option` / `List` /
/// a `Map` value (body + response). The `Url` query/header exercises
/// `parseUrl`/`urlRe` validation; the `Bytes` fields exercise the base64 ↔
/// `Uint8Array`/`[]byte`/`bytes` encode/decode (TS `encodeBytes` +
/// `bytesFromBase64`). The `Map<String, Bytes>` field covers the only remaining
/// combinator over `Bytes` — the TS `encodeBytes` deep-walk over a `Record` and
/// the `Object.fromEntries` revival, Go `map[string][]byte` auto-base64, and the
/// pydantic `Bytes` alias applied to dict values. Auth header + `error` block
/// mirror the other schemas. `replace` adds a MULTI-STATUS
/// (`response { 200: Asset … }`) endpoint whose shared body carries `Bytes`, so
/// the `result.body` branch of the response-envelope codegen is exercised for the
/// `encodeBytes` wrapping (keyed on `ep.response`, which mirrors the shared body).
/// `page` adds a PAGINATED (`pagination { cursor }`) endpoint whose item type
/// (`Asset`) carries `Bytes`, so the TS server's `encodeBytes` wrap on the page
/// envelope and the client's per-item `bytesFromBase64` revival (via
/// `pagination.item_type`) are both exercised — the one combinator the other
/// endpoints don't reach. `get` carries a required + optional `Url` RESPONSE
/// HEADER (`Link`/`X-Digest`), exercising the `Url` arms of the response-header
/// write (server) and read coercion (client) — pass-through, since a `Url`'s
/// runtime representation is already `str` (see `header_read_coercion`). `raw`
/// and `rawList` carry a BARE `Bytes` / `List<Bytes>` RESPONSE (not struct-wrapped),
/// exercising the inline decode path the struct fixtures skip: TS inlines
/// `bytesFromBase64`, Go relies on `encoding/json`, and Python decodes via
/// `base64.b64decode` (its `py_decode_expr` `Bytes` arm + `import base64`) rather
/// than a model's pydantic alias.
const URL_BYTES_SCHEMA: &str = r#"
struct Asset {
    id: Uuid
    source: Url
    mirror: Option<Url>
    thumbnails: List<Url>
    checksum: Bytes
    signature: Option<Bytes>
    chunks: List<Bytes>
    tags: Map<String, Bytes>
}

endpoint upload: POST "/assets" {
    headers { authorization: String }
    body Asset
    response Asset
    error { Unauthorized(401) }
}

endpoint find: GET "/assets" {
    headers { authorization: String  origin: Url as "X-Origin" }
    query { source: Url  mirrors: List<Url> }
    response List<Asset>
    error { Unauthorized(401) }
}

endpoint get: GET "/assets/{id}" {
    headers { authorization: String }
    response Asset headers { canonical: Url as "Link"  digest: Option<Url> as "X-Digest" }
    error { Unauthorized(401)  NotFound(404) }
}

endpoint replace: PUT "/assets/{id}" {
    headers { authorization: String }
    body Asset
    response { 200: Asset  201: Asset  204 }
    error { Unauthorized(401)  NotFound(404) }
}

endpoint page: GET "/assets/page" {
    headers { authorization: String }
    response List<Asset>
    pagination { cursor }
    error { Unauthorized(401) }
}

endpoint raw: GET "/assets/{id}/raw" {
    headers { authorization: String }
    response Bytes
}

endpoint rawList: GET "/assets/raw" {
    headers { authorization: String }
    response List<Bytes>
}
"#;

/// Exercises target-language reserved words as schema identifiers — names that are
/// valid Phoenix identifiers but keywords (or unsafe predeclared identifiers) in
/// Python/Go/TypeScript, which the generators must escape (keeping the wire name
/// intact) or the output won't compile. Covers: struct **field** names
/// (`class`/`async`/`lambda` break Python attrs), **query** params (`range`/`map`
/// break Go locals; `class` breaks Python params; `nil`/`iota` shadow Go's
/// predeclared identifiers — `nil` so `return nil, err` stops compiling, `iota` the
/// predeclared counter — both escaped by `is_go_unsafe_predeclared`), a
/// **request-header** param, and a
/// **path** param (`{func}` breaks Go locals + a Python `Path(alias=…)` bind). Also
/// exercises a multi-word path param (`{widgetId}`), whose Python identifier
/// (`widget_id`) likewise needs the `Path(alias=…)` bind, and a `{arguments}` path
/// param — not a reserved word but illegal as a strict-mode (ES-module) `const`
/// binding, which TS must escape. The body reuses the struct so the derived-body
/// model + validator + client/server/handler all round through the escaped field
/// names.
///
/// COVERAGE NOTE: this fixture proves keyword *escaping* compiles lint-clean on
/// every target. The *runtime wire* behavior of keyword fields is covered
/// separately by the round-trip suite: the shared `gen_api.phx` `Catalog` struct
/// carries reserved-word fields (`class`/`async`), echoed through `syncCatalog`, so
/// `syncCatalog_roundtrips_composite_types` proves the escaped Python field
/// (`class_`/`async_`, whose wire key diverges since the model carries no alias)
/// serializes AND decodes on both legs, and that Go/TS carry the `class`/`async`
/// wire keys verbatim. The other runtime-sensitive case — a path param whose Python
/// identifier diverges from the wire segment, which silently 422s if mis-bound — is
/// round-tripped via `listComments_multiword_path_param`. The keyword *query*/
/// *header*/*path* params and the Go predeclared-`nil`/`iota` escapes in THIS
/// fixture remain compile-lint-only (their wire names ride a tag/alias/string-
/// literal already exercised by every non-keyword param).
const RESERVED_WORDS_SCHEMA: &str = r#"
struct Widget {
    class: String
    async: Bool
    lambda: Int
    func: String
    interface: Bool
    map: Int
}

endpoint listWidgets: GET "/widgets/{func}" {
    query { range: String  map: Int  class: String  nil: Int  iota: Int }
    headers { lambda: String as "X-Lambda" }
    response List<Widget>
    error { NotFound(404) }
}

endpoint getWidget: GET "/widgets/{widgetId}/detail" {
    response Widget
    error { NotFound(404) }
}

endpoint getThing: GET "/things/{arguments}" {
    response Widget
    error { NotFound(404) }
}

endpoint createWidget: POST "/widgets" {
    body Widget
    response Widget
    error { ValidationError(400) }
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

/// Generates the Go files for the chi server framework. Only `server.go` differs
/// from the net/http output (chi router/registration/`URLParam`); the other three
/// files are framework-independent.
fn generate_go_chi_files(schema: &str) -> phoenix_codegen::GoFiles {
    let (program, result) = parse_and_check(schema);
    phoenix_codegen::generate_go_with(&program, &result, phoenix_codegen::GoServerFramework::Chi)
}

/// Writes the generated `api/*.go` into the committed `go-chi` scaffold, then runs
/// `go build`, `gofmt -l`, and `golangci-lint` against them.
///
/// Unlike [`check_go_output`] (which scaffolds a stdlib-only module in a fresh
/// tempdir), the chi server imports an external module, so this reuses the
/// committed `tests/scaffold/go-chi/` project whose `go.mod`/`go.sum` pin chi —
/// `go build` resolves it from the module cache (or the proxy under E2E). Like
/// the TS/Python scaffolds, it mutates the gitignored `api/` dir in place, so all
/// calls MUST stay funneled through the single `go_output_compiles_and_lints`
/// test to avoid a `generated`-dir race under cargo's parallel `#[test]` running.
fn check_go_chi_output(scaffold: &Path, files: &phoenix_codegen::GoFiles) {
    let api_dir = scaffold.join("api");
    let _ = std::fs::remove_dir_all(&api_dir);
    std::fs::create_dir_all(&api_dir).expect("create api dir");
    std::fs::write(scaffold.join(".golangci.yml"), GOLANGCI_CONFIG).expect("write .golangci.yml");
    std::fs::write(api_dir.join("types.go"), &files.types).expect("write types.go");
    std::fs::write(api_dir.join("client.go"), &files.client).expect("write client.go");
    std::fs::write(api_dir.join("handlers.go"), &files.handlers).expect("write handlers.go");
    std::fs::write(api_dir.join("server.go"), &files.server).expect("write server.go");

    // 1. `go build ./...` must succeed (resolving chi from the module cache).
    let (built, build_out) = run(scaffold, "go", &["build", "./..."]);
    assert!(built, "go (chi) build failed:\n{build_out}");

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
    let (_, gofmt_out) = run(scaffold, "gofmt", &gofmt_arg_refs);
    assert!(
        gofmt_out.trim().is_empty(),
        "gofmt -l reported files needing formatting:\n{gofmt_out}"
    );

    // 3. `golangci-lint run ./api/...` (only the generated package — not the
    //    chi-anchoring root file) must exit 0.
    if tool_available("golangci-lint") {
        let (linted, lint_out) = run(scaffold, "golangci-lint", &["run", "./api/..."]);
        assert!(linted, "golangci-lint (chi) failed:\n{lint_out}");
    } else if e2e_required() {
        panic!("PHOENIX_GEN_E2E=1 but golangci-lint not found on PATH");
    } else {
        eprintln!("SKIP golangci-lint step (not installed)");
    }
}

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
    // `DateTime` across fields/list/nested/query/headers — `time.Time`, the
    // `time` import, and `time.Parse`/`.Format(time.RFC3339)` (de)serialization.
    check_go_output(&generate_go_files(DATETIME_SCHEMA));
    // `Uuid` across fields/list/map/nested/query/headers + bare responses; the
    // `uuidRe` `Validate()` check and `regexp` import.
    check_go_output(&generate_go_files(UUID_SCHEMA));
    // `Decimal` across fields/list/map/nested/query/headers + bare responses; the
    // `decimalRe` `Validate()` check.
    check_go_output(&generate_go_files(DECIMAL_SCHEMA));
    // Composite `Money`: struct + `Validate()` (decimal + ISO-4217), recursed into.
    check_go_output(&generate_go_files(MONEY_SCHEMA));
    // `Money` as the file's last definition (no trailing user struct).
    check_go_output(&generate_go_files(MONEY_ONLY_SCHEMA));
    // Enum query/header params (server `Valid()` validation + 400).
    check_go_output(&generate_go_files(ENUM_PARAM_SCHEMA));
    // Inline response projection (`<Endpoint>Response` struct emission).
    check_go_output(&generate_go_files(PROJECTION_SCHEMA));
    // List-valued params (repeated query keys + comma-separated headers).
    check_go_output(&generate_go_files(LIST_PARAM_SCHEMA));
    // `Url` (validated string) + `Bytes` (`[]byte`, auto base64).
    check_go_output(&generate_go_files(URL_BYTES_SCHEMA));
    check_go_output(&generate_go_files(RESERVED_WORDS_SCHEMA));

    // Realistic schema fixture library (see FILE_FIXTURES).
    for (name, schema) in FILE_FIXTURES.iter().copied() {
        eprintln!("fixture library: {name}");
        check_go_output(&generate_go_files(schema));
    }

    // chi server.go variant. Only `server.go` differs from net/http, so we re-lint
    // the same rich schemas (full surface, all-optional headers) plus the fixture
    // library — covering every chi route shape through go build + gofmt +
    // golangci-lint. Uses the committed `go-chi` scaffold, whose `go.mod`/`go.sum`
    // pin chi; `go build` resolves it from the module cache (or the proxy under
    // E2E). Skip with a log if chi isn't cached and the network isn't permitted.
    let go_chi_scaffold = chi_scaffold_dir();
    // Pinned chi version comes from the scaffold's own `go.mod` (the single source
    // of truth, shared with the round-trip suite) rather than a constant here that
    // could drift from the scaffold after a `go get …@<version>` bump.
    let chi_at_version = chi_module_at_version(&go_chi_scaffold);
    if !e2e_required() && !go_module_cached(&chi_at_version) {
        eprintln!(
            "SKIP go chi checks (set PHOENIX_GEN_E2E=1 to enforce): \
             {chi_at_version} not in the Go module cache"
        );
        return;
    }
    eprintln!("chi: gen_api full surface");
    check_go_chi_output(&go_chi_scaffold, &generate_go_chi_files(SCHEMA));
    check_go_chi_output(&go_chi_scaffold, &generate_go_chi_files(HEADER_SCHEMA));
    check_go_chi_output(&go_chi_scaffold, &generate_go_chi_files(DATETIME_SCHEMA));
    check_go_chi_output(&go_chi_scaffold, &generate_go_chi_files(UUID_SCHEMA));
    check_go_chi_output(&go_chi_scaffold, &generate_go_chi_files(DECIMAL_SCHEMA));
    check_go_chi_output(&go_chi_scaffold, &generate_go_chi_files(MONEY_SCHEMA));
    check_go_chi_output(&go_chi_scaffold, &generate_go_chi_files(MONEY_ONLY_SCHEMA));
    check_go_chi_output(&go_chi_scaffold, &generate_go_chi_files(ENUM_PARAM_SCHEMA));
    check_go_chi_output(&go_chi_scaffold, &generate_go_chi_files(PROJECTION_SCHEMA));
    check_go_chi_output(&go_chi_scaffold, &generate_go_chi_files(LIST_PARAM_SCHEMA));
    check_go_chi_output(&go_chi_scaffold, &generate_go_chi_files(URL_BYTES_SCHEMA));
    check_go_chi_output(
        &go_chi_scaffold,
        &generate_go_chi_files(RESERVED_WORDS_SCHEMA),
    );
    for (name, schema) in FILE_FIXTURES.iter().copied() {
        eprintln!("chi fixture library: {name}");
        check_go_chi_output(&go_chi_scaffold, &generate_go_chi_files(schema));
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
    check_openapi_output("DATETIME_SCHEMA", DATETIME_SCHEMA);
    check_openapi_output("UUID_SCHEMA", UUID_SCHEMA);
    check_openapi_output("DECIMAL_SCHEMA", DECIMAL_SCHEMA);
    check_openapi_output("MONEY_SCHEMA", MONEY_SCHEMA);
    check_openapi_output("MONEY_ONLY_SCHEMA", MONEY_ONLY_SCHEMA);
    check_openapi_output("ENUM_PARAM_SCHEMA", ENUM_PARAM_SCHEMA);
    check_openapi_output("PROJECTION_SCHEMA", PROJECTION_SCHEMA);
    check_openapi_output("LIST_PARAM_SCHEMA", LIST_PARAM_SCHEMA);
    check_openapi_output("URL_BYTES_SCHEMA", URL_BYTES_SCHEMA);
    check_openapi_output("RESERVED_WORDS_SCHEMA", RESERVED_WORDS_SCHEMA);

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

/// Like [`generate_typescript_files`] but emits the Fastify `server.ts` variant.
/// `types.ts`/`client.ts`/`handlers.ts` are framework-independent, so the only
/// file that differs from the Express output is `server.ts` (which imports
/// `fastify` — installed in the scaffold's `devDependencies`).
fn generate_typescript_fastify_files(schema: &str) -> phoenix_codegen::GeneratedFiles {
    let (program, result) = parse_and_check(schema);
    phoenix_codegen::generate_typescript_with(
        &program,
        &result,
        phoenix_codegen::TsServerFramework::Fastify,
    )
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
    // `DateTime` → `Date` plus the generated `revive*` pass: struct/list/nested
    // revival, paginated-items revival, body+response-header envelope revival, and
    // `.toISOString()` query/header encoding. Compiles + lints (tsc/eslint/prettier).
    check_typescript_output(&scaffold, &generate_typescript_files(DATETIME_SCHEMA));
    // `Uuid` branded alias + `parseUuid` validate-on-decode pass across all positions.
    check_typescript_output(&scaffold, &generate_typescript_files(UUID_SCHEMA));
    // `Decimal` branded alias + `parseDecimal` validate-on-decode across all positions.
    check_typescript_output(&scaffold, &generate_typescript_files(DECIMAL_SCHEMA));
    // Composite `Money`: interface + `reviveMoney` + ISO-4217 `CURRENCY_CODES`.
    check_typescript_output(&scaffold, &generate_typescript_files(MONEY_SCHEMA));
    // `Money` as the file's last definition (no trailing user struct).
    check_typescript_output(&scaffold, &generate_typescript_files(MONEY_ONLY_SCHEMA));
    // Enum query/header params: `parse<Enum>` validator + ValidationError → 400.
    check_typescript_output(&scaffold, &generate_typescript_files(ENUM_PARAM_SCHEMA));
    // Inline response projection: `<Endpoint>Response` type + revival + paginated.
    check_typescript_output(&scaffold, &generate_typescript_files(PROJECTION_SCHEMA));
    // List params: repeated-key query (toStringArray) + comma-split headers.
    check_typescript_output(&scaffold, &generate_typescript_files(LIST_PARAM_SCHEMA));
    // `Url` branded + `Bytes` (`Uint8Array` revival + `encodeBytes` on send).
    check_typescript_output(&scaffold, &generate_typescript_files(URL_BYTES_SCHEMA));
    check_typescript_output(&scaffold, &generate_typescript_files(RESERVED_WORDS_SCHEMA));

    // Realistic schema fixture library (see FILE_FIXTURES).
    for (name, schema) in FILE_FIXTURES.iter().copied() {
        eprintln!("fixture library: {name}");
        check_typescript_output(&scaffold, &generate_typescript_files(schema));
    }

    // Fastify server.ts variant. Only `server.ts` differs from Express, so we
    // re-lint the same rich schemas (full surface, all-optional headers) plus the
    // fixture library — covering every Fastify route shape (path/query/body/
    // headers/multi-status/binary/multipart/errors) through tsc + eslint +
    // prettier, not just the snapshot.
    eprintln!("fastify: gen_api full surface");
    check_typescript_output(&scaffold, &generate_typescript_fastify_files(SCHEMA));
    check_typescript_output(&scaffold, &generate_typescript_fastify_files(HEADER_SCHEMA));
    // Locks the multi-status guard wrap (3 typed statuses) against prettier at
    // Fastify's deeper route indent — see MULTI_STATUS_WRAP_SCHEMA.
    eprintln!("fastify: multi-status guard wrap");
    check_typescript_output(
        &scaffold,
        &generate_typescript_fastify_files(MULTI_STATUS_WRAP_SCHEMA),
    );
    for (name, schema) in FILE_FIXTURES.iter().copied() {
        eprintln!("fastify fixture library: {name}");
        check_typescript_output(&scaffold, &generate_typescript_fastify_files(schema));
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
    // `DateTime` → `datetime` (+ import) across fields/list/nested/query/headers,
    // with `.isoformat()`/`fromisoformat` encoding. Checks black/ruff/mypy.
    check_python_output(
        &scaffold,
        &venv_bin,
        &generate_python_files(DATETIME_SCHEMA),
    );
    // `Uuid` → `uuid.UUID` (+ import) across fields/list/map/nested/query/headers
    // and bare responses, with `str()`/`UUID(...)` encoding. Checks black/ruff/mypy.
    check_python_output(&scaffold, &venv_bin, &generate_python_files(UUID_SCHEMA));
    // `Decimal` -> `decimal.Decimal` (+import) across fields/list/map/nested/
    // query/headers and bare responses, with `str()`/`Decimal(...)` encoding.
    check_python_output(&scaffold, &venv_bin, &generate_python_files(DECIMAL_SCHEMA));
    // Composite `Money`: pydantic model + currency `field_validator`.
    check_python_output(&scaffold, &venv_bin, &generate_python_files(MONEY_SCHEMA));
    // `Money` as models.py's last definition (no trailing user model) — exercises
    // the blank-line/trailing-newline tail of `emit_money_model`.
    check_python_output(
        &scaffold,
        &venv_bin,
        &generate_python_files(MONEY_ONLY_SCHEMA),
    );
    // Enum query/header params: FastAPI enum coercion (422) + enum defaults.
    check_python_output(
        &scaffold,
        &venv_bin,
        &generate_python_files(ENUM_PARAM_SCHEMA),
    );
    // Inline response projection: `<Endpoint>Response` pydantic model.
    check_python_output(
        &scaffold,
        &venv_bin,
        &generate_python_files(PROJECTION_SCHEMA),
    );
    // List params: FastAPI `list[T] = Query(default_factory=list)` + comma-split
    // request headers.
    check_python_output(
        &scaffold,
        &venv_bin,
        &generate_python_files(LIST_PARAM_SCHEMA),
    );
    // `Url` (`Annotated[str, BeforeValidator]`) + `Bytes` (custom `Annotated[bytes,
    // BeforeValidator, PlainSerializer]` alias).
    check_python_output(
        &scaffold,
        &venv_bin,
        &generate_python_files(URL_BYTES_SCHEMA),
    );
    check_python_output(
        &scaffold,
        &venv_bin,
        &generate_python_files(RESERVED_WORDS_SCHEMA),
    );

    // Realistic schema fixture library (see FILE_FIXTURES).
    for (name, schema) in FILE_FIXTURES.iter().copied() {
        eprintln!("fixture library: {name}");
        check_python_output(&scaffold, &venv_bin, &generate_python_files(schema));
    }
}
