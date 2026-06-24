# Phoenix Gen — Design Decisions

Design decisions (the what and the why) for **Phoenix Gen**, the multi-language
API codegen tool. The main language/compiler decisions live in
[design-decisions.md](design-decisions.md); the user-facing feature set lives in
[phoenix-gen.md](phoenix-gen.md) and the path to v1.0 in
[phoenix-gen-roadmap.md](phoenix-gen-roadmap.md).

Each section records a locked decision and its rationale. Entries are dated and
ordered roughly chronologically.

---

## Phoenix Gen v1.0 — resolved open decisions (2026-05-30)

The [Phoenix Gen roadmap](phoenix-gen-roadmap.md) §9 listed several open decisions, five of which block downstream v1.0 work. They are recorded here as the **current working direction** — each is the roadmap's own recommendation, adopted as the decision of record. They can be revisited if a strong reason emerges, but absent that they are the plan.

1. **v1.0 server-framework list (locked)** → TypeScript: Express + Fastify; Python: FastAPI; Go: `net/http` + chi. Rationale: a small, popular set covers most users while bounding maintenance cost; lock it before beta.
2. **Pagination shape** → Support **both** cursor and offset pagination, selected via an explicit annotation on the response type. Rationale: it is the most common API shape and forcing a single style would push teams to reinvent the other.

---

## Phoenix Gen — headers feature design (2026-06-04)

Adds request **and** response headers to the endpoint schema. Request headers are
the close analog of `query` params (typed endpoint inputs threaded into the
client signature, sent on the wire, parsed server-side into handler args).
Response headers are new shape: the handler *sets* them and the client *reads*
them. Locked decisions:

1. **Grammar — a `headers { ... }` block** parallel to `query { ... }`, holding
   request headers, plus response headers declared on the response (see §5). New
   reserved keyword: `headers` — a **breaking change** to the surface language:
   any existing schema using `headers` as an identifier (struct field, param,
   binding) will no longer parse. Accepted pre-1.0. Each entry:
   `Type name [as "Wire-Name"] [= default]`.
   Optionality via `Option<T>` (same as query). No `where` constraints (headers
   are leaf values, like query params).

2. **Wire naming — hybrid (auto-transform + explicit override).**
   - **Default (auto):** the camelCase identifier maps to a `Title-Case-Kebab`
     HTTP header name: `authorization → Authorization`,
     `idempotencyKey → Idempotency-Key`, `xRequestId → X-Request-Id`. This is the
     trivial-case ergonomic path and adds zero per-field syntax.
   - **Override (explicit):** `Type name as "Exact-Wire-Name"` pins the wire name
     verbatim for headers whose casing is externally fixed (e.g.
     `rateLimit as "X-RateLimit-Limit"`, `etag as "ETag"`). Reuses the existing
     `as` keyword — no new reserved word — and keeps `=` free for the default
     value. Order: `Type name [as "..."] [= default]`.
   - The wire name (auto or override) is the single source of truth for BOTH
     directions and for the OpenAPI `in: header` parameter name. Internally each
     target still uses its idiomatic local name (Go camelCase, Python snake_case,
     TS camelCase) and aliases to the wire name.

3. **Request headers** behave like query params per target: client method input,
   sent via the framework's header API (`req.Header.Set` / `headers={}` /
   fetch `headers`), parsed server-side (`r.Header.Get` / FastAPI `Header(alias=)`
   / express `req.header(...)`) into the handler signature.
   - **Scalar wire encoding (cross-language contract).** Non-string headers are
     stringified for the wire identically across every target so a header
     round-trips regardless of which client talks to which server. In particular
     `Bool` is always lowercase `true`/`false` on every target, and is read back
     with a lowercase `== "true"` check. Python's `str(True)` → `"True"`
     is deliberately NOT used — the TS server's `=== "true"` read would reject it.

4. **Response headers — typed envelope.** An endpoint that declares response
   headers returns a generated wrapper type bundling the body + each response
   header (typed); endpoints WITHOUT response headers keep returning the bare
   body unchanged (no churn for the common case). Example (Go): an endpoint with
   response header `ratelimitRemaining` →
   `GetPost(id string) (*GetPostResult, error)` where
   `GetPostResult { Body Post; RatelimitRemaining int64 }` (the field types are
   the targets' resolved scalar types — Go `Int` → `int64`); an endpoint without →
   `GetUser(id string) (*User, error)` unchanged. Chosen over a mutable
   setter/out-param because it preserves the pure-function handler shape and is
   symmetric across client/server. The envelope type name is derived from the
   endpoint (e.g. `<Endpoint>Result`). A response header may NOT carry a `= default`
   (the handler sets it; a default is meaningless) — sema rejects it rather than
   silently dropping it. Optionality is `Option<T>` only.

5. **Response-header declaration site.** Response headers attach to the response,
   distinct from the request `headers { }` block. Finalized surface:
   `response Post headers { ratelimitRemaining: Int as "X-RateLimit-Remaining" }`,
   where the `headers` keyword must appear **on the same line as the response
   type** to bind as response headers. A `headers` block on its own line is always
   the standalone request section, regardless of whether it comes before or after
   `response` — so section ordering stays free and a request header cannot
   silently rebind to the response. (The parser deliberately does not skip
   newlines between the response type and an inline `headers` block.)

6. **Scope of the first increment:** request + response headers, all four targets
   (TS/Python/Go/OpenAPI). Constraint/validation behavior on headers is out of scope for v1
   (no `where`); auth remains middleware-shaped per roadmap §4 — headers are the
   transport, not an auth model.

7. **Framework-managed response headers (caveat).** Some server frameworks set
   transport headers automatically (Express's auto-`ETag`, `Content-Length`,
   etc.). Because HTTP header names are case-insensitive, a response header whose
   wire name collides with one of these (e.g. an `etag` header → `Etag`) can be
   overwritten by the framework's value before the client reads it. The generated
   code emits only a router/handler factory, not the full app, so disabling the
   framework default is the caller's responsibility (e.g. `app.set("etag", false)`
   for Express). Documented here so the
   collision is explicit rather than a silent surprise; a future increment may
   warn at codegen time when a response-header wire name shadows a known
   framework-managed header.

8. **Header validation (sema).** Because headers add identifiers to the generated
   parameter scope and to the wire, sema rejects the collisions that would
   otherwise surface as a generated-code compile error or a silent wire bug:
   - A **request header** local name may not collide with a path param, a query
     param, or another request header (they share one generated parameter list).
   - A **response header** local name may not collide with another response header
     or the reserved `body` field of the `<Endpoint>Result` envelope.
   - No two headers in the **same direction** may resolve to the same wire name
     (checked case-insensitively, since HTTP header names are). Request and
     response headers are different directions and share no namespace.

