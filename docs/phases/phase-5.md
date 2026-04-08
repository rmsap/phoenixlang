# Phase 5: Differentiating Features

**Status: Not started**

These are what make Phoenix worth choosing over TypeScript/Rust/Go. Implement them after the core is solid.

## 5.1 Built-in Serialization

- Compiler generates serialization/deserialization code for every type
- Zero-cost: no reflection, no runtime overhead
- Format-agnostic: JSON, binary, query params, form data
- Integrates with refinement types — deserialization validates constraints
- Types serialize identically on backend and frontend (shared type system)

## 5.2 Refinement Types

- `type PositiveInt = Int where self > 0`
- Inline constraints on struct fields: `Int age where age >= 0 and age < 200`
- Compile-time verification when the value is known statically
- Automatic runtime validation at trust boundaries (user input, network data)
- Integrates with serialization — deserializing into a refined type validates the constraint
- **Complexity:** Very high — requires an SMT solver or restricted predicate logic in the type checker

## 5.3 First-Class Reactivity

- `Signal<T>` — a built-in reactive type; `signal(value)` creates one, `.get()` reads, `.set(value)` writes
- `derived(fn)` — creates a computed value that auto-recomputes when its signal dependencies change
- `effect(fn)` — runs a side-effect function whenever its signal dependencies change (e.g. DOM updates, logging)
- `component` declarations for UI — contain signals, derived values, and a `render()` method
- Compiler analyzes `.get()` calls within `derived`/`effect`/`render` to track dependencies and generate fine-grained DOM update code
- No virtual DOM — direct, targeted mutations to only the DOM nodes whose signal dependencies changed
- Only relevant on the WASM/frontend target
- **Why signals over `reactive let`:** Signals are explicit (`get`/`set` instead of invisible assignment interception), composable (they are regular values — store them in structs, pass to functions), and follow the proven approach of SolidJS, Angular Signals, and Svelte 5 runes. No special compiler syntax or reactive compilation pass needed.
- **Complexity:** High — requires a signal runtime, dependency tracking, effect scheduler, and component rendering model. The compiler may optimize by statically resolving dependency graphs where possible, but the core mechanism is runtime-based.

## 5.4 Typed Endpoints

- `endpoint` declarations define an API contract: HTTP method, URL pattern, request/response types, and error cases
- The compiler generates three things from each endpoint definition:
  1. **Server handler**: a typed function stub the developer implements — request parameters are extracted and deserialized automatically
  2. **Client call function**: a typed async function that constructs the URL, serializes parameters, makes the HTTP request, and deserializes the response
  3. **Serialization glue**: JSON (or binary) serialization/deserialization for request and response types, including error variants mapped to HTTP status codes
- Compile-time checks guarantee the client and server agree on the contract — if you change a field in the endpoint definition, every call site and handler is checked
- Error variants in the endpoint definition map to HTTP status codes and are represented as a `Result` on both sides
- Works across the stack: the endpoint definition compiles to a native handler (backend) and a typed fetch call (frontend WASM)

```phoenix
/** Retrieve a single user by their unique ID */
endpoint getUser: GET "/api/users/{id}" {
  // path params are inferred from the URL pattern
  response User
  error {
    NotFound(404)
    Unauthorized(401)
  }
}

/** List all users, optionally filtered by search query */
endpoint listUsers: GET "/api/users" {
  query {
    Int page = 1
    Int limit = 20
    Option<String> search
  }
  response List<User>
}

/** Create a new user */
endpoint createUser: POST "/api/users" {
  body CreateUserRequest
  response User
  error {
    ValidationError(400)
    Conflict(409)
  }
}

// Server: implement the handler
impl getUser {
  async function handle(id: Int) -> Result<User, getUser.Error> {
    let user: User = await db.find(id)?
    Ok(user)
  }
}

// Client: compiler-generated typed call
async function showProfile(userId: Int) {
  let result = await getUser.call(userId)
  match result {
    Ok(user) -> print("Hello, {user.name}")
    Err(NotFound) -> print("User not found")
    Err(Unauthorized) -> print("Not authorized")
  }
}
```

