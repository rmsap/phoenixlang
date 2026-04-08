# Phase 3: Tooling

**Status: Not started**

Developers will not adopt a language without good tooling. This must come before or alongside the stdlib, not after.

## 3.1 Package Manager

- `phoenix.toml` manifest file (name, version, dependencies)
- Dependency resolution (semver)
- Start with git-based dependencies, add a registry later
- `phoenix init`, `phoenix add`, `phoenix build`, `phoenix test`
- **Depends on:** Module system and visibility (2.6) — cross-package imports require intra-project modules

## 3.2 Language Server Protocol (LSP)

- Go-to-definition, hover for type info, find references
- Real-time error diagnostics (run the type checker on every keystroke)
- Auto-completion for fields, methods, and function parameters
- VS Code extension as the first-class IDE integration
- **Why critical:** developers evaluate a language by opening a file in their editor. If there's no autocomplete or inline errors, the language feels broken.

## 3.3 Formatter

- `phoenix fmt` — opinionated code formatter
- One canonical style (no configuration bikeshedding)
- Format-on-save in the LSP

## 3.4 Test Framework

A built-in, batteries-included test framework. Tests are first-class — no external test runner, no registration boilerplate, no separate test DSL. The annotation system (4.5) provides the `@test` marker, and the compiler discovers and runs tests automatically.

### Core: `@test` annotation and assertions

```phoenix
import math { sqrt }

@test
function testSqrtOfPerfectSquare() {
    assertEq(sqrt(25.0), 5.0)
}

@test
function testSqrtOfZero() {
    assertEq(sqrt(0.0), 0.0)
}

@test
function testNegativeSqrtReturnsError() {
    let result: Result<Float, MathError> = sqrtChecked(-1.0)
    assert(result.isErr())
}
```

```bash
# Run all tests in the project
phoenix test

# Run tests in a specific file
phoenix test tests/math_test.phx

# Run tests matching a name pattern
phoenix test --filter "sqrt"

# Run with verbose output (show passing tests too)
phoenix test --verbose
```

**Test discovery:** The compiler scans all project files for functions annotated with `@test`. No registration, no test suites, no manifest. A function with `@test` is a test.

**Assertions:**

| Function | Purpose |
|----------|---------|
| `assert(Bool)` | Fails if the value is `false` |
| `assertEq(T, T)` | Fails if the two values are not equal; prints both values on failure |
| `assertNe(T, T)` | Fails if the two values are equal |
| `assertErr(Result<T, E>)` | Fails if the result is `Ok` |
| `assertOk(Result<T, E>)` | Fails if the result is `Err` |
| `assertSome(Option<T>)` | Fails if the option is `None` |
| `assertNone(Option<T>)` | Fails if the option is `Some` |
| `fail(String)` | Unconditionally fails with a message |

On failure, assertions print the source location, the expression that failed, and the actual values involved — no guessing what went wrong.

### Async test support

```phoenix
@test
async function testFetchUser() {
    let user: Result<User, DbError> = await findUser(1)
    assertOk(user)
    assertEq(user.unwrap().name, "Alice")
}
```

Async tests run within the async runtime. Each test gets its own task scope — if it spawns subtasks, they are cleaned up automatically when the test completes (structured concurrency applies to tests too).

### Test lifecycle: setup and teardown

```phoenix
// Shared setup for a group of tests
@beforeEach
function setup() -> TestContext {
    TestContext {
        db: createTestDatabase(),
        user: User("Alice", "alice@example.com", 30)
    }
}

@afterEach
function teardown(ctx: TestContext) {
    ctx.db.drop()
}

// Tests that accept the context type receive the setup result
@test
function testInsertUser(ctx: TestContext) {
    let result = ctx.db.insert(ctx.user)
    assertOk(result)
}

@test
function testDuplicateEmailFails(ctx: TestContext) {
    ctx.db.insert(ctx.user).unwrap()
    let duplicate = ctx.db.insert(ctx.user)
    assertErr(duplicate)
}
```

- `@beforeEach` runs before every test in the same file; its return value is passed to tests that declare a matching parameter
- `@afterEach` runs after every test, receiving the same context (for cleanup)
- Tests without a context parameter skip setup/teardown — they're standalone

### HTTP testing utilities

Test route handlers without starting a server or making real network requests.

```phoenix
import testing.http { TestClient }

@test
async function testGetUserReturns200() {
    let client: TestClient = TestClient.fromRouter(app)

    let response = await client.get("/api/users/1")
    assertEq(response.status, 200)

    let body: User = response.json<User>().unwrap()
    assertEq(body.name, "Alice")
}

@test
async function testMissingUserReturns404() {
    let client: TestClient = TestClient.fromRouter(app)

    let response = await client.get("/api/users/999")
    assertEq(response.status, 404)
}

@test
async function testCreateUser() {
    let client: TestClient = TestClient.fromRouter(app)

    let response = await client.post("/api/users", json: User("Bob", "bob@example.com", 25))
    assertEq(response.status, 201)
}
```

