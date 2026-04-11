# Phase 1: Core Language Completeness

**Status: Complete** — All sub-phases (1.1–1.13) finished as of 2026-04-06.

These features are prerequisites for everything else. Without them, you cannot write real programs.

### 1.1 Break and Continue ✅

- Add `break` and `continue` keywords to `while` and `for` loops
- Small change across all layers (lexer, parser, sema, interpreter)
- **Why first:** trivial to implement, immediately makes loops useful

### 1.2 Closures and First-Class Functions ✅

- Functions as values: `(Int, Int) -> Int` as a type
- Anonymous functions: `function(x: Int) -> Int { return x * 2 }`
- Capture variables from enclosing scope
- Pass functions as arguments, return them from functions
- **Why now:** required for iterators, collection methods, callbacks, and eventually async

### 1.3 Collections ✅ (List done, Map deferred)

- `List<T>` — dynamic array (backed by `Vec` in the interpreter)
- `Map<K, V>` — hash map (deferred to stdlib phase)
- List literals: `[1, 2, 3]`
- Map literals: `{"key": "value"}`
- Built-in methods: `push`, `pop`, `get`, `set`, `length`, `contains`, `map`, `filter`, `reduce`
- For-in over lists: `for x in my_list { ... }`
- **Why now:** you cannot write useful programs without collections
- **Depends on:** Generics (1.4) — but can ship a non-generic version first with `List<Int>`, `List<String>`, etc., then generalize

### 1.4 Generics ✅

- Type parameters on functions: `function map<T, U>(items: List<T>, f: (T) -> U) -> List<U>`
- Type parameters on structs: `struct Pair<A, B> { A first, B second }`
- Type parameters on enums: `enum Option<T> { Some(T), None }`
- Monomorphization (generate specialized code per concrete type) for the interpreter; keep it simple
- **Why now:** required for Result, Option, collections, and any reusable data structure
- **Complexity:** High — this touches the type system, parser (angle bracket ambiguity with `<`/`>`), and sema deeply

### 1.5 Result and Option Types ✅

- Built-in `Result<T, E>` and `Option<T>` as standard enum types
- Standard methods: `isOk()`, `isErr()`, `unwrap()`, `unwrapOr()`, `map()`
- `unwrap()` panics with a clear error message if called on `Err`/`None`
- **Why now:** error handling is fundamental; every I/O operation needs this
- **Depends on:** Generics (1.4)

### 1.6 Traits ✅

- Trait declarations: `trait Display { function toString(self) -> String }`
- Trait implementations: `impl Display for Point { ... }`
- Trait bounds on generics: `function printAll<T: Display>(items: List<T>)`
- Built-in traits: `Display`, `Eq`, `Ord`, `Hash`, `Serialize`
- Derive-style auto-implementation for simple traits
- **Why now:** traits enable polymorphism, operator overloading, and the stdlib
- **Depends on:** Generics (1.4)

### 1.7 Memory Model (Garbage Collection) ✅

- All values are garbage collected — no ownership tracking, no move semantics, no borrow checking
- The `mut` keyword controls mutability, not ownership
- In the interpreter, Rust's heap allocation (clone-on-use) acts as the GC
- A real tracing or reference-counted GC will be implemented during the compilation phase (Phase 2)
- **Why:** GC eliminates the steep learning curve of ownership/borrow systems while keeping the language safe and ergonomic for web development
- **Complexity:** Low in the interpreter (Rust manages memory); medium in the compiler (requires GC runtime)

### 1.8 Ergonomics and Usability ✅

These features address verbosity and day-to-day usability. They don't add new capabilities — they make existing patterns less painful to write. Ship these before the async/HTTP phases (Phase 4), where error handling becomes pervasive.

#### 1.8.1 Error Propagation Operator (`?`) ✅

- `?` on a `Result<T, E>` value: if `Ok(v)`, evaluates to `v`; if `Err(e)`, returns `Err(e)` from the enclosing function
- `?` on an `Option<T>` value: if `Some(v)`, evaluates to `v`; if `None`, returns `None` from the enclosing function
- The enclosing function's return type must be `Result` or `Option` respectively — type-checked at compile time
- This is purely syntactic sugar for a match + early return; no new semantics

