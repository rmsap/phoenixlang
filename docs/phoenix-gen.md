# Phoenix Gen: Typed API Code Generation

**Status: Proposal**

Phoenix Gen is a **standalone code generation tool** that uses Phoenix syntax to define API schemas and generates idiomatic client SDKs, server handler interfaces, validation logic, and OpenAPI specs for existing languages. It is a parallel workstream to the main language roadmap — buildable now with the existing parser and type checker — designed to bring Phoenix's type safety story to developers before the full compiler exists.

## Motivation

Phoenix's most differentiating features — typed endpoints, built-in serialization, refinement types — are planned for Phases 4-5 of the language roadmap, which depend on compilation (Phase 2) and the async runtime. That means the features most likely to drive adoption are years away from being usable.

Phoenix Gen inverts this by extracting the **schema and code generation** aspects of typed endpoints into a standalone tool that works today. Developers write `.phx` schema files, run `phoenix gen`, and get typed code in their existing language. This:

- **Proves the value proposition now.** Developers experience Phoenix's type safety without adopting a new language.
- **Builds a user base.** Every developer using `phoenix gen` learns Phoenix syntax and joins the ecosystem.
- **Validates the design.** Real-world usage of `endpoint` and `schema` declarations shapes the full language design with feedback instead of speculation.
- **Creates a migration path.** When the full language ships, `.phx` schema files become importable modules with zero rewrite.

## How it works

### 1. Define schemas in Phoenix syntax

Schema files use a subset of existing Phoenix syntax (structs, enums, type aliases) plus new `endpoint` and `schema` declarations:

```phoenix
/** A registered user */
struct User {
  Int id
  String name
  String email
  Int age
}

/** List all users, optionally filtered by search query */
endpoint listUsers: GET "/api/users" {
  query {
    Int page = 1              // default value
    Int limit = 20
    Option<String> search     // optional, can be omitted
  }
  response List<User>
}

/** Create a new user */
endpoint createUser: POST "/api/users" {
  body User omit { id }       // all User fields except id
  response User
  error {
    ValidationError(400)
    Conflict(409)
  }
}

/** Update an existing user — all fields optional */
endpoint updateUser: PUT "/api/users/{id}" {
  body User omit { id } partial         // name, email, age — all optional
  response User
  error { NotFound(404) }
}

/** Get a user by ID */
endpoint getUser: GET "/api/users/{id}" {
  // path params are inferred from the URL pattern
  response User
  error { NotFound(404) }
}
```

### 2. Generate code for target languages

```bash
# Generate TypeScript client and server
phoenix gen --target typescript --out ./generated

# Generate only a client SDK (no server handlers)
phoenix gen --target typescript --client --out ./frontend/src/generated

# Generate only server handlers (no client SDK)
phoenix gen --target python --server --out ./backend/generated

# Generate OpenAPI 3.1 spec
phoenix gen --target openapi --out ./api.yaml
```