- `TestClient.fromRouter(router)` creates an in-memory client that dispatches requests through the router without TCP — fast and isolated
- Supports all HTTP methods, headers, JSON bodies, and query parameters
- WebSocket testing: `TestClient.ws("/ws/chat")` returns a `TestWebSocket` for send/receive assertions

### Database test helpers

```phoenix
import testing.db { testTransaction }

@test
async function testUserQuery() {
    // testTransaction wraps the test in a DB transaction that rolls back on completion
    // — no test data leaks between tests, no cleanup needed
    await testTransaction(db, async function(tx: Transaction) {
        await tx.execute(INSERT INTO users (name, email, age) VALUES ($n, $e, $a),
            n: "Alice", e: "alice@example.com", a: 30)

        let users = await tx.query(SELECT name FROM users WHERE age >= 18)
        assertEq(users.length(), 1)
        assertEq(users.get(0).name, "Alice")
    })
    // Transaction is automatically rolled back — the users table is unchanged
}
```

- `testTransaction()` wraps a test body in a database transaction that rolls back when the test completes — tests are isolated without manual cleanup
- Works with the schema system (4.7) — the test database matches the declared schema

### Snapshot testing

Capture the output of a function and compare it against a saved reference. Useful for API responses, serialization output, error messages, and rendered HTML.

```phoenix
import testing.snapshot { assertSnapshot }

@test
function testUserSerialization() {
    let user: User = User("Alice", "alice@example.com", 30)
    let json: String = json.encode(user)

    // First run: saves the snapshot to tests/snapshots/testUserSerialization.snap
    // Subsequent runs: compares against the saved snapshot
    assertSnapshot("user_json", json)
}

@test
async function testApiResponse() {
    let client: TestClient = TestClient.fromRouter(app)
    let response = await client.get("/api/users/1")
    assertSnapshot("getUserResponse", response.body)
}
```

```bash
# Update snapshots when output intentionally changes
phoenix test --update-snapshots
```

- Snapshots are stored as plain text files in a `snapshots/` directory alongside the test file
- `phoenix test` shows a diff when a snapshot doesn't match
- `phoenix test --update-snapshots` accepts the new output and overwrites the snapshot file

### Test execution model

- **Parallel by default**: tests in different files run in parallel; tests within a file run sequentially (to respect `@beforeEach`/`@afterEach` ordering)
- **Isolation**: each test gets its own scope — no shared mutable state between tests unless explicitly passed through context
- **Fail-fast mode**: `phoenix test --fail-fast` stops after the first failure
- **Filtering**: `phoenix test --filter "user"` runs only tests whose name contains "user"
- **Output**: clean, minimal output by default (only failures); `--verbose` shows all tests; failure output includes source location, assertion expression, and actual/expected values
- **Exit code**: `0` if all tests pass, `1` if any test fails — integrates with CI

### Future integration with property-based testing

When refinement types (5.2) are available, the test framework can generate random values that satisfy type constraints:

```phoenix
import testing.property { assertProperty }

// The framework generates random PositiveInt values and checks the property holds for all of them
@test
function testSqrtIsPositive() {
    assertProperty(function(n: PositiveInt) -> Bool {
        sqrt(toFloat(n)) >= 0.0
    })
}
```

This is deferred until refinement types exist, but the `@test` annotation and assertion infrastructure are designed to support it.

- **Why `@test` over `test` blocks:** Annotations are already planned (4.5) and provide a uniform metadata mechanism. A `test` keyword would add a new syntactic form for something that's just a function with metadata. Using `@test` means tests are regular functions — they can be async, accept parameters, return values, and use all normal language features.
- **Why built-in over library:** A test runner that understands the compiler (annotation discovery, async runtime integration, type-aware assertions) provides a better experience than a third-party library. The compiler can give source-level failure messages, and `phoenix test` works with zero configuration.
- **Complexity:** Medium — test discovery via annotations is straightforward once annotations exist. The bulk of the work is the test runner (parallel execution, output formatting, fail-fast), HTTP test client (in-memory request dispatch), and snapshot infrastructure (file management, diffing). Database test helpers depend on the database layer (4.7).
- **Depends on:** Annotations (4.5) for `@test`/`@beforeEach`/`@afterEach`; Async runtime (4.3) for async tests; HTTP (4.4) for `TestClient`; Database (4.7) for `testTransaction`

## 3.5 Error Messages

- Invest heavily in error message quality
- Every error should say what went wrong, where, and suggest a fix
- Use source-annotated diagnostics (like Rust's or Elm's error messages)
- This is not a feature — it is a continuous effort that should improve with every release