```phoenix
// Before: explicit matching everywhere
async function getUser(id: Int) -> Result<String, DbError> {
  let connResult: Result<Connection, DbError> = await db.connect()
  match connResult {
    Err(e) -> return Err(e)
    Ok(conn) -> {
      let userResult: Result<User, DbError> = await conn.query("SELECT * FROM users WHERE id = ?", id)
      match userResult {
        Err(e) -> return Err(e)
        Ok(user) -> return Ok(user.name)
      }
    }
  }
}

// After: ? operator
async function getUser(id: Int) -> Result<String, DbError> {
  let conn: Connection = await db.connect()?
  let user: User = await conn.query("SELECT * FROM users WHERE id = ?", id)?
  return Ok(user.name)
}
```

- **Why now:** without `?`, every `Result`-returning call requires 3–5 lines of match boilerplate. In web code (HTTP, JSON, database), nearly every line produces a `Result`. The verbosity will be the #1 complaint from developers trying Phoenix for web development.
- **Complexity:** Medium — lexer adds `?` as a postfix operator, parser treats it as sugar for match + early return, sema validates the enclosing function returns a compatible `Result`/`Option` type.
- **Depends on:** Result/Option (1.5)

#### 1.8.2 String Interpolation ✅

- `"hello {name}, you are {age} years old"` — expressions inside `{}` are evaluated and converted to strings
- Equivalent to manual concatenation with `toString()`, but far more readable
- Nested braces `{{` and `}}` produce literal `{` and `}`

```phoenix
// Before
print("User: " + user.name + " (age " + toString(user.age) + ")")

// After
print("User: {user.name} (age {user.age})")
```

- **Why now:** string building is one of the most common operations in web development (HTML, JSON, log messages, error messages). Manual concatenation with `+` and `toString()` is tedious and error-prone.
- **Complexity:** Medium — lexer must recognize `{` inside strings and switch to expression parsing, parser builds a `StringInterp` node, interpreter evaluates each segment.
- **Depends on:** —

#### 1.8.3 Field Assignment (`obj.field = value`) ✅

- Allow assignment to struct fields via dot notation: `point.x = 10`
- The struct variable must be declared with `let mut` and the assignment must be type-compatible
- Nested field assignment: `user.address.city = "NYC"` (if intermediate fields are also mutable)

```phoenix
// Currently not possible — must reconstruct the entire struct
let mut p: Point = Point(1, 2)
p = Point(10, p.y)  // awkward workaround to change one field

// With field assignment
let mut p: Point = Point(1, 2)
p.x = 10
```

- **Why now:** without field assignment, mutable structs are nearly unusable. You must reconstruct the entire struct to change one field, which is both verbose and defeats the purpose of `mut`. This will be critical once structs are used for HTTP request/response handling, database models, and UI state.
- **Complexity:** Medium — parser extends assignment to accept lvalue expressions (field access chains), sema validates mutability and type, interpreter implements field mutation on `Value::Struct`.
- **Depends on:** —

#### 1.8.4 Type Alias ✅

- `type UserId = Int` — a named alias for an existing type
- `type Handler = (Request) -> Response` — simplify complex function types
- `type StringResult<T> = Result<T, String>` — partially applied generic types

```phoenix
type Handler = (Request) -> Response
type StringResult<T> = Result<T, String>

function registerRoute(path: String, handler: Handler) { ... }

function parseInt(s: String) -> StringResult<Int> { ... }
```

- **Why now:** function types like `(Int, Int) -> (Int) -> Bool` quickly become unreadable. In web code, you'll have repeated types like `Result<T, HttpError>` everywhere. Aliases reduce noise and make signatures self-documenting.
- **Complexity:** Small — parser adds `type Name = TypeExpr` declarations, sema resolves aliases during type resolution. No runtime impact.
- **Depends on:** Generics (1.4)

