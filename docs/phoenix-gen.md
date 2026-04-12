# Phoenix Gen: Typed API Code Generation

Phoenix Gen generates idiomatic client SDKs, server handler interfaces, validation logic, and OpenAPI specs from `.phx` schema files. Write your API schema once, generate typed code for TypeScript, Python, Go, and OpenAPI.

## Quick Start

### 1. Install Phoenix

```bash
curl -fsSL https://raw.githubusercontent.com/rmsap/phoenixlang/main/install.sh | sudo sh
```

Or download binaries from [GitHub Releases](https://github.com/rmsap/phoenixlang/releases).

### 2. Write a schema

```phoenix
struct User {
  Int id
  String name
  String email
  Int age
}

endpoint listUsers: GET "/api/users" {
  query {
    Int page = 1
    Int limit = 20
    Option<String> search
  }
  response List<User>
}

endpoint createUser: POST "/api/users" {
  body User omit { id }
  response User
  error {
    ValidationError(400)
    Conflict(409)
  }
}

endpoint getUser: GET "/api/users/{id}" {
  response User
  error { NotFound(404) }
}
```

### 3. Generate code

```bash
phoenix gen schema.phx                        # TypeScript (default)
phoenix gen schema.phx --target python        # Python (Pydantic + FastAPI)
phoenix gen schema.phx --target go            # Go (net/http)
phoenix gen schema.phx --target openapi       # OpenAPI 3.1 JSON spec
phoenix gen schema.phx --client               # Types + client SDK only
phoenix gen schema.phx --server               # Types + handlers + router only
phoenix gen schema.phx --watch                # Re-generate on file changes
phoenix gen                                   # Use settings from phoenix.toml
```

---

## Schema Syntax

Schema files use Phoenix syntax: structs, enums, type aliases, and `endpoint` declarations.

### Structs and enums

```phoenix
/** A registered user */
struct User {
  Int id
  String name
  String email
  Int age
}

enum Role { Admin, Editor, Viewer }
```

### Endpoints

Endpoints map directly to HTTP semantics with distinct sections for path params, query params, body, response, and errors:

```phoenix
endpoint updateUser: PUT "/api/users/{id}" {
  query {
    Bool notify = false           // query string: ?notify=true
  }
  body User omit { id } partial   // all User fields except id, all optional
  response User
  error {
    NotFound(404)
    ValidationError(400)
  }
}
```

- **Path params** are inferred from the URL pattern. `{id}` expects an `Int id` parameter — no separate declaration needed. The type is inferred from the matching struct field or defaults to `String`.
- **`query { }`** defines URL query parameters. Supports default values (`Int page = 1`) and optional params (`Option<String> search`).
- **`body TypeName`** defines the JSON request body. Supports `omit`, `pick`, and `partial` modifiers (see [Type derivation](#type-derivation-omit-pick-and-partial)). Only valid on POST, PUT, and PATCH — the type checker rejects `body` on GET and DELETE.
- **`response TypeName`** defines the JSON response body.
- **`error { }`** defines error variants with explicit HTTP status codes.

### Doc comments

`/** */` comments attach to the next declaration and flow through to generated code as JSDoc, Go doc comments, Python docstrings, and OpenAPI `description` fields:

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

### Multiple schema files

All `.phx` files matched by the config glob are parsed and merged into a flat namespace. A type defined in any file is available in all files. Name conflicts are a compile error.

```
api/
  types.phx         // User, Post, Comment structs
  enums.phx         // Role, Status enums
  users.phx         // user endpoints
  posts.phx         // post endpoints
```

No import syntax needed between schema files.

---

## Type Derivation: `omit`, `pick`, and `partial`

API endpoints almost never accept the exact same shape as the full domain type. `omit`, `pick`, and `partial` are compile-time type operators that derive new types from existing structs, so you don't need to define a separate struct for every request body.

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
- **`partial`** — makes *all* fields optional. Typically used for update endpoints where only changed fields are sent.
- **`partial { field1, field2 }`** — makes only the *listed* fields optional. Unlisted fields remain required.
- Operators chain left to right: `User omit { id } partial { age }` means "start with User, remove id, make age optional"
- The type checker validates that all named fields exist on the base type — `User omit { nonexistent }` is a compile error
- `where` constraints on the base type's fields are inherited by the derived type

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

---

## Validation from `where` Constraints

`where` constraints on struct fields generate validation logic automatically. This eliminates hand-written validation boilerplate:

```phoenix
struct User {
  Int id
  String name where self.length > 0 && self.length <= 100
  String email where self.contains("@") && self.length > 3
  Int age where self >= 0 && self <= 150
}

endpoint createUser: POST "/api/users" {
  body User omit { id }       // constraints on name, email, age are inherited
  response User
}
```

Constraints are inherited by derived types. The generated server code validates incoming requests and returns 400 errors on constraint violations:

```typescript
export function validateCreateUserBody(input: unknown): CreateUserBody {
  if (typeof input !== 'object' || input === null) throw new ValidationError('expected object');
  const obj = input as Record<string, unknown>;
  if (typeof obj.name !== 'string') throw new ValidationError('name: expected string');
  if (!((obj.name.length > 0) && (obj.name.length <= 100))) throw new ValidationError('name: constraint violated');
  if (typeof obj.email !== 'string') throw new ValidationError('email: expected string');
  if (!((obj.email.includes("@")) && (obj.email.length > 3))) throw new ValidationError('email: constraint violated');
  if (typeof obj.age !== 'number') throw new ValidationError('age: expected number');
  if (!((obj.age >= 0) && (obj.age <= 150))) throw new ValidationError('age: constraint violated');
  return obj as CreateUserBody;
}
```

For OpenAPI, `where` constraints map to JSON Schema validation keywords (`minimum`, `maximum`, `minLength`, `maxLength`, `exclusiveMinimum`, `exclusiveMaximum`).

---

## Generated Output

### TypeScript

```bash
phoenix gen schema.phx --target typescript --out ./generated
```

**Types and client:**
```typescript
// generated/client.ts

/** A registered user */
export interface User {
  id: number;
  name: string;
  email: string;
  age: number;
}

export type CreateUserBody = Omit<User, "id">;

export const api = {
  /** List all users, optionally filtered by search query */
  async listUsers(opts?: { page?: number; limit?: number; search?: string }): Promise<User[]> { ... },
  /** Create a new user */
  async createUser(body: CreateUserBody): Promise<User> { ... },
  /** Get a user by ID */
  async getUser(id: number): Promise<User> { ... },
}
```

**Server handlers:**
```typescript
// generated/handlers.ts
export interface Handlers {
  listUsers(query: { page: number; limit: number; search?: string }): Promise<User[]>;
  createUser(body: CreateUserBody): Promise<User>;
  getUser(id: number): Promise<User>;
}

// generated/server.ts — Express router wiring
export function createRouter(handlers: Handlers): Router { ... }
```

### Python

```bash
phoenix gen schema.phx --target python --out ./generated
```

```python
# generated/models.py
from pydantic import BaseModel

class User(BaseModel):
    id: int
    name: str
    email: str
    age: int

class CreateUserBody(BaseModel):
    name: str
    email: str
    age: int

# generated/handlers.py — developer implements these
class Handlers:
    async def list_users(self, page: int, limit: int, search: str | None) -> list[User]: ...
    async def create_user(self, body: CreateUserBody) -> User: ...
    async def get_user(self, id: int) -> User: ...
```

### Go

```bash
phoenix gen schema.phx --target go --out ./generated
```

```go
// generated/types.go
type User struct {
    ID    int    `json:"id"`
    Name  string `json:"name"`
    Email string `json:"email"`
    Age   int    `json:"age"`
}

// generated/handlers.go — developer implements this interface
type Handlers interface {
    ListUsers(query ListUsersQuery) ([]User, error)
    CreateUser(body CreateUserBody) (*User, error)
    GetUser(id int) (*User, error)
}
```

### OpenAPI

```bash
phoenix gen schema.phx --target openapi --out ./api.json
```

Generates an OpenAPI 3.1 spec with paths, schemas, parameters, error responses, and JSON Schema validation keywords from `where` constraints. Usable with Swagger UI, Postman, API gateways, and documentation tools.

---

## Cross-Language Usage

A single `.phx` schema can generate client code in one language and server code in another. The schema is the shared contract.

```bash
# TypeScript frontend
phoenix gen schema.phx --target typescript --client --out ./frontend/src/api

# Python backend
phoenix gen schema.phx --target python --server --out ./backend/api
```

Both sides are derived from the same `.phx` file. If a field is added, renamed, or removed in the schema, both the client and server code are regenerated — the contract cannot drift out of sync.

Other combinations work the same way: TypeScript + Go, React Native + Python, multiple clients from one schema, etc.

---

## Configuration

A `phoenix.toml` in your project configures Gen defaults so `phoenix gen` works with no arguments.

### Single target

```toml
[gen]
schema = "api/schema.phx"
target = "typescript"
out_dir = "./generated"
mode = "both"                 # "client", "server", or "both"
```

### Multiple targets

```toml
[gen]
schema = "api/schema.phx"

[gen.targets.typescript]
out_dir = "frontend/src/generated"
mode = "client"

[gen.targets.python]
out_dir = "backend/generated"
mode = "server"

[gen.targets.openapi]
out_dir = "docs"
```

Running `phoenix gen` with this config generates all targets from the same schema.

### CLI overrides

CLI flags always override config values:

```bash
# Config defines multiple targets, but only generate python
phoenix gen --target python

# Config says mode = "both", but override to client-only
phoenix gen --client

# Override output directory
phoenix gen --out ./custom-dir
```

---

## Reference

### Naming conventions

Phoenix uses `camelCase` for fields and endpoints. Generated code is automatically converted to the target language's conventions:

| Phoenix (source) | TypeScript | Go | Python |
|---|---|---|---|
| `listUsers` (endpoint) | `listUsers` | `ListUsers` | `list_users` |
| `createdAt` (field) | `createdAt` | `CreatedAt` | `created_at` |
| `User` (type) | `User` | `User` | `User` |

JSON wire format uses camelCase. Generated server code for Go and Python handles the mapping automatically.

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

**Enums with payloads** (ADTs) map to tagged unions with `tag` as the discriminator:

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

### Optional fields

| Phoenix | TypeScript | Go | JSON | OpenAPI |
|---|---|---|---|---|
| `String name` | `name: string` | `Name string` | required | in `required` array |
| `Option<String> name` | `name?: string` | `Name *string` | absent or `null` | not in `required` |

Both absent and `null` in JSON map to `None`.

### Error variants

Error variants carry explicit HTTP status codes:

```phoenix
error {
  NotFound(404)
  Unauthorized(401)
  ValidationError(400)
  Conflict(409)
  RateLimited(429)
}
```

Generated server code maps each variant to the corresponding HTTP status. Generated client code maps response status codes back to typed error variants.

### Authentication

Auth is not modeled in Gen schemas. The generated handler interface receives the full request context — wire auth in your framework's own way (Express middleware, FastAPI dependencies, Go middleware, etc.).

### Generated code stability

Regenerating without schema changes produces byte-identical output. The generator uses declaration order, consistent formatting, and no timestamps. Adding one field produces a minimal diff — only lines related to that field change.

---

## Background

### Motivation

Phoenix's most differentiating features — typed endpoints, built-in serialization, refinement types — are planned for later phases of the language roadmap, which depend on compilation and the async runtime. Phoenix Gen inverts this by extracting the schema and code generation aspects into a standalone tool that works today:

- **Proves the value proposition now.** Developers experience Phoenix's type safety without adopting a new language.
- **Builds a user base.** Every developer using `phoenix gen` learns Phoenix syntax and joins the ecosystem.
- **Validates the design.** Real-world usage of `endpoint` and `schema` declarations shapes the full language design with feedback instead of speculation.
- **Creates a migration path.** When the full language ships, `.phx` schema files become importable modules with zero rewrite.

### Competitive landscape

| Tool | Strengths | Gap Phoenix Gen fills |
|------|-----------|----------------------|
| OpenAPI / Swagger | Universal ecosystem support | Verbose YAML, poor authoring experience, mediocre code generators |
| Protobuf / gRPC | Excellent multi-language codegen | Designed for RPC, not REST/HTTP APIs with path params and JSON |
| TypeSpec (Microsoft) | Clean DSL, generates OpenAPI | No validation/refinement types, no direct client SDK generation |
| tRPC | Best-in-class DX for TypeScript full-stack | TypeScript-only — locks both client and server to one language |
| Smithy (AWS) | Powerful service modeling | Complex, AWS-centric, steep learning curve |
| GraphQL | Strong type system, introspection | Different paradigm, N+1 problems, complexity for simple APIs |

### Implementation status

| Phase | Status |
|-------|--------|
| Gen Phase 1: Foundation (parser, type checker, CLI) | Complete |
| Gen Phase 2: TypeScript target + VS Code extension | Complete |
| Gen Phase 3: OpenAPI target | Complete |
| Gen Phase 4: Python and Go targets | Complete (Rust deferred) |
| Gen Phase 5: Watch mode, integration testing, LSP | Complete |

### Relationship to the full language

Phoenix Gen is a stepping stone, not a fork:

1. Schema files are valid Phoenix code — when the full compiler ships, `.phx` schema files become importable modules.
2. Parser extensions built for Gen feed directly into the compiler.
3. `where` constraints are a subset of refinement types — real-world Gen usage informs the full design.
4. Users who adopt Phoenix Gen become the first users of the full language.

The long-term trajectory: Gen starts as a code generation tool for other languages, and gradually becomes less necessary as the full Phoenix language handles both client and server natively. But OpenAPI generation and multi-language client SDK generation remain valuable even then.