Endpoint structure:
- **Path params** are inferred from the URL pattern — `{id}` means the handler receives `id: Int`
- **`query { }`** defines URL query parameters with optional defaults and `Option<T>` for optional params
- **`body TypeName`** defines the JSON request body (POST/PUT/PATCH only — the type checker rejects `body` on GET/DELETE)
- **`response TypeName`** defines the JSON response body
- **Error variants** carry explicit HTTP status codes: `NotFound(404)`, not convention-based guessing

- **Why:** The #1 source of bugs in full-stack web development is client/server contract drift — a field gets renamed on the server but the client still sends the old name. TypeScript + OpenAPI codegen partially solves this but requires a separate schema file, a code generation step, and runtime validation. Phoenix can do this at the language level with zero boilerplate because it already has built-in serialization (5.1) and compiles to both native (backend) and WASM (frontend).
- **Complexity:** High — requires a new `endpoint` declaration in the parser, compiler code generation for handlers and client stubs, integration with the HTTP server (4.4) and serialization (5.1), and URL pattern matching with typed parameter extraction.
- **Depends on:** HTTP (4.4), Built-in serialization (5.1), Async runtime (4.3), WASM target (2.4)

## 5.5 Compile-Time Evaluation (`comptime`)

Typed queries, typed routes, typed endpoints, and refinement types are all instances of the same principle: **the compiler validates domain-specific data at build time**. Rather than adding each as a one-off language feature forever, Phoenix should provide a general mechanism — `comptime` functions that the compiler evaluates during compilation.

A `comptime` function is a pure function whose arguments are known at compile time. The compiler runs it during the build, and any errors it produces become compile errors. This lets library authors create their own compile-time validated types without language changes.

```phoenix
// comptime functions run during compilation when all arguments are known
comptime function regex(pattern: String) -> Regex {
  // Validates the pattern at compile time — syntax errors become compile errors
  Regex.compile(pattern)?
}

comptime function url(raw: String) -> Url {
  Url.parse(raw)?
}

// Usage — the compiler evaluates these calls and reports errors at build time
let emailRe = regex("[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\\.[a-zA-Z]{2,}")
let badRe = regex("[unclosed")  // compile error: unclosed bracket in regex

let api = url("https://api.example.com/v1")
let badUrl = url("not a url ://")  // compile error: invalid URL
```

### Relationship to existing features

Several planned features are special cases of `comptime`:

| Feature           | Currently planned as                      | With `comptime`                                               |
| ----------------- | ----------------------------------------- | ------------------------------------------------------------- |
| Typed SQL queries | Built-in SQL parser in the compiler (4.6) | `comptime function sql(...)` validates against schema         |
| Typed routes      | Built-in route pattern parser (4.4)       | `comptime function route(...)` validates patterns             |
| Refinement types  | Compiler-level constraint checking (5.2)  | Constraint predicates run at `comptime` for literal values    |
| Regex validation  | Not yet planned                           | `comptime function regex(...)` validates at build time        |
| URL validation    | Not yet planned                           | `comptime function url(...)` validates at build time          |
| Date/time formats | Not yet planned                           | `comptime function dateFormat(...)` validates format strings |

The special-case features (typed queries, typed routes, typed endpoints) should be built first as dedicated compiler support in their respective phases. `comptime` generalizes the pattern later, allowing the community to create new compile-time validated types without modifying the compiler.

### Design constraints

- **Purity**: `comptime` functions must be pure — no I/O, no mutation, no network calls. They can only compute on their inputs and return a value or an error.
- **Determinism**: the same inputs must always produce the same output, so builds are reproducible.
- **Error propagation**: a `comptime` function that returns `Err(...)` or panics produces a compile error with the error message.
- **Fallback to runtime**: if a `comptime` function is called with arguments that are not known at compile time (e.g. user input), it falls back to a normal runtime call. The function works in both contexts.
- **No arbitrary code execution**: `comptime` is not a macro system — it doesn't generate AST nodes or modify syntax. It evaluates expressions and validates values.