#### 1.8.5 Implicit Return (Last Expression) ✅

- A block whose last statement is an expression (not a `return`) implicitly returns that value
- Functions, match arms, and if/else branches all benefit

```phoenix
// Before
function add(a: Int, b: Int) -> Int {
  return a + b
}

// After — last expression is the return value
function add(a: Int, b: Int) -> Int {
  a + b
}

// Match arms become cleaner
function describe(s: Shape) -> String {
  match s {
    Circle(r) -> "circle with radius {r}"
    Rect(w, h) -> "rectangle {w}x{h}"
  }
}
```

- **Why now:** explicit `return` for single-expression functions and match arms is boilerplate. This is standard in Rust, Kotlin, Ruby, and Scala. It makes functional patterns (map, filter, match) feel natural.
- **Complexity:** Medium — parser must distinguish expression-statements from implicit returns (last statement in a block), sema must infer block return types. This interacts with `check_block_type` which was just added.
- **Depends on:** —

### 1.9 Interpreter-Level Completeness ✅

These features can be implemented in the tree-walk interpreter before compilation. They fill critical gaps for writing real programs and require no compilation infrastructure.

#### 1.9.1 `Map<K, V>` Built-in Collection ✅

- `Map<K, V>` — hash map with literal syntax `{"key": "value"}`
- Built-in methods: `get`, `set`, `contains`, `remove`, `keys`, `values`, `length`
- `for (key, val) in myMap { ... }` iteration
- **Why now:** maps are fundamental to web development (HTTP headers, query params, JSON objects). Deferring to stdlib leaves a critical gap — you cannot represent a JSON object without `Map`.
- **Complexity:** Medium — parallels `List<T>` implementation. Requires `Eq` + `Hash` trait bounds on K (enforce via built-in constraint for now).
- **Depends on:** Generics (1.4)

#### 1.9.2 String Methods ✅

- Core string methods: `split`, `trim`, `contains`, `replace`, `startsWith`, `endsWith`, `toLowerCase`, `toUpperCase`, `length`, `substring`, `indexOf`
- Implemented as built-in methods on `String` (like `List<T>.push()`)
- **String ordering comparisons** (`<`, `>`, `<=`, `>=`): the runtime (`value.rs`) already implements `PartialOrd` for strings via Rust's lexicographic ordering, but the type checker (`checker.rs:1374-1380`) restricts ordering operators to numeric types (`is_numeric()`). Fix: extend the ordering check to also accept `Type::String` on both sides.
- **Why now:** string manipulation is the most common operation in web development. Without these, even basic URL parsing or form handling is impossible. String ordering is needed for sorting, alphabetical comparisons, and range checks.
- **Complexity:** Small — each method is a thin wrapper around Rust's `str` methods. String ordering fix is a one-line change in the checker. No new language features needed.
- **Depends on:** —

#### 1.9.3 `for...in` Over Collections ✅

- `for item in myList { ... }` — iterate over `List<T>` elements
- `for (key, val) in myMap { ... }` — iterate over `Map<K, V>` entries (if Map is implemented)
- Currently only `for i in start..end` (range-based) is supported
- **Why now:** without this, iterating a list requires `while + get()` which is verbose and error-prone. This is the #1 usability gap for collection-heavy code.
- **Complexity:** Small — parser already handles `for...in`, just needs to accept list expressions on the right-hand side. Interpreter evaluates the collection once and iterates.
- **Depends on:** Collections (1.3)

#### 1.9.4 Functional Collection Methods ✅

