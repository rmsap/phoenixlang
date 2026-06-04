# Phoenix Gen — round-trip test suite design

Status: implemented for all three targets (Go, TypeScript, Python). Companion
to the compile-and-lint harness
(`crates/phoenix-codegen/tests/compiles_and_lints.rs`), which proves generated
code is *valid + clean* but explicitly NOT *behaviorally correct* (see that
file's scope note). This suite closes that gap.

## Goal
Prove **runtime correctness** and **client/server mutual consistency**: the
generated client and the generated server, for the same schema, agree on the
wire and round-trip data + errors + constraint violations correctly.

## Approach (decided with user)
**Same-language live round-trip against ONE shared contract fixture set.**
Each target runs *its own* generated client against *its own* generated server.
Because all targets conform to the same fixtures with concrete expected
decoded-inputs and observed-results, cross-target consistency follows
transitively without an N×M live matrix.

Coverage v1: **happy path + error-variant→status mapping + constraint violations.**

## Shared contract fixtures
`crates/phoenix-codegen/tests/roundtrip/contract.json` — language-agnostic, one
entry per interaction. The authoritative, field-by-field schema lives in
`crates/phoenix-codegen/tests/roundtrip/README.md` (so target drivers have it
next to the file they consume); the shape is:
```
{
  "name": "...",                  // unique test id (becomes the subtest name)
  "endpoint": "createPost",        // generated method/handler name (camelCase)
  "kind": "ok",                    // "ok" | "error" | "constraint"
  "call": {                        // what the client is invoked with
    "path_params": {"id": "..."},  //   path params are always JSON strings
    "query": {...},                //   query params as native JSON types
    "body": {...}
  },
  "handler": {                     // how the stub handler responds
    "expect_received": {...},       // assert handler got these decoded args
    "returns": {...}                // canned success payload (object OR array)
    // OR "raises": "NotFound"      // error-variant name the handler signals
    // OR "expect_not_called": true // constraint cases: handler must NOT run
  },
  "expect_client": {               // what the client should observe
    "ok": {...}                     // expected typed result (object OR array)
    // OR "error": {                // error / constraint cases
    //   "variant": "NotFound",
    //   "status_per_target": {"go":404,"typescript":404,"python":404}
    // }
  }
}
```

`kind` makes each case's intent explicit and drives the driver's assertion
branch: `ok` (data round-trips + `expect_received`), `error` (handler `raises`
→ status mapping), `constraint` (server rejects an invalid body before the
handler — `expect_not_called` is asserted false-was-called).

### Constraint-violation cases (documented divergence)
Invalid-body cases assert a **per-target** status because the three frameworks
enforce constraints idiomatically differently — this is intrinsic, not a bug:
- **Go**: generated server calls `body.Validate()` → **400** `ValidationError`.
- **TypeScript**: server calls `validateXBody(req.body)` → **400** `ValidationError`.
- **Python**: pydantic `Field(...)` validates at parse time → **422** (FastAPI default).

Fixture marks these with `status_per_target` so the divergence is explicit and
tracked rather than hidden.

#### Can Python emit 400 instead of 422?

Not a structural impossibility — but every route to 400 trades something away,
and the answer hinges on a policy decision we have **not** yet made: *is "the
declared `error { ValidationError(N) }` status is respected on every target" a
contract-level promise Phoenix makes, or does each target emit idiomatic code
for its language?*

Root cause of the 422: the Python generator emits each `where` constraint as
pydantic `Field(...)` kwargs **on the model** (`python.rs`, `constraint_to_field`
→ `min_length`, `ge`, …). Because the constraint lives on the model, FastAPI
validates the body during **request binding** — inside dependency resolution,
*before* the handler body runs — and a pydantic failure raises
`RequestValidationError`, whose framework default is 422. Go/TS instead call an
explicit `Validate()`/`validateXBody()` *inside* the handler path, so they
choose their own status (400).

Three ways to force 400, worst-to-best for honoring a declared status:

- **A. Global `RequestValidationError` → 400 handler.** Cheapest. But blunt: it
  remaps *every* validation failure (query-param coercion, malformed JSON,
  missing field) to 400, not just body-constraint violations, and it cannot
  honor a declared status other than 400 (nothing stops a schema writing
  `ValidationError(409)`). Also lands outside the generated unit — the generator
  emits a `create_router(...)` (`APIRouter`), not an app, so the override would
  attach to the app the consumer builds.
- **B. Mirror the Go target — explicit post-binding validation.** Drop the
  `Field(...)` constraints from the model, accept the raw body, then generate
  `raise HTTPException(status_code=<declared status>, detail="ValidationError")`.
  The only option that gives true status parity for *arbitrary* declared codes.
  Cost: the generated Python stops being idiomatic (pydantic models normally
  carry their own constraints), loses the automatic 422 entry in the OpenAPI
  schema and pydantic's structured error messages, and re-emits constraint logic
  in a second place.
- **C. Custom `APIRoute` that remaps only body-validation errors.** Keep the
  idiomatic `Field(...)` constraints on the model; generate a small
  `_ValidatingRoute(APIRoute)` whose `get_route_handler` catches
  `RequestValidationError`, inspects each error's `loc` tuple (`"body"` vs
  `"query"`/`"path"`), and re-raises body failures with the endpoint's declared
  `ValidationError` status while leaving query/path coercion untouched. Cleanest
  if parity is the goal: idiomatic models preserved, declared status honored, one
  localized helper. Cost: the most generator machinery, and a policy call for
  non-body validation errors.