- **Why not macros:** Macro systems (Rust proc macros, Lisp macros, Zig comptime) are extremely powerful but also the most complex feature a language can have. They create a two-language problem (macro language vs. regular language), make error messages worse, and make code harder to read. Phoenix's `comptime` is deliberately limited: it evaluates pure functions, not arbitrary code generation. This covers the validation use case (which is 90% of what web developers need from compile-time execution) without the complexity of a full macro system.
- **Complexity:** Very high — requires a compile-time interpreter or evaluator for a subset of Phoenix, cycle detection, and clear error reporting when `comptime` evaluation fails.
- **Depends on:** Compilation (2.2), Result/Option (1.5)

## 5.6 Auto-Generated API Documentation

Since typed endpoints (5.4) already declare the complete API contract — HTTP method, URL pattern, request/response types, error cases — the compiler has all the information needed to generate API documentation automatically. No annotations, no separate schema files, no drift.

```phoenix
/** Retrieve a single user by their unique ID */
endpoint getUser: GET "/api/users/{id}" {
    response User
    error {
        NotFound(404)
        Unauthorized(401)
    }
}

// The compiler automatically generates an OpenAPI 3.1 spec from all endpoint declarations:
//
// paths:
//   /api/users/{id}:
//     get:
//       description: "Retrieve a single user by their unique ID"
//       parameters:
//         - name: id
//           in: path
//           required: true
//           schema: { type: integer }
//       responses:
//         200:
//           content:
//             application/json:
//               schema:
//                 $ref: '#/components/schemas/User'
//         404:
//           description: NotFound
//         401:
//           description: Unauthorized
```

```bash
# Generate the spec
phoenix docs api --format openapi > api.json
phoenix docs api --format openapi > api.yaml

# Serve an interactive API explorer during development
phoenix docs serve --port 3001
```

### What's generated

- **OpenAPI 3.1 spec** from all `endpoint` declarations — paths, methods, parameters, request/response schemas, error codes
- **JSON Schema** for every type used in endpoints — structs, enums, type aliases, including generic instantiations
- **Refinement type constraints** (5.2) appear as schema `minimum`, `maximum`, `pattern`, etc. — `type PositiveInt = Int where self > 0` becomes `{ type: integer, minimum: 1 }`
- **Human-readable documentation** from doc comments on endpoints, types, and fields (when doc comments are added as a language feature)

### Why this is differentiating

Every other approach to API documentation has a drift problem:

