# Phoenix Language Vision

This document describes the **planned features** that define Phoenix's long-term direction as a full-stack web language. None of the features below are implemented yet — the code examples show the intended design, not working code. For what works today, see the [README](../README.md).

For implementation timeline and priorities, see the [roadmap](roadmap.md).

---

## Execution Model

- **Backend:** Native compiled binaries for high-performance server applications.
- **Frontend:** Compiles to **WebAssembly** for running in browsers.
- Shared core language and standard library ensures **consistency across platforms**.

## Concurrency Model

- **Structured concurrency** — every spawned task belongs to a parent scope; the parent cannot exit until all children complete or are cancelled. No orphaned tasks, no leaked resources.
- **async/await** with lightweight tasks for high-performance async execution.
- **Cooperative cancellation** — cancelled tasks check at each `await` point and exit cleanly. No unsafe preemptive kills.
- **Task groups** — `TaskGroup.run()`, `TaskGroup.race()`, `TaskGroup.any()` for managing concurrent operations with automatic cancellation on failure or early completion.

```phoenix
// Async functions return their result type wrapped in a future
async function fetchData(url: String) -> Result<String, HttpError> {
  let response: Response = await http.get(url)
  if response.status != 200 {
    return Err(HttpError("request failed: {response.status}"))
  }
  Ok(response.body)
}

// Structured concurrency with TaskGroup — all tasks are scoped,
// and if any task fails the remaining tasks are automatically cancelled
async function fetchAll() -> Result<List<String>, HttpError> {
  await TaskGroup.run(function(group: TaskGroup) {
    group.spawn(fetchData("https://api.example.com/users"))
    group.spawn(fetchData("https://api.example.com/posts"))
  })
}
```

## Memory Management

- **Garbage collected** — all heap values are automatically managed at runtime.
- No manual memory management, ownership tracking, or borrow checking required.
- The `mut` keyword controls **mutability**, not ownership.
- Enables **safe, ergonomic code** without the complexity of manual memory management.

## Standard Library

**Opinionated, batteries-included standard library** for web development:

- Async runtime (task scheduling, timers, event loops)
- HTTP client and server
- JSON serialization and deserialization
- Database access layer (basic and pluggable)
- Concurrency primitives (channels, async mutex, atomic operations)
- Utilities: collections, math, logging, and simple I/O

## Built-in Serialization

Every type in Phoenix is **automatically serializable** — no annotations, no boilerplate, no code generation. The compiler knows the structure of every type and can convert it to and from JSON, binary, or any wire format at zero cost.

```phoenix
struct User {
  String name
  String email
  Int age
}

async function handleCreate(req: Request) -> Response {
  // Deserialization is automatic — the compiler generates the parser
  let user: User = req.json()

  // Serialization is automatic — any type can become JSON
  return Response.json(user)
}
```

- **Zero-cost**: serialization code is generated at compile time, not through reflection
- **Type-safe**: deserialization returns `Result<T, SerializeError>` — malformed input is caught, not silently accepted
- **Format-agnostic**: the same type works with JSON, binary protocols, query parameters, and form data
- **Across the stack**: types defined in shared code serialize identically on backend (native) and frontend (WASM)

## Refinement Types

Phoenix supports **refinement types** — types with compile-time constraints on their values. Instead of writing validation logic scattered across your codebase, you encode the rules directly in the type system.

```phoenix
// A type that can only hold positive integers
type PositiveInt = Int where self > 0

// A type for valid email addresses
type Email = String where self.contains("@") && self.length > 3

// A bounded range
type Percent = Float where self >= 0.0 && self <= 100.0

// Non-empty collections
type NonEmptyList<T> = List<T> where self.length > 0

function setVolume(level: Percent) {
  // `level` is guaranteed to be 0.0–100.0
  // No runtime validation needed inside the function
}

setVolume(50.0)   // compiles — 50.0 satisfies Percent
setVolume(150.0)  // compile error — 150.0 violates constraint
```

When the compiler can prove a value satisfies the constraint (literal values, prior checks), no runtime cost is incurred. When it cannot prove it statically (user input, network data), it inserts a validation check at the boundary and returns a `Result`.

## First-Class Reactivity

Phoenix provides **reactive primitives** as built-in types in the standard library. `Signal<T>` holds reactive state, `derived()` computes values that automatically update when their dependencies change, and `effect()` runs side effects in response to state changes.

```phoenix
component Counter {
  let count = signal(0)

  // Derived values recompute automatically when dependencies change
  let label = derived(function() -> String {
    "Count: " + toString(count.get())
  })

  function render() -> Html {
    return <div>
      <p>{label.get()}</p>
      <button onClick={function() { count.set(count.get() + 1) }}>+</button>
    </div>
  }
}
```

- **Fine-grained**: only the specific DOM nodes that depend on changed signals are updated — no tree diffing
- **No hidden magic**: reads are `signal.get()`, writes are `signal.set(value)` — no assignment operator overloading
- **Works with WASM**: reactive components compile to WebAssembly for high-performance frontend rendering

## Frontend Strategy

Phoenix aims to be a **single-language full-stack** web language where compile-time type safety flows from database to DOM. That vision requires a frontend framework — but building one from scratch before the compiler is mature would be premature. The strategy is **JS interop first, native framework second**.

### Stage 1: JavaScript Interop (Phase 2.5)

Phoenix compiles to WebAssembly and calls into the JavaScript ecosystem via `extern js` declarations and npm imports. Developers use **existing frameworks** (React, Svelte, Vue) for the UI layer and write business logic, state management, and backend code in Phoenix. This delivers immediate value: Phoenix's type safety for data and API calls, the JS ecosystem for UI.