The `--client` and `--server` flags generate only the relevant half. This is how you support **different languages for frontend and backend** — the schema is the shared contract, and each side gets code in its own language (see [Cross-language usage](#cross-language-usage) below).

### 3. Use the generated code in existing projects

**TypeScript client** (generated):
```typescript
// generated/client.ts

/** A registered user */
export interface User {
  id: number;
  name: string;
  email: string;
  age: number;
}

// Derived types — generated from `omit`, `pick`, and `partial` operators
export type CreateUserBody = Omit<User, "id">;
export type UpdateUserBody = Partial<Omit<User, "id">>;

export const api = {
  /** List all users, optionally filtered by search query */
  async listUsers(opts?: { page?: number; limit?: number; search?: string }): Promise<User[]> { ... },
  /** Create a new user */
  async createUser(body: CreateUserBody): Promise<User> { ... },
  /** Update an existing user */
  async updateUser(id: number, body: UpdateUserBody): Promise<User> { ... },
  /** Get a user by ID */
  async getUser(id: number): Promise<User> { ... },
}
```

**TypeScript server handlers** (generated interface, developer implements):
```typescript
// generated/handlers.ts
export interface Handlers {
  listUsers(query: { page: number; limit: number; search?: string }): Promise<User[]>;
  createUser(body: CreateUserBody): Promise<User>;
  updateUser(id: number, body: UpdateUserBody): Promise<User>;
  getUser(id: number): Promise<User>;
}

// generated/server.ts — Express/Fastify/Hono router wiring
export function createRouter(handlers: Handlers): Router { ... }
```

**OpenAPI spec** (generated):
```yaml
# api.yaml — usable with Postman, API gateways, documentation tools
openapi: "3.1.0"
paths:
  /api/users:
    get:
      operationId: listUsers
      description: "List all users, optionally filtered by search query"
      parameters:
        - name: page
          in: query
          schema: { type: integer, default: 1 }
        - name: limit
          in: query
          schema: { type: integer, default: 20 }
        - name: search
          in: query
          required: false
          schema: { type: string }
      responses:
        '200':
          content:
            application/json:
              schema:
                type: array
                items:
                  $ref: '#/components/schemas/User'
    post:
      operationId: createUser
      # ...
```

### Cross-language usage

A single `.phx` schema can generate client code in one language and server code in another. This is a key differentiator over tools like tRPC, which lock both sides into TypeScript.

**Example: TypeScript frontend + Python backend**

```bash
# Frontend team generates a typed fetch client
phoenix gen --target typescript --client --out ./frontend/src/api

# Backend team generates FastAPI handler stubs
phoenix gen --target python --server --out ./backend/api
```

From the same schema, the TypeScript frontend gets:
```typescript
// frontend/src/api/client.ts
export const api = {
  async createUser(body: CreateUserBody): Promise<User> { ... },
  async getUser(id: number): Promise<User> { ... },
}
```

And the Python backend gets:
```python
# backend/api/handlers.py
from dataclasses import dataclass
from typing import Optional

@dataclass
class User:
    id: int
    name: str
    email: str
    age: int

@dataclass
class CreateUserBody:
    name: str
    email: str
    age: int

# Handler interface — developer implements these
class Handlers:
    async def create_user(self, body: CreateUserBody) -> User: ...
    async def get_user(self, id: int) -> User: ...
```

Both sides are derived from the same `.phx` file. If a field is added, renamed, or removed in the schema, both the client and server code are regenerated — the contract cannot drift out of sync.

**Other cross-language combinations work the same way:**
- TypeScript frontend + Go backend
- TypeScript frontend + Rust backend
- React Native client + Python backend
- Multiple clients (web, mobile, CLI) from one schema

The `phoenix.toml` config supports multiple targets simultaneously, so a single `phoenix gen` command can generate everything at once.

## Leverages existing Phoenix infrastructure

Phoenix Gen is not a separate project — it extends the existing codebase:

| Existing component | How Phoenix Gen uses it |
|--------------------|------------------------|
| Phoenix parser | Parses structs, enums, type aliases, generics in `.phx` files |
| Phoenix type checker | Validates that endpoint request/response types exist and are consistent |
| AST representation | Code generators traverse the same AST the interpreter/compiler uses |

The new components are:

| New component | Purpose |
|---------------|---------|
| `endpoint` / `schema` AST nodes | Extend the parser with API-specific declarations |
| Code generation backends | Emit idiomatic code for each target language |
| CLI `gen` subcommand | Orchestrate parsing, checking, and code generation |

## Validation from type constraints

One of the strongest differentiators over existing schema tools is generating validation logic from type constraints. This is a limited form of refinement types (Phase 5.2) that can be implemented without a full constraint solver:

```phoenix
struct User {
  Int id
  String name where self.length > 0 and self.length <= 100
  String email where self.contains("@") and self.length > 3
  Int age where self >= 0 and self <= 150
}

endpoint createUser: POST "/api/users" {
  body User omit { id }       // constraints on name, email, age are inherited
  response User
}
```

The `where` constraints on `User` fields are inherited by derived types. When the code generator emits a validation function for the `createUser` body, it includes the constraints for the fields that are present:

```typescript
export function validateCreateUserBody(input: unknown): CreateUserBody {
  if (typeof input !== 'object' || input === null) throw new ValidationError('expected object');
  const { name, email, age } = input as Record<string, unknown>;
  if (typeof name !== 'string') throw new ValidationError('name: expected string');
  if (!(name.length > 0 && name.length <= 100)) throw new ValidationError('name: length must be 1-100');
  if (typeof email !== 'string') throw new ValidationError('email: expected string');
  if (!(email.includes('@') && email.length > 3)) throw new ValidationError('email: invalid format');
  if (typeof age !== 'number') throw new ValidationError('age: expected number');
  if (!(age >= 0 && age <= 150)) throw new ValidationError('age: must be 0-150');
  return { name, email, age };
}
```

This eliminates an entire category of hand-written boilerplate that every web project requires.

## Competitive landscape

| Tool | Strengths | Gap Phoenix Gen fills |
|------|-----------|----------------------|
| OpenAPI / Swagger | Universal ecosystem support | Verbose YAML, poor authoring experience, mediocre code generators |
| Protobuf / gRPC | Excellent multi-language codegen, dominant in microservices | Designed for RPC, not REST/HTTP APIs with path params and JSON |
| TypeSpec (Microsoft) | Clean DSL, generates OpenAPI | No validation/refinement types, no direct client SDK generation |
| tRPC | Best-in-class DX for TypeScript full-stack | TypeScript-only — locks both client and server to one language |
| Smithy (AWS) | Powerful service modeling | Complex, AWS-centric, steep learning curve |
| GraphQL | Strong type system, introspection | Different paradigm (query language), N+1 problems, complexity for simple APIs |

Phoenix Gen's position: **a clean, expressive schema language with type constraints that generates idiomatic code and OpenAPI specs, without locking you into any particular backend or frontend language.**

## Design decisions

### Naming conventions

Phoenix uses `camelCase` for fields and endpoints. Generated code is automatically converted to the target language's conventions:

| Phoenix (source) | TypeScript | Go | Rust | Python |
|---|---|---|---|---|
| `listUsers` (endpoint) | `listUsers` | `ListUsers` | `list_users` | `list_users` |
| `createdAt` (field) | `createdAt` | `CreatedAt` | `created_at` | `created_at` |
| `User` (type) | `User` | `User` | `User` | `User` |

**JSON wire format uses camelCase.** This is the overwhelming convention for web APIs and what JavaScript developers expect. Phoenix source code already uses `camelCase`, so field names map directly to JSON keys with no conversion needed. Generated server code for other languages (Go, Rust, Python) handles the mapping between Phoenix's `camelCase` field names and the target language's conventions automatically.

### Endpoint structure: path params, query params, and body

Endpoints use distinct sections that map directly to HTTP semantics:

```phoenix
endpoint updateUser: PUT "/api/users/{id}" {
  query {
    Bool notify = false           // query string: ?notify=true
  }
  body User omit { id } partial    // derived type — all User fields except id, all optional
  response User
  error {
    NotFound(404)
    ValidationError(400)
  }
}
```

- **Path params** are inferred from the URL pattern. `{id}` means the endpoint expects an `Int id` parameter — no separate declaration needed. The type is inferred from the matching struct field or defaults to `String`.
- **`query { }`** defines URL query parameters. Supports default values (`Int page = 1`) and optional params (`Option<String> search`). Only valid on any HTTP method, but most common on GET.
- **`body TypeName`** defines the JSON request body. Supports `omit` and `pick` modifiers (see [Type derivation](#type-derivation-omit-and-pick) below). Only valid on POST, PUT, and PATCH — the type checker rejects `body` on GET and DELETE endpoints.
- **`response TypeName`** defines the JSON response body.

This separation matters for code generation: query params become URL-encoded query strings, body becomes JSON serialization, path params become URL template substitution. Collapsing them into a single `request` block would lose this information.

### Type derivation: `omit` and `pick`

API endpoints almost never accept the exact same shape as the full domain type. A `createUser` endpoint shouldn't accept `id` (the server generates it). An `updateUser` endpoint might only allow changing certain fields. Without type derivation, you'd define a separate struct for every request body — duplicating fields, drifting out of sync, and adding boilerplate.

`omit`, `pick`, and `partial` are compile-time type operators that derive a new anonymous type from an existing struct. They chain left to right like a pipeline:

```phoenix
struct User {
  Int id
  String name
  String email
  Int age
  String createdAt
}

// All fields except id and createdAt, all required
endpoint createUser: POST "/api/users" {
  body User omit { id, createdAt }
  response User
}

// All fields except id and createdAt, all optional
endpoint updateUser: PUT "/api/users/{id}" {
  body User omit { id, createdAt } partial
  response User
  error { NotFound(404) }
}

// name is required, email and age are optional
endpoint patchUser: PATCH "/api/users/{id}" {
  body User omit { id, createdAt } partial { email, age }
  response User
  error { NotFound(404) }
}

// Just email, required
endpoint updateEmail: PATCH "/api/users/{id}/email" {
  body User pick { email }
  response User
  error { NotFound(404) }
}
```

- **`omit { field1, field2 }`** — all fields from the base type *except* the listed ones
- **`pick { field1, field2 }`** — *only* the listed fields from the base type
- **`partial`** — bare `partial` makes *all* fields optional (`Option<T>`). Typically used for update endpoints where only changed fields are sent.
- **`partial { field1, field2 }`** — with a field list, makes only the *listed* fields optional. Unlisted fields remain required. This allows a mix of required and optional fields in a single derived type.
- Operators chain left to right: `User omit { id } partial { age }` means "start with User, remove id, make age optional"
- The type checker validates that all named fields exist on the base type — `User omit { nonexistent }` is a compile error, as is `partial { nonexistent }`
- `where` constraints on the base type's fields are inherited by the derived type — an optional field that *is* present still validates its constraint

**Generated TypeScript:**

```typescript
// User omit { id, createdAt }
export type CreateUserBody = Omit<User, "id" | "createdAt">;

// User omit { id, createdAt } partial
export type UpdateUserBody = Partial<Omit<User, "id" | "createdAt">>;

// User omit { id, createdAt } partial { email, age }
export type PatchUserBody = {
  name: string;          // required — not listed in partial
  email?: string;        // optional
  age?: number;          // optional
};
```

This keeps the `User` struct as the single source of truth. Endpoints declare how they relate to it, and the type checker ensures everything stays in sync.

### Error variants and HTTP status codes

Error variants carry explicit status codes — no convention-based guessing:

```phoenix
error {
  NotFound(404)
  Unauthorized(401)
  ValidationError(400)
  Conflict(409)
  RateLimited(429)
}
```

This reuses the existing enum variant-with-payload syntax. The generated code maps each variant to the corresponding HTTP status:

```typescript
// Generated error handling in server router
if (error instanceof NotFoundError) {
  res.status(404).json({ error: "NotFound" });
} else if (error instanceof ValidationError) {
  res.status(400).json({ error: "ValidationError" });
}
```

On the client side, the SDK maps response status codes back to typed error variants, so the caller can match on them.

### Optional fields

`Option<T>` in Phoenix maps to nullable/optional fields in generated code:

| Phoenix | TypeScript | Go | JSON | OpenAPI |
|---|---|---|---|---|
| `String name` | `name: string` | `Name string` | required, must be present | in `required` array |
| `Option<String> name` | `name?: string` | `Name *string` | absent or `null` → `None` | not in `required` |

Both absent and `null` in JSON map to `None`. This matches how most real APIs behave — clients that omit a field and clients that send `null` both mean "no value."

In generated validation: required fields throw if missing, optional fields skip validation when absent and validate the inner type when present.

### Multiple schema files

All `.phx` files matched by the config glob are parsed and merged into a **flat namespace**. A type defined in any file is available in all files. Name conflicts are a compile error.

```
api/
  types.phx         // User, Post, Comment structs
  enums.phx         // Role, Status enums
  users.phx         // user endpoints
  posts.phx         // post endpoints
```

No import syntax needed between schema files. This is simple and works for what schema files actually are — a flat collection of type and endpoint definitions. The full Phoenix language has a proper module system (Phase 2.6); Gen doesn't need that complexity. If a project outgrows a flat namespace, they're likely ready for the full language.

### Doc comments

`/** */` is a doc comment that attaches to the next declaration. Doc comments flow through to generated code as JSDoc, Go doc comments, Python docstrings, and OpenAPI `description` fields:

```phoenix
/** A registered user in the system */
struct User {
  /** Full display name */
  String name
  /** Primary email address */
  String email
  Int age
}

/** Retrieve a single user by their unique ID */
endpoint getUser: GET "/api/users/{id}" {
  response User
  error { NotFound(404) }
}
```

Generates:

```typescript
/** A registered user in the system */
export interface User {
  /** Full display name */
  name: string;
  /** Primary email address */
  email: string;
  age: number;
}
```

In OpenAPI, doc comments populate the `description` field on operations, parameters, and schema properties.

### Enum mapping

**Simple enums** (no payloads) map to string values:

```phoenix
enum Role { Admin, Editor, Viewer }
```

| Target | Output |
|---|---|
| TypeScript | `type Role = "Admin" \| "Editor" \| "Viewer"` |
| Go | `type Role string` + `const RoleAdmin Role = "Admin"` ... |
| JSON | `"Admin"`, `"Editor"`, `"Viewer"` |
| OpenAPI | `{ type: "string", enum: ["Admin", "Editor", "Viewer"] }` |

**Enums with payloads** (ADTs) map to tagged unions. The tag field is `tag` (avoids collision with `type`, which is common in domain models):

```phoenix
enum Shape {
  Circle(Float)
  Rect(Float, Float)
}
```

```typescript
type Shape =
  | { tag: "Circle"; value: number }
  | { tag: "Rect"; value: [number, number] }
```

In JSON: `{ "tag": "Circle", "value": 3.14 }`. In OpenAPI: `discriminator` with `propertyName: "tag"`.

### Authentication — deferred

Auth is not modeled in Gen schemas. Every framework handles auth differently (Express middleware, Fastify decorators, Go handler wrapping), and trying to model it in the schema would produce a leaky abstraction. The generated handler interface receives the full request context — the user wires auth in their own framework's way. This can be revisited if a clean, framework-agnostic pattern emerges from real-world usage.

### Generated code stability

Regenerating without schema changes must produce **byte-identical output**. The generator uses declaration order (not alphabetical sorting), consistent formatting, and no timestamps or generated-at comments. Adding one field to a struct should produce a minimal diff in the generated output — only the lines related to that field change.

## Implementation phases

### Gen Phase 1: Foundation

- Add `endpoint` as a new AST node in the parser
- Add `schema` declarations for database table definitions (optional, for forward compatibility)
- Add `where` constraints on struct fields (limited predicate syntax)
- Type-check endpoint definitions: validate that request/response types exist, path parameters match request fields, error variants are defined
- Add `phoenix gen` subcommand to the CLI

### Gen Phase 2: TypeScript target and editor support

TypeScript first — largest potential user base, most frustration with existing tools. Editor support ships alongside the first target, not after — developers will not write `.phx` files in a plain text editor.

- Generate TypeScript interfaces from Phoenix structs and enums
- Generate typed client SDK (fetch-based, framework-agnostic)
- Generate server handler interfaces (Express and Fastify adapters)
- Generate validation functions from `where` constraints
- Generate router wiring that connects handlers to endpoints with automatic deserialization
- VS Code extension with syntax highlighting and basic diagnostics (see [Required tooling](#required-tooling) below)

### Gen Phase 3: OpenAPI target

Free interop with the entire API tooling ecosystem.

- Generate OpenAPI 3.1 specs from endpoint declarations
- Map Phoenix types to JSON Schema
- Map `where` constraints to JSON Schema validation keywords (`minimum`, `maximum`, `pattern`, `minLength`, etc.)
- Map error variants to HTTP status codes

### Gen Phase 4: Additional targets

- Go: structs, handler interfaces (net/http and Echo/Gin adapters), client
- Rust: types, handler traits (Axum/Actix adapters), client
- Python: Pydantic models, FastAPI handler stubs, client

### Gen Phase 5: Watch mode and integration

- `phoenix gen --watch` re-generates on `.phx` file changes
- Integration testing: generated client and server can communicate correctly
- LSP enhancements: autocomplete for endpoint fields, go-to-definition for type references

## Required tooling

Phoenix Gen does not need the full tooling suite planned in Phase 3 of the language roadmap. But it does need a targeted subset — without editor support, the authoring experience is worse than OpenAPI YAML, which already has schema validation in every editor.

### VS Code extension (ships with Gen Phase 2)

The extension ships alongside the first code generation target. It is not optional — developers evaluate tools by opening a file in their editor. No highlighting, no adoption.

**TextMate grammar for syntax highlighting:**
- Keywords: `struct`, `enum`, `endpoint`, `schema`, `where`, `function`, `let`, type names
- Literals: strings, numbers, booleans
- Comments: `//` and `/* */`
- This is a single `.tmLanguage.json` file — a few days of work, not weeks
- Provides immediate visual feedback that `.phx` files are a real, supported format

**Basic diagnostics (via the existing type checker):**
- Run the Phoenix parser and type checker on save
- Report errors inline: undefined types, invalid endpoint definitions, malformed `where` constraints
- This does not require a full LSP server — a simple "run checker, parse JSON errors, show squiggles" extension is sufficient for launch
- The existing Phoenix type checker already produces span-annotated errors, so the infrastructure exists

**What can wait:**
- Full LSP with autocomplete, go-to-definition, find references — valuable, but not a launch blocker. Add incrementally in Gen Phase 5.
- Hover for type info — useful for complex generic types, not critical for schema files which are typically simple structs and endpoints.
- Extensions for other editors (JetBrains, Neovim, etc.) — VS Code first, expand based on demand.

### Error message quality

Schema files are typically short, so when something goes wrong, the error message is the entire debugging experience. Every diagnostic should include:

- **What** went wrong: `unknown type "Usr" in endpoint response`
- **Where**: file path, line number, and column span pointing at the offending token
- **Suggestion**: `did you mean "User"?` (fuzzy matching against known types)

The existing Phoenix error infrastructure supports span-annotated diagnostics. The work here is ensuring new AST nodes (`endpoint`, `schema`, `where`) produce equally good errors, and adding "did you mean" suggestions for common mistakes in schema files.

### Configuration file

A `phoenix.toml` in the project root configures Gen behavior. Multiple targets can be configured simultaneously — a single `phoenix gen` command generates everything:

```toml
[gen]
schema = "api/schema.phx"         # or a glob: "api/**/*.phx"

# TypeScript client for the frontend
[gen.typescript]
mode = "client"                   # "client", "server", or "both" (default)
out_dir = "frontend/src/generated"
client_framework = "fetch"        # "fetch" (default), "axios"
validation = true                 # generate validation functions from `where` constraints

# Python server for the backend
[gen.python]
mode = "server"
out_dir = "backend/generated"
server_framework = "fastapi"      # "fastapi", "flask"

# OpenAPI spec for documentation and external tooling
[gen.openapi]
output = "api.yaml"               # or "api.json"
version = "3.1"
```

With this config, running `phoenix gen` with no flags generates the TypeScript client, the Python server handlers, and the OpenAPI spec in one pass.

This is a config file parser, not a package manager — no dependency resolution, no registry, no lockfile. It tells `phoenix gen` where to find schema files and how to generate output.

### What Gen does NOT need from Phase 3

| Phase 3 item | Why Gen doesn't need it |
|--------------|------------------------|
| 3.1 Package manager | Schema files are standalone — no cross-package imports, no dependency resolution |
| 3.3 Formatter | Schema files are short and simple; manual formatting is fine. A formatter can come later. |
| 3.4 Test framework | Gen produces code for other languages — those languages have their own test frameworks |
| 3.2 Full LSP | A full LSP with autocomplete, references, and rename is valuable but not required at launch. Basic diagnostics via the type checker are sufficient. |

## Distribution and release plan

Binary artifacts are hosted as GitHub Release assets and built automatically by GitHub Actions on each tagged release.

### Day one: GitHub Releases + install script + Homebrew tap

**GitHub Releases with install script:**
- GitHub Actions builds platform-specific binaries on each tagged release: `darwin-arm64`, `darwin-x64`, `linux-x64`, `linux-arm64`, `win32-x64`
- Binaries are uploaded as GitHub Release assets (free for public repos)
- A shell installer script (hosted in the repo, e.g. `curl -fsSL https://raw.githubusercontent.com/rsaperstein/phoenixlang/main/install.sh | sh`) detects the platform and downloads the correct binary
- One-line install command in the README — lowest friction for first-time users

**Homebrew tap:**
- Create a `homebrew-phoenix` repo with a formula that points to the GitHub Release binaries
- Users install with `brew tap rsaperstein/phoenix && brew install phoenix`
- The formula is a single Ruby file updated on each release (can be automated via CI)
- No approval process — taps are self-published

### With TypeScript target (Gen Phase 2): npm

Publishing to npm makes sense when the TypeScript target ships — developers generating TypeScript are already in the npm ecosystem.

- Create an npmjs.com account (free for public packages)
- Publish platform-specific binary packages: `@phoenixlang/cli-darwin-arm64`, `@phoenixlang/cli-linux-x64`, etc.
- Publish a base package (`phoenixlang`) that detects the platform and pulls in the correct binary via `optionalDependencies`
- Developers can then run `npx phoenixlang gen` or `npm install -g phoenixlang`
- This is the same pattern used by esbuild, Turbo, and Biome for distributing Rust/Go binaries via npm

### With traction: Homebrew core + crates.io

**Homebrew core** (official `brew install phoenix` without a tap):
- Submit a PR to the [homebrew-core](https://github.com/Homebrew/homebrew-core) repo
- Must meet their criteria: notable project (some GitHub stars and usage), stable tagged releases, passing CI
- Reviewed by Homebrew maintainers — pursue once Phoenix Gen has real users, not on day one

**crates.io** (`cargo install phoenix-cli`):
- Free, zero setup — `cargo publish` uploads to the Rust package registry
- Compiles from source, so it requires users to have the Rust toolchain installed
- Good as an additional option for Rust developers, not a primary distribution method

### Release automation

All of the above should be automated in a single GitHub Actions workflow triggered by a version tag:

1. `git tag v0.1.0 && git push --tags`
2. CI builds binaries for all platforms
3. CI creates a GitHub Release with the binaries attached
4. CI publishes to npm (platform packages + base package)
5. CI updates the Homebrew tap formula with new URLs and SHA256 checksums

Once set up, releasing a new version is a single `git tag` command.

## Relationship to the full language

Phoenix Gen is a **stepping stone**, not a fork. The relationship is:

1. **Schema files are valid Phoenix code.** When the full compiler (Phase 2) ships, `.phx` schema files become importable modules. No rewrite needed.
2. **`endpoint` and `schema` parsing built for Gen feeds directly into the compiler.** The parser extensions are shared.
3. **Validation from `where` constraints is a subset of refinement types (Phase 5.2).** Real-world usage in Gen informs the design of the full refinement type system.
4. **Users who adopt Phoenix Gen become the first users of the full language.** They already know the syntax, have `.phx` files in their projects, and have a reason to care about the compiler shipping.

The long-term trajectory: Phoenix Gen starts as a code generation tool for other languages, and gradually becomes less necessary as the full Phoenix language can handle both client and server natively. But even then, OpenAPI generation and multi-language client SDK generation remain valuable — not every consumer of a Phoenix API will be written in Phoenix.