**Known limitation (defaulted inputs).** A defaulted request header (`maxStale: Int
= 60`) does not produce a uniform client shape across targets — it inherits each
target's existing defaulted *query*-param behavior, which itself diverges (Go/Python
always send it, TypeScript omits it when unset). This is a pre-existing,
cross-cutting generator convention question, not a headers-specific decision, so it
is tracked as a limitation in
[known-issues.md](known-issues.md#defaulted-request-and-query-inputs-diverge-per-target-and-mostly-cant-trigger-the-server-default),
not here.

---

## Phoenix Gen — multipart / file-upload (and download) design (2026-06-05)

Adds a `File` primitive type so endpoints can carry binary uploads (multipart
request bodies) and downloads (binary response bodies). Locked decisions:

1. **`File` is a new primitive `Type` variant** (alongside Int/Float/String/Bool/
   Void), recognized as a built-in type name.

2. **Implicit multipart — no new body grammar.** A multipart/binary body is a
   normal struct that *contains* a `File` field; there is NO dedicated
   `multipart { }` block. `struct AvatarUpload { avatar: File  caption: String }` +
   `body AvatarUpload` IS the multipart upload. Rationale: matches the type-driven
   model of every target (OpenAPI `format: binary`, FastAPI `UploadFile`, Go
   `multipart.FileHeader`), composes with the existing derived-body machinery
   (omit/pick/partial — bodies are always struct-derived today), and adds the
   minimum to the frozen grammar. The
   load-bearing invariant: **a `File` cannot be JSON-serialized, so a body
   containing a `File` is *necessarily* multipart/binary** — detection is
   type-determined, not a heuristic.

3. **Direction asymmetry (request vs response).**
   - **Request body** (upload): may freely mix one-or-more `File` fields with
     scalar fields → `multipart/form-data`. (That is exactly what multipart is
     for.)
   - **Response body** (download): a struct used as a response body containing a
     `File` must contain **exactly one `File` and no other fields** — a binary
     stream cannot be multiplexed with JSON fields in one response body. Sema
     rejects a mixed response body. The generated client reads a stream/blob; the
     server streams the file; OpenAPI marks the response content binary.

4. **`File` scope — endpoint bodies only (for now), restriction liftable.** `File`
   is valid only in a field of a struct used as an endpoint request or response
   body. Sema rejects `File` elsewhere (function params, variables, regular struct
   data, query params, headers). Because endpoints are compile-time-only (never
   lowered to IR), `File` never reaches the execution pipeline.
   - **Transitive rule:** a struct that contains a `File` (directly) is
     "body-only" — sema forbids using it as a regular runtime value
     (instantiation in normal code, function param/return, variable). It is legal
     only in `body`/`response` position.
   - **Forward-compat:** when the language eventually gains real file-handle
     semantics (far off — see roadmap), this is a *relaxation*: drop the sema
     restriction and replace the `unreachable!` with real lowering. Relaxing a
     restriction is non-breaking — every schema valid today stays valid, and a
     File-containing struct simply becomes a normal struct usable everywhere. The
     body-meaning ("File in a request body = the uploaded file; in a response body
     = the downloaded file") stays true and unifies with the future handle type.
     The name `File` is chosen deliberately for this continuity.

5. **Per-target codegen** (parallel to the JSON body path, branched on "body
   contains a File"):
   - **TypeScript**: client builds `FormData` (append files + scalar fields),
     omits explicit `Content-Type` (browser/runtime sets the boundary); server
     uses multipart parsing (multer/busboy) instead of `req.body` JSON. Download:
     client reads `response.blob()`/stream; server streams the file.
   - **Python/FastAPI**: server params become `UploadFile = File(...)` +
     `Form(...)` for scalars; the client takes each file field as a `FileUpload`
     dataclass (filename + content bytes) and sends `(upload.filename,
     upload.content)` in httpx's `files=` (so the caller-supplied filename
     travels on the wire — parity with Go's `FileUpload` and a TS `File`/`Blob`),
     scalars via `data=`. Download: `Response`/`StreamingResponse`; client reads
     `response.content`.
   - **Go**: client builds `multipart.Writer` (CreateFormFile + WriteField),
     sets `FormDataContentType()`; server uses `r.ParseMultipartForm` +
     `r.FormFile`. Download: `io.Copy` to the `ResponseWriter`; client reads
     `resp.Body`.
   - **OpenAPI**: request body `multipart/form-data` with file fields as
     `type: string, format: binary`; response content binary for downloads.

   **Buffering (all targets, this slice):** uploads and downloads are fully
   buffered in memory, not streamed — the client holds the file bytes
   (`FileUpload.content` / a `Blob`), and the server reads/returns whole-body
   bytes (`response.content`, `Response(content=...)`, `io.Copy` over the full
   body). This keeps the generated code simple and uniform across targets; true
   streaming (`StreamingResponse`, chunked `io.Reader` plumbing, `ReadableStream`)
   is a demand-triggered follow-up if large-payload endpoints need it.

6. **Scope of this slice:** the `File` primitive + multipart request bodies
   (uploads) + binary response bodies (downloads), all four targets. If downloads
   prove to carry enough per-target streaming nuance to balloon the slice, they
   split into a clean follow-up — flagged at that point, not assumed.

### `File` scope rules (sema-enforced)

The restrictions above are enforced by sema. The user-observable rules:

- **`Option<File>` is ALLOWED as a struct field** (optional file upload).
  **`List<File>`, `Map<String, File>`, and every other generic over `File` are
  REJECTED** — multiple-file arrays add per-target complexity and are deferred
  (known limitation; liftable later). `Option<File>` counts as carrying a `File`
  for both the body-only and multipart determinations.
- **Transitive body-only rule.** A struct that (directly) contains a `File` (or
  `Option<File>`) field may be used only as a **direct** endpoint request or
  response type. `response List<Doc>` / `response Option<Doc>` (where `Doc` is
  file-bearing) are rejected — a `File` cannot be JSON-serialized inside a
  list/option. Every other position (function param/return, `let`, nested struct
  field, enum payload, generic arg, type alias) rejects a file-bearing struct.
- **Direction asymmetry.** Request bodies may mix `File` + scalar fields
  (multipart). A `File`-bearing *response* struct must contain exactly one
  field, of type `File`, and nothing else (pure binary download).
- **Binary download excludes response headers.** A binary download's response
  body is the raw file stream; there is no `<Endpoint>Result` envelope to carry
  typed response-header fields (every target returns a stream/blob/`Response`
  for it). A binary-download endpoint that also declares `headers { … }` on its
  response therefore has no coherent generated shape, so the combination is
  rejected rather than silently dropping the headers or emitting contradictory
  code.
- **Multipart fields are scalar-or-file.** A `multipart/form-data` part is text
  (or a file) on the wire, so every *non-file* field of a multipart request body
  must be a scalar (`Int`/`Float`/`Bool`/`String`) or `Option<scalar>`. A
  `List`, `Map`, nested struct, or enum field has no form encoding and is
  rejected rather than emitted as broken client/server code. (A non-multipart
  JSON body keeps its full type freedom — this rule fires only once a body
  contains a `File`.)
- **OpenAPI `required` excludes `Option<T>` body fields (behavior change).**
  While wiring multipart schemas, `Option<T>` fields were excluded from a body
  schema's `required` array. This also corrects *plain JSON* bodies: an
  `Option<String>` body field is no longer emitted as `required`. The fix is
  correct (an optional field is not required) but it changes pre-existing
  JSON-body OpenAPI output — a consumer that relied on the old (incorrect)
  `required` set will see the field drop out.

---

## Phoenix Gen — pagination design (2026-06-06)

Adds first-class cursor and offset pagination, the single most common API shape
("every team reinvents it"). Locked decisions:

1. **Surface — a `pagination { <mode> }` endpoint block**, a named section peer to
   `query` / `headers` / `response` / `error`, NOT a response-type modifier.
   Rationale: pagination spans request *and* response, so attaching it to the
   response type alone understates it; a named block matches the grammar's
   "concerns are blocks" pattern and keeps the mode on its own clear line. `<mode>`
   is `offset` or `cursor`. New contextual handling for `pagination` + the two
   mode words (prefer contextual identifiers over new reserved keywords where the
   lexer allows, as `version` did for `api`).

2. **Scope — envelope only; the user declares the request inputs (Approach 2).**
   Declaring `pagination { offset }` generates the response *envelope* only. The
   pagination *inputs* (`page`/`limit`, `cursor`/`limit`) are written by the user
   in the normal `query { }` block and flow through the existing query-param
   machinery untouched. Rationale: Phoenix can't know the right param names or
   defaults for every API; reusing `query` keeps inputs explicit and flexible and
   adds zero new input machinery. The `pagination` block governs the response
   shape, nothing else.

3. **Envelope fields — fixed canonical per mode, grammar extensible.** Phoenix
   fixes the standard fields (the opinionated convention that makes pagination
   first-class — a team wanting a bespoke shape uses a plain struct + `response`):
   - **offset** → `<Endpoint>Page { items: List<T>, totalCount: Int }`. `totalCount`
     is the defining offset signal (enables "page X of Y" / jump-to-last).
   - **cursor** → `<Endpoint>Page { items: List<T>, nextCursor: Option<String> }`.
     `nextCursor` null/absent = last page.
   The handler **supplies** the metadata values (Phoenix cannot compute a total or
   a cursor); Phoenix only types the envelope shape and wires it onto the response
   body. The block grammar is a natural subset of a future
   `pagination { offset  <extra fields> }`, so additive fields (e.g. `hasMore`)
   are a non-breaking later slice — ship minimal-per-mode now, let demand pull
   extras. Minimal-canonical chosen over batteries-included (no forcing every
   offset handler to compute a COUNT for a `hasMore` it may not want).
   **Cross-target wire-name caveat:** the metadata field name follows each
   target's pre-existing model convention — camelCase (`totalCount`/`nextCursor`)
   on Go/TS/OpenAPI, snake_case (`total_count`/`next_cursor`) on Python (the
   Python generator emits no `Field(alias=...)` on any model, so the wire form is
   snake_case). Same-language client↔server (incl. the round-trip suite) agree, so
   this is not a round-trip bug — but it does mean a Python client cannot be mixed
   with a Go/TS/OpenAPI server, and for offset this now lands on a *required*
   field (`total_count` vs `totalCount`) rather than only optional struct fields.
   This is the same pre-existing Python wire-name divergence affecting every
   model, not something pagination introduces; a future `Field(alias=...)` pass on
   the Python generator would unify all of them at once.

4. **Response must be `List<T>`.** `pagination { }` requires the endpoint's
   `response` to be a bare `List<T>`; the envelope's `items` is that same
   `List<T>`. Sema rejects pagination on a non-list response. **`Option<List<T>>`
   is explicitly rejected** (not merely unsupported): a paginated call always
   returns a page; emptiness is `items: []` inside the envelope, so a *null page*
   is meaningless/ambiguous. A struct that already nests a list is manual
   pagination — use a plain `response`, not this block.

5. **Naming.** Envelope type is `<Endpoint>Page` (distinct from the response-headers
   `<Endpoint>Result`, and reads clearly at the call site, e.g. `ListPostsPage`).
   The list field is always `items`. These are user-facing.

6. **Inputs are NOT validated against the mode (decoupled).** Per Approach 2, sema
   does not require an offset endpoint to declare `page`/`limit`, nor a cursor
   endpoint to declare `cursor`. The block governs only the envelope; input
   correctness is the user's responsibility (a lint could be added later). Keeps
   the two halves decoupled and the query machinery untouched.

7. **Pagination + response headers on the same endpoint: REJECTED for v1.**
   Both features wrap the handler's single return value in a generated envelope
   (`<Endpoint>Result` for headers, `<Endpoint>Page` for pagination), and a
   handler has exactly one return slot — so the two envelope *types* cannot both
   be the return type. (On the wire they are orthogonal: pagination metadata rides
   in the response *body*, headers in HTTP *headers* — the collision is purely at
   the generated return-type level.) Sema rejects the combination with a clear
   message. It is rare, and the alternatives below are clean *additive* follow-ups
   (the headers envelope's existing `body` slot is the natural seam), so rejecting
   keeps this slice tight without painting us into a corner.
   - **Future option A — nest:** `<Endpoint>Result { body: <Endpoint>Page { items,
     totalCount }, <headers...> }`. Composes with minimal special-casing because
     the headers envelope already has a `body` slot pagination can fill. Cost: the
     user navigates `result.body.items`.
   - **Future option B — flat-merge:** `{ items, totalCount, <headers...> }` with
     codegen knowing which fields serialize to the body vs. become HTTP headers.
     Flattest for the user, most special-casing across all four targets.
   The user-facing "you'll hit a sema error if you combine them" angle is also
   noted in [known-issues.md](known-issues.md#pagination-and-response-headers-cannot-be-combined-on-one-endpoint-v1).

8. **Scope of this slice:** the `pagination { }` block (offset + cursor), envelope
   generation in all four targets (TS/Python/Go/OpenAPI), reusing the
   response-envelope precedent from headers. OpenAPI emits the `<Endpoint>Page`
   object schema as the 200 response body.

9. **Route-ordering fix (surfaced by this slice, not pagination-specific).** The
   pagination round-trip exposed a latent bug in the TypeScript (Express) and
   Python (FastAPI) server generators: both frameworks match routes
   **first-registered-wins**, and the generators emitted routes in schema source
   order, so a parametric route (`/api/posts/{id}`) declared before a static
   sibling (`/api/posts/paged`) **shadowed** the static one — the static path was
   captured as `id = "paged"` and dispatched to the wrong handler. Fix: both
   generators now register routes **most-specific (most-static) first**
   (per-segment static-before-`{param}` ordering, stable for equal specificity),
   matching the most-specific-wins semantics Go's `net/http.ServeMux` (1.22+)
   already provides — Go needed no change, OpenAPI has no routing. This is a
   general correctness fix (it also covers e.g. `/users/me` vs `/users/{id}`).

---

## Phoenix Gen — multi-status responses design (2026-06-07)

Adds multiple **success status codes** to one endpoint (e.g. a create-or-update
returning `200` when it updated or `201` when it created). Scoped deliberately to
**multi-status, NOT content negotiation**: the roadmap's
`response { 200: User, 200 text: String }` sketch bundles two features —
(a) multiple status codes and (b) multiple content-types per status (Accept-header
negotiation with union return types). We are doing (a) only. (b) — content
negotiation — is the expensive part (runtime client dispatch on the response
`Content-Type`, union/sum return types that have no clean Go representation) and is
deferred; see "Deferred" below. Locked decisions:

1. **Shared body type across statuses (Option A — no unions).** All typed statuses
   in a `response { }` block must share ONE body type. `response { 200: User
   201: User }` is allowed; `response { 200: User  201: Receipt }` (different body
   types per status) is REJECTED by sema. Rationale: differing body types per
   status is a discriminated union, which has no idiomatic Go representation
   (`interface{}`/hand-rolled wrapper) and reintroduces exactly the
   content-negotiation complexity this slice avoids. The common real cases
   (create-or-update 200/201, accepted-vs-done 202/200) all carry the same body.
   Endpoints genuinely needing different shapes per status use separate endpoints
   or `error { }` variants. (Allowing differing types later is an additive
   extension if demand appears.)

2. **Grammar — a `response { <status>[: Type] ... }` block** alongside the
   existing bare `response Type`. The bare form is unchanged (implicit `200`, no
   envelope). The block form lists one or more success statuses, each either typed
   (`200: User`) or **typeless** (`204` — no body). Typeless statuses may be mixed
   with typed ones (`200: User  204`). All typed entries must use the same type
   (decision 1). A typed entry must name a **struct** type: `List<T>`, scalars,
   `Option<T>`, and enums are rejected by sema (the bare `response List<Post>`
   etc. is unchanged). The envelope's `body: Option<T>` slot serializes through
   the struct machinery in every target, so a non-struct `T` would generate code
   that fails at runtime (Python's serialization in particular relies on pydantic
   model methods). Relaxing this later (e.g. via pydantic `TypeAdapter`) is additive.
   Status codes must be in the success range (2xx); failures stay in
   the `error { }` block. Duplicate status codes are rejected. Bodyless statuses
   (`204` No Content, `205` Reset Content) must be typeless: HTTP (RFC 9110)
   forbids a body on them, and the generated servers could not honor a typed
   entry either way — on a 204, Go's `net/http` and Express silently drop body
   writes; on a 205 (which neither framework suppresses) the body would hit the
   wire as an illegal response. So a typed `204: T` or `205: T` is a contract
   the wire cannot honor — sema rejects it. An empty `response { }` is
   a parse error (it would otherwise silently mean "no response declared").

3. **Return shape — a status-carrying envelope `<Endpoint>Response { status: Int,
   body: Option<T> }`.** A `response { }` block makes the handler return, and the
   client observe, this envelope (vs. the bare body for a plain `response Type`).
   `status` is the actual HTTP status; the handler sets it, the server writes it,
   the client reads it. **`body` is ALWAYS `Option<T>`** — uniform across all
   blocks regardless of whether a typeless status is present. Rationale: one
   envelope shape = one codegen path per target (simpler, fewer branches); the
   caller unwraps the Option once. (A block with only typeless statuses — e.g.
   `response { 202  204 }` — has no `T`; the envelope is just `{ status: Int }`
   with no `body` field.) The envelope type name is `<Endpoint>Response` (distinct
   from the response-headers `<Endpoint>Result` and the pagination
   `<Endpoint>Page`).

4. **Composition — multi-status is mutually exclusive with response headers AND
   with pagination (v1).** All three wrap the handler's single return value in a
   generated envelope (`<Endpoint>Response` / `<Endpoint>Result` /
   `<Endpoint>Page`), and one return slot can hold only one envelope type — the
   same constraint that already makes headers and pagination mutually exclusive.
   Sema rejects multi-status + pagination; the parser rejects an inline
   `headers { ... }` after a `response { }` block (the response-header spelling)
   with a targeted error — without that, the trailing block would re-dispatch as
   the standalone REQUEST `headers` section and silently change semantics. Rare
   combination; rejecting keeps the slice tight. The user-facing note is recorded
   in
   [known-issues.md](known-issues.md#multi-status-responses-cannot-be-combined-with-response-headers-or-pagination-v1).
   Future option (additive, non-breaking): nest the envelopes (the
   `<Endpoint>Result` envelope's `body` slot could hold a `<Endpoint>Response`),
   per the same reasoning recorded for headers+pagination.

5. **Per-target codegen.** The server writes the handler-chosen status code
   (instead of the hardcoded 200/204) and serializes `body` when present. The
   client reads the status into `status` and parses the body (when the response
   carries one) into `body: Option<T>`. Clients detect an empty body by
   **content**, never by special-casing a status code — any typeless status
   (202, 204, …) sends an empty body, not just 204 (Go: `ContentLength`/EOF
   tolerance; TypeScript: non-empty `response.text()`; Python:
   `response.content`). The server **validates the handler-chosen envelope
   against the declared contract** before writing it and answers 500 on a
   mismatch — three guards, all handler bugs reported instead of written to the
   wire:
   - *undeclared status* ("handler returned undeclared status"): a buggy
     handler can return a zero-value envelope (Go's `WriteHeader(0)` panics,
     Express's `res.status(0)` throws) or smuggle a 4xx through the success
     envelope past the `error { }` mapping;
   - *body on a typeless status* ("handler returned a body for a bodyless
     status"): the frameworks only suppress bodies on 204/304 (plus 1xx in
     Go), so a body paired with e.g. a typeless 202 WOULD hit the wire — and
     the content-guarded client would parse it, silently violating the
     contract;
   - *missing body on a typed status* ("handler returned no body for a typed
     status"): the contract — and the emitted OpenAPI spec — promise a body
     there; without the guard the client would surface a contract-violating
     absent body.
   An all-typeless block has no body field, so only the membership guard
   applies there. **Clients are deliberately lenient**: they envelope whatever
   success status the wire delivers without checking it against the declared
   set — only the server enforces the contract. A generated client may be
   pointed at a non-Phoenix implementation of the same API, and failing hard
   on an undeclared 2xx would help nobody; the caller sees the real status and
   can decide. (Minor target divergence at the success/redirect edge: the TS
   client throws on any non-2xx via `!response.ok`, while the Go client
   (`>= 400`) and the Python client (`raise_for_status()`) would envelope a
   3xx — unreachable in practice, since redirects are auto-followed and the
   generated clients never send conditional headers.) JSON content-type
   throughout (no negotiation — decision is multi-status only). OpenAPI lists
   each declared status as a separate entry in the operation's `responses`
   map, each with the shared `T` schema (or no content for a typeless status)
   — OpenAPI represents this natively and needs no envelope.

6. **Representation — bare form stays primary.** A bare `response Type` carries no
   multi-status data — `response` stays the single source of truth, so every
   existing endpoint's generated output is byte-identical (no churn for the
   non-multi-status case, mirroring how headers/pagination left non-using
   endpoints unchanged). A `response { }` block carries the per-status data (its
   presence is the multi-status signal to codegen) and mirrors the shared body
   type `T` back into `response` so downstream "what is the success body type"
   reads keep working. (An earlier sketch instead lowered the bare form to an
   implicit single-200 entry; the empty-for-bare representation was chosen
   because it needs no special-casing to keep existing output unchanged.)

7. **Scope of this slice:** multi-status success responses (shared body type +
   typeless statuses), all four targets. Content negotiation (multiple
   content-types per status, union returns, Accept dispatch) is OUT.

### Deferred — content negotiation (the other half of the roadmap sketch)
Multiple content-types at one status (`200 json: User`, `200 text: String`) with
Accept-header dispatch and union return types is deferred indefinitely. It is the
high-complexity / low-frequency half: it forces runtime client dispatch on the
response `Content-Type` and a sum/union return type that Go cannot express
idiomatically (only `interface{}` or a generated discriminated wrapper), which
would undercut the "idiomatic per-target output" quality bar. Revisit only if real
demand appears; the multi-status grammar above leaves room to add a per-status
content-type qualifier later without breaking existing schemas.

### Generated-type-name collision check
An endpoint declaration synthesizes up to five type names in the generated
output: the envelopes `<Endpoint>Result` / `<Endpoint>Page` / `<Endpoint>Response`
(mutually exclusive), plus the request-body types `<Endpoint>Body` (any `body`
clause; combinable with the envelopes) and `<Endpoint>ClientBody` (Go only,
multipart bodies). Multi-status made a collision materially more likely —
`Response` is a natural user struct name. In Go/TS a collision is a loud
generated-code compile error, but in Python a duplicate `class X(BaseModel)` is a
**silent redefinition** (last wins) — a quiet miscompile. So sema rejects, with a
clear message, a user-defined struct or enum named exactly like one of an
endpoint's synthesized type names. The check only fires when the feature is
actually declared (a like-named struct alongside a plain endpoint is fine — no
false positives). Related rules that landed with it:

- **Endpoint-vs-endpoint name collisions** are rejected at the *exported-name*
  level. Endpoint names are unique only case-sensitively, so `getUser` and
  `GetUser` are both distinct names — but Go builds the client method, server
  method, and handler-interface method from the capitalized name, so that pair
  emits two `GetUser` methods on one struct, a Go compile error **regardless of
  what else the endpoints declare**. (TS/Python keep the name as written and are
  unaffected, but sema is target-agnostic, matching how `ClientBody` is reserved
  on every target.) The predicate is exported-name equality, not full
  case-insensitivity — `getUser` / `getuSer` export as distinct Go methods and
  stay legal. Because every generated type name is `exported + suffix`, this
  name-level check subsumes all *same-stem* type collisions, leaving one case
  live: cross-stem suffix overlap. `"ClientBody"` ends with `"Body"`, so `upload`
  (multipart) and `uploadClient` (any body) both generate `UploadClientBody`
  despite distinct stems — the only suffix-overlap pair among the five, and
  worst here because codegen's derived-type dedupe is first-wins in every
  backend, so without the check the second endpoint silently bound to the first
  one's struct.
- The fixed-name multipart helper `FileUpload` (Go; emitted once, shared by every
  multipart endpoint) is reserved too — a user type of that name duplicates the
  declaration in generated Go.
- Known scope limit: the user-type lookup resolves in the endpoint's module scope
  while the name claims are global — if endpoints ever live in non-entry modules,
  a same-named type in a sibling module would be missed.

### Grammar — comma-separated entries in `error { }` / `response { }`
Both `error { }` and the new `response { }` block accept an optional comma after
each entry (`error { NotFound(404), Conflict(409) }`), matching the forgiving
`omit { a, b }` field-list style. The comma spelling is clearly the habit users
will bring (the roadmap's own sketch used it). Endpoint sections remain
canonically newline-separated; the comma is tolerated, not required.

---

## Phoenix Gen — type-system gaps surfaced by the fixture library (2026-06-09)

The §6 fixture library (six realistic schemas: payments, multitenant_saas,
webhooks, file_storage, social, internal_admin — ~2,900 lines) was written to
stress the generators against real API shapes. As the roadmap predicted ("adding
a fixture often surfaces a missing schema feature — that's the point"), the
exercise produced a consistent audit of what realistic APIs *want* that Phoenix
Gen's type/feature surface doesn't yet express. Recorded here as a forward-looking
list (these are scope/roadmap items, not bugs; the genuine *bugs* found are in
known-issues.md). Every fixture independently hit the same first three, which is
the strongest signal:

**Missing primitive types (hit by nearly every fixture):**
- **DateTime / timestamp** — modeled everywhere as `Int` Unix epoch seconds. The
  single most-wanted missing type; every fixture has created/updated/expires
  fields. A native instant type would also let codegen emit `string`/`datetime`
  with `format: date-time` in OpenAPI instead of a bare integer.
- **UUID / opaque id** — modeled as `String`. Every fixture's ids and tokens. A
  distinct id type would enable `format: uuid` and stronger typing.
- **Money / Decimal** — payments modeled amounts as `Int` minor units (cents).
  No fixed-precision decimal exists; a payments domain really wants currency-aware
  money.
- **bytes / binary scalar** — checksums, signatures, raw tokens modeled as
  `String`. `File` exists but only in endpoint-body position, not as a value type.
- **URL** — destination/avatar/media URLs modeled as validated `String`s
  (e.g. `self.contains("https://")`). Lower-value than the four above, but hit
  by webhooks (subscription destinations) and social (avatar/media URLs); a
  native type would enable `format: uri` in OpenAPI.

**Feature/expressiveness gaps:**
- **Enum-typed query / filter params** — a `query { Status status }` filter can't
  use an enum type; it degrades to `Option<String>` the handler must re-parse.
  Hit by social, internal_admin (admin filters), webhooks (status filters).
- **Enum fields in multipart bodies** — a `File`-bearing (multipart) body's
  non-file fields must be scalar/`Option`-scalar; an enum field had to become a
  `String` (file_storage `StorageClass` → `storageClassName`).
- **Inline response projection** — there is no `response Struct pick { ... }` /
  `omit`; a read-only/lightweight response shape (public profile, usage summary)
  must be declared as its own dedicated struct. Hit by social (`PublicProfile`),
  file_storage (`BucketUsage`).
- **Constraints on optionals are asymmetric** — `where self.length > 0` is
  *accepted* on `Option<String>` but `.contains(...)` on `Option<String>` and
  numeric comparison on `Option<Int>` are rejected. Caveat: "accepted" is not
  "validated" — `.length` parses as a field access, which sema silently skips on
  non-struct types, so the constraint is unchecked rather than unwrapped.
  (Tracked as two bugs in known-issues — the inconsistency plus the silent
  field-access skip; the broader "constraints on optionals" story is a design
  question.)
- **Pagination + response-headers can't co-occur** — a paginated feed can't also
  carry rate-limit response headers (the one-envelope rule, decision recorded in
  the pagination/multi-status sections). Hit by social (`getHomeFeed`).
- **No list-valued query params** — a batch endpoint can't declare
  `List<String>` in a `query { }` block; ids arrive as a comma-separated
  `String` the handler must split. Hit by social (`batchReactionCounts`).
- **No reusable header sets** — response headers (e.g. a standard rate-limit
  trio) are declared inline per endpoint; no way to define and share a header
  group.
- **No Range-request / partial-content representation** — file_storage can only
  express a full binary download, not byte-range/partial reads.

**Prioritization read.** The three primitive gaps (DateTime, UUID, Money) are the
highest-value because they're universal and would immediately improve generated
type fidelity (and OpenAPI `format`s). They are additive type-system work, not
breaking changes. The feature gaps (enum query params, inline response
projection) are smaller, additive, and demand-rankable. None of these block the
existing slices — they are the natural "what's next for the schema language after
the v1.0 must-adds" backlog, surfaced empirically rather than guessed.

## Phoenix Gen — DateTime & UUID scalar types (2026-06-16)

The first cut at closing the "missing primitive types" gap above. Of the three
top-ranked primitives (DateTime, UUID, Money), **this work ships both `DateTime`
and `Uuid`** (DateTime first, then UUID, each through the full
add→4-generators→compile-lint→round-trip loop); **Money/Decimal is deferred** — it
carries currency-awareness and fixed-precision-arithmetic questions (rounding
mode, scale, ISO-4217 coupling) that DateTime/UUID don't, so it's a design
discussion of its own rather than a mechanical "add a scalar" pass.

**These are first-class scalar types, NOT position-restricted like `File`.** A
`File` is a body-transport sentinel that sema forbids outside endpoint
body/response position; DateTime and UUID are ordinary values, legal in struct
fields, query params, request/response headers, and scalar response bodies. They
flow through `resolve_type_expr` as plain builtins (added to `Type::from_name`);
no scope gate is needed or wanted.

**Deferred: DateTime/UUID as a multipart form field.** The one position they do
*not* cover is a non-file field of a `File`-bearing (multipart) body, whose
scalars are still restricted to `Int`/`Float`/`Bool`/`String` (sema's
`is_multipart_field_type`). A `DateTime`/`Uuid` there is
rejected with the existing clean diagnostic rather than silently mis-encoded.
Timestamps/ids in a multipart upload are the rarest position; lifting the
whitelist (plus the per-target form encode/parse) is a small, additive follow-up.
Not a silent gap — it errors at check time with a precise message.

**Wire format is always a string.** DateTime serializes as an RFC 3339 / ISO 8601
instant string (`2026-06-16T12:00:00Z`); UUID as the canonical hyphenated uuid
string. Every target encodes/decodes them through its existing string path for
query/header/path positions — the only target-specific work is the in-memory body
representation, JSON revival (TS DateTime), and validation (see below).

**Per-target representation:**

| | Phoenix | Wire (JSON) | TypeScript | Python | Go | OpenAPI |
|---|---|---|---|---|---|---|
| `DateTime` | `Type::DateTime` | RFC 3339 string | `Date` (+ generated revival) | `datetime.datetime` | `time.Time` | `{type: string, format: date-time}` |
| `Uuid` | `Type::Uuid` | uuid string | branded `string` (`type Uuid = string & {…}`) + `parseUuid` validate-on-decode | `uuid.UUID` | `string` (regex-checked in `Validate()`) | `{type: string, format: uuid}` |

**Why TS DateTime = `Date` with generated revival, not `string`.** JS *has* a
`Date`, but `JSON.parse` never revives it — a parsed date field is a string at
runtime. Typing the field `Date` is therefore a lie unless codegen emits a
recursive revival pass that walks the decoded body and reconstructs `Date`s at the
DateTime field paths (`JSON.stringify` handles the reverse for free — it emits ISO
strings). We pay that generation cost because a `Date`-typed API is what TS users
expect and it's the whole point of a *typed* client. The revival runs on **both
sides**: the client revives the decoded response, and the server revives the
decoded *request body* (`express.json()` / Fastify's parser also yields strings)
before handing it to the handler — otherwise the handler's `Date`-typed body field
would be a raw string at runtime, the same lie on the inbound path. So a
Date-bearing request body emits a `revive<Endpoint>Body` (keyed on the derived
body fields) that the route calls on the cast/validated body. Query params and
request/response headers are coerced inline (`new Date(...)`), so the body is the
only position needing a generated reviver.

**UUID validation level: validated, no Go dependency** (chosen 2026-06-16). The
targets diverge on how much they validate a `Uuid`, and we did NOT add a UUID
library to any of them:
- **Python** — `uuid.UUID`; pydantic parses the wire string into a `UUID` on
  both server (request) and client (response), rejecting malformed input for free.
- **TypeScript** — a branded alias `type Uuid = string & { … }` (JS has no UUID
  type) PLUS a generated `parseUuid` that regex-checks the RFC 4122 format and
  brands the value. It reuses the same recursive decode pass as DateTime revival
  (`Uuid` → `parseUuid` where `DateTime` → `new Date`), so it validates on the
  client response decode AND the server request-body decode; query/request-header
  and response-header `Uuid`s are validated inline (`parseUuid`). The brand gives
  nominal distinctness (a bare `string` can't be passed as a `Uuid` without a
  cast); the regex gives a runtime guarantee.
- **Go** — `string` (no stdlib UUID type, and we add no dependency like
  `google/uuid` to keep the policy simple), format-checked by the generated
  `Validate()` via a package-level `uuidRe` (`regexp`). The check covers direct
  `Uuid` / `Option<Uuid>` struct & body fields; `List`/`Map` elements and
  query/header `Uuid`s are NOT checked — Go is the documented weak link, accepted
  for this slice. The server already calls `body.Validate()`, now also for
  uuid-bearing (not just constrained) bodies.

This deliberately leaves query/header `Uuid` validation as the per-target weak
spot (TS validates them, Go does not), mirroring how DateTime's server-side
handling is the weak spot there — bodies are where ids overwhelmingly live.
**Superseded 2026-06-18:** the Go query/request-header `Uuid` weak spot is closed
— scalar query/header params and `List<Uuid>`-valued query/header param elements
are now format-checked inline → 400, matching TS/Python. (Struct
`List<Uuid>`/`Map<String, Uuid>` *field* elements in `Validate()` remain
unchecked — a separate weak link, see the `Money` entry.) See *"Phoenix Gen —
tighten scalar query/header `Uuid`/`Decimal` validation on Go (2026-06-18)"*
below.

**Language-runtime semantics (`lower_type`).** Both `DateTime` and `Uuid` lower to
`IrType::StringRef` — a branded-string runtime representation. The Gen path
(`cmd_gen`) never lowers to IR, so this only
matters if a DateTime/Uuid-bearing struct is used in actual Phoenix *language*
code (`run`/`build`). Neither has literals or operations in the language yet
(opaque scalars), so a string-backed runtime identity is sufficient and correct,
and —
unlike `File`'s `unreachable!` arm — it can't panic if such a struct is ever
lowered. Liftable to a richer representation if the language later gains temporal
(or uuid) semantics.

## Phoenix Gen — Decimal scalar type (2026-06-16)

The third of the top-ranked "missing primitive types," closing the
`Int`-cents / `Float`-amount workaround the fixtures used. Shipped via the same
add→4-generators→compile-lint→round-trip loop as DateTime/UUID.

**Scope: `Decimal` only; `Money` is compose-your-own for now.** The fixture audit
asked for "currency-aware money," but `Money` is just `Decimal` + a currency, and
a general `Decimal` also covers rates/percentages/quantities/tax. So we ship the
`Decimal` primitive; a money amount is modeled as a user-defined struct
(`struct Money { amount: Decimal  currency: String }`) until there's demand for a
first-class type. **(Built-in `Money` shipped next — see the Money section below;
this Decimal-only framing was the initial cut.)**

**Wire format: JSON string** (`"19.99"`). The only representation that is exact in
all three targets: a JSON *number* is parsed to an IEEE-754 double in nearly every
parser, and in JS precision is unrecoverable (`JSON.parse` yields the double before
any hook runs). String is also what Stripe and most money APIs use, and JSON
Schema has no decimal type. Integer-minor-units was rejected (couples to currency
scale — JPY 0 / USD 2 / KWD 3 — and is exactly the workaround being replaced).

**Representation: transport-only, dependency-free (Decision 3 = option A).** Phoenix
Gen's job here is exact, typed *transport* — not arithmetic. No decimal library is
added to any generated target.

| | Phoenix | Wire (JSON) | TypeScript | Python | Go | OpenAPI |
|---|---|---|---|---|---|---|
| `Decimal` | `Type::Decimal` | decimal string | branded `string` (`type Decimal = string & {…}`) + `parseDecimal` validate-on-decode | `decimal.Decimal` (stdlib — exact arithmetic for free) | `string` (regex-checked in `Validate()`) | `{type: string, format: decimal}` |

This mirrors the UUID approach exactly (reuse the TS revival/`parse*` pipeline, Go
`Validate()` regex, Python's native type). It is deliberately **asymmetric**:
Python gets real `decimal.Decimal` arithmetic for free; Go/TS get an exact,
validated, distinctly-typed string with NO arithmetic — the user reaches for their
own decimal lib for math. This matches the established "Gen is a transport layer"
philosophy and the dependency-aversion of the UUID decision.

**Deferred to a future slice (documented commitment): real decimal arithmetic in
Go and TS** via MIT-licensed libraries — `shopspring/decimal` (Go) and
`decimal.js` / `big.js` (TS) — as an opt-in, so Go/TS users get ergonomic
add/multiply instead of string-only transport. (Confirm each library's license is
MIT at adoption per the MIT-only policy; the candidates are believed MIT, unlike
`google/uuid`'s BSD-3.) This is the natural companion to a built-in `Money`.

**Precision: arbitrary for now; fixed scale is the long-term goal.** v1 carries
whatever precision the wire string holds; validation only checks the value is a
well-formed decimal. **Deferred (documented goal): a fixed-scale annotation**
(`Decimal(2)` — a parameterized type, new parser/type-system surface) so a schema
can pin scale and the generators can round/validate against it.

**The smaller pieces (fall out of the above, same patterns as UUID):**
- Validation: a decimal-format regex (optional leading `-`, digits, optional
  `.` + fraction, optional `eE` exponent) reused through the TS `parseDecimal` and
  Go `Validate()` paths; Python's `Decimal(str)` raises on malformed input (free,
  both sides via pydantic). **Strictness diverges across targets:** Go/TS accept
  only finite base-10 numbers (the regex), while Python's `Decimal` also admits
  `NaN`/`Infinity` and bare-exponent forms. So a `Decimal("NaN")` produced by a
  Python client round-trips in Python but is rejected by a Go or TS server. This is
  intentional — those are not canonical decimal values — and is the Decimal analogue
  of the query-validation divergence below; a fixed-scale `Decimal(N)` would tighten
  all three to one grammar.
- `lower_type` → `IrType::StringRef`, same as DateTime/UUID.
- OpenAPI: `{type: string, format: decimal}` (no standard JSON-Schema decimal; the
  string + `format: decimal` convention; an optional `pattern` could pin the regex).
- Multipart form-field `Decimal`: same deferral as DateTime/UUID (sema's
  `is_multipart_field_type` whitelist), rejected cleanly until lifted.

## Phoenix Gen — Money composite type (2026-06-16)

The first *composite* built-in, and the "currency-aware money" the fixture audit
wanted — shipped right after `Decimal` (its `amount` is a `Decimal`). Currency
validation level: **full ISO-4217 code list** (chosen over a 3-letter regex), so a
non-conforming currency is rejected, not just a malformed shape.

**Wire format:** the object `{ "amount": "19.99", "currency": "USD" }` — `amount`
is exactly a `Decimal` (string, exact; inherits all Decimal handling), `currency`
an ISO-4217 alphabetic code.

**Modeling:** a `Type::Money` builtin treated as a composite — each generator
emits a `Money` type definition *once* (gated on `schema_uses_money`) and maps the
type to it. A composite isn't URL/header encodable, so `Money` is
**position-restricted to struct/body fields and responses**: sema rejects a
`Money` (or `Option`/`List`/`Map` of it) in query-param or header position
(`check_endpoint`, mirroring the multipart-field restriction). This keeps the
`schema_uses_money` emit-gate — which scans only those legal positions — from ever
having to face a `Money` it can't account for, so no target emits a dangling
reference. `lower_type` → `StringRef` (a never-hit placeholder; Gen never lowers
and the language has no `Money` literal).

| Target | `Money` representation | Currency validation |
|---|---|---|
| TypeScript | `interface Money { amount: Decimal; currency: string }` + `reviveMoney` (revives amount via `parseDecimal`, checks currency) — slots into the revival pipeline like a struct reviver | `CURRENCY_CODES` `Set`, checked in `reviveMoney` on decode (client response, server request body, nested list/map elements) |
| Python | `class Money(BaseModel) { amount: Decimal; currency: str }` + a `field_validator` | `_CURRENCY_CODES` `set`, checked by the validator on parse (server + client, free via pydantic) |
| Go | `type Money struct { Amount string; Currency string }` + `Validate()` | `currencyCodes` `map`, checked in `Validate()` (`decimalRe` for amount); **containing structs' `Validate()` recurse into `Money` fields** (new nested-validation) |
| OpenAPI | a shared `Money` component (`$ref`'d), `amount` `format: decimal` | `currency` `enum` of the full code list |

**The ISO-4217 list & MIT policy.** The active codes are factual data (an ISO
standard's alphabetic codes — not copyrightable), so they are **hand-authored** as
a single shared `crate::iso4217::ISO_4217_CODES` const (the "compute constants from
definitions" path of the MIT-only policy, not a vendored list). Each target emits
its own membership structure from it (TS `Set`, Python `set`, Go `map`, OpenAPI
`enum`). A well-formedness guard checks the authored codes (all `^[A-Z]{3}$`,
unique, ascending, plausible count); a differential test against an MIT reference
set is a possible future hardening. Scope: active
national/supranational transaction currencies (incl. `EUR`/`XOF`/`XAF`/`XPF`/
`XCD`/`XDR`); precious-metal/fund/test/no-currency `X` codes are excluded.

**Deferred (documented).** Money *arithmetic* (add/multiply across currencies)
stays out — transport-only, like `Decimal`; it arrives with the Go/TS decimal-lib
opt-in. Currency-aware rounding/scale per ISO-4217 minor units is future. A
`Money` field in a multipart body is rejected (same `is_multipart_field_type`
deferral as the scalars). Go's `Validate()` recurses into direct
`Money`/`Option<Money>` fields but NOT into `List<Money>`/`Map<String, Money>`
elements (Python/TS do validate those) — the documented weak link parallel to the
regex scalars; see [known-issues.md](known-issues.md).

### Fixture library adopts the new types (2026-06-17)

The realistic fixture library (`payments`, `social`, `webhooks`, `file_storage`,
`internal_admin`, `multitenant_saas` under `tests/fixtures/*.phx` — all in
`FILE_FIXTURES`, so each runs through the compile-lint harness on all four targets
and both server frameworks) was migrated off the old placeholder modeling — opaque-`String` ids,
`Int` epoch-seconds timestamps, `Int` minor-unit amounts — to the built-in types
that motivated the slices: ids → `Uuid`, timestamps → `DateTime`, currency amounts
→ `Money` (`payments`' charge/refund/line-item amounts, `internal_admin`'s account
credit), and the capture-amount query param → `Decimal` (a `Money` composite isn't
URL-encodable). The honest exclusions are kept and re-commented: the `file_storage`
**multipart** upload body (`ObjectUpload`) stays all-`String`/`Int` — its object
key stays a validated `String`, not a `Uuid` (the composite scalars are rejected
in multipart); checksums/etags/opaque tokens/IPs/the feature-flag key stay
validated `String`s (they are not UUIDs); webhook signature-replay header timestamps
stay `Int` epoch (the convention for signature schemes); and the `internal_admin`
deliberate trailing-period doc-comment repro is preserved verbatim.

## Phoenix Gen — enum query/header params (2026-06-17)

The next schema slice after the type-fidelity work: allow **simple (unit-variant)
enums in query params and request headers**, instead of degrading them to
`Option<String>` the handler re-parses (the most-requested gap from the fixture
audit — three fixtures hit it). Scope is **query + request headers** (response-header enums are also handled — client casts on read);
**enum-variant defaults are supported** (`priority: TicketStatus = Normal`).

**Locked behavior.** On the wire an enum is the bare variant string (identical to
its JSON-body encoding — TS string unions, Go typed-string consts, Python
`(str, Enum)`), so `?status=Pending`. The server **validates** the inbound string
into the typed enum, rejecting an unknown variant — the headline soundness win:
- **TypeScript**: a generated `parse<Enum>` (a `<ENUM>_VALUES` membership check)
  throws `ValidationError`; the route catch maps it to **400**. The catch's
  `ValidationError → 400` guard, previously gated on body constraints, now also
  fires when the endpoint has an enum param.
- **Go**: a `Valid()` method on the enum type (emitted only for param-enums); the
  server seeds the default (or empty), overwrites from the wire, and rejects an
  invalid/missing-required value with **400** (`http.Error`).
- **Python**: the FastAPI route types the param as the `Enum` class, so FastAPI
  coerces + rejects natively with **422**.
- **OpenAPI**: the param schema `$ref`s the enum component (with `default`) — no
  generator change needed; redocly-clean.

Only **simple** enums are allowed in these positions — a tagged/payload-carrying
enum serializes to an object, not a URL/header string, so sema rejects it (along
with an enum-variant default that names an unknown variant, and a literal default
on an enum type). An enum default on an **optional** param (`Option<Enum> =
Variant`) is also rejected — `Option` already encodes "may be absent", so a
default is contradictory; this mirrors the pre-existing literal check on
`Option<Int> = 5` and keeps the backends consistent (Go's optional decode never
seeds a default, so allowing it would silently drop the value there while
Python/TS rendered it). A **struct** in a query/header position is rejected for the same
reason (it also serializes to an object — carry it in the body); without this the
backends would emit broken code (Go `Item(v)`, Python `item.value`, a TS struct
cast). The restriction applies to response headers too.

**Cross-target client encoding.** TS sends `String(enumValue)` (the union value);
Go `string(v)` / `fmt.Sprint`; Python sends `.value` (NOT `str(enum)`, which would
emit `Color.Red`). The Python enum **member** is the SCREAMING_SNAKE form
(`Color.RED`), but its **value** is the bare variant (`"Red"`), so defaults render
`Color.RED` while the wire stays `Red`. The SCREAMING_SNAKE conversion is shared
between the Python member name and the TS `<ENUM>_VALUES` const name so the two
cannot drift, and is acronym-aware (`HTTPError` → `HTTP_ERROR`, `RED` → `RED`).
This only affects the Python enum **member identifier** for acronym/all-caps
variants — the `.value` and wire string are unaffected, so it is a self-consistent
improvement, not a wire-format change.

**Response-header read validation (deliberate asymmetry).** The *inbound* server
validation above is uniform (TS 400 / Go 400 / Python 422). The *client read* of a
**response-header** enum is not, and this is intentional: a response-header-only
enum (one never used in a query/request header, e.g. `Tier`) has no generated
validator, so there is nothing to call. TS casts the wire string straight into the
union (`raw as Color`); Go casts into its typed-string (`Color(raw)`); only Python
reconstructs through the enum constructor (`Color(raw)`), which happens to raise on
an unknown value. So a misbehaving server's bad response-header enum surfaces as a
runtime error on the Python client but is silently accepted (as an out-of-union
value) by the TS and Go clients. This matches how branded-scalar *response* headers
are the only read-side values TS/Go validate, and the contract treats the server as
trusted for what it writes; tightening response-header reads to validate uniformly
(emit a `parse<Enum>`/`Valid()` for response-header-only enums too) is a possible
future cleanup, tracked alongside the Go `Uuid`/`Decimal` 500-vs-400 divergence
below.

**Out of scope (deferred).** `List<Enum>` in query (part of the separate
list-valued-query-param slice). The pre-existing `Uuid`/`Decimal` query params
remain unchecked on the Go server (they pass the malformed value through to the
handler rather than 400 — a divergence from enums, which validate); unifying
param-validation→400 across all branded types is a possible future cleanup.
**Done 2026-06-18:** that cleanup landed — Go now format-checks scalar (and
`List`-element) query/request-header `Uuid`/`Decimal` against `uuidRe`/`decimalRe`
→ 400, matching enums and TS/Python. See *"tighten scalar query/header
`Uuid`/`Decimal` validation on Go (2026-06-18)"* below.

## Phoenix Gen — inline response projection (2026-06-17)

Lets a `response` reference an existing struct narrowed by `pick`/`omit`/`partial`
instead of declaring a dedicated read-model struct (the fixture pain points: social
`PublicProfile`, file_storage `BucketUsage`). Decisions: support
the **full `pick`/`omit`/`partial` chain** (same as `body`); scope includes the
**bare response, `List<Struct pick…>`, and paginated projected items** (plus
projection with response headers).

**Grammar (least-invasive).** A trailing `omit`/`pick`/`partial` chain attaches to
a bare `Named` type, so a projection is accepted wherever a named type appears —
crucially **inside `List<…>`** (`List<User pick {…}>`) without any
generic-grammar change. Existing plain-`Named` uses keep compiling.

**Naming & composition.** A projection generates an `<Endpoint>Response` struct
(bare → that struct, list → `List<…>` of it). The `<Endpoint>Response` name is
reserved by the generated-type collision check — it reuses the multi-status
envelope's name slot, but the two never co-occur (block form has no bare response
to project), and it composes with a `Result`/`Page` envelope (which wrap the
projected struct). A `Named` carrying modifiers anywhere other than a
`body`/`response` base is a sema error (projection misplaced).

**Per-target output.** Each generator emits `<Endpoint>Response` mirroring
`<Endpoint>Body`: Go struct (no `Validate()` — responses are outbound), pydantic
model, TS `type` alias, OpenAPI component schema. Everything downstream
(response/list/pagination/response-header handling, imports) then treats it as an
ordinary `Named` struct. The TS client revives a projected `DateTime`/`Uuid`/
`Decimal`/`Money` in a projected response (incl. paginated items), the same
revival machinery struct responses use.

**Out of scope (deferred).** Projection on a multi-status `response { 200: Struct
pick }` (the block form), an `Option<Struct pick>` response, and a `Map<_, Struct
pick>` value projection — all need the projection to nest in those positions (the
grammar already permits it syntactically, but sema/codegen don't wire those
shapes). A projection nested in one of these unwired response shapes hits the
misplaced-projection error, whose message names the SUPPORTED shapes ("only allowed
directly on a `body` base type or a `response` type, optionally as the element of a
`List<…>`") rather than claiming the position isn't a response — so
`response Option<User pick …>` isn't misdescribed. `partial` on a response is
allowed (optional fields) per the modifier-parity decision.

A projection that **picks a `File`-typed field** off the base struct is **rejected**
(not deferred): the projection path resolves before (and instead of) the normal
response path, so it bypasses the file/multipart response validation (the
single-`File` binary-download rule). Rather than silently emit a `File`-bearing
`<Endpoint>Response` with no multipart handling, a projected field set carrying
`File`/`Option<File>` errors. Supporting projected file responses later would mean
running the same file/multipart checks the bare-response path does.

## Phoenix Gen — list-valued query/header params (2026-06-17)

Allows `List<T>` in query params and request headers (the batch-endpoint gap from
the fixture audit), where `T` is a permitted scalar (`Int`/`Float`/`Bool`/`String`/
`DateTime`/`Uuid`/`Decimal`) or a simple enum. `List<Money>`/`List<struct>`/
`List<tagged-enum>`/nested `List`/`List<Map>`, `Option<List<…>>`, and a default on
a list are all rejected by sema (`check_list_param`).

**Wire format.** Query params
use **repeated keys** (`?ids=a&ids=b`) — clean everywhere (nothing collapses query
strings; FastAPI `list[T]=Query()`, Go `r.URL.Query()[k]`, Express/Fastify array
parsing, OpenAPI `style: form, explode: true` default). Request headers use
**comma-separated** values (`X-Role: a,b`), NOT repeated header lines: Node (both
`fetch`'s `Headers` and the http server) collapses duplicate request headers to a
single `", "`-joined value, and FastAPI's native `list[str]` header parsing then
can't recover them — so repeated header lines do NOT round-trip cross-language.
Comma-separated (join on send; on receive, join any multiple values then split on
`,` and trim) is the only encoding that works across Go/Node/Python. Caveat: a
comma inside a header element value mis-splits (documented; rare for header lists).
OpenAPI gets this for free — a header array param's default `style: simple` IS
comma-separated.

**Per target.** Client: append one query value per element / comma-join the encoded
header elements into one value. Server: query reads all values for the key and
coerces each (Python via FastAPI's native `list[T] = Query(...)`); a list request
header is received as a raw joined value and split+trimmed+coerced (on Python in
the route body before the handler call — FastAPI can't split a comma header into
`list[T]` natively). Enum list elements are validated per element (Go `Valid()`→400,
TS `parse<Enum>`→400; Python query elements via FastAPI→422, header elements
construct the enum → ValueError→500, a documented minor divergence). A
`Uuid`/`Decimal` element is format-checked per element on the server (Go → 400, TS
`parseUuid`/`parseDecimal` → 400; Python *query* elements via FastAPI
`list[UUID]`/`Decimal` coercion → 422, but a `Uuid`/`Decimal` *header* element —
coerced manually in the route body, like the numeric/enum header elements below —
raises → 500, the same documented header divergence, not 422). (The Go *scalar*
`Uuid`/`Decimal` query/header path was format-lenient when this slice landed; it
was tightened the next day so a list element and a scalar validate identically —
see *"tighten scalar query/header `Uuid`/`Decimal` validation on Go (2026-06-18)"*
below.)

The same query-vs-header status divergence applies to malformed *numeric*
elements: a bad `List<Int>`/`List<Float>` query element is dropped (Go/TS scalar
leniency) or 422'd (Python FastAPI), whereas a bad numeric **header** element
raises on coercion (Python `int(...)`→500; Go/TS keep the scalar-header leniency
and skip it). This matches the existing scalar-header behavior and is accepted as
a minor divergence, not a defect.

**Out of scope (deferred).** Response-header lists (the server-write/client-read
paths don't handle them — sema rejects `List` response headers).

## Phoenix Gen — tighten scalar query/header `Uuid`/`Decimal` validation on Go (2026-06-18)

Closes the long-documented Go "weak link": scalar `Uuid`/`Decimal` query params and
request headers reached the handler **unvalidated** on the Go target (a malformed
value passed straight through), whereas TS validated them inline via
`parseUuid`/`parseDecimal` and Python via FastAPI's `UUID`/`Decimal` coercion. The
divergence was called out across the UUID, Decimal, and enum-param slices as an
accepted weak spot and a possible future cleanup; this slice does that cleanup —
and, while closing it, also fixes the TS *status code* (see "TypeScript" below): TS
rejected the malformed value but with a **500**, not the 400 an enum param already
gave, because `parse*` threw a plain `Error` rather than `ValidationError`.

**Behavior.** A scalar `Uuid`/`Decimal` query/header param is now format-checked on
Go, rejecting a malformed value with **400** — the required branch also rejects an
absent (empty) value, matching the enum required path. This brings the **scalar**
path in line with the `List`-element path, so a `Uuid`/`Decimal` validates
identically whether it rides as a scalar param or a `List`-valued param element,
across all three targets.

**TypeScript (same slice).** TS already validated each query/header `Uuid`/`Decimal`
(scalar or `List` element), but a malformed value surfaced as a catch-all **500**
(e.g. a batch `GET ?ids=<bad-uuid>` 500'd), diverging from the 400 an enum param
gave and from Go's new 400, because the parse threw a plain `Error` the route's
`ValidationError → 400` guard never matched. Fixed so a malformed `Uuid`/`Decimal`
now rejects with **400** on both Go and TS (Python: 422 from FastAPI), regardless of
what else the endpoint carries. (Intended side effect: a constrained body's bad
branded field now 400s too, instead of 500.)

**Was a weak link, now closed (separate code path).** At the time of this slice
Go's struct `Validate()` still did not recurse into `List<Uuid>`/`Map<String, Uuid>`
(or `List<Money>`) **field** elements — the general-nested-validation feature,
orthogonal to this query/header-param slice. That gap was **closed 2026-06-20** —
see "Go nested `Validate()` recursion" below.

## Phoenix Gen — URL & bytes scalar types (2026-06-19)

Adds two scalars the fixture library wanted: `Url` (a validated URL string) and
`Bytes` (a first-class binary value). The semantics are fixed up front: `Bytes`
is a **real binary value** at runtime (`Uint8Array` / `[]byte` / `bytes`) carried as
base64 on the JSON wire — not a string the caller has to encode themselves; `Url` is
a **branded + validated** string (the `Uuid`/`Decimal` model), validated everywhere
but **never normalized**, so it round-trips byte-for-byte.

**`Url` — validated, never normalized.** All three targets validate the SAME way —
**scheme presence only** — so the servers agree on which strings are valid. A fuller
parse (`URL.canParse`, `net/url.ParseRequestURI`) was rejected precisely because the
language URL libraries disagree at the edges, which would mean a value one generated
server accepts and another rejects — breaking the whole-stack contract guarantee.
(One residual edge: Python's `urlparse` strips embedded `\t`/`\r`/`\n` before
parsing, so a URL containing a raw tab/newline can validate in Python while the
Go/TS anchored scheme regex rejects it — pathological input that would never
round-trip identically anyway, accepted as out of scope.)
- **TS:** a branded `Url = string & {…}` validated by the shared scheme regex
  `/^[a-zA-Z][a-zA-Z0-9+.-]*:/` (the same as Go's). (An earlier draft used
  `URL.canParse`, which validated *more* than the other two and so was inconsistent —
  replaced with the scheme regex.)
- **Go:** a `string` format-checked by the same scheme regex (requires a scheme) in
  struct `Validate()` and, **this slice**, in the scalar query/header param branches
  and the `List`-element branch — so a single `Url` query/header param now 400s on a
  malformed value, matching the `List<Url>` element path and the TS/Python behavior.
  (Before this, a single `Url` query/header reached the handler unvalidated, the same
  gap the prior slice closed for `Uuid`/`Decimal`.)
- **Python:** the value stays `str` (so it round-trips exactly), with a validator
  rejecting a value whose parsed scheme is empty; FastAPI runs the validator on
  query/header params.
- **OpenAPI:** `{type: string, format: uri}`.

**`Bytes` — real binary, base64 wire.**
- **TS:** runtime `Uint8Array`, rewritten to its base64 string before serialization
  (on the client request body and the server response) and decoded back on revival.
- **Go:** `[]byte` — `encoding/json` already base64s it both ways, no extra machinery.
- **Python:** runtime `bytes`. The original `Base64Bytes` choice was **behaviorally
  wrong** and the round-trip caught it: `Base64Bytes` treats *all* construction input
  as base64 to decode, so a caller passing raw `bytes` (or the echo handler
  re-wrapping the decoded value) got corrupted output. Replaced with a custom alias
  whose validator decodes a base64 *string* but passes raw `bytes` through unchanged
  (so a caller works with binary directly), serializing as base64 on dump.
- **OpenAPI:** `{type: string, contentEncoding: base64}` (the spec is 3.1 /
  JSON Schema 2020-12, where this — not the 3.0 `format: byte` — is the idiomatic
  base64 representation; `format: byte` would be only an ignored annotation under 3.1).

**Position restrictions (sema).** `Bytes` is body/struct/response-only — rejected in
query/header/path (a binary value is not a URL/header-encodable scalar), the binary
analogue of the existing `Money` rejection. `Url` is allowed everywhere (it is a
validated string).

**Wire behavior to note.** `Bytes` survives as raw binary including **non-UTF-8 bytes**
(0x00/0xFF/0xFE/0x80) through body/`Option`/`List`/`Map<String, Bytes>`/response — the
server receives a real `Uint8Array`/`bytes`, not the base64 string. `Url` round-trips
**byte-for-byte** (query string + fragment + non-lowercased host all preserved) through
body/`Option`/`List`, a query param, a `List<Url>` query param, and a request header; a
malformed `Url` rejects with 400 on TS/Go, 422 on Python. A bare `Bytes` response leaf
(not struct-wrapped) is decoded inline by the client.

**TS body-revival reject → 400 (gap closed in this slice).** A `Url` (or `Uuid`/
`Decimal`/`Money`) **body field** validates on body revival even with no
`@`-constraint. The TS server's `ValidationError → 400` guard previously did not fire
for a body whose *only* validation is a branded-scalar field (with no validated
query/header param), so the throw fell through to the catch-all **500**, diverging
from Go's body `Validate() → 400`. Such a body now 400s in both frameworks. (The
`URL_BYTES` schemas masked this because they always pair a `Url` body with a `Url`
query param.)

**Deferred (documented).** `Url` is validated for a scheme only (no full RFC 3986
component validation / percent-encoding normalization — intentional, to round-trip
exactly). No streaming/large-`Bytes` story (the whole value is in memory and base64'd
in one pass) — fine for the small payloads the schema language targets; a multipart
`File` body remains the path for large uploads.

## Phoenix Gen — Go nested `Validate()` recursion (2026-06-20)

Closes the documented Go validation weak link: the generated Go `Validate()` did not
recurse into `List`/`Map`/`Option` **elements**, so a malformed `Uuid`/`Decimal`/
`Url`/`Money` (or a constraint-violating nested struct) carried *inside a collection*
was accepted by the Go server while Python (pydantic recurses into list/map items and
nested models) and TypeScript (`revive*` walks the same structure) rejected it with a
400/422. A direct branded-scalar or `Money` field (`total: Money`, `id: Uuid`) was
already validated on all three. This is the **soundness-consistency** fix flagged
before the distribution/docs push: of the two documented validation divergences, it is
the one where the generated code was silently *less safe* on one target. (The
mirror-image gap — multipart `where` constraints validated only in Go, not Python/TS —
is a different mechanism and remains deferred; see
[known-issues.md](known-issues.md).)

The fix is in fact broader than the documented weak link. The old code's only
struct-`Validate()` recursion was Money-specific (`money_field_shape` matched just
`Money`/`Option<Money>`), so a **direct non-Money nested-struct field** — e.g. a
`primary: Address` where `Address` carries a `where` constraint — was *also* skipped by
the Go server (Python/TS validated it). Routing every field through
`type_is_validatable` / `emit_value_validate` closes that direct-nested-struct case
together with the collection-element case; both were the same missing `Type::Named`
recursion, one level apart.

**The fix (Go target only).** Go's `Validate()` now descends recursively into every
field that can carry validatable content: a regex scalar (`Uuid`/`Decimal`/`Url`)
format-checks; a `Money` or a validatable named struct calls its own `Validate()`;
`List<T>` / `Map<String, V>` validate every element (the map key is always `String`,
never validatable); `Option<T>` nil-guards then recurses on the pointee. So
`List<Money>`, `Map<String, Uuid>`, `List<NestedStruct>`, and arbitrarily nested
combinations now validate every element, matching Python's pydantic recursion and
TS's revive walk. A single "is this type validatable?" predicate drives all three
gates that must agree — the source-struct emit gate, the derived-body emit gate, and
the server-side `body.Validate()` *call* gate — so a `Validate()` is generated iff
it has a body and called iff it was generated. A **struct that previously got no
`Validate()`** (e.g. one whose only validatable content is a `List<Uuid>`) now gets
one. A `visited` cycle-guard keeps a self-referential struct (`Tree { children:
List<Tree> }`) finite (the recursion is the runtime data walk, not generated code).

**Bug closed:** the `known-issues.md` entry "`Money` element validation inside
`List`/`Map` is skipped in the Go target" (which also covered the regex-scalar and
nested-struct element cases) is removed.

**Still deferred.** Multipart body field `where` constraints remain validated only in
Go (Python/FastAPI explodes the body into `Form(...)` params with no model; TS does
not call the body validator on `Blob`-bearing multipart bodies) — the inverse
divergence, a separate `Form`-validator-generation feature. See
[known-issues.md](known-issues.md).

## Phoenix Gen — multipart `where` constraints in Python/TS (2026-06-20)

Closes the mirror-image of the Go-nested-validation gap: a `where` constraint on a **multipart** body's scalar
field was enforced server-side only by Go. Go assembles the `<Endpoint>Body` from the
parsed form and calls `body.Validate()`, but Python exploded the body into per-field
`Form(...)` params with no validation, and TypeScript assembled the body field-by-field
without calling `validate<Endpoint>Body`. So an out-of-range multipart scalar (e.g.
`caption: String where self.length > 0` sent empty) was a 400 on Go but accepted by
Python/TS. With this change each target validates a multipart scalar constraint to the
same extent it validates the equivalent JSON body.

**Python.** The multipart route binds each scalar as `name: T = Form(...)`; the
constraint extraction now feeds the `Form(...)` validation kwargs
(`min_length`/`max_length`/`ge`/`le`/`gt`/`lt`), so a violation is a **422** (FastAPI's
validation status), identical to the JSON pydantic path. The JSON and multipart paths
share one extraction, so they cover exactly the same subset (see the residual note
below).

**TypeScript.** The generated body validator already (a) is emitted whenever the body
has a constrained field — including a multipart body — and (b) **safely ignores `File`
fields** (a `File` carries no `typeof` check and no constraint). So the fix is simply
to call it on the multipart route: file fields pass through untouched, scalar fields
are `typeof`-checked and constraint-checked, and a violation throws `ValidationError`
which the route maps to **400**.

**Residual (documented, separate).** Python validates only the **extractable**
(numeric/length) constraint subset, on **both** JSON and multipart — a constraint like
`self.contains("/")` is enforced by Go/TS (full-expression translation) but not Python
(no `Field`/`Form` kwarg). This is a pre-existing Python limitation that this fix neither
introduced nor worsened (multipart now matches Python's own JSON behavior); it is
tracked in [known-issues.md](known-issues.md) ("Python validates only the extractable
(numeric/length) `where` subset") as the general "Python constraint-expression parity"
follow-up. With this, the two cross-target validation divergences flagged before the
v1 distribution/docs push are both addressed: Go now recurses into nested collections, and Python/TS now validate multipart scalar constraints.

## Phoenix Gen — schema-constraint checking hardening (2026-06-20)

Closes the schema-language footgun flagged before the v1 distribution/docs push: a
malformed `where` constraint was **silently swallowed** rather than reported. The
root cause was `check_field_access` (phoenix-sema): for a non-struct base type it
returned `Type::Error` with no diagnostic, and downstream checks go quiet on error
types to avoid cascades — so `String name where self.lenght > 0` (a typo) passed
`phoenix check` and landed in the constraint AST as a no-op, and `.length`
constraints were never actually type-checked anywhere (only meaningful because
codegen renders them). For a tool whose pitch is type-safety, a typo'd constraint
compiling to nothing is a trust hole.

**Change (phoenix-sema, checker only — no codegen change).** A new `in_constraint`
flag is set while type-checking a struct field's `where` expression. It is the one
place `self.<x>` legitimately appears on a built-in base, so the strictness is
scoped there and general expression checking (function bodies, module/enum-qualified
names) is untouched. Within a constraint:
- `check_field_access` recognizes `self.length` on a `String`/`List` (an `Int`, the
  established constraint idiom every target renders — TS `.length`, Go `len(...)`,
  Python `min_length`/`max_length`), unwrapping a single `Option` first so
  `Option<String> bio where self.length > 0` checks the inner `String`. Any other
  field on a built-in base (a typo, or `.length` on an `Int`/`Map`) is a hard error
  ("type `T` has no property `x`") instead of a silent skip.
- `check_binary` unwraps a single `Option` from each operand, so a numeric
  constraint on an optional — `Option<Int> n where self >= 0 && self <= 10` — checks
  the inner `Int` rather than being rejected as `Option<Int>`-vs-`Int`.
- `self` stays bound to the field's **full** type (including `Option<T>`), so a
  presence check like `Option<Int> x where self.isSome()` still resolves `isSome` on
  the `Option`. Inner-value access unwraps at the use site (above), not at the bind.

This fixes the `.length`-never-checked bug and the `Option<T>` numeric/length
inconsistency, while preserving `.isSome()`/`.isNone()`. The constraint AST handed
to the generators is unchanged, so generated output is byte-identical.

**Residual (documented, separate).** Two narrow leftovers, both now LOUD (real
diagnostics) rather than silent: (1) a String/List **method** call (`self.contains`)
on an `Option<T>` field is still rejected — the binary-op and field-access paths
unwrap `Option` in a constraint but the method-call *dispatch* does not (a cleaner
fallback would try the `Option` method set first, then the inner type; a larger
restructure, no fixture needs it); (2) field access on a built-in **outside** a
constraint (a function body) stays lenient by design, to avoid touching general
expression checking. Both are tracked in [known-issues.md](known-issues.md).

### Follow-up — uniform `Option` unwrap for method-call constraints (2026-06-20)

Residual (1) above is now closed. A String/List **method** call on an `Option<T>`
field in a constraint — `email: Option<String> where self.contains("@")` — checks
clean, completing the "every constraint form behaves uniformly on `Option`" bar
(`.length`, numeric, `.contains`, `.isSome()` all work). When no path resolves the
method on the `Option` itself, the constraint context retries on the unwrapped inner
type **as a last resort** — *after* every `Option`-level path (built-in `Option`
methods, user-method table, trait bounds), so a real `Option` method like `isSome`
still resolves on the `Option` and is never shadowed.

The single remaining residual is now just struct-field access on an `Option<Struct>`
field (`self.zip` on `Option<Address>`) — a separate path (the struct branch looks
up the outer type), loud, no fixture hits it; tracked in
[known-issues.md](known-issues.md).

## Phoenix Gen — reserved-word handling + multi-word path-param fix (2026-06-20)

Found by stress-testing rather than curated fixtures: a schema whose identifiers
collide with a **target-language keyword** generated **uncompilable output**, with
no diagnostic. The generators used Phoenix identifiers verbatim, so a field `class`
emitted `class: str` (a Python `SyntaxError`); a query/path param `range`/`map`/
`func` emitted Go `range := …` (a Go syntax error). Phoenix's own parser rejects
names that are *Phoenix* keywords (`type`, `return`, `import`), which had masked the
gap — but `class`, `async`, `interface`, `func`, `range`, `map`, … are valid Phoenix
identifiers and only blow up downstream. No fixture used such names.

**The rule: escape the language identifier, never the wire name** — with one
acknowledged exception, Python **model fields**, which carry no alias and so
serialize by the (escaped) attribute name (`class` → wire key `class_`). That is
*not* a regression: Python models already serialize multi-word fields by their
snake_case name (`avatarUrl` → `avatar_url`), so the Python wire form already
diverges from Go/TS — the Python round-trip is same-language, where the convention
holds (see the "no `Field(alias=…)`" note in python.rs). Keyword field escaping just
extends that existing convention. Everywhere else (Go fields/params, TS
fields/params, all query/header/path params on every target) the wire name is
preserved verbatim.

Each target has its own keyword set and its own positions where a schema name
becomes a *binding* (vs. a wire string or an object/property key, which tolerate
keywords):

- **Go** — struct fields are exported (`to_pascal_case`, capitalized), and Go
  keywords are lowercase, so fields are inherently safe; the wire name rides a JSON
  tag or a `Query().Get("…")` literal. Only lowercase **param/local** identifiers
  collide. Fixed by escaping in `to_camel` (append `_`: `range_`, `map_`, `func_`) —
  the single function all param idents flow through. Beyond the 25 keywords, two
  **predeclared** identifiers, `nil` and `iota`, are also escaped — they are not
  keywords but the generated body uses them as bare literals no local-renaming can
  dodge (a param `nil` would shadow the predeclared `nil`, so `return nil, err` stops
  compiling). Predeclared *types* (`int`, `string`) and *builtins* (`len`, `make`)
  are left alone: shadowing them as a parameter is legal Go, so escaping would be pure
  churn.
- **TypeScript** — object **property** names accept reserved words (`{ class: … }`,
  `interface Item { class: string }` are legal), so struct fields and the
  `query`/`headers` objects need nothing. Only **path params** become standalone
  bindings (a client positional param + a server `const` + the URL template var), so
  only those are escaped (trailing `_`); member access (`req.params.class`) keeps the
  original wire key. The reserved set covers the ECMAScript reserved words plus
  `eval`/`arguments`, which are not reserved words but are illegal as binding names
  in strict-mode code — and generated TS is an ES module, hence always strict, so a
  path param `{eval}` would otherwise emit `const eval = …`, a `SyntaxError`.
- **Python** — the worst case: model **field** names and **param** names are all
  attribute/parameter bindings, so the escape (trailing `_`) applies to both. For a
  param the escaped snake form diverges from the wire name, so the **existing**
  alias logic (`Field`/`Query`/`Header`/`Form(alias=…)`) carries the original name to
  the wire for free. Model fields carry no alias and serialize by the (escaped) field
  name, which the Python client/server agree on.

**Latent multi-word path-param bug (fixed in passing).** A separate, pre-existing bug
surfaced: a multi-word path param `{postId}` became the Python parameter `post_id`,
but FastAPI matches path params by the *function-parameter name* — so `{postId}`
never bound (`post_id` was read as a missing query param → **422 at runtime**). It
compiled fine, so the compile-lint harness never caught it, and the round-trip
contract only ever used single-word path params (`{id}`), so it was untested. The fix
is the same `Path(alias="…")` the keyword path-param case needs: whenever the Python
identifier diverges from the wire `{pp}` segment (camelCase *or* keyword), emit
`Path(alias="pp")` so FastAPI binds by the wire name.

**Scope / residual (documented).** Handled: **field names** and **all param names**
(query/header/path). NOT handled, both tracked in
[known-issues.md](known-issues.md) for the broader robustness pass: (1) a **type
name** (struct/enum) that is itself a keyword — `struct class { … }` still emits
Python `class class(BaseModel)` / TS `interface class` (rarer, conventionally
PascalCase, and a larger change touching every reference site); and (2) an
**escape collision** — escaping appends `_` without a uniqueness pass, so a schema
declaring both `class` and `class_` in one scope yields duplicate identifiers (a Go/TS
compile error; in Python the duplicate pydantic field is silently dropped). Both share
one root cause — escaping is per-identifier with no global uniqueness pass — so the
robustness pass should fix them together.

## Phoenix Gen — v1 robustness pass (2026-06-20)

A deliberate sweep against *valid-but-adversarial* schemas — the class of input the
curated fixtures never exercise, which is where the prior slices' bugs kept hiding.
Probed several dimensions by generating + compiling/importing the output; fixed what
broke, rejected what can't be cleanly generated, and documented the rest. Guiding
principle (matching the existing collision checks): **a valid schema must produce
clean output or a clear schema-check error — never silently-wrong or uncompilable
output.**

**Fixed — silent data loss → sema rejection** (the worst class: wrong, not loud):
- A **leading-underscore field name** (`_hidden`) — Python's pydantic treats it as a
  private attribute (dropped from the model) and Go leaves it unexported (skipped by
  `encoding/json`); two targets silently drop the field. Now rejected with a clear
  diagnostic.
- Two field names that **collapse to the same `snake_case`** (`fooBar` + `foo_bar`) —
  the Python model declares the attribute twice and the second silently wins. Now
  rejected. The shared `to_snake_case` (the same one codegen uses) backs the check so
  it predicts exactly the names codegen emits.

The check covers struct-field names — the primary data-bearing surface
(request/response bodies are derived from named structs, and enum variant fields are
positional, not named). Query/header param names are intentionally out of scope: a
snake-collision there produces a duplicate-kwarg Python `SyntaxError` (loud,
harness-caught) and a leading-underscore param is a valid, non-dropped function
argument — neither is the silent-loss failure mode this check targets.

**Fixed — crash / uncompilable on degenerate schemas:**
- A schema with **no endpoints** (a types-only package) generated an empty Python
  `class Handlers(Protocol):` body — an `IndentationError`. Now emits `pass`. The
  rest of the no-endpoint output was made lint-clean across targets too (Go gates the
  unused `net/http` import, TypeScript emits an empty-handlers type and `export const
  api = {};`), so a no-endpoint schema now generates and compiles/lints on all four
  targets.

**Probed and clear (no action needed):** unicode identifiers (`Café`/`naïve` — all
three languages accept them), an empty struct, a single-variant enum, redefining a
built-in type name (`struct Money` — already guarded), and derived-body-type-name
collisions (already rejected).

**Deferred (documented in [known-issues.md](known-issues.md)).** Two remaining
identifier-collision cases, both **loud** (a target compile error the harness would
catch), not silent: a **keyword type name** (`struct class`) and a **user type
colliding with a fixed generated helper** (`struct ValidationError` vs the TS
`ValidationError` class). Both are longer reserved-name-list extensions of the
existing checks, lower-frequency (types are PascalCase by convention), and were
scoped out behind the higher-value silent-data-loss rejections.

## Phoenix Gen — Python wire names made cross-language compatible (2026-06-21)

Closes the cross-language interop gap surfaced by stress-testing: the Go and TS
targets put the schema's field name on the wire verbatim (Go `json:"avatarUrl"`, TS
`avatarUrl`), but the Python target serialized **snake_case** (`avatar_url`) with no
alias. So a Python client/server could not interoperate with a Go/TS counterpart —
a TS frontend POSTing `{avatarUrl}` to a FastAPI backend expecting `avatar_url`
silently dropped the field. Every round-trip test is same-language (Py↔Py), so this
was never exercised; it directly undercuts the product's whole-stack-integration
pitch ("one schema → a TS client + a Go/Python server that actually talk"). Now all
three targets share one wire format: the schema's (camelCase) names.

**Scope — fields only.** Query/header/path **params** already emitted
`Query/Header/Path(alias="<schemaName>")`, so they were camelCase on the wire
already. Only struct/derived/projection bodies and the pagination envelope used
snake_case. (The response-header and multi-status envelopes are internal carriers —
the body is serialized by its own model and header/status values go to HTTP
headers/status, never as JSON keys — so they need no alias.)

This also shifts the wire key for a **keyword field**: a field named `class`
serialized as the escaped `class_` before (model fields had no alias), but now
aliases to the original `class` — matching Go's `json:"class"` and TS's `class`. The
escaped form survives only as the Python attribute name (`class_`), no longer on the
wire. This is the same camelCase-parity fix applied to keyword-escaped names, but
worth calling out because it changes the wire key of an existing case.

**The wire contract.** A Python model field carries `Field(alias="<schemaName>")`
whenever the snake_case Python attribute diverges from the schema name (a camelCase
field, or a keyword escaped with a trailing `_`), so the wire key is the schema's
camelCase name — matching Go's JSON tag and TS. Aliased models set
`populate_by_name=True` so handlers and tests still construct by the Python name
while the wire uses the alias, and serialization emits the alias (`by_alias=True`).
Inbound parsing is unchanged — pydantic matches by alias by default. (FastAPI direct
returns already serialize `by_alias=True` by default.)

(The test that would have caught this — and now guards it — shipped next as the
cross-language wire-conformance tests below.)

## Phoenix Gen — cross-language wire-conformance tests (2026-06-21)

The companion guard to the Python-camelCase-wire fix: every prior round-trip pairs a
target's client with its OWN server (same-language), so a cross-language wire
divergence — like Python's snake_case keys — slips through invisibly. This adds the
missing layer.

**Architecture: conformance to one golden wire, not N×N pairing.** Standing up live
cross-process pairs (Py-client↔Go-server, …) is O(N²), dual-toolchain, flaky. But the
guarantee factors: if every target's client *emits* the canonical wire and every
target's server *accepts+emits* it, then any client interoperates with any server by
transitivity — O(N) conformance checks, no pairing. A committed golden
(`tests/roundtrip/cross_lang/wire.json`) is that canonical contract.

**Mechanism (augments the existing per-language round-trips).** A new bespoke
round-trip (`cross_lang`) drives each target's generated client against its generated
server through a wire **recorder** at the client transport — Go a custom
`http.RoundTripper`, Python an httpx wrapping transport, TS a `fetch` wrapper — and
asserts the captured request/response bytes equal the golden. All three drivers
compare against the *same* file, so conformance ⟹ pairwise interop.

**Comparison is structural with two deliberate equivalences:**
- **datetime as instant.** Go emits RFC 3339 `…Z`, Python `…+00:00`, TS `….000Z` —
  all valid RFC 3339 and mutually parseable, so the targets interoperate despite the
  differing strings. The comparator parses datetime-shaped strings and compares
  instants; everything else (Uuid/Decimal/Url/Bytes/Money/enum/String) is a canonical
  passthrough and compares exactly.
- **absent ≡ null for optionals.** Surfaced by the test: TS omits an absent optional
  field (`{displayName}`) while Go (`*string`→`null`) and Python (`None`→`null`) emit
  it explicitly. For a Phoenix `Option` these are semantically identical — every
  decoder accepts both (Go→nil, Python→None, TS→undefined), so interop holds. The
  comparator treats a missing key as `null`; a present *value* vs a missing key still
  differs, so a dropped REQUIRED field is still caught. **Corollary (the one soundness
  gap):** a *renamed* field is caught only when its golden value is non-null — a renamed
  null optional (e.g. `avatarUrl`), or equivalently any *extra spurious null-valued
  field* a target might emit, reads as `null≡null` and slips through. So the schema is
  built with `avatarUrl` as the *only* null field; every other field (`displayName`,
  `createdAt`, the scalar zoo, …) is non-null and would catch a snake_case rename. This
  is what makes the test the regression guard for the Python snake-wire bug.

**Concentrated schema** (one schema, the divergence-prone surfaces): camelCase fields
+ a nested struct, enum *values* (`Role`), the scalar zoo
(`Uuid`/`DateTime`/`Decimal`/`Url`/`Bytes`/`Money`), an `Option` (absent→null), a
`List`, a multi-word path param, a camelCase query param, a `List<enum>` repeated-key
query, an aliased request header, and the pagination envelope (`totalCount`).

**Scope boundary.** This proves the wire *contract*; it does not run real cross-process
pairings (the transitivity argument makes them redundant). A single true
Py-client↔Go-server smoke remains an optional belt-and-suspenders follow-up. It also
asserts only that each client *emits* the golden param wire (path/query/header): the
stub handlers ignore their decoded params, so server-side *param decode* is not
re-checked here — it is delegated to the per-language round-trips (each proves its
server decodes its own client's output, which == golden, so decode-of-golden holds
transitively). Request-body decode *is* exercised directly, via the `createAccount`
echo handler.

**Meta-guard (the test tests itself).** The whole guarantee rests on the comparator
actually *rejecting* a divergence — so each driver first runs three negative assertions
before any conformance check. (1) `jsonEqual(rename("createdAt"→"created_at",
golden.account), golden.account)` must be **false** — guards the key rule (if a future
edit intersected keys instead of unioning them, every conformance assertion would pass
vacuously). (2) Two *different* RFC 3339 instants must compare **unequal**, and (3) two
different non-datetime strings (`"admin"`/`"guest"`) must compare **unequal** — together
these guard the datetime-instant path (if the RFC-3339 prefix gate were dropped or the
instant rule over-broadened, an over-lenient comparator would likewise pass vacuously).
Each driver also pins the `listAccounts` request query (`page=2`), not just
`getAccount`, so the pagination param wire is checked alongside the response envelope.

## Phoenix Gen — Python constraint-expression parity (closed 2026-06-23)

**The gap.** The Python target enforced a field's `where` constraint by extracting
pydantic `Field(...)` / FastAPI `Form(...)` validation kwargs (`ge`/`le`/`gt`/`lt`
from `self <op> N`, `min_length`/`max_length` from `self.length <op> N`). That covers
only constraints reducible to a kwarg. A non-reducible one — `email: String where
self.contains("@")`, or any boolean/equality/multi-term expression — produced **no
kwarg**, so the Python server **silently accepted** values that Go and TypeScript
(which translate the *full* expression to `!strings.Contains(...)` /
`!x.includes(...)`) reject. A pure cross-language enforcement divergence: same-language
round-trips never sent constraint-violating data of the non-extractable kind, so it
never surfaced. This was the last *silent* (vs. loud/compile-time) Python codegen
residual.

**The fix — one full-expression mechanism, mirroring Go/TS.** Replace the
extractable-kwarg path with a single translator + enforcement, so Python has ONE
constraint path (like each of Go/TS) instead of a kwarg/none split:

- A **precedence-aware** expression translator (mirroring Go/TS) renders the full
  constraint to Python: `self.length`→`len(...)`, `self.contains(x)`→`x in …`,
  `&&`/`||`→`and`/`or`, etc.
- **JSON bodies / projections:** a `@model_validator` raises `ValueError` (→ FastAPI
  **422**) on violation; an optional field is checked only when present.
- **Multipart bodies:** an inline route-body check raises `HTTPException(422)` (no
  model to attach a validator to); `Form(...)`/`Field(...)` carry only the wire `alias=`.

So a non-extractable constraint like `email: String where self.contains("@")` — which
the old code silently accepted on Python — now rejects with 422, matching Go/TS. The
generated guards are pre-shaped to pass `black`/`ruff` with no formatter post-pass.

Closes the known-issue of the same name; see [phase-5.md](phases/phase-5.md) "Bugs
closed in this phase".

## Phoenix Gen — error responses: status code is the discriminator (2026-06-23)

**Observation.** The generated error-response BODIES differ per target: Go writes
`http.Error(w, "<Variant>", code)` (plain text), Python raises
`HTTPException(status_code=code, detail="<Variant>")` (`{"detail": …}` JSON), and TS
does `res.status(code).json({ error: "<Variant>" })` (`{"error": …}` JSON). Three
shapes, two content-types.

**Why this is NOT an interop bug (unlike the camelCase wire bug).** No generated
*client* decodes the error body: Go returns `fmt.Errorf("HTTP %d", status)`, Python
calls `response.raise_for_status()`, and TS maps the **status code** to the variant
(`if (response.status === 404) throw new ApiError("NotFound", 404, body)`) and carries
the body only as an opaque text field. So every client discriminates errors by **HTTP
status code**, which IS consistent across all targets; the body is opaque and
target-idiomatic. A cross-language client therefore recovers the same error from the
status regardless of which server it talks to.

**Decision.** The status code is the cross-language error contract; the error body is
deliberately target-idiomatic and opaque (not part of the contract). We do NOT force a
uniform structured error body in v1 — a structured, body-decoded error scheme (e.g.
RFC 7807 `application/problem+json` with a discriminator field) is a real feature that
would let multiple variants share a status and be distinguished by body; it is deferred
to a deliberate design pass (beta will show whether it's wanted).

**Consequence — enforced.** Because the discriminator is the status code, two error
variants in one endpoint's `error { }` block MUST have distinct status codes; otherwise
the variant is unrecoverable — the TS client's second `if (status === …)` branch is
dead code, and Go/Python can't tell the variants apart at all. Sema now **rejects**
duplicate error status codes within an endpoint (alongside the existing duplicate-name
and 400–599-range checks), turning what was silently-wrong client output into a clear
compile-time error.