- `List<T>.map((T) -> U) -> List<U>` — transform each element
- `List<T>.filter((T) -> Bool) -> List<T>` — keep elements matching predicate
- `List<T>.reduce(U, (U, T) -> U) -> U` — fold into a single value
- `List<T>.flatMap((T) -> List<U>) -> List<U>` — map then flatten
- `List<T>.find((T) -> Bool) -> Option<T>` — first element matching predicate
- `List<T>.any((T) -> Bool) -> Bool` — true if any element matches
- `List<T>.all((T) -> Bool) -> Bool` — true if all elements match
- `List<T>.first() -> Option<T>` — first element, or None if empty
- `List<T>.last() -> Option<T>` — last element, or None if empty
- `List<T>.contains(T) -> Bool` — true if element is present (uses equality)
- `List<T>.sortBy((T, T) -> Int) -> List<T>` — sort with comparator (negative/zero/positive)
- `List<T>.take(Int) -> List<T>` — first N elements
- `List<T>.drop(Int) -> List<T>` — all elements after the first N
- `List<T>.enumerate() -> List<List<dynamic>>` — pairs of `[index, element]` (proper tuple/pair type deferred until tuples are added)
- `List<T>.zip(List<U>) -> List<List<dynamic>>` — pairs from two lists (proper tuple type deferred)
- **Why now:** functional collection operations are essential for data transformation, which dominates web backend code. Without them, every list transformation is a manual loop.
- **Complexity:** Medium — requires closures (done) and generics (done). Each method is implemented as a built-in on `List<T>`. `enumerate` and `zip` return pairs; the ideal representation is a tuple type, but a two-element list or `Pair<A, B>` struct can serve as a stopgap.
- **Depends on:** Closures (1.2), Generics (1.4)
- **Note:** `contains` requires an equality check. For now, use built-in `==` on primitive types and structural equality on structs. A proper `Eq` trait constraint can be added later.

#### 1.9.5 Result/Option Combinators ✅

**Option combinators:**

- `Option<T>.map((T) -> U) -> Option<U>` — transform the inner value if present
- `Option<T>.andThen((T) -> Option<U>) -> Option<U>` — chain fallible operations
- `Option<T>.orElse(() -> Option<T>) -> Option<T>` — provide a fallback
- `Option<T>.filter((T) -> Bool) -> Option<T>` — keep value only if predicate holds
- `Option<T>.unwrapOrElse(() -> T) -> T` — unwrap with lazy default
- `Option<T>.okOr(E) -> Result<T, E>` — convert to Result, using given error for None
- `Option<T>.okOrElse(() -> E) -> Result<T, E>` — convert to Result with lazy error
- `Option<T>.inspect((T) -> Void) -> Option<T>` — run side effect (e.g. logging) without consuming
- `Option<Option<T>>.flatten() -> Option<T>` — collapse nested Options

**Result combinators:**

- `Result<T, E>.map((T) -> U) -> Result<U, E>` — transform the success value
- `Result<T, E>.mapErr((E) -> F) -> Result<T, F>` — transform the error value
- `Result<T, E>.andThen((T) -> Result<U, E>) -> Result<U, E>` — chain fallible operations
- `Result<T, E>.orElse((E) -> Result<T, F>) -> Result<T, F>` — recover from errors
- `Result<T, E>.unwrapOrElse((E) -> T) -> T` — unwrap with lazy default
- `Result<T, E>.ok() -> Option<T>` — discard the error, keep success as Option
- `Result<T, E>.err() -> Option<E>` — discard the success, keep error as Option
- `Result<T, E>.inspect((T) -> Void) -> Result<T, E>` — run side effect on success value
- `Result<T, E>.inspectErr((E) -> Void) -> Result<T, E>` — run side effect on error value
- `Result<Result<T, E>, E>.flatten() -> Result<T, E>` — collapse nested Results

```phoenix
function parsePort(s: String) -> Option<Int> {
  // Chain operations: parse, then validate range
  parseInt(s)
    .filter(function(p: Int) -> Bool { return p > 0 && p < 65536 })
}

function findUser(id: Int) -> Result<User, HttpError> {
  // Convert Option (not found) to Result (HTTP 404)
  db.findById(id)
    .okOr(HttpError(404, "user not found"))
}

function fetchUser(id: Int) -> Result<User, String> {
  db.find(id)
    .inspectErr(function(e: DbError) { log.error("DB lookup failed: {e.message}") })
    .mapErr(function(e: DbError) -> String { return e.message })
    .andThen(function(row: Row) -> Result<User, String> { return User.fromRow(row) })
}
```

