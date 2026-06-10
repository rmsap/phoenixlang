# Phoenix Gen round-trip drivers

Behavioral round-trip suite: each target generates a client **and** a server
from `tests/fixtures/gen_api.phx`, runs the generated client against the
generated server over the shared `contract.json` fixtures, and asserts they
agree on the wire. See `docs/phoenix-gen-roundtrip-design.md` for the rationale
and `crates/phoenix-codegen/tests/roundtrip.rs` for the Rust harness that drives
each target.

```
roundtrip/
  contract.json        # language-agnostic interaction cases (THE shared contract)
  README.md            # this file
  go/                  # Go driver
    roundtrip_test.go
    go.mod.template
  typescript/          # TypeScript driver (driver.ts + pinned npm project)
  python/              # Python driver (driver.py + pinned venv)
```

## `contract.json` schema

A JSON **array** of interaction cases. Every target driver parses this same
file and conforms to it. Each case:

```jsonc
{
  "name": "getPost_happy_path_param",     // unique test id (becomes the subtest name)
  "endpoint": "getPost",                  // the generated client/handler method (camelCase)
  "kind": "ok",                           // "ok" | "error" | "constraint"

  "call": {                               // how the CLIENT is invoked
    "path_params": { "id": "42" },        //   string path params (object, optional)
    "query":       { "page": 3, ... },    //   query params as native JSON types (optional)
    "body":        { "title": "...", ...} //   request body as a JSON object (optional)
  },

  "handler": {                            // how the STUB handler behaves
    "expect_received": { ... },           //   assert the handler decoded exactly these args
    "returns": { ... } | [ ... ],         //   canned success payload (ok cases) — object or array
    "raises": "NotFound",                 //   variant name the handler signals (error cases)
    "expect_not_called": true             //   assert the handler was NOT invoked (constraint cases)
  },

  "raw_response": {                       // OPTIONAL: serve this canned response
    "status": 203,                        //   instead of the generated server
    "body": { ... }                       //   (JSON body; omit for an empty body)
  },

  "expect_client": {                      // what the CLIENT should observe
    "ok": { ... } | [ ... ],              //   expected typed result (ok cases)
    "error": {                            //   expected error (error / constraint cases)
      "variant": "NotFound",              //     the logical variant name
      "status_per_target": {              //     HTTP status each target maps it to
        "go": 404, "typescript": 404, "python": 404
      }
    }
  }
}
```

### `raw_response` — bypassing the generated server

When `raw_response` is present, the driver does **not** mount the generated
server: it answers every request itself with the canned `status` (+ optional
JSON `body`, sent as `application/json`; the Python driver snake_cases the body
keys to match the Python generator's documented wire format). The stub handler
is never invoked, so `handler` is empty and the ok-case "handler was called"
assertion is skipped.

This exists for the **client-leniency** cases: generated clients deliberately
envelope ANY 2xx status — even one the contract never declared — because a
proxy or middlebox can rewrite a success status (see `docs/design-decisions.md`,
"Clients are deliberately lenient"). The generated server can never put an
undeclared status on the wire (its envelope guard answers 500 instead), so a
canned raw response is the only way to exercise that client path end-to-end.

### Case kinds

| `kind`       | `handler`                          | `expect_client`        | What it proves |
|--------------|------------------------------------|------------------------|----------------|
| `ok`         | `expect_received` + `returns`      | `ok`                   | data round-trips both ways; args decoded/coerced correctly |
| `error`      | `expect_received` + `raises`       | `error` (+ status)     | handler error-variant → server status mapping |
| `constraint` | `expect_not_called: true`          | `error` (+ status)     | server rejects an invalid body BEFORE the handler runs |

### Header fields (optional; for endpoints that declare headers)

Request and response headers are exercised with three optional fields. Header
names in the contract use the Phoenix identifier (camelCase); each driver maps to
its idiomatic local name (Go/TS camelCase, Python snake_case) just as it does for
query params.

- **`call.headers`** — object of request-header name → JSON value the client
  sends. The driver passes these to the client in its generated header-input
  shape (Go positional args, TS/Python a `headers` object / kwargs). Request
  headers arrive at the handler as args, so they are asserted via the existing
  **`handler.expect_received`** (a `null` value means an optional header was
  omitted → handler sees nil/None/undefined). NOTE: a request header with a
  *default* is generated as a client-required value (always sent) — see the
  "defaulted request headers" open question in `docs/design-decisions.md`; the
  contract supplies such headers explicitly rather than relying on the server
  default.
- **`handler.returns_headers`** — object of response-header name → value the stub
  sets on the returned **envelope** (`<Endpoint>Result`). A `null` value leaves an
  optional response header unset (server omits it).
- **`expect_client.ok_headers`** — object of response-header name → value the
  client must read back off the response. The driver compares the envelope's
  response-header fields against this (`null` = the header was absent on the
  wire). For `ok` cases with `ok_headers`, the body is compared against
  `expect_client.ok` and the headers against `ok_headers` separately (the result
  is an envelope, not a bare body).

### Field-by-field rules drivers must follow

- **`call.path_params`** — values are always JSON **strings** (the generated Go
  `GetPost(id string)` takes a string; the contract keeps them stringly so every
  target agrees). Substitute them into the client call positionally by name.
- **`call.query`** — values are native JSON types (`number`, `bool`, `string`).
  Drivers must pass them to the client *with the client's declared type* (e.g.
  Go `page int64`, `minScore float64`, `featured bool`, `tag *string`). Missing
  optional params are simply omitted (client uses its default / nil). This is the
  case that catches **query-coercion bugs**: the handler asserts the decoded
  server-side value via `expect_received`.
