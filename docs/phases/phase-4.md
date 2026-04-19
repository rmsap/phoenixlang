# Phase 4: Standard Library

**Status: Not started**

A batteries-included stdlib for web development. This is what makes Phoenix practical rather than theoretical. Each module should be small, well-documented, and opinionated (one way to do things).

## 4.1 Core Types and Collections

- `List<T>`, `Map<K, V>`, `Set<T>`
- `String` methods (split, trim, contains, replace, startsWith, etc.)
- Math functions (abs, min, max, floor, ceil, sqrt, pow)
- Type conversion (Int to Float, etc.)
- **Numeric recovery methods** (per the [numeric error semantics decision](../design-decisions.md#numeric-error-semantics-division-overflow-integer-edge-cases)): `Int.checkedDiv`, `Int.checkedAdd`, `Int.checkedSub`, `Int.checkedMul`, `Int.checkedRem`, `Int.checkedNeg` — each returning `Option<Int>` for validation paths. Float predicates: `Float.isFinite()`, `Float.isNaN()`, `Float.isInfinite()` — IEEE 754 is the recovery story for floats, not checked arithmetic.

### Tuple types

Lightweight, anonymous product types for grouping values without defining a struct. Tuples are fixed-size, heterogeneously typed, and support destructuring.

```phoenix
// Tuple literals
let pair: (Int, String) = (1, "one")
let triple: (Int, String, Bool) = (42, "hello", true)

// Access by position
print(pair.0)   // 1
print(pair.1)   // one

// Destructuring
let (id, name) = pair
print(name)  // one

// Functions returning multiple values
function divide(a: Int, b: Int) -> (Int, Int) {
    (a / b, a % b)
}
let (quotient, remainder) = divide(17, 5)

// Single-element tuples are just the inner type (no wrapping)
// Unit type () is the zero-element tuple (equivalent to Void)
```

- Tuples replace the current `List<dynamic>` workaround used by `enumerate()` and `zip()`
- Pattern matching works on tuples: `match pair { (0, s) -> ... (n, s) -> ... }`
- **Complexity:** Medium — requires a new type constructor in the type system, tuple literal parsing, positional field access (`.0`, `.1`), and destructuring integration.

### Date and Time

Built-in types for temporal values. Web applications constantly work with dates, timestamps, durations, and timezones — these should be first-class, not an afterthought.

```phoenix
// Instant — a point in time (UTC, nanosecond precision)
let now: Instant = Instant.now()
let epoch: Instant = Instant.fromUnix(0)

// DateTime — an Instant with a timezone
let local: DateTime = now.inTimezone("America/New_York")
print(local.year)    // 2026
print(local.month)   // 4
print(local.day)     // 8
print(local.hour)    // 14

// Duration — a span of time
let timeout: Duration = Duration.seconds(30)
let oneDay: Duration = Duration.hours(24)

// Arithmetic
let tomorrow: Instant = now + oneDay
let elapsed: Duration = tomorrow - now

// Formatting and parsing
let formatted: String = local.format("YYYY-MM-DD HH:mm:ss")
let parsed: Result<DateTime, ParseError> = DateTime.parse("2026-04-08", "YYYY-MM-DD")

// Comparison
if now > epoch {
    print("we're past the epoch")
}
```

- `Instant` is the core type — UTC, no timezone ambiguity, comparable and arithmetic-capable
- `DateTime` adds a timezone for display purposes — all storage and comparison should use `Instant`
- `Duration` is the difference between two instants
- IANA timezone database for timezone names
- When refinement types (5.2) are available: `type FutureInstant = Instant where self > Instant.now()`
- **Complexity:** Medium — the types themselves are straightforward, but timezone handling (IANA database, DST rules) adds bulk. Consider bundling a timezone database or requiring system timezone data.

### Regular expressions

A `Regex` type for pattern matching on strings. Essential for web development (input validation, parsing, routing, text processing).

```phoenix
// Compile a regex — returns Result in case of invalid pattern
let emailRe: Result<Regex, RegexError> = Regex.new("[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\\.[a-zA-Z]{2,}")

// When comptime (5.5) is available, invalid patterns become compile errors:
// let emailRe: Regex = regex("[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\\.[a-zA-Z]{2,}")

// Matching
let re: Regex = Regex.new("\\d+").unwrap()
print(re.isMatch("abc123"))   // true
print(re.find("abc123"))       // Some("123")
print(re.findAll("a1b2c3"))   // ["1", "2", "3"]

// Capture groups
let dateRe: Regex = Regex.new("(\\d{4})-(\\d{2})-(\\d{2})").unwrap()
match dateRe.captures("2026-04-08") {
    Some(caps) -> {
        print(caps.get(1))  // "2026"
        print(caps.get(2))  // "04"
    }
    None -> print("no match")
}

// Replace
print(re.replace("a1b2c3", "X"))       // "aXbXcX"
print(re.replaceFirst("a1b2c3", "X")) // "aXb2c3"
```

- Start with a runtime `Regex.new()` that returns `Result<Regex, RegexError>`
- When `comptime` (5.5) is added, a `regex("...")` function validates patterns at compile time
- **Complexity:** Medium — use an existing regex engine (Rust's `regex` crate is already available as a dependency) and wrap it for Phoenix. The main work is the type integration and method bindings.

### Iterator protocol

A lazy evaluation protocol for processing sequences without materializing intermediate collections. Important for large datasets, streaming, and composable data pipelines.

```phoenix
// The Iterator trait
trait Iterator {
    type Item
    function next(mut self) -> Option<Self.Item>
}

// Lazy chaining — no intermediate lists are created
let result: List<String> = users
    .iter()
    .filter(function(u: User) -> Bool { u.age >= 18 })
    .map(function(u: User) -> String { u.name })
    .take(10)
    .collect()

// Ranges are iterators
for i in 0..1000000 {
    if i > 10 { break }  // only 11 iterations, not 1M
}

// Custom iterators
struct Fibonacci {
    Int a
    Int b
}

impl Iterator for Fibonacci {
    type Item = Int
    function next(mut self) -> Option<Int> {
        let val: Int = self.a
        self.a = self.b
        self.b = val + self.b
        Some(val)
    }
}

let fibs: List<Int> = Fibonacci(0, 1).take(10).collect()
```

- Existing eager methods (`map`, `filter`, etc.) on `List<T>` remain for convenience
- `.iter()` converts a collection to a lazy iterator
- `.collect()` materializes an iterator back into a collection
- `for` loops work on anything that implements `Iterator`
- **Complexity:** Medium-high — requires an associated type system (`type Item`), lazy evaluation semantics, and integration with the `for` loop desugaring. The associated type feature is a prerequisite that affects the trait system.
- **Depends on:** Traits (Phase 1, complete), associated types (new trait feature)

### Error context and chaining

Ergonomic error handling for adding context as errors propagate up the call stack. The `?` operator already handles the mechanical propagation — this adds the "why did it fail" layer.

```phoenix
// Wrap an error with additional context
function loadConfig(path: String) -> Result<Config, AppError> {
    let contents: String = readFile(path)
        .context("failed to load config file: {path}")?
    let config: Config = parseJson(contents)
        .context("config file has invalid JSON")?
    Ok(config)
}

// .context() wraps any error type into a ContextError with a message chain
// When printed, it shows the full chain:
//   failed to load config file: app.json
//   caused by: file not found: app.json

// For custom error types, implement the Error trait
trait Error {
    function message(self) -> String
    function source(self) -> Option<Error>
}
```

- `.context(String)` is available on any `Result<T, E>` — wraps `E` into a `ContextError` that chains messages
- `ContextError` implements `Error` and stores the chain of causes
- When printed or logged, the full chain is displayed (similar to Rust's `anyhow` or Go's `fmt.Errorf` wrapping)
- **Complexity:** Small — `.context()` is a method on `Result`, `ContextError` is a built-in type, and the `Error` trait is a simple interface.

### Struct update syntax

Create a copy of a struct with some fields changed, without mutating the original. Essential for functional patterns and immutable data transformation (common in web frameworks, state management, API response mapping).

```phoenix
struct User {
    String name
    String email
    Int age
}

let alice: User = User("Alice", "alice@example.com", 30)

// Create a new User with some fields changed — unlisted fields are copied
let updated: User = User { ...alice, email: "newalice@example.com" }
print(updated.name)   // Alice (copied)
print(updated.email)  // newalice@example.com (overridden)
print(updated.age)    // 30 (copied)

// Original is unchanged
print(alice.email)     // alice@example.com

// Works well with deeply nested updates
let config: ServerConfig = ServerConfig {
    ...defaultConfig,
    database: DatabaseConfig { ...defaultConfig.database, port: 5433 }
}
```

- `Type { ...source, field: value }` syntax — the `...` spread copies all fields from `source`, then named fields override specific values
- The spread source must be the same type as the struct being constructed
- All private fields are copied from the source (you can spread a struct even if you can't access its private fields individually)
- **Complexity:** Small-medium — parser change for `...expr` in struct literal position, type checking to ensure same-type spread, code generation to copy non-overridden fields.

## 4.2 I/O, File System, and Configuration

- File reading and writing
- Standard input/output
- Path manipulation
- Environment variables

### Environment and configuration

Type-safe configuration loading from environment variables, `.env` files, and config files. Every web application needs configuration (database URLs, API keys, feature flags, port numbers) and getting it wrong is a common source of bugs and security incidents.

```phoenix
// Define a config struct — fields map to environment variable names
@config(prefix: "APP")
struct AppConfig {
    @config(env: "DATABASE_URL")
    String databaseUrl

    Int port               // reads APP_PORT, defaults to env var name based on prefix + field name

    @config(default: "info")
    String logLevel       // reads APP_LOG_LEVEL, falls back to "info" if missing

    Option<String> secret  // reads APP_SECRET, None if missing (optional fields don't fail)
}

// Load config — validates all required fields are present
let config: Result<AppConfig, ConfigError> = Config.load<AppConfig>()

// ConfigError lists all missing/invalid variables at once (not one at a time)
match config {
    Ok(c) -> startServer(c)
    Err(e) -> {
        print("Configuration error:")
        print(e.message())  // "missing required: APP_DATABASE_URL, APP_PORT"
    }
}
```

- `.env` file loading: `Config.loadDotenv(".env")` reads a `.env` file into the environment before loading the config struct
- All required fields (non-`Option`, no `@config(default: ...)`) must be present — a missing value is an error
- Type coercion: environment variables are strings, but `Int`, `Float`, `Bool` fields are automatically parsed (invalid format is a `ConfigError`)
- All validation happens at once — the error reports all missing/invalid fields, not just the first one
- When refinement types (5.2) are available, config fields can have constraints: `Int port where port > 0 and port < 65536`
- **Complexity:** Small-medium — the config loader is a library function, annotations provide the metadata, and serialization (4.6) provides the type coercion machinery.
- **Depends on:** Annotations (4.5), Built-in serialization (4.6/5.1)

## 4.3 Async Runtime and Structured Concurrency

Phoenix uses **structured concurrency** — every spawned task has a parent scope, and the parent cannot complete until all its children have finished or been cancelled. This prevents orphaned tasks, leaked resources, and fire-and-forget concurrency bugs.

**Open design decisions for this phase** (see [design-decisions.md](../design-decisions.md)):

- **[`mut` aliasing / shared mutable captures](../design-decisions.md#mut-gives-no-aliasing-guarantees-closures-share-mutable-captures-freely)** — today closures can share mutable captures freely. Benign under single-threaded execution; the shape of a data race once tasks run concurrently. Decide task-boundary constraint model (forbid shared mutable captures? require `Atomic` / `Mutex` at task-crossing? channel-only?) before `spawn` / task APIs get designed here — retrofitting `Send`-equivalent bounds after task APIs exist creates permanent coloring problems.
- **[`defer` for resource cleanup](../design-decisions.md#defer-for-resource-cleanup)** — forced by the tracing GC decision (no `Drop`-style deterministic destruction). The "do we need it" question is answered yes; syntax (`defer`, `using`, `with`) still open. Resolve before async resource-management patterns solidify.

**GC context:** the [tracing GC decision](../design-decisions.md#gc-strategy) was made partly with this phase in mind — tracing GCs compose cleanly with concurrent collection, whereas RC would need atomic refcount ops (10–100× slower under contention).

### Core primitives

- `async function` — declares a function that can `await` and be suspended
- `await expr` — suspends until the async operation completes
- `spawn expr` — launches a child task, returns `Task<T>`
- `TaskGroup` — manages a set of related tasks with collective cancellation

### Task groups and scoped concurrency

```phoenix
async function fetchAll() -> Result<List<String>, HttpError> {
  // TaskGroup ensures all spawned tasks complete (or are cancelled) before
  // the group scope exits — no orphaned tasks possible
  let results = await TaskGroup.run(function(group: TaskGroup) {
    group.spawn(fetchData("https://api.example.com/users"))
    group.spawn(fetchData("https://api.example.com/posts"))
  })
  // `results` contains all task results in spawn order
  Ok(results)
}
```

- `TaskGroup.run(fn)` creates a scope — the closure receives the group and can spawn tasks into it
- The group `await`s all spawned tasks before returning
- If any task fails and the error propagates (via `?`), the group **cancels all remaining tasks** before returning the error

### Cancellation model

Cancellation is **cooperative** — a cancelled task is not killed mid-execution. Instead:

1. The runtime sets a cancellation flag on the task
2. At the next `await` point, the task checks the flag and returns `Err(CancelledError)` instead of resuming
3. The task's `Result` type naturally propagates the cancellation — no special syntax needed
4. Tasks can also check `Task.isCancelled()` for more granular control

```phoenix
async function longPoll(url: String) -> Result<String, HttpError> {
  while true {
    // Each await is a cancellation point — if this task is cancelled,
    // the await returns Err(CancelledError) instead of the response
    let response = await http.get(url)?
    if response.body != "pending" {
      return Ok(response.body)
    }
    await sleep(1000)
  }
}

async function fetchFirst(url1: String, url2: String) -> Result<String, HttpError> {
  // Race two tasks — when the first completes, cancel the other
  await TaskGroup.race(function(group: TaskGroup) {
    group.spawn(longPoll(url1))
    group.spawn(longPoll(url2))
  })
}
```

### Key APIs

- `TaskGroup.run(fn)` — run all spawned tasks, wait for all, cancel remaining on first error
- `TaskGroup.race(fn)` — run all spawned tasks, return first result, cancel remaining
- `TaskGroup.any(fn)` — run all spawned tasks, return first `Ok`, cancel remaining (ignores errors unless all fail)
- `Task.cancel()` — request cancellation of a specific task
- `Task.isCancelled() -> Bool` — check if cancellation was requested
- `sleep(Int ms)` — suspend for a duration (also a cancellation point)
- `timeout(Int ms, async fn) -> Result<T, TimeoutError>` — cancel a task if it exceeds a deadline

### Additional runtime features

- Event loop (single-threaded for simplicity, like Node.js)
- Timers (`sleep`, `interval`)
- Channels for inter-task communication (`Channel<T>` with `send`/`recv`)
- All `await` points are cancellation points — no explicit opt-in needed

### Background jobs and scheduled tasks

Many web applications need periodic or deferred work beyond request handling: cleanup jobs, notification sends, report generation, cache warming, health checks. Phoenix provides a built-in scheduler that integrates with structured concurrency.

```phoenix
// Define a recurring job
async function cleanupExpiredSessions() -> Result<Void, DbError> {
    let deleted: Int = await db.execute(
        DELETE FROM sessions WHERE expires_at < $now,
        now: Instant.now()
    )
    log.info("cleaned up {deleted} expired sessions")
    Ok(())
}

// Schedule jobs in the server setup
async function main() {
    let scheduler: Scheduler = Scheduler.new()

    // Cron-style scheduling
    scheduler.every(Duration.minutes(15), cleanupExpiredSessions)
    scheduler.every(Duration.hours(1), generateReports)

    // One-off delayed execution
    scheduler.after(Duration.seconds(30), warmCache)

    // Start the scheduler alongside the server
    await TaskGroup.run(function(group: TaskGroup) {
        group.spawn(server.serve(app))
        group.spawn(scheduler.run())
    })
}
```

- `Scheduler` runs within structured concurrency — when the parent scope exits, scheduled jobs are cancelled cleanly
- Jobs that fail log the error and continue on schedule (configurable: retry, backoff, or stop)
- `scheduler.every()` for recurring jobs, `scheduler.after()` for one-off deferred work
- Jobs run as spawned tasks within the scheduler's `TaskGroup`, so they respect cancellation and don't leak
- For persistent job queues (surviving restarts), use the database as a job store — this is a library concern, not a language primitive

- **Why structured:** Unstructured concurrency (Go goroutines, JS `Promise.all` with no scope) leads to leaked tasks, uncaught errors, and resource cleanup bugs. Structured concurrency (Kotlin coroutines, Swift structured concurrency, Java virtual threads) guarantees that task lifetimes are bounded by their parent scope. This is especially important for a web server where each request handler spawns subtasks — if the request is aborted, all its subtasks must be cancelled.
- **Why cooperative cancellation:** Preemptive cancellation (killing a task at any point) is unsafe — it can leave shared state in an inconsistent state. Cooperative cancellation (checking at `await` points) is safe because the task always reaches a consistent state before observing the cancellation. This is the model used by Kotlin, Swift, and Python asyncio.
- **Complexity:** High — requires a task scheduler, cancellation propagation, `TaskGroup` scoping, and integration with I/O operations (HTTP, database, timers) so they respect cancellation.
- **Depends on:** Compilation (2.2), Closures (1.2)

## 4.4 HTTP and Typed Routing

- HTTP client: `http.get(url)`, `http.post(url, body)`
- HTTP server: `http.listen(addr, port)` with typed route handlers
- Request/Response types with headers, status codes, body
- Built-in JSON parsing and serialization (leveraging built-in serialization)

### Typed router

Routes are declared in a `router` block. The compiler validates handler signatures against URL patterns, checks for conflicts, and generates the dispatcher.

```phoenix
router app {
  GET  "/"                     -> handleIndex
  GET  "/api/users"            -> handleListUsers
  GET  "/api/users/{id: Int}"  -> handleGetUser
  POST "/api/users"            -> handleCreateUser
  GET  "/api/posts/{slug: String}" -> handleGetPost
}

// Handler signature must match the route's path parameters
async function handleGetUser(id: Int) -> Response {
  let user = await db.query(SELECT name, email FROM users WHERE id = $id)
  Response.json(user)
}

async function main() {
  let server = http.listen("0.0.0.0", 8080)
  await server.serve(app)  // pass the router to the server
}
```

**Compile-time guarantees:**

- **Signature validation**: `handleGetUser` must accept `id: Int` because the route declares `{id: Int}` — a type mismatch or missing parameter is a compile error
- **Conflict detection**: two routes that match the same URL pattern (e.g. `GET "/api/{x: Int}"` and `GET "/api/{name: String}"`) are a compile error, not a runtime ambiguity
- **Exhaustive HTTP methods**: if you define `GET` and `POST` for a path, the router automatically returns `405 Method Not Allowed` for other methods — no manual handling needed
- **Generated dispatcher**: URL pattern matching and parameter extraction (path params, query params) are compiled into efficient matching code, not interpreted at runtime

### Integration with typed endpoints

When typed endpoints (Section 5.4) are implemented, they automatically register in a router:

```phoenix
// The endpoint declaration...
endpoint getUser: GET "/api/users/{id}" { ... }

// ...automatically creates a route. The router just references endpoints:
router api {
  endpoint getUser
  endpoint createUser
  endpoint listPosts
}
```

This closes the loop: the endpoint defines the contract (types, serialization, errors), the router wires it to a URL, and the compiler validates everything at compile time.

### Middleware and auth guards

Routers support middleware — functions that wrap handlers and can short-circuit with an error response. The compiler verifies that middleware return types are compatible with the handler they wrap.

```phoenix
// Middleware that checks authentication — returns the handler's response
// type or an early error response
async function requireAuth(req: Request) -> Result<AuthenticatedUser, Response> {
  let token = req.header("Authorization")
  match verifyJwt(token) {
    Ok(user) -> Ok(user)
    Err(_) -> Err(Response.error(401, "Unauthorized"))
  }
}

router api {
  // Public routes — no middleware
  GET  "/api/health"          -> handleHealth
  POST "/api/login"           -> handleLogin

  // Protected routes — requireAuth runs before the handler
  // The compiler verifies handleGetUser accepts AuthenticatedUser as its first parameter
  with requireAuth {
    GET  "/api/users"           -> handleListUsers
    GET  "/api/users/{id: Int}" -> handleGetUser
    POST "/api/users"           -> handleCreateUser
  }
}

// Handler receives the AuthenticatedUser from the middleware
async function handleGetUser(caller: AuthenticatedUser, id: Int) -> Response {
  let rows = await db.query(SELECT name, email FROM users WHERE id = $id)
  Response.json(rows.get(0))
}
```

The `with` block applies middleware to a group of routes. The compiler checks that:

- The middleware's `Ok` type matches the handler's first parameter (e.g. `AuthenticatedUser`)
- The middleware's `Err` type is `Response` (so it can short-circuit)
- Every non-public route has appropriate middleware — routes without a `with` block can optionally be required to be annotated `public` to catch accidentally unprotected endpoints

Auth-specific implementations (JWT verification, session management, OAuth, password hashing) belong in the standard library (Phase 4) and starter templates (Phase 6.3), not the language. The router provides the typed middleware mechanism; the stdlib provides the auth logic.

### WebSockets

Full-duplex communication for real-time features: chat, live notifications, collaborative editing, dashboards, multiplayer. WebSocket support is critical for a full-stack web language — most modern web applications need some form of real-time communication.

```phoenix
// Server: WebSocket handler
router app {
    GET "/api/users" -> handleListUsers
    WS  "/ws/chat"   -> handleChat       // WebSocket route
}

async function handleChat(ws: WebSocket) {
    // Send a welcome message
    await ws.send("Welcome to the chat!")

    // Receive messages in a loop
    while true {
        match await ws.recv() {
            Ok(Message.Text(text)) -> {
                // Broadcast to all connected clients
                await broadcast(text)
            }
            Ok(Message.Close) -> break
            Err(e) -> {
                log.error("WebSocket error: {e}")
                break
            }
        }
    }
}

// Client (WASM): connect to a WebSocket
async function connectChat() -> Result<WebSocket, WsError> {
    let ws: WebSocket = await WebSocket.connect("wss://example.com/ws/chat")

    // Send and receive
    await ws.send("Hello from Phoenix!")
    let msg: Message = await ws.recv()?
    Ok(ws)
}
```

- `WS` route type in the router — the compiler validates the handler accepts `WebSocket` as its parameter
- `WebSocket` type with `send()`, `recv()`, `close()` methods — all async
- `Message` enum: `Text(String)`, `Binary(List<Int>)`, `Ping`, `Pong`, `Close`
- Works on both server (native) and client (WASM) — same API
- Integrates with structured concurrency: WebSocket handlers are tasks that can be cancelled

### Server-Sent Events (SSE)

Simpler alternative to WebSockets for server-to-client streaming. Many real-time use cases (live feeds, progress updates, notifications) only need one-way data flow.

```phoenix
router app {
    SSE "/events/feed" -> handleFeed
}

async function handleFeed(stream: SseStream) {
    let mut counter: Int = 0
    while not stream.isClosed() {
        await stream.send(SseEvent {
            event: "update",
            data: json.encode(getLatestData())
        })
        counter = counter + 1
        await sleep(1000)
    }
}
```

- `SSE` route type, `SseStream` type with `send(SseEvent)` and `isClosed()`
- Automatic reconnection handling (sends `Last-Event-ID` header)
- Lower overhead than WebSockets for one-way streaming

- **Complexity:** Medium — requires a URL pattern parser, a route conflict checker, code generation for the dispatcher, and middleware chaining with type validation. The pattern matching is straightforward (static segments + typed captures). Integration with typed endpoints adds minimal overhead since endpoints already declare their HTTP method and URL. WebSocket support adds a protocol upgrade handler and frame parser. SSE adds a streaming response writer.
- **Depends on:** Async runtime (4.3), Built-in serialization (4.6/5.1)

## 4.5 Annotation System

A general-purpose annotation/attribute system for attaching metadata to declarations and fields. Annotations use the `@name` syntax (or `@name(args)` with arguments) and can appear on struct fields, struct declarations, and (later) function declarations and enum variants.

### Syntax

```phoenix
@jsonSerializable
struct User {
    @primary
    Int id

    @unique
    @jsonName("user_name")
    String name

    @skip
    String cachedDisplayName
}
```

- `@name` — marker annotation (no arguments)
- `@name(arg1, arg2)` — annotation with positional literal arguments (strings, ints, floats, booleans)
- Multiple annotations per field/declaration, one per line (idiomatic) or on the same line
- Unknown annotations produce a **warning**, not an error — forward-compatible with user-defined annotations when `comptime` (5.5) is added

### Built-in annotations (initial set)

| Annotation | Target | Purpose |
|-----------|--------|---------|
| `@jsonName("...")` | Field | Custom JSON key for serialization (4.6) |
| `@skip` | Field | Exclude from serialization |
| `@jsonSerializable` | Struct | Opt-in/opt-out for serialization (design TBD) |

Database-hint annotations (`@primary`, `@unique`, `@index`) may also be recognized, but the canonical source of relational constraints is the `schema` block (4.7). The compiler may cross-validate annotations against schema declarations.

### Implementation notes

- Adds `@` as a new token kind in the lexer
- `@` suppresses the following newline (so `@primary\nInt id` parses as one annotated field)
- AST: new `Annotation` and `AnnotationArg` types; `annotations: Vec<Annotation>` added to `FieldDecl` and `StructDecl`
- Semantic checker validates known annotations (correct target, argument types) and warns on unknowns
- The interpreter ignores annotations — they are compile-time metadata only

- **Why `@` syntax:** Familiar from Java, Kotlin, Python, TypeScript decorators. The `@` sigil is unambiguous in Phoenix's grammar since fields start with a type name (uppercase), making `@lowercase` clearly an annotation, not a field.
- **Why not a macro system:** Annotations are metadata, not code generation. Phoenix's `comptime` (5.5) may later allow user-defined annotation processing, but the initial system is a fixed set known to the compiler.
- **Complexity:** Small-medium — lexer change is trivial, parser change is moderate (new parsing functions, integration with existing struct/field parsing), semantic validation is straightforward.
- **Depends on:** Nothing (can be added at any point)

## 4.6 JSON and Serialization

- Every type auto-serializes (compiler-generated, zero boilerplate)
- `json.encode(value) -> String`
- `json.decode<T>(String) -> Result<T, JsonError>`
- Custom serialization names/formats via annotations (4.5)
- Binary serialization format for high-performance internal communication

## 4.7 Database Access (Compile-Time Typed Queries)

Phoenix validates SQL queries against an explicit schema declaration at compile time. The compiler checks column names, column types, table relationships, and infers the result type from the query's SELECT clause — no manual struct mapping, no runtime type mismatches.

### Schema declarations

Tables can either declare columns inline or reference an existing struct with `from Type`:

```phoenix
struct User {
    Int id
    String name
    String email
    Int age
    @skip
    String cachedDisplayName
}

struct Post {
    Int id
    Int authorId
    String title
    String body
}

schema db {
  // Table backed by a struct — inherits fields as columns
  table users from User {
    primary key id
    unique email
    index age
    exclude cachedDisplayName   // omit non-persistent fields
  }

  // Foreign keys reference other tables
  table posts from Post {
    primary key id
    foreign key authorId references users(id) on delete cascade
    index createdAt
  }

  // Standalone table — declares columns inline (for join tables, audit logs, etc.)
  table sessions {
    String token primary key
    Int userId references users(id)
    Int expiresAt
  }
}
```

- Schemas are declared in Phoenix source files using a `schema` block
- **`from Type`**: table inherits all fields from the struct as columns — no redeclaring types. The body contains only relational constraints (keys, indexes, foreign keys) and `exclude` directives.
- **Standalone tables**: declare columns inline with Phoenix types (`Int`, `String`, `Float`, `Bool`) for tables that don't map 1:1 to a struct
- `exclude fieldName` omits struct fields that shouldn't be database columns (computed values, cached data, etc.)
- `primary key`, `unique`, `index`, `foreign key ... references`, `on delete cascade|set_null|restrict` constraints are expressed in the schema
- `Option<T>` fields map to nullable columns; non-optional fields are `NOT NULL` by default
- The schema is the single source of truth for relational structure — migrations are generated from schema diffs (future work)

### Why relational constraints live in the schema, not on struct fields

Structs are domain types — they represent data in the application. Database schemas are persistence concerns — they describe how data is stored, indexed, and related. These often diverge:

- Not all structs are tables (DTOs, value objects, computation results)
- Composite primary keys, foreign keys, and cascade rules are cross-field or inter-table concerns that don't fit on individual fields
- The same struct might map to different tables in different schemas

Annotations on struct fields (4.5) handle cross-cutting concerns like serialization (`@jsonName`, `@skip`). The schema block handles relational concerns. The compiler cross-validates both.

### Compile-time query validation

```phoenix
// Compiler validates: users.name and users.age exist, age is Int (comparable to 18)
// Result type is inferred as List<{ String name, Int age }>
let adults = await db.query(
  SELECT name, age FROM users WHERE age >= 18
)
print(adults.get(0).name)  // typed as String — no cast needed

// Compiler error: column `nonexistent` does not exist on table `users`
let bad = await db.query(SELECT nonexistent FROM users)

// Compiler error: cannot compare String column `name` to Int literal
let wrong = await db.query(SELECT name FROM users WHERE name > 42)

// Joins are validated — authorId references users.id
let postsWithAuthors = await db.query(
  SELECT posts.title, users.name
  FROM posts
  JOIN users ON posts.authorId = users.id
)
// Result type: List<{ String title, String name }>
```

### Parameterized queries (SQL injection prevention)

```phoenix
// Parameters are type-checked against the schema column types
async function findUser(id: Int) -> Result<{ String name, String email }, DbError> {
  let rows = await db.query(SELECT name, email FROM users WHERE id = $id)
  if rows.length() == 0 {
    return Err(DbError("user not found"))
  }
  Ok(rows.get(0))
}
```

- Query parameters use `$variable` syntax — the compiler checks that the variable type matches the column type
- Parameters are always sent as prepared statement bindings, never string-interpolated — SQL injection is impossible by construction
- The compiler rejects `$name` in a `WHERE id = $name` position if `name` is `String` and `id` is `Int`

### Key features

- **Compile-time validation**: column names, types, table existence, join conditions, and parameter types are all checked before the program runs
- **Inferred result types**: the compiler generates a typed row struct from the SELECT clause — no manual mapping between query results and Phoenix types
- **SQL injection impossible**: parameterized queries are enforced at the language level, not by convention
- **Pluggable backends**: start with PostgreSQL and SQLite; the schema declaration is backend-agnostic
- **Connection pooling**: built-in pool management with configurable size and timeout

- **Why explicit schemas:** Introspecting a live database at compile time requires a running database during builds, which complicates CI/CD and reproducibility. Explicit schema declarations are self-contained, version-controlled, and work offline. Database introspection can be added later as an optional convenience feature.
- **Complexity:** Very high — requires a SQL parser (subset), a type inference pass from SELECT clauses to Phoenix types, schema validation, prepared statement compilation, and backend-specific SQL generation.
- **Depends on:** Compilation (2.2), Async runtime (4.3), Built-in serialization (5.1), Annotations (4.5)

### Auto-generated migrations

Because `schema` blocks are declarative and version-controlled, the compiler can diff the current schema against a migration history and generate DDL automatically. This eliminates hand-written migration files for the common case.

```bash
# Compare current schema to last-applied migration, generate SQL
phoenix migrate generate --name add_user_age

# Apply pending migrations
phoenix migrate apply

# Show what would change without applying
phoenix migrate plan
```

**Generated migration example:**

```sql
-- 0003_add_user_age.sql (auto-generated by phoenix migrate)
ALTER TABLE users ADD COLUMN age INT NOT NULL DEFAULT 0;
CREATE INDEX idx_users_age ON users (age);
```

**Design principles:**

- **Additive changes are automatic**: adding fields/columns, adding tables, adding indexes, adding constraints — the compiler generates these without intervention
- **Destructive changes require confirmation**: dropping columns, dropping tables, or changing column types produce a warning and require an explicit `--allow-destructive` flag or an interactive confirmation. This prevents accidental data loss.
- **Data migrations are explicit**: backfilling values, transforming existing data, or populating new non-nullable columns without defaults cannot be auto-generated. Phoenix generates a migration file with a `TODO` placeholder that the developer fills in:

```phoenix
// 0004_make_email_required.phoenix (generated with placeholder)
migrate "make_email_required" {
  // Auto-generated DDL:
  sql("ALTER TABLE users ALTER COLUMN email SET NOT NULL")

  // TODO: Phoenix detected that column `email` is changing from nullable to
  // non-nullable. Add a data migration to backfill NULL values:
  // sql("UPDATE users SET email = 'unknown@example.com' WHERE email IS NULL")
}
```

- **Migration history is tracked**: a `_phoenix_migrations` table records which migrations have been applied, preventing double-application
- **Rollbacks are opt-in**: the compiler can generate `down` migrations (reverse DDL) for simple changes, but complex data migrations have no automatic reverse. Developers can write explicit rollback logic when needed.
- **Schema-from-struct aware**: when a struct field is added or removed and the table uses `from Type`, the migration reflects the struct change automatically

- **Why not introspect the live database:** The same reason schemas are explicit — reproducibility. Migrations should be generated from code (deterministic), not from a live database whose state may vary across environments. `phoenix migrate plan` can optionally connect to a database to verify the migration is safe, but generation is always from the schema source.
- **Complexity:** High — requires schema diffing, DDL generation per backend (PostgreSQL, SQLite), migration file management, and a migration runner.
- **Depends on:** Schema declarations (4.7), Compilation (2.2)

## 4.8 Logging

- Structured logging: `log.info("user created", userId: id)`
- Log levels: debug, info, warn, error
- Configurable output (stdout, file, structured JSON)