- **Why now:** the `?` operator handles the early-return case, but combinators handle the transformation case — mapping, chaining, and recovering without unwrapping. `okOr`/`ok()`/`err()` for Option↔Result conversion are especially critical in web code where "not found" (`None`) frequently needs to become an HTTP error (`Err`). `inspect`/`inspectErr` enable logging within chains without breaking the pipeline.
- **Complexity:** Medium — each combinator is a built-in method on `Option`/`Result` that accepts a closure. Requires closures (done) and generics (done). The type signatures involve multiple type variables which the checker must resolve correctly. `flatten` requires detecting nested `Option<Option<T>>` / `Result<Result<T, E>, E>` which adds minor complexity to the checker.
- **Depends on:** Closures (1.2), Generics (1.4), Result/Option (1.5)

#### 1.9.6 Named/Default Parameters ✅

- Named arguments at call sites: `http.listen(host: "0.0.0.0", port: 8080)`
- Default parameter values: `function listen(host: String = "0.0.0.0", port: Int = 8080)`
- Named arguments can be passed in any order; positional arguments must come first
- **Why now:** web APIs frequently have many optional parameters. Without named/default args, every function with more than 3 parameters requires a builder pattern or options struct. This dramatically improves API ergonomics.
- **Complexity:** Medium — parser needs named-arg syntax, sema needs default value type checking, interpreter needs argument reordering.
- **Depends on:** —

#### 1.9.7 Pipe Operator (`|>`) ✅

- `expr |> f(args)` desugars to `f(expr, args)` — the left-hand side becomes the first argument
- Chains naturally: `data |> parse() |> validate() |> save()`
- **Why now:** Phoenix's FP focus makes data pipelines common. Without pipes, deeply nested function calls like `save(validate(parse(data)))` are hard to read. This is a pure syntactic sugar with zero semantic complexity.
- **Complexity:** Small — lexer adds `|>` token, parser treats it as a left-associative infix operator that desugars to a function call. No type system changes.
- **Depends on:** —

#### 1.9.8 Destructuring in Variable Declarations ✅

- `let Point { x, y } = getPoint()` — destructure struct fields into local variables
- `let (a, b) = getPair()` — tuple destructuring (if tuples are added)
- **Why now:** pattern matching only works in `match`. Allowing destructuring in variable bindings reduces boilerplate when unpacking return values or struct fields.
- **Complexity:** Medium — parser extends variable declaration syntax, sema validates field names and types, interpreter extracts values from structs.
- **Depends on:** —

#### 1.9.9 Inline Methods and Trait Implementations ✅

- Move all methods — including trait implementations — into the struct/enum body
- Standalone `impl Type` blocks are removed entirely
- Trait implementations use `impl TraitName { ... }` nested inside the type body
- All behavior for a type lives in one place: fields, methods, and trait impls

```phoenix
// Before (current): separate impl blocks
struct Point {
  Int x
  Int y
}
impl Point {
  function distance(self) -> Float { ... }
}
impl Display for Point {
  function toString(self) -> String { ... }
}

// After: everything inside the type body
struct Point {
  Int x
  Int y

  function distance(self) -> Float { ... }

  impl Display {
    function toString(self) -> String { ... }
  }
}

// Same for enums
enum Shape {
  Circle(Float)
  Rect(Float, Float)

  function area(self) -> Float {
    match self {
      Circle(r) -> 3.14 * r * r
      Rect(w, h) -> w * h
    }
  }

  impl Display {
    function toString(self) -> String { "a shape" }
  }
}
```