**Recommendation.** Decide the policy question first. 422 is the genuinely
*correct, expected* status in the FastAPI/OpenAPI ecosystem — a Python consumer
would be mildly surprised by a 400 for a body-validation failure — so if Phoenix
follows "idiomatic per-target output" (as the rest of the generator does), the
honest move is to **keep 422 and treat this section as the documentation of the
divergence**. If instead the declared `ValidationError(N)` status is meant to be
a cross-target guarantee, implement **Option C**; **avoid Option A**, because it
silently cannot keep that promise for any status other than 400.

Current state: **422 retained** (idiomatic default), pending that decision.

## Per-target drivers (integration surface — verified)
- **Go** (`roundtrip/go/`): `httptest.NewServer(api.NewRouter(stub))` +
  `api.NewApiClient(srv.URL)`. Stub `Handlers` impl is fixture-driven (records
  received args, returns canned output or an error whose message contains the
  variant name — matches the server's `strings.Contains(err, "X")` mapping).
- **TypeScript** (`roundtrip/typescript/`): mount `createRouter(stub)` on an
  express app, `app.listen(0)`, drive with the generated fetch client pointed at
  the ephemeral port. Stub handlers throw `new Error("NotFound")` etc.
- **Python** (`roundtrip/python/`): mount `create_router(stub)` on `FastAPI()`,
  drive the generated httpx client **in-process** via `ASGITransport` (no port).
  Stub handlers raise `Exception("NotFound")` etc.

Client error surfacing differs (assert accordingly):
- Go client → `fmt.Errorf("HTTP %d", status)` (status only, no variant name).
- TS client → `ApiError { code, status, body }` (variant name + status).
- Python client → `raise_for_status()` → `httpx.HTTPStatusError` (status).

## Harness integration
New `crates/phoenix-codegen/tests/roundtrip.rs`, same gating as
compile-and-lint: skip-with-log unless `PHOENIX_GEN_E2E=1`. For each target:
generate into a tempdir (or the committed scaffold's gitignored `generated/`),
drop in the committed driver + the shared `contract.json`, run the target's
driver (`go test`, `tsx driver.ts`, and `python driver.py` — a plain script, no
pytest), assert exit 0. CI: runs in the `gen-checks` job (toolchains already
installed).