```phoenix
// Use React from Phoenix via JS interop
extern js {
  function createElement(tag: String, props: Map<String, String>) -> JsValue
  function render(element: JsValue, root: JsValue)
}

import js "react" { function useState(init: Int) -> (Int, (Int) -> Void) }

// Phoenix logic, rendered by React
async function loadUser(id: Int) -> Result<User, ApiError> {
  await getUser.call(id)  // typed endpoint — compiler checks both sides
}
```

### Stage 2: Phoenix-Native Components (Phase 5.3 + 5.8)

Once WASM compilation, the module system, and the signal runtime are stable, Phoenix ships its own **component model** with fine-grained reactivity — no virtual DOM, no JavaScript runtime overhead. Components compile directly to WASM and manipulate the DOM through targeted mutations.

```phoenix
component UserProfile {
  let user = signal(None: Option<User>)
  let loading = signal(true)

  effect(async function() {
    let result = await getUser.call(currentUserId())
    match result {
      Ok(u) -> user.set(Some(u))
      Err(_) -> ()
    }
    loading.set(false)
  })

  function render() -> Html {
    if loading.get() {
      return <p>"Loading..."</p>
    }
    match user.get() {
      Some(u) -> <div>
        <h1>{u.name}</h1>
        <p>{u.email}</p>
      </div>
      None -> <p>"User not found"</p>
    }
  }
}
```

### Why both stages

- **Stage 1 makes Phoenix usable for frontend today** — developers don't wait years for a framework to mature. They write Phoenix where it's strongest (typed APIs, business logic, backend) and use proven UI tools for the rest.
- **Stage 2 delivers the differentiating promise** — one language, one type system, compile-time safety from database query to DOM node. No marshalling, no FFI overhead, no contract drift between "the Phoenix part" and "the React part."
- **Stage 1 is the fallback for Stage 2** — JS interop remains available after the native framework ships. Developers can use React component libraries, charting packages, or any npm module alongside Phoenix components. The native framework doesn't have to cover every use case on day one.

## Typed Endpoints

Phoenix treats **API endpoints as a language-level concept**. You define an endpoint once — the compiler generates the server handler, the client call function, and all serialization code. The types are checked at compile time across both sides, so the client and server can never drift out of sync.

```phoenix
/** Retrieve a single user by their unique ID */
endpoint getUser: GET "/api/users/{id}" {
  // path params are inferred from the URL pattern — no separate declaration needed
  response User
  error {
    NotFound(404)
    Unauthorized(401)
  }
}

/** Create a new user */
endpoint createUser: POST "/api/users" {
  body CreateUserRequest       // JSON request body
  response User
  error {
    ValidationError(400)
    Conflict(409)
  }
}

// SERVER: the compiler generates a handler you implement
impl getUser {
  async function handle(id: Int) -> Result<User, getUser.Error> {
    let user: User = await db.find(id)?
    Ok(user)
  }
}

// CLIENT: the compiler generates a typed call function
async function showProfile(userId: Int) {
  let result: Result<User, getUser.Error> = await getUser.call(userId)
  match result {
    Ok(user) -> print("Hello, {user.name}")
    Err(NotFound) -> print("User not found")
    Err(Unauthorized) -> print("Not authorized")
  }
}
```

## Typed Database Queries

Phoenix validates SQL queries at **compile time** against an explicit schema declaration. The compiler checks column names, column types, and join conditions, and infers the result type from the SELECT clause.

```phoenix
struct User {
  Int id
  String name
  String email
  Int age
}

schema db {
  table users from User {
    primary key id
    unique email
    index age
  }
}

async function getAdults() -> Result<List<{ String name, Int age }>, DbError> {
  let adults = await db.query(SELECT name, age FROM users WHERE age >= 18)
  Ok(adults)
}

async function findUser(id: Int) -> Result<{ String name, String email }, DbError> {
  let rows = await db.query(SELECT name, email FROM users WHERE id = $id)
  if rows.length() == 0 { return Err(DbError("not found")) }
  Ok(rows.get(0))
}
```

- **Compile-time validation**: column names, types, table existence, and join conditions are checked before the program runs
- **SQL injection impossible**: `$variable` parameters are always sent as prepared statement bindings, never interpolated
- **End-to-end type safety**: a request flows through the entire stack — `client -> endpoint -> handler -> query -> row -> response -> client` — with compile-time type checking at every boundary

## Full-Stack HTTP Server

```phoenix
router app {
  GET  "/"                    -> handleIndex
  GET  "/api/users"           -> handleListUsers
  GET  "/api/users/{id: Int}" -> handleGetUser
}

async function handleIndex() -> Response {
  Response.ok("Welcome to Phoenix!")
}

async function handleListUsers() -> Response {
  let users = await db.query(SELECT name, email FROM users)
  Response.json(users)
}

async function handleGetUser(id: Int) -> Response {
  let rows = await db.query(SELECT name, email FROM users WHERE id = $id)
  if rows.length() == 0 { return Response.error(404, "Not Found") }
  Response.json(rows.get(0))
}

async function main() {
  let server = http.listen("0.0.0.0", 8080)
  print("Phoenix server running on :8080")
  await server.serve(app)
}
```

---

> *Phoenix: named for the bird that rises from its own ashes — a language that reimagines web development to be safe, expressive, and productive, letting developers write full-stack code in a single language with compile-time safety from client to database.*