- **`call.body`** — a JSON object the driver unmarshals into the generated body
  type, then passes to the client method.
- **`call.multipart`** — a multipart/file-upload request body (mutually exclusive
  with `call.body`). Shape: `{ "files": { "<field>": { "filename": "...",
  "content": "<utf8>" } }, "fields": { "<scalar>": <value> } }`. The driver
  encodes each file's `content` string to bytes and supplies it to the generated
  client's file-input shape (Go `FileUpload{Filename, Content}`, TS `Blob`,
  Python `FileUpload(filename, content)`). Scalar `fields` are JSON-typed
  (`String` → string, `Int` → number, `Bool` → boolean); each driver coerces a
  field into the generated body's typed slot (e.g. Go narrows a JSON number to
  `int64`) so the round-trip exercises each target's scalar coercion — including
  the bool wire encoding (`"true"`/`"false"`) — end-to-end. The handler asserts
  what it received via `expect_received`, where each file is checked as
  `<field>_content` (decoded UTF-8) and `<field>_filename`; an omitted optional
  file is asserted absent via `<field>_content: null`.
- **`handler.returns_file`** — for a binary **download** (`response_is_binary`):
  the UTF-8 content the stub streams back as the raw response body. The driver
  encodes it to bytes and returns it from the handler in the target's binary
  shape (Go `io.Reader`, TS `Buffer`, Python `bytes`).
- **`expect_client.expect_download`** — for a binary download: the bytes the
  client must read off the (non-JSON) response body, compared as decoded UTF-8.
  Present instead of `expect_client.ok`.
- **`handler.returns_status`** — for a multi-status endpoint (a `response { }`
  block): the HTTP status the stub handler chooses to return (e.g. `201` or
  `204`). The driver sets it on the generated `<Endpoint>Response` envelope the
  handler returns; the generated server writes that status to the wire. Also
  used by `kind: "error"` cases to drive the server's handler-bug guards: an
  undeclared status, a body paired with a typeless status, or a missing body on
  a typed status each make the generated server answer 500 instead of writing
  the bad envelope to the wire (asserted via `expect_client.error` with
  `variant: "Unknown"`, the TS client's code for an unmapped error status).
- **`expect_client.status`** — for a multi-status endpoint: the status code the
  client must observe on the returned envelope (`result.status`). Asserted
  alongside the body.
- **`expect_client.ok_absent`** (bool) — for a multi-status endpoint whose chosen
  status carries NO body (e.g. `204`): asserts the client's envelope body is
  absent/null. Present instead of `expect_client.ok`. (When a body IS expected,
  use `expect_client.ok` as usual — the body is compared against it.) For an
  ALL-TYPELESS endpoint the envelope has no body field at all; `ok_absent` is
  trivially satisfied there and the drivers assert the status only.
- **`handler.expect_received`** — a map of arg-name → expected value. Drivers
  compare the **decoded args the handler actually received** against these.
  Numbers are compared numerically (don't assume int vs float). `null` means the
  optional arg was absent (nil/None). Only listed keys are checked.
- **`handler.returns`** — the canned success value the stub returns; the driver
  unmarshals it into the generated response type and hands it to the framework
  to serialize. Compared against `expect_client.ok` after the client decodes it.
- **`handler.raises`** — the variant name the stub signals as an error. The
  generated Go server maps it via `strings.Contains(err.Error(), "<variant>")`,
  so the stub returns an error whose message **contains** this string. (TS/Python
  drivers throw/raise an error carrying the variant name per the design doc.)
- **`expect_client.error.status_per_target`** — the HTTP status each target's
  server maps the variant to. **The Go client surfaces errors as
  `fmt.Errorf("HTTP <status>")` — status only, no variant name** — so the Go
  driver parses the integer out and compares to `status_per_target["go"]`. TS and
  Python clients surface richer errors (see the design doc); those drivers assert
  accordingly.

### Documented per-target divergence (constraint cases)

Constraint violations map to **different statuses per framework** — this is
intrinsic, not a bug, so the contract makes it explicit via
`status_per_target`:

- **Go** — generated server calls `body.Validate()` → **400** `ValidationError`.
- **TypeScript** — server validates the body → **400** `ValidationError`.
- **Python** — pydantic `Field(...)` validates at parse time → **422** (FastAPI
  default). This rejection only fires if the Python generator actually emits the
  `Field(...)` constraints on the body model; if it ever stopped, the constraint
  case would surface as "handler WAS called" rather than a direct "no validation"
  message — the failure is still loud, but the coverage leans on that generator
  behavior (which no unit test asserts in isolation).

Each status is verified live by its target's round-trip test: Go (400) by
`go_roundtrip`, TypeScript (400) by `typescript_roundtrip`, and Python (422) by
`python_roundtrip`.

## Adding a target driver

1. Add a `<target>/` dir under `roundtrip/` with a driver that reads
   `contract.json` and conforms to the rules above. The driver implements a
   fixture-driven stub handler (records `expect_received`, returns `returns` or
   signals `raises`, tracks whether it was called for `expect_not_called`).
2. Add a `<target>_roundtrip` test in `roundtrip.rs` mirroring `go_roundtrip`:
   generate the target's files, assemble a runnable project (tempdir or committed
   scaffold), drop in the driver + `contract.json`, run the target's test runner,
   assert exit 0. Gate on the target's toolchain via the shared `gate(..)`.