| Approach | Problem |
|----------|---------|
| Swagger annotations (Java, C#) | Annotations can be wrong or outdated — they're not checked against the actual code |
| OpenAPI-first codegen | Requires maintaining a separate schema file; code and schema diverge over time |
| Runtime introspection (FastAPI) | Mostly accurate, but runtime types can differ from declared types; requires running the app |
| Phoenix typed endpoints | **The spec IS the code.** There is no separate artifact to maintain. The compiler generates the spec from the same type information it uses for type checking. It is impossible for the docs to be wrong. |

- **Complexity:** Small — the hard work is already done by typed endpoints (5.4) and built-in serialization (5.1). This feature is primarily a serialization pass that walks the endpoint registry and emits OpenAPI JSON/YAML.
- **Depends on:** Typed endpoints (5.4), Built-in serialization (5.1)

## 5.7 Built-in Observability (Structured Tracing)

Phoenix's structured concurrency (4.3) gives the runtime something most languages lack: a complete picture of the task tree for every request. Every `await`, every spawned task, every database query can be automatically traced without the developer adding instrumentation. This turns observability from an afterthought into a built-in capability.

### Automatic request tracing

```phoenix
// Every request automatically gets a trace — no manual instrumentation
async function handleGetUser(id: Int) -> Response {
    // These awaits are automatically recorded as spans in the trace:
    let user = await db.query(SELECT name, email FROM users WHERE id = $id)
    //  └─ span: db.query (table=users, duration=3ms)

    let avatar = await http.get("https://avatars.example.com/{id}")
    //  └─ span: http.get (url=..., status=200, duration=45ms)

    let notifications = await countUnread(id)
    //  └─ span: countUnread (duration=2ms)
    //       └─ span: db.query (table=notifications, duration=1ms)

    Response.json(UserProfile(user, avatar.body, notifications))
}
// Total trace: handleGetUser (duration=52ms)
//   ├─ db.query (3ms)
//   ├─ http.get (45ms)
//   └─ countUnread (2ms)
//       └─ db.query (1ms)
```

Every async function call becomes a **span** in a trace. The runtime automatically:

- Assigns a **trace ID** to each incoming request
- Propagates the trace ID through all spawned tasks and sub-calls (via structured concurrency — the task tree IS the trace tree)
- Records timing, status (ok/error), and metadata for each span
- Exports traces in **OpenTelemetry** format for integration with existing observability tools (Jaeger, Datadog, Grafana Tempo, etc.)

### Explicit spans for custom context

```phoenix
// Add custom spans for application-level context
async function processOrder(order: Order) -> Result<Receipt, OrderError> {
    // trace.span() creates a named span wrapping the closure
    let validated = await trace.span("validateOrder", async function() {
        validateInventory(order)?
        validatePayment(order)?
        Ok(order)
    })?

    let receipt = await trace.span("chargePayment", async function() {
        charge(validated.paymentMethod, validated.total)?
    })?

    Ok(receipt)
}
```

### Metrics

```phoenix
// Built-in metrics for common web patterns — no setup required
// The HTTP server automatically tracks:
//   - request_count (by method, path, status)
//   - request_duration_ms (histogram)
//   - active_connections (gauge)
//   - error_count (by type)

// Custom metrics
let orderTotal = Metric.histogram("order_total_usd")
orderTotal.record(order.total)

let activeUsers = Metric.gauge("active_users")
activeUsers.set(count)
```

### Health checks

```phoenix
// Built-in health check endpoint
router app {
    GET "/health" -> Health.check(function() -> Result<Void, String> {
        // Check dependencies
        await db.ping()?
        await redis.ping()?
        Ok(())
    })
}
// Returns 200 with { "status": "healthy" } or 503 with { "status": "unhealthy", "error": "..." }
```

### Configuration

```phoenix
async function main() {
    // Configure tracing output
    Trace.configure(TraceConfig {
        exporter: "otlp",                        // OpenTelemetry protocol
        endpoint: "https://traces.example.com",
        sampleRate: 0.1,                         // sample 10% of traces in production
    })

    // Configure metrics output
    Metric.configure(MetricConfig {
        exporter: "prometheus",
        endpoint: "/metrics",                     // serve Prometheus metrics on this path
    })

    await server.serve(app)
}
```

### Why this is differentiating

Most languages treat observability as a library concern — developers must manually instrument their code with spans, propagate context, and configure exporters. This leads to inconsistent coverage: critical paths get traced, but the gaps between them are invisible.

Phoenix's structured concurrency makes **automatic tracing possible** because:

1. **The task tree is the trace tree.** Every spawned task has a parent — that's exactly the parent-child relationship traces need. No manual context propagation.
2. **Every `await` is a natural span boundary.** The runtime already knows when async operations start and complete — recording that as a span is nearly free.
3. **Built-in HTTP and DB know their own semantics.** The HTTP client can automatically record request URL, status, and duration. The DB layer can record query text, table name, and row count. No wrapping or middleware needed.

The result: a Phoenix web server has production-grade observability **out of the box**, with zero instrumentation code. Developers who need more detail can add custom spans, but the baseline covers 90% of debugging needs.

- **Complexity:** High — requires trace context propagation through the async runtime, span recording with low overhead, OpenTelemetry export, metrics aggregation, and configuration. The structured concurrency model makes context propagation straightforward (it follows the task tree), but the export and configuration layers are substantial.
- **Depends on:** Async runtime (4.3), Structured concurrency (4.3), HTTP (4.4), Database access (4.7)