- **Why now:** grouping data and behavior together is more readable. Separate `impl` blocks split context unnecessarily — you define a struct's fields in one place and its methods and trait impls scattered elsewhere. The nested `impl TraitName { ... }` syntax keeps everything in one place while still clearly delineating which methods satisfy which trait. Swift, Kotlin, and Scala all co-locate methods with their types.
- **Complexity:** Medium — parser extends `parse_struct_decl` and `parse_enum_decl` to recognize `function` keywords and `impl TraitName { ... }` blocks inside the body. AST gains `methods` and `trait_impls` fields on `StructDecl` and `EnumDecl`. Sema and interpreter register these into the existing method/trait maps. Top-level `impl` declarations are removed from the grammar. ~150 lines of code.
- **Note:** Orphan impls (implementing an external trait for an external type) are not supported. This is a simplification — Phoenix doesn't have a module system yet, and disallowing orphan impls avoids coherence issues. This can be revisited when a package system is added.
- **Depends on:** —

#### 1.9.10 `else` Clauses on Loops ✅

- `for ... { ... } else { ... }` — the `else` block runs when the loop completes without `break`
- `while ... { ... } else { ... }` — same semantics
- **Why now:** search patterns are common in web code. `for user in users { if user.id == target { break } } else { return Err("not found") }` is a clean, readable pattern. Python demonstrates this is popular.
- **Complexity:** Small — parser extends for/while to accept an optional else block. Interpreter tracks whether `break` was hit.
- **Depends on:** —

### 1.10 Recursive Types ✅

Self-referential data structures require indirection since a struct cannot contain itself (infinite size). Under garbage collection, the GC handles heap allocation automatically, but the compiler still needs a way to express indirection.

- Recursive struct fields are allowed in the interpreter (all values are heap-allocated)
- For the compiled backend, an `Indirect<T>` wrapper or compiler-inserted indirection will be needed
- **Why now:** linked lists, trees, and ASTs are common data structures that require self-reference
- **Complexity:** Low in the interpreter (already works); medium in the compiler (requires GC-aware indirection)

### 1.11 Compiler-Readiness ✅

These are architectural fixes and design decisions that don't add new language features, but must be resolved before Phase 2. The compiler will depend on correct behavior in all of these areas — deferring them into Phase 2 would mean designing the IR and code generation around known bugs.

#### 1.11.1 Fix Generic Type Parameters in `impl` Blocks ✅

**File:** `phoenix-sema/src/checker.rs:447`

`let self_type = Type::from_name(&imp.type_name)` creates `Type::Named("Point")` for generic structs, losing type parameters. Methods on generic structs like `Wrapper<T>` cannot reference `T` in their signatures:

```phoenix
struct Wrapper<T> { T value }
impl Wrapper {
    function get(self) -> T {   // ERROR: unknown type `T`
        return self.value
    }
}
```

The compiler needs full type information to monomorphize or box generic methods. If the checker can't track `T` through an impl block, the compiler has no type information to work with.

**Fix:** When registering an impl block for a type that has type parameters, inject those parameters into `current_type_params` during both registration and body-checking passes. The impl block should inherit the type parameters of its target type.

- **Complexity:** Medium — touches `register_impl` and `check_impl` in the checker. Must also update method resolution to carry type bindings through.
- **Depends on:** —
- **Note:** If Phase 1.9.9 (Inline Methods) is implemented first, this fix naturally falls out of moving methods inside the type body where type params are already in scope. If 1.9.9 is deferred, fix this standalone.

#### 1.11.2 Decide Closure Capture Semantics ✅

Closures currently deep-clone the entire scope stack at creation time (value capture). This must be a deliberate language design decision before the compiler is built, because it determines how closures are represented at the machine level.

**Current behavior (value capture):**

- Mutations to outer variables after closure creation are invisible to the closure
- Multiple closures created in the same scope get independent copies
- Matches C++ `[=]` semantics

**Alternative (reference capture):**

- Closures see mutations to outer variables (like JavaScript, Python)
- Multiple closures share the same environment
- Requires GC to keep captured variables alive
- More familiar to web developers (Phoenix's target audience)

**Decision needed:** Which model does Phoenix use? This is not a bug — both models are valid. But the compiler needs to know:

- Value capture → closures store copies of captured values; no special heap allocation needed
- Reference capture → captured variables must be heap-allocated (or "boxed") so closures and the enclosing scope share them; requires GC integration at the closure boundary

**Recommendation:** Document the chosen model, update the interpreter if switching to reference capture, and add tests that explicitly verify the chosen behavior. The decision should be made based on what Phoenix's target users (web developers) expect, not implementation convenience.

- **Complexity:** Low if keeping value capture (just document it). Medium if switching to reference capture (interpreter needs `Rc<RefCell<Value>>` or similar for captured variables).
- **Depends on:** —
- **Final Decision:** Reference capture

#### 1.11.3 Refactor `check_expr` and Centralize Built-in Method Dispatch ✅

**File:** `phoenix-sema/src/checker.rs` (`check_expr` is 475+ lines), `phoenix-interp/src/interpreter.rs` (inline method dispatch)

Phase 1.9 will add string methods (1.9.2), Map methods (1.9.1), and functional collection methods (1.9.4). Each of these adds new match arms to both the checker and interpreter. Without refactoring first, `check_expr` will grow to 1000+ lines and the built-in method dispatch will be scattered across two files with no shared structure.

**Fix (two parts):**

1. **Split `check_expr`** into ~10 smaller methods, one per expression variant (`check_binary`, `check_call`, `check_method_call`, `check_match`, etc.). This is a mechanical refactor with no behavior change.

2. **Extract a `builtins` module** that declaratively registers built-in method signatures (for the checker) and implementations (for the interpreter). New built-in types and methods should be added by registering them in one place, not by scattering match arms across two files.

- **Complexity:** Medium — purely structural, no semantic changes. ~2 hours of mechanical refactoring.
- **Depends on:** Should be done **before** Phase 1.9 features are added.
- **Why before Phase 2:** The compiler's type-checking pass will reuse or closely mirror the checker. A clean, modular checker is much easier to adapt than a monolithic one.

#### 1.11.4 Fix String Interpolation Brace-Counting Bug ✅

**File:** `phoenix-parser/src/expr.rs:60-75`

The interpolation parser counts `{` and `}` depth to find the end of an interpolated expression, but doesn't account for braces inside string literals within the expression:

```phoenix
let msg: String = "result: {func("}")}"  // parser truncates at wrong }
```

The parser uses simple character-level brace counting rather than tracking whether it's inside a string literal. This is a correctness bug that will affect real programs.

**Fix:** Track string-literal context during brace counting — when a `"` is encountered inside the interpolation expression, toggle a flag and skip brace counting until the matching `"` is found. Handle escaped quotes (`\"`) within the inner string.

- **Complexity:** Small-medium — modify the brace-counting loop in `parse_string_interpolation` to handle quoted strings.
- **Depends on:** —
- **Why before Phase 2:** The compiler frontend will use the same parser. A parser bug that silently produces wrong ASTs is far harder to debug when there's a compilation step between parsing and execution.

### 1.12 Test Infrastructure ✅

Test infrastructure improvements that must be in place before Phase 2, so that the interpreter's test suite can serve as a conformance suite for the compiler.

#### 1.12.1 Output Capture for Integration Tests ✅

Most integration tests use `run()` which only checks "no panic" — actual `print()` output is not validated. When both an interpreter and compiler exist, the test suite must verify that they produce **identical output** for every program.

**Fix:** Add a `run_and_capture(source) -> Vec<String>` helper that collects all `print()` output, and update key integration tests to assert against expected output. Not every test needs output capture — error-path tests (`expect_type_error`, `expect_runtime_error`) are already precise. Focus on the happy-path tests where output correctness matters.

- **Complexity:** Small — requires threading a `Vec<String>` or `Write` through the interpreter's `print` built-in instead of writing to stdout directly.
- **Depends on:** —
- **Why before Phase 2:** Without this, there is no way to verify the compiler produces the same results as the interpreter. You would be building a compiler with no conformance test suite.

#### 1.12.2 CI Pipeline ✅

No CI/CD configuration exists. Set up a minimal pipeline that runs on every push:

- `cargo test` — all unit and integration tests
- `cargo clippy -- -D warnings` — lint for common mistakes
- `cargo fmt --check` — enforce consistent formatting

- **Complexity:** Small — a single GitHub Actions workflow file.
- **Depends on:** —
- **Why before Phase 2:** Phase 2 introduces a second backend. CI prevents regressions in the interpreter while the compiler is under development.

### 1.13 Phase 2 Pre-Work ✅

Items identified during a full codebase audit (2026-04-06). All resolved.

#### 1.13.1 Extend `CheckResult` with Type Registries ✅

**File:** `phoenix-sema/src/checker.rs:10-17`

`CheckResult` currently only exposes `diagnostics` and `lambda_captures`. The compiler will also need access to struct layouts, enum variant info, function signatures, method maps, trait implementations, and type alias resolutions — all of which are already computed by the `Checker` but stored in private fields.

**Fix:** Either extend `CheckResult` to include the type registries, or expose the `Checker` struct itself after analysis. The codegen crate needs these for layout computation, monomorphization, and method dispatch.

- **Complexity:** Small — expose existing data, no new computation.
- **Why before codegen:** Without type metadata, the compiler cannot generate code for struct construction, enum matching, function calls, or trait dispatch.

#### 1.13.2 Add Expression-Level Type Annotations ✅

**File:** `phoenix-sema/src/checker.rs` (`check_expr` and callees)

The checker computes the resolved type of every expression during `check_expr` but discards it (returns `Type` to the caller but doesn't persist it). The compiler needs to know the type of every expression for code generation — e.g., whether `a + b` is `Int` addition or `Float` addition, what the return type of a method call is, etc.

**Fix:** Add a `HashMap<Span, Type>` (or similar) to `CheckResult` that maps each expression's span to its resolved type. Alternatively, design a typed IR as Phase 2.1's first deliverable that captures this information during lowering.

- **Complexity:** Medium — requires threading a type map through `check_expr` and its callees.
- **Why before codegen:** A compiler that re-infers types during lowering is fragile and wasteful. Storing types once during checking is the standard approach.

#### 1.13.3 Convert `run()` Integration Tests to `run_expect()` ✅

**File:** `phoenix-driver/tests/*.rs`

Approximately 241 of 585 integration tests use `run()` which only asserts "no crash" — actual `print()` output is not validated. When both an interpreter and compiler exist, these tests will confirm "the compiler doesn't crash" but not "the compiler produces the same output."

**Fix:** For every `run()` call whose source code contains `print()`, capture the output and convert to `run_expect()` with pinned expected values. This is mechanical work — run each test, capture output, assert it.

- **Complexity:** Small but tedious — no design decisions, just pinning output.
- **Why before compiler output validation:** Without this, there is no way to verify the compiler produces the same results as the interpreter for ~40% of happy-path tests.

#### 1.13.4 Remove `EnumVariantLiteral` Dead AST Node ✅

**File:** `phoenix-parser/src/ast.rs:501`

`Expr::EnumVariantLiteral` is defined in the AST but never produced by the parser — all enum constructors are parsed as `StructLiteral`. The checker returns `Type::Error` for it (`checker.rs:763`), and the interpreter returns an error (`interpreter.rs:735`). Every new pass (IR lowering, codegen) would need a dead match arm for it.

**Fix:** Remove the variant from the `Expr` enum and all associated match arms.

- **Complexity:** Small — mechanical deletion across 4 files.
- **Why before codegen:** Dead AST variants add confusion and dead code to every compiler pass.

#### 1.13.5 Strengthen Trait Test Coverage ✅

**File:** `phoenix-driver/tests/traits.rs` (currently 17 tests)

Traits have the thinnest test coverage of any major feature. Missing scenarios: generic trait method return types, multiple bounds on a single type parameter (`<T: Display + Eq>`), trait dispatch through generic function calls with concrete types, trait methods that themselves take generic parameters.

**Fix:** Add 15–20 integration tests covering the above scenarios.

- **Complexity:** Small — writing tests, no implementation changes.
- **Why before codegen:** Trait dispatch is one of the harder things to lower to IR. A thin conformance suite means compiler bugs in trait handling will go undetected.

---
