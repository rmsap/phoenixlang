# Design Decisions

Open design questions awaiting a decision, and records of decisions already made along with their rationale. Tracked bugs and code-quality issues live in [known-issues.md](known-issues.md).

---

## Open Questions

### `List.get` panics on out-of-bounds

`List.get(index)` terminates the process (panic/exit) when the index is out of bounds, rather than returning `Option<T>`. This is inconsistent with `Map.get(key)`, which returns `Option<V>` for missing keys.

**Workaround:** Check `list.length()` before calling `get`, or use `first`/`last` which return `Option<T>`.

### Pipe operator argument position

Phase 1.9.6 specifies `expr |> f(args)` desugars to `f(expr, args)` (first argument). Most pipe-heavy languages (Elixir, F#) use last-argument or an explicit placeholder.

**Options:**

1. First argument (current spec): `data |> parse()` → `parse(data)`. Simple, matches Elixir.
2. Placeholder: `data |> transform(_, config)` → `transform(data, config)`. More flexible.
3. Last argument: `data |> process(config)` → `process(config, data)`. Matches F# and some FP conventions.

**Recommendation:** Start with option 1 (simplest). Add placeholder syntax later if needed.

### `else if` as a single token

Currently `else if` is parsed as `else { if ... }` (nested). This works but complicates span tracking and error messages for chained conditions. Consider whether `else if` should be a first-class construct in the parser.

**Recommendation:** Low priority. The current approach is correct and well-tested. Revisit if error messages for `else if` chains prove confusing.

### Unbraced single-statement `if` branches

`if`/`else` branches currently require braces: `if c { return a } else { return b }`. For guard-clause patterns, the braces are ceremony — most concise forms would be `if c return a else return b` or `let x = if c a else b`. Now that `if` is a first-class expression, this is a pure parser-ergonomics change with no impact on sema, IR, or runtime.

**Implementation cost:** modest (~100–150 lines in `parse_if_expr`). When the token after the condition is not `{`, parse a single statement and wrap it in a synthetic one-statement `Block`. Downstream layers (sema, IR lowering, interpreter) already operate on `Block` and need no changes.

**Design questions (the real cost):**

1. **What counts as an unbraced branch?** Options: single expression only; single expression plus `return`/`break`/`continue`; any single statement (including `let`). Rust allows none (always braces); Go disallows unbraced; Swift/Kotlin allow single expressions.
2. **Dangling else.** `if a if b x else y` — standard rule binds `else` to the nearest `if`, falls out of recursive descent for free. Explicit test needed.
3. **Newline handling.** Phoenix's parser consumes trailing newlines on statements. `if c\n  return a\nelse return b` must skip newlines before looking for `else`. Cheap to get right, cheap to get wrong.
4. **Nested unbraced `if`.** `if c if d a else b else e` parses unambiguously by dangling-else but reads as a puzzle. Rust requires braces here; Phoenix should probably forbid nested unbraced `if` and require parens or braces.
5. **Coexistence with braced form.** Two valid spellings means style debates. Worth a lint or a formatter decision, not a blocker.

**Recommendation:** Scope to "unbraced branch = single expression, or a single `return`/`break`/`continue` statement" — forbid `let` (no scope to hold the binding) and forbid nested unbraced `if` (require parens/braces). Keeps the grammar restrictive enough to avoid style fights and parsing corners while delivering the ergonomic win for the guard-clause case. Decide before users build muscle memory around the braced-only form.

### `defer` for resource cleanup

**Status: decision locked 2026-05-04, implemented 2026-05-06.**

Resolved in Phase 2.3 as the implied commitment of the GC strategy decision. **Decision: Go-style statement-level `defer expr;`.** See [G. Scope-bound cleanup syntax](#g-scope-bound-cleanup-syntax-go-style-statement-level-defer) under GC subordinate decisions for the full rationale and the alternatives rejected (block-binding `using`/`with` syntax was considered and declined). Implementation landed across the lexer, parser, sema (placement check + return/`?` rejection inside the deferred expression), AST interpreter, IR lowering, and all three backends — see the [Decision G entry in phase-2.md](phases/phase-2.md#design-decisions-locked-in-this-phase) for the per-backend contract and the ten matrix fixtures pinning each exit path.

### Lambda parameter inference at call sites

Lambda literals require full type annotations on every parameter and an explicit return type, producing heavy call sites for common higher-order methods:

```
nums.filter(function(n: Int) -> Bool { n % 2 == 0 })
nums.reduce(0, function(acc: Int, n: Int) -> Int { acc + n })
```

The call site already knows the closure's signature from the callee's parameter type, so the annotations are redundant ceremony.

**Root cause (current sema):**
- `Param.type_annotation` is `TypeExpr`, not `Option<TypeExpr>` (`phoenix-parser/src/ast.rs`) — the grammar requires annotations on every param.
- `check_lambda` (`phoenix-sema/src/check_expr.rs`) eagerly resolves each param's annotation and has no "expected type" parameter.
- `check_expr` has no bidirectional mode; `expected_type` threading only exists at the statement level (`let x: T = ...`).
- `check_call` (`phoenix-sema/src/check_expr_call.rs`) resolves the callee signature then checks arguments independently; it does not push the i-th param type into the i-th argument.

**Options:**

1. Status quo: keep `function(n: Int) -> Bool { ... }` everywhere. Consistent with named-function syntax but verbose at call sites.
2. Bidirectional inference only: make `Param.type_annotation` optional for lambdas, thread expected types through `check_expr`, and infer lambda param/return types from the call site's expected function type. No syntax change.
3. Inference plus terser syntax: (2) plus a lighter lambda form (e.g., `|n| n % 2 == 0` or `n => n % 2 == 0`). Biggest ergonomic win, biggest surface area.

**Generic-call complication:** For `.map<U>(|n| n * 2)` the receiver pins `T = Int`, but `U` must be inferred *from* the closure body — which requires fixing `n`'s type first. Bidirectional inference interleaved with generic argument inference requires either a real constraint solver or a staged two-pass approach in `infer_and_check_call_generics`.

**Recommendation:** Option 2 first, scoped to non-generic callees, so the terse form works in the common case without pulling in a constraint solver. Extend to generic calls as a follow-up. Reopen the syntax question (Option 3) only if annotation-free lambdas still read as ceremonial after inference lands. Implementation plan to live in a phase doc once scope is settled.

Soundness note: every variable remains strongly typed. If context is insufficient to infer a lambda's parameter or return type, the compiler rejects the program and asks for an annotation rather than falling back to a dynamic/`Any` type.

### Float equality and NaN consistency across the language

`Map<Float, V>` deliberately uses byte-wise key comparison (documented below under Decided), but the rest of the language's float-equality behavior is unspecified: `==` on floats in expressions, `List.contains` on `List<Float>`, sort comparators, pattern-match equality, and equality in `Set` (if/when one is added) all quietly inherit whatever the backing implementation does. Users will reasonably expect `if x == 0.0` and `map.get(0.0)` to agree — today they do not necessarily.

**Recommendation:** Pick one semantics for "Phoenix equality on `Float`" (byte-wise, IEEE 754, or reject `Float` equality entirely and force `approxEq`) and apply it uniformly — map keys, set keys, `List.contains`, pattern matching, `==` in expressions. Document it once. Do this before `Set<Float>` or richer collection APIs solidify the current drift.

### String interpolation spawns a sub-parser

String interpolation (`"…{expr}…"`) instantiates a nested `Parser` to tokenize and parse expressions inside `{}` holes (`phoenix-parser/src/expr.rs`). It works, but it duplicates operator-precedence assumptions and has its own error-reporting quirks — any change to expression parsing must be mirrored into the interpolation path or the two will drift.

**Recommendation:** Low priority today. Revisit if string formatting grows format specifiers, nested interpolation, or multi-line forms, at which point the sub-parser should be retired in favor of sharing the main expression-parser entry point. Flagging so the drift does not silently widen.

### `?` operator ignores error type parameter

`check_try` (`phoenix-sema/src/check_expr.rs:297-311`) only verifies that the *base constructor name* (`Result` or `Option`) of the operand matches the enclosing function's return type. The generic error parameter `E` is never compared. Consequence: propagating a `Result<T, DbError>` via `?` inside a function returning `Result<U, HttpError>` passes type-checking despite the error types being unrelated.

This is either a latent bug or a tacit design decision to punt on error conversion. Either way it needs a resolution story — especially before web/HTTP code starts converting between layered error types as a matter of routine.

**Options:**

1. **Strict match.** Require the error type to be identical between the operand and the enclosing return type. Simple, no conversions, but forces users to hand-write error mapping at every boundary (`.mapErr(...)`).
2. **Implicit conversion via a `From`/`Into`-style trait.** When error types differ, require a user-declared conversion impl (`impl From<DbError> for HttpError`) and have `?` invoke it on the error branch. Matches Rust's model; ergonomic but pulls trait-based conversion into the core language.
3. **Explicit conversion only.** Keep strict match at the `?` site; provide `result.mapErr(fn)` and `opt.okOr(err)` in the stdlib as the only way to bridge. Clearest semantics, most verbose user code.

**Recommendation:** Option 2. The `?` operator exists precisely to remove boilerplate on the happy path, and forcing `.mapErr(...)` before every `?` defeats the point. But do not ship the current lax check — either enforce it strictly (Option 1) or wire up the conversion story (Option 2). Today's behavior silently accepts programs that almost certainly do not mean what the author wrote.

Also: `?` on `Option<T>` requires the enclosing function to return `Option`, and `?` on `Result` requires `Result`. There is no automatic `Option → Result` lifting (e.g., via a "missing value" error). Decide whether to support it now or explicitly forbid it; today it works by accident of the constructor-name check and may or may not be intentional.

### `mut` gives no aliasing guarantees; closures share mutable captures freely

`mut` in Phoenix is a per-binding mutability flag tracked in `phoenix-sema/src/scope.rs`. There is no borrow checker, no aliasing analysis, and no restriction on how many closures capture the same mutable variable. Two closures that each capture `mut x` can both mutate it, and nothing at the type-system level prevents this.

Today this is benign because the interpreter and compiled backend are single-threaded. Under Phase 4.3 (async runtime), the same pattern becomes the shape of a data race — and structured concurrency as described in `phase-4.md` organizes *task lifetimes*, it does not by itself prevent aliased mutation across concurrent tasks.

**Options:**

1. **Adopt a Rust-style borrow checker.** Strongest safety guarantees, highest implementation cost, and a significant user-facing complexity tax (lifetimes, borrow errors). Likely too much machinery for a language whose pitch is not "memory-safe systems programming."
2. **Forbid shared mutable captures.** A closure may capture a variable by value (copy / move) or immutably, but not mutably-and-shared. Simple rule, catches the async foot-gun, but changes how common patterns (accumulators, counters, stateful callbacks) are written.
3. **Allow shared mutable captures in single-threaded contexts; require explicit sync primitives (`Atomic<T>`, `Mutex<T>`) to share mutable state across tasks.** Checked at the point of `spawn` / task-boundary crossings rather than at capture time.
4. **Status quo plus runtime task-local isolation.** Tasks get their own scope; cross-task sharing requires explicit channel / message-passing APIs, and the type system forbids capturing non-`Send`-equivalent values across task boundaries.

**Recommendation:** Option 3 or 4 — surface the constraint at task boundaries rather than at every closure, so sequential code stays ergonomic while concurrent code is forced to be explicit about shared state. Pick before Phase 4.3 designs `spawn` / task APIs; retrofitting `Send`-equivalent bounds after task APIs exist is the scenario where languages end up with permanent coloring problems.

### No `Display` / printing story for user types

The runtime exposes `phx_print_i64`, `phx_print_f64`, `phx_print_bool`, `phx_print_str` (`phoenix-runtime/src/lib.rs`) — and nothing else. Structs, enums, closures, lists-of-user-types, and maps have no defined printable form. `print(myStruct)` either fails to compile or produces something unhelpful, depending on where the user tries it. Every debugging session on user types hits this wall.

**Options:**

1. **Auto-derive a debug representation for every user type.** `print` on any value walks the type and produces something like `User { name: "Alice", age: 30 }`. Zero user effort; the compiler generates a `phx_debug_<type>` function per declared type. Cost: code size, and the debug output is whatever the compiler decides it is.
2. **User-implemented `Display` / `ToString` trait.** Users opt in per type. Flexible, but makes `print` on a struct fail until someone writes the impl — painful in the early-exploration phase where `print` is the debugger.
3. **Both.** Auto-derive a `Debug` representation used by default; let users implement `Display` for pretty-printing. Matches Rust. Most ergonomic, slightly more machinery.
4. **Reflective printing in the runtime.** Each allocation carries a type tag; the runtime walks structures at print time. Zero compile-time cost but bakes in a runtime type-info representation that is hard to remove.

**Recommendation:** Option 3. Auto-derive a sane default so `print(anything)` always works during development, with `Display` as the opt-in override for user-facing output. Ship the auto-derive alongside `print` — debugging without it is unreasonable, and users will build janky workarounds (custom `.toString()` methods on every type) that later conflict with whatever formal trait gets introduced.

---

## Decided

### No first-class ORM; a transparent typed data layer instead (Phase 4.7)

**Decided 2026-06-19.** Phoenix ships **no ORM**. The compile-time typed SQL committed in [`phase-4.md` §4.7](phases/phase-4.md#47-database-access-compile-time-typed-queries) — raw `db.query(SELECT ...)` validated against an explicit `schema` block, with row types inferred from the SELECT clause — stays the foundation. On top of it Phoenix layers a thin, *transparent* data layer for ergonomics: schema-derived typed CRUD (`db.users.insert/find/update/delete`), **explicit, never-lazy** relationship loading (`db.load(posts, .author)` — one visible round trip per call), and an optional typed query builder (Kysely/jOOQ-style, inspectable SQL) for dynamic queries only. The default surface remains string SQL.

**The governing rule:** no data-layer operation may hide its database round trips — no lazy-loaded associations, no implicit identity map, no change-tracking writes, no opaque generated SQL. Every relationship traversal and every write is explicit and predictable. That rule is the line between this data layer and an ORM, and the absence of an ORM is a deliberate position, not a gap to fill later.

**Rationale.**

1. **An ORM contradicts the wedge.** Phoenix's promise is that the compiler checks every boundary and you can see what runs ("from database to DOM, no drift"). An ORM's defining behavior — lazy loading, identity maps, change tracking, opaque SQL — is precisely the *hiding* of the database boundary (N+1 surprises, surprise writes). Adopting it would undercut the one thing the language is selling.
2. **It would be undifferentiated.** A dozen mature ORMs already exist; "another good ORM" is not a reason to choose Phoenix. SQL whose row types provably cannot drift from the schema and flow unbroken into the endpoint and the DOM is a claim no one else makes — that is the differentiator worth building.
3. **The ecosystem already moved this way.** The sophisticated end of the field (sqlc, Drizzle's SQL-like API, Kysely, jOOQ, sqlx) has been migrating *away* from heavy ORMs toward typed-SQL and thin builders. The committed §4.7 design is aligned with where good teams are going.
4. **Scope.** Item 51 (typed queries) is already "Very High" effort. A real ORM (entity lifecycle, unit-of-work, relationship DSL, cascade orchestration) would balloon scope and steal oxygen from the Phase 5 differentiators (reactivity, typed endpoints, frontend) that are the actual moat.

This *reinforces* the §4.7 decision rather than overturning it. The CRUD / relationship-loading / query-builder pieces are scoped as roadmap items 51a–51c. Concrete grammar and parsing for them is detailed design work for when Phase 4.7 is built.

### Field declarations use `name: Type` colon syntax

**Decided 2026-06-10.** Every named, typed field in the language declares as `name: Type`: struct fields (`x: Int`, with `public`, `where` constraints, and doc comments unchanged), endpoint `query` parameters (`page: Int = 1`), and endpoint `headers` entries (`rateLimit: String as "X-RateLimit-Limit" = default`). The previous type-first form (`Int x`) was the lone holdout against the rest of the language — function parameters, return annotations, `let` bindings, and map literals all already used colon syntax — and it actively trapped users: writing the natural `x: Int` in a struct didn't error, it **hung the parser** (see the Phase 2.4 "Bugs closed" entry). The old form is a hard parse error with a targeted migration diagnostic ("write `x: Int`, not `Int x`"); no dual-syntax transition period (pre-1.0, single-user — dual grammar would be pure debt against the consistency goal). The phase-4 `schema`/`table` DSL sketch was updated to match so the future column grammar starts consistent. Out of scope: enum variants stay positional (`Circle(Float)` — they mirror constructor calls, not field declarations).

### `Map<Float, V>` uses byte-wise key comparison

`Map<Float, V>` compares keys using byte-wise equality, not IEEE 754 floating-point equality. This means `-0.0` and `0.0` are treated as different keys, and `NaN` equals itself (unlike IEEE 754 where `NaN != NaN`). This is deliberate — byte-wise comparison provides consistent, deterministic behavior for map lookups.

Note: this decision is scoped to `Map` keys only. Float equality elsewhere in the language is still open — see [Float equality and NaN consistency across the language](#float-equality-and-nan-consistency-across-the-language).

### `substring` clamps out-of-range indices silently

`substring(start, end)` silently clamps out-of-range indices instead of returning an error:
- Negative `start` is clamped to `0`
- `end` beyond the string length is clamped to the string length
- `start > end` produces an empty string

This matches the behavior of JavaScript's `String.prototype.substring()` but may surprise users expecting strict bounds checking.

### `break`/`continue` in match arms inside loops

`break` and `continue` inside a match arm are now rejected at the semantic analysis stage with a compile error. This prevents the silent conversion to `Void` that occurred in the interpreter. Option 1 (threading `StmtResult` through expression evaluation) can be revisited in the IR phase if needed.

### Generic function monomorphization strategy

User-defined generic functions currently lower to a `GENERIC_PLACEHOLDER` sentinel in the IR (`phoenix-ir/src/lower.rs`) and do not yet compile to runnable code. Built-in generics (`List`, `Map`, `Option`, `Result`) work only because their layouts are hardcoded in the Cranelift backend. The question: when user code writes `function f<T>(...)` and calls it at multiple concrete types, what does the compiler produce?

**Decision:** Monomorphization. The compiler collects each concrete instantiation of a generic function and emits one specialized copy per instantiation. No runtime indirection; zero-cost generics in the Rust / C++ template sense.
**Decided:** 2026-04-19
**Target phase:** Phase 2.2 (Cranelift native compilation, in flight)
**Implemented:** 2026-04-20. `monomorphize` pass in `crates/phoenix-ir/src/monomorphize.rs` runs post-lowering inside `lower()`. Sema records per-call-site concrete type-arg bindings in `CheckResult.call_type_args` (keyed by span); IR lowering propagates them by embedding the type args directly into the middle slot of `Op::Call(FuncId, Vec<IrType>, Vec<ValueId>)`, so every `Op::Call` is self-describing and no side table is required. The BFS pass walks each non-template caller in deterministic `(FuncId, block, instr)` order, specializes each `(template, targs)` pair, substitutes `IrType::TypeVar(name)` with the concrete type, rewrites `Op::Call` destinations to their specialized FuncIds, and clears the embedded `type_args` vectors. Specialized function names use a symbol-safe grammar (see "Mangling grammar" below); templates remain in `module.functions` as inert stubs (`is_generic_template = true`) to preserve the `FuncId`-as-vector-index invariant, and every downstream pass that walks functions should iterate via `IrModule::concrete_functions()`. Orphan `IrType::TypeVar` (from unresolved sema inference, e.g. empty list literals) is erased to `StructRef(GENERIC_PLACEHOLDER)` post-pass for the built-in-generic inference strategies in the Cranelift backend to handle at use sites. MVP scope covers multiple type params, recursion, generic-to-generic calls, and generic methods on user-defined types (method's own type parameters); parent-type-parameter substitution in methods on generic structs, trait-bounded method-call specialization, generic closures, and cross-module instantiation are deferred.

**Mangling grammar.** Specialized function names are built from the original name plus one `__{mangled_type}` segment per type argument, where each mangled type matches `[A-Za-z0-9_]`:

| Source type | Mangled form |
|---|---|
| `Int` / `Float` / `Bool` / `Void` / `String` | `i64` / `f64` / `bool` / `void` / `str` |
| `StructRef(name)` | `s_{name}` |
| `EnumRef(name, args)` | `e_{name}` (empty args) or `e_{name}__{mangle(arg1)}__…__{mangle(argN)}_E` |
| `List<T>` | `L_{mangle(T)}_E` |
| `Map<K, V>` | `M_{mangle(K)}_{mangle(V)}_E` |
| `(P1, …, Pn) -> R` | `C{n}_{mangle(P1)}_…_{mangle(Pn)}_{mangle(R)}_E` |

The `__` segment separator cannot appear in a Phoenix identifier, so specialized names are collision-free with user-defined function names. The Cranelift context prepends `phx_` to the final mangled name and replaces `.` with `__` for method symbols (e.g. `TypeName__method`). No further symbol sanitization is required.

**`EnumRef`'s name/arg delimiter.** `EnumRef` is the only variadic-arg type constructor without an arity prefix, so its mangling needs a delimiter that cannot appear inside a name or arg encoding:

- A single-`_` separator would collide: `EnumRef("Opt", [StructRef("foo_i64")])` and `EnumRef("Opt", [StructRef("foo"), I64])` would both mangle to `e_Opt_s_foo_i64_E`.
- Phoenix identifiers forbid `__`, so `__` splits cleanly between name and first arg and between adjacent args.
- `Closure` dodges the problem differently — it starts with an arity prefix (`C3_…`) — but `EnumRef` reuses the identifier invariant the outer mangler already relies on.

**Rationale:** Phoenix is positioned as a compiled language with native performance, and the stdlib generics are already hand-monomorphized in the Cranelift backend. Any other strategy creates a two-tier world where stdlib is fast and user generics are slow. Monomorphization also stacks cleanly with a future vtable ABI for `dyn Trait`. The compile-time fan-out cost is real but mitigable (shared specialization where layouts match, incremental caching of instantiations) and only bites meaningfully once the stdlib grows in Phase 4.

**Enum layouts are keyed by name, enum *types* by name + args.** `IrType::EnumRef(name, args)` carries generic args so backend payload-type inference can read them directly, but `IrModule.enum_layouts` (and the `e_{name}` prefix of mangled symbols) keys on the bare name. This works today because payloads are uniformly heap-boxed and one-slot, so every `Option<T>` shares a layout. If a future specialization ever packs payloads inline (e.g. `Option<Int>` unboxed vs `Option<String>` boxed), layouts must also key on name + args, and the mangle grammar's `_E` terminator on the args segment is already unambiguous for that.

**`EnumRef` carries args but `StructRef` drops them.** The two reference types are asymmetric by design:

- **Struct fields are reified at monomorphization time.** When a generic struct `Pair<A, B>` is used as `Pair<Int, String>`, the struct-monomorphization pass (`phoenix-ir/src/monomorphize.rs::monomorphize_structs`, landed 2026-04-21) substitutes each field's `TypeVar` and registers a specialized layout under a mangled name (`Pair__i64__str`). `StructRef` carries its concrete args through lowering, the struct-mono pass rewrites every use site to `StructRef(mangled_name, [])`, and the Cranelift backend reads concrete field types by bare mangled-name lookup — the long-term "drops args" shape is now fully realized. Methods on generic structs are cloned and specialized in lockstep with their struct; `dyn Trait` over a generic struct works because the vtable is rekeyed from `(bare, trait)` to `(mangled, trait)` in the same pass. Recursive generics (`Node<T>`) converge via a fixed-point worklist.
- **Enum layouts are shared across type arguments.** Stdlib `Option`/`Result` encode their payload fields as the `GENERIC_PLACEHOLDER` sentinel in `enum_layouts` (one layout, any `T`). The concrete payload type is resolved per use site by Cranelift inference strategies — Strategy 0 reads `EnumRef.args[i]` directly — so `EnumRef` must carry the args forward through lowering. User-defined generic enums with methods are gated off for the same underlying reason — see [known-issues.md: *Methods on generic enums are gated off*](known-issues.md#methods-on-generic-enums-are-gated-off-payload-inference-fallbacks-kept-alive-as-a-consequence).

If the "layouts keyed by name" decision above is ever reversed (inline-packed payloads), this asymmetry shrinks: enum layouts would also be reified, and the args on `EnumRef` would exist purely as a key into `enum_layouts`, mirroring how `StructRef` would then work.

**Alternatives considered:**
- **Uniform boxed representation (Go / Java-erasure style)** — one compiled copy, values passed as type-tagged pointers. Rejected: contradicts the native-performance positioning, forces the hand-monomorphized stdlib to either be rewritten boxed or live in a different world than user generics, and overlaps awkwardly with the planned `dyn Trait` vtable ABI.
- **Hybrid (monomorphize hot, box the rest)** — two ABIs plus a heuristic. Rejected: adds per-function uncertainty ("will this be fast?") without a clear rule, and the compile-time / binary-size savings don't matter at Phoenix's current scale.

### Dynamic dispatch via `dyn Trait`

Traits in Phoenix are statically dispatched only today — every trait use must be monomorphized at a concrete type via a generic bound, and a grep for `dyn` / `vtable` / `witness` / `trait_object` in `phoenix-sema`, `phoenix-ir`, `phoenix-cranelift` finds nothing. The consequence is that heterogeneous collections of trait implementors are unexpressible: a web router cannot hold `List<Handler>` where each element is a different concrete handler type. This question is whether and how to add runtime-dispatched trait objects.

**Decision:** Add `dyn Trait` with a vtable ABI. A `dyn Trait` value is a fat pointer `(data_ptr, vtable_ptr)` — the same shape as Phoenix's existing fat-pointer conventions (e.g., `String` is `(ptr, len)`). Static dispatch remains the default; users pay the indirection only when they explicitly write `dyn`. This is the Rust-style model, not Java's virtual-by-default.
**Decided:** 2026-04-19
**Target phase:** Phase 2.2 (Cranelift native compilation, in flight)
**Rationale:** Phoenix is AOT-compiled via Cranelift with no JIT planned. Java-style virtual-by-default only works because the JVM's JIT devirtualizes hot call sites at runtime; without that, virtual-by-default is just "slow by default" and would contradict the monomorphization decision made above. Every AOT language without a JIT (C++, Swift, Rust) has landed on opt-in dynamic dispatch for the same reason. The vtable ABI is the long-lived commitment that must be set before Cranelift's calling conventions solidify; object-safety rules can be tightened over time.

**Alternatives considered:**
- **Stay fully static.** Rejected: fights Phoenix's web-framework positioning — middleware chains, route handlers, plugin APIs all want heterogeneous collections. Forcing enums (closed sets) for cross-type polymorphism rules out extension patterns entirely.
- **Existential types / `impl Trait` in return position only.** Rejected on its own as too narrow — does not solve heterogeneous collections. Could be layered on top of the chosen option later as a sugar for hiding concrete return types, but not as a replacement.

**Follow-ups (not in scope here):**
- Object-safety rules (which traits are dyn-compatible). Can evolve after the ABI lands.
- Sugar for bare trait names in type position (`List<Handler>` auto-meaning `List<dyn Handler>`). Pure ergonomic layer; revisit if explicit `dyn` feels noisy in practice.

**Implemented:** 2026-04-20. Single-bound `dyn Trait` in function params, return types, `let` annotations, and struct fields. ABI is 2 slots inline, parallel to `StringRef`: `(data_ptr, vtable_ptr)` in `IrType::DynRef(trait_name)`. Coercion from concrete types to `dyn Trait` happens at assignment boundaries (`types_compatible` in `check_types.rs` + `coerce_to_expected` in `phoenix-ir/src/lower_dyn.rs`). Vtables are emitted once per `(concrete_type, trait_name)` pair as rodata in the Cranelift backend (`translate/dyn_trait.rs`, 8-byte aligned), ordered by trait-method-declaration index. `Op::DynCall` carries the pre-resolved slot index (not the method name) so codegen is a direct `vtable[slot * 8]` load. Object-safety is validated at trait-registration time (`phoenix-sema/src/object_safety.rs`) and cached on `TraitInfo.object_safety_error`: `Self` must not appear anywhere inside a method's parameter or return types — not as a bare type, not nested inside a `Generic` (`Option<Self>`, `List<Self>`), not inside a `Function` type (`Fn(Self) -> Int`). Non-object-safe traits remain usable as generic bounds (`<T: Trait>`). IR invariants for `Op::DynAlloc` and `Op::DynCall` are enforced by `phoenix-ir/src/verify.rs` (receiver type matches trait, vtable registered, slot index in range, and every `DynRef`-typed value traces to a `DynAlloc` / function or block param).

**Why explicit `dyn` (vs. implicit `Drawable` as a dynamic-dispatch type)** (decided 2026-04-20): bare `Drawable` as a type remains a compile error. Users must write `dyn Drawable` for runtime dispatch or `<T: Drawable>` for static. Reasons: (a) Phoenix already has static-dispatch generic bounds, so implicit-dyn would create a subtle perf gotcha where `foo<T: Drawable>(x: T)` and `foo(x: Drawable)` look similar but compile very differently; (b) explicit `dyn` makes runtime cost visible (indirect call, no inlining); (c) leaves syntactic room for future `impl Trait` / existential return types; (d) follows Rust 2018 / Swift 5.6 precedent — both started implicit and added explicit markers after user confusion about performance. The tradeoff accepted: one more keyword to learn, in exchange for a clearer distinction between Phoenix's two trait-dispatch modes.

**Deferred follow-ups.** Each carries a phase target; see known-issues.md for the concrete tripwires and workarounds.

| Follow-up | Target | Summary |
|---|---|---|
| Multi-bound trait objects (`dyn Foo + Bar`) | Phase 3 | Requires deciding whether bounds must be object-safe individually or only in combination, and whether the vtable is merged-method or multi-pointer. |
| Supertraits (`trait Sub: Super { ... }`) | Phase 3 | Affects trait-declaration syntax, `dyn Sub → dyn Super` coercion, and vtable layout. Sema doesn't model supertrait relations today. |
| `where Self: Sized` method carve-outs | Phase 3+ | Rust's mechanism for "mostly object-safe" traits. Open whether Phoenix needs it or users should split the trait. |
| Drop slot / custom destructor in vtable | **✅ Resolved 2026-05-06 (not needed)** | Phase 2.3 shipped a pure tracing GC and Go-style statement-level `defer`. Tracing GC reclaims unreachable objects without per-type destruction, and `defer` covers user-driven cleanup at scope exit — neither requires a vtable drop slot. Revisit only if a future feature (e.g., FFI handle wrappers requiring deterministic finalization) reintroduces the need. |
| Heterogeneous list literals (`[Circle(1), Square(2)]` typed `List<dyn Drawable>`) | Phase 3 | Blocked on bidirectional type inference in list-literal checking. The previously suggested `push()` workaround does not work today (sema rejects `let xs: List<dyn Trait> = []` because the empty literal types as `List<T>`). See [known-issues.md](known-issues.md#listdyn-trait-literal-initialization-in-compiled-mode). |
| `<T: Trait>` method calls in compiled mode | **✅ Implemented 2026-04-21** | Resolved in function-monomorphization: `Op::BuiltinCall(".method", ...)` emitted at IR lowering (with empty type-name prefix because sema's receiver is `TypeVar`) is rewritten to a direct `Op::Call` after substitution, using `method_index[(substituted_type, method)]`. See `phoenix-ir/src/monomorphize.rs::resolve_trait_bound_builtin_calls`. |
| `dyn Trait` over generic user-defined structs | **✅ Implemented 2026-04-21** | Lands as part of the struct-monomorphization pass — the `dyn_vtables` rekey from `(bare_name, trait)` to `(mangled_name, trait)` runs during struct-mono's rewrite phase, and method FuncIds in the vtable entries are re-resolved through the specialized `method_index`. Two instantiations of the same generic struct implementing the same trait no longer collide. |
| `<T: Trait>` → `dyn Trait` coercion in compiled mode | **✅ Implemented 2026-04-24** | Same shape as the method-call fix: IR lowering emits `Op::UnresolvedDynAlloc(trait, value)` when the source is a `TypeVar`; function-monomorphization's Pass B rewrites it to a concrete `Op::DynAlloc` after substitution, registering the `(concrete, trait)` vtable through the shared `IrModule::register_dyn_vtable` helper. See `phoenix-ir/src/monomorphize/placeholder_resolution.rs::resolve_unresolved_dyn_allocs`. |
| Default argument values in compiled mode | **✅ Implemented 2026-04-24** | Sema's `FunctionInfo` now carries `default_param_exprs: HashMap<usize, Expr>` (replacing the previous index-only `default_param_indices`); `merge_call_args` in IR lowering synthesizes each missing positional slot by lowering the default expression at the call site. Matches the AST-interpreter semantics; `coerce_call_args` handles any downstream concrete-to-`dyn` wrap. |

### Default-argument lowering strategy

Phoenix supports default parameter values: `function render(title: String = "untitled") -> String`.  Sema accepts the declaration and the AST interpreter evaluates defaults at call time, but when Cranelift compilation landed, a design question surfaced: *where* does the default expression get materialized?  Two plausible sites exist — at the caller's call site (inline the default once per omitted slot) or at the callee's entry block (synthesize the default once, guarded by a "this slot was omitted" flag passed from every caller).

**Decision:** Caller-site materialization.  `FunctionInfo` on the sema side carries `default_param_exprs: HashMap<usize, Expr>` — the full parsed expression cloned at function registration — and `merge_call_args` in `phoenix-ir/src/lower_expr.rs` lowers each missing slot's default into the caller's IR at the call site, before `coerce_call_args` runs.  No ABI change; no fill-mask; every `Op::Call` is emitted with a complete argument vector.
**Decided:** 2026-04-24
**Target phase:** Phase 2.2 (closed).  Landed as the fix for the "Default argument values in compiled mode" entry formerly in known-issues.md.

**Rationale:** The alternative (callee-side synthesis) requires an ABI change — the caller has to tell the callee which slots were omitted, via a fill-mask parameter or a sentinel per slot.  That's a permanent commitment on the calling convention for a language still in Phase 2.  Caller-side lowering is ABI-neutral: it's a pure IR transformation that the backend never sees.  It also matches the AST interpreter's existing semantics (defaults are evaluated at call time with only globals in scope, not at callee entry with a fill-mask), so the three backends (AST interp / IR interp / compiled) agree without a per-backend divergence.

**Principle — default-expression scope.** A default expression is *lexically authored* inside its callee but *evaluated* in the caller's scope.  Sema enforces this as an invariant: Pass 1 of `check_function` (`phoenix-sema/src/checker.rs`) type-checks every default expression with **no parameters of the enclosing function in scope** — only module-level globals, other declared functions, and the callee's own type parameters (for type-resolution of the param annotation, not for identifier lookup).  This is the rule the two accepted downsides below fall out of, not a local patch.

**Accepted downsides, and how they're contained:**

- **Scope mismatch.** The default expression is lexically scoped to the callee but evaluated in the caller's scope.  If the default references an earlier parameter of the same function — `function f(x: Int, y: Int = x + 1)` — the identifier `x` resolves against the caller's scope, not the callee's, which produces either a runtime failure (AST interp) or a sema-hidden miscompile (compiled).  **Resolution:** sema's `check_function` (`phoenix-sema/src/checker.rs`) now type-checks every default expression *before* binding any parameter into scope.  `f(x: Int, y: Int = x)` is rejected at sema time with "undefined identifier `x`" rather than reaching lowering.
- **Free type variables.** Inside a generic callee, a default expression whose inferred type references the callee's type parameters (`function f<T>(x: T = zero<T>())`) cannot be meaningfully lowered at a concrete caller — the caller's type-arg substitution binds the *caller's* parameters, not the callee's, so residual `TypeVar`s would trip the `contains_type_var` assertion in function-monomorphization.  **Resolution:** sema's `check_function` rejects any default whose inferred type has free type vars (`Type::has_type_vars()`), with a diagnostic that names the offending type.  Defaults that are concrete (plain literals, calls returning concrete types) remain allowed in generic functions.  *Conservative:* this unconditionally rejects defaults whose *inferred* type is type-parametric, even cases the caller-site lowering could in principle handle if bidirectional inference were threaded through (e.g. `function f<T>(x: Option<T> = None)` where the caller's `T` is knowable from context).  Lifting this is tied to the Phase 3 bidirectional-inference rework — same machinery as `List<dyn Trait>` and match-arm dyn coercion — so defaults referencing `T` land alongside that work, not before.
- **Code size.** Every call site that omits a defaulted slot inlines the default expression.  For simple literal defaults this is a few extra IR ops; for a default like `someHelper<ComplexType>()` the inlining compounds.  Accepted as a tradeoff for ABI neutrality; if this becomes measurable, a CSE pass or a memoizing helper can reduce duplication without changing the ABI.

**Alternatives considered:**

- **Callee-side synthesis with a fill-mask parameter.** The caller passes a `u64` (or per-param bit) indicating which slots were omitted; the callee's entry block contains a conditional initialization per defaulted slot.  Rejected: permanent ABI commitment; interacts poorly with indirect calls through closures (`CallIndirect` would need the same mask shape in every closure signature); forces every defaulted parameter slot to be laid out mutably even when the caller filled it.
- **Desugar at parse / sema time.** Rewrite `f()` into `f(defaultExpr())` in the AST before lowering.  Rejected: loses the "defaults only" semantics (the desugar is indistinguishable from an explicit argument, so tooling that wants to distinguish "user passed the value" from "default filled it" can't), and the rewrite has to happen after sema has picked the callee, which is itself after type inference — a chicken-and-egg for generic calls.
- **Per-function "default initializers" table in the IR, lowered once inside the callee's body and invoked from each call site via a new `Op::CallWithDefaults(..., mask)`.** Rejected: same ABI-commitment and closure-shape concerns as the fill-mask option, plus a new IR opcode whose only consumer is default-argument handling.

### Placeholder-op resolution via a dedicated concretize pass

IR lowering emits placeholder ops — `Op::UnresolvedTraitMethod` (for trait-bound method calls on type-variable receivers, 2026-04-21) and `Op::UnresolvedDynAlloc` (for `dyn Trait` coercion from a generic parameter, 2026-04-24) — because their concrete-type arguments are only known after monomorphization substitutes.  Today both are rewritten inline inside function-monomorphization's Pass B, and each new placeholder costs five coordinated edits: enum variant, verifier branch, mono-time resolver, Cranelift error arm, IR-interp error arm.  Phase 2.6 (closure representation via IR blocks), Phase 3 (bidirectional inference for list literals and match arms), and several Phase 3 trait-system follow-ups will each plausibly want one more placeholder — the five-edit tax compounds.

**Decision:** Introduce a dedicated `concretize` IR pass that runs after both function-mono and struct-mono.  Monomorphization's sole job becomes TypeVar substitution; every placeholder→concrete rewrite lives in `concretize`.  The verifier gains one clean invariant: "no `Unresolved*` op after concretize runs."  Each new placeholder adds one enum variant plus one arm inside `concretize`, not five edits across five files.
**Decided:** 2026-04-24
**Target phase:** Gate on placeholder count reaching 4, or alongside the first post-mono pass that independently needs one — whichever comes first.  On the current trajectory, somewhere in Phase 2.6 or early Phase 3.

**Near-term preparation — ✅ Landed 2026-04-24.** The two existing resolvers (`resolve_trait_bound_method_calls`, `resolve_unresolved_dyn_allocs`) and their shared helpers (`receiver_type_name`, `primitive_type_name`) live in `phoenix-ir/src/monomorphize/placeholder_resolution.rs`; `function_mono` calls them by `use super::placeholder_resolution::*`.  All placeholder-specific logic colocated; behavior identical.  This makes the eventual promotion-to-its-own-pass a pass-boundary change rather than a structural reorg.  Rule-of-three still applies — don't promote at count 2 or 3.

**Rationale — why a pass, not a collapsed `Op::Unresolved(kind)`.** The alternative (single `Op::Unresolved(UnresolvedKind, payload)` with a uniform payload) loses shape information — placeholder X carries exactly one value, placeholder Y carries receiver + method name + args.  Each resolver relies on that shape; unifying them would reintroduce per-kind dispatch without removing the bookkeeping.  A separate pass with a typed enum variant per placeholder keeps the type system doing the bookkeeping and still centralizes the rewriting.

**Rationale — this simplifies struct-mono, not just function-mono.** Today, function-mono registers `dyn_vtables[(Container, Drawable)]` when it specializes a generic function at `T = Container<Int>`, and struct-mono has to rekey that entry to `(Container__i64, Drawable)` when it mangles the struct name.  With concretize running after both, the placeholder is resolved once, with the final mangled name already in hand — struct-mono's vtable-rekey step disappears entirely.

**Alternatives considered:**
- **Status quo with per-placeholder resolvers inside mono.** Rejected long-term: five edits per addition; no single enforcement point; struct-mono's rekey step remains load-bearing.
- **Collapse all placeholders into `Op::Unresolved(UnresolvedKind, payload)`.** Rejected: uniform payload loses shape information; introduces its own verifier / interp match-on-kind dispatch without removing the per-kind logic.
- **Land the pass now at placeholder count 2.** Rejected: abstraction cost exceeds savings until count 4.  The `mod placeholder_resolution` move gives us the option without paying for it.

### Post-sema ownership: `ResolvedModule` as the sema→IR handoff

Sema's `CheckResult` has grown into a live side-table: `functions: HashMap<String, FunctionInfo>` (now holding cloned default-expression ASTs alongside types and names), `structs`, `enums`, `traits`, `method_index`, `expr_types` (span→type), `call_type_args` (span→[Type]).  IR lowering, Cranelift codegen, and `phoenix-lsp` all read from this structure — typically **by function name**.

This is becoming structurally load-bearing in four ways, each visible today:

1. **Lookup by name, not by id.** `merge_call_args` in `phoenix-ir/src/lower_expr.rs` does `self.check.functions.get(&func_name)`; sema registers by bare source name.  Cross-module name collisions break this pattern the moment Phase 2.6 lands.
2. **Sema is a live dependency, not a pipeline stage.** Lowering borrows `&CheckResult` for its duration.  Nothing after sema can drop sema's state — it stays rooted until every downstream consumer releases.
3. **LSP strain.** `phoenix-lsp` holds a `Checker` per workspace and re-checks files against shared state.  Real modules want per-module re-check; the current single-pass `Checker` model does not support it cleanly.
4. **Drift risk under future incremental checking.** Two sources of truth (sema's `CheckResult`, IR's metadata on `IrModule`) can only stay consistent while sema is strictly single-pass.  That guarantee will not survive Phase 2.6.

**Decision:** Introduce `ResolvedModule` as the single post-sema handoff type.  Everything downstream needs from sema — function signatures, struct / enum layouts, trait registry, method index, per-span expression types, per-span call type args, default-expression clones, visibility metadata once modules land — lives on `ResolvedModule`, indexed by stable ids (`FuncId`, `StructId`, `EnumId`, `TraitId`) not by source name.  Sema is the factory; IR lowering, IR interpreter, Cranelift backend, and LSP are readers.  `Checker` is consumed by `resolve(program) -> ResolvedModule` and dropped.
**Decided:** 2026-04-24
**Target phase:** Phase 2.2 — lands before the phase wraps.  Pure refactor, no semantic change.  Ships ahead of Phase 2.6 so the module-system work can build on a settled sema→IR boundary rather than redesigning it mid-phase.
**Status:** ✅ **Implemented 2026-04-24.**  Sema now returns [`Analysis`](../crates/phoenix-sema/src/resolved.rs) from [`check`](../crates/phoenix-sema/src/checker.rs).  `Analysis` wraps a [`ResolvedModule`](../crates/phoenix-sema/src/resolved.rs) (the IR-facing schema) alongside auxiliary sema outputs that don't participate in the schema:

- **`Analysis.module: ResolvedModule`** — the id-indexed schema of resolved declarations: callables (free functions, user methods, built-in methods), types (structs, enums, traits), and the per-span maps (`expr_types`, `call_type_args`, `var_annotation_types`, `lambda_captures`).  This is what IR lowering, the IR interpreter, and the Cranelift backend consume.
- **`Analysis.{diagnostics, endpoints, symbol_references, trait_impls, type_aliases}`** — auxiliary outputs.  `phoenix-codegen` reads `endpoints`; the LSP reads `symbol_references` and `diagnostics`; the driver/tests read `diagnostics`; `trait_impls` and `type_aliases` are sema-internal but kept around for future tooling.

Stable ids (`FuncId`, `StructId`, `EnumId`, `TraitId`) live in [`phoenix_common::ids`](../crates/phoenix-common/src/ids.rs) and are allocated by [`Checker`](../crates/phoenix-sema/src/checker.rs)'s registration pass.  IR lowering does **not** re-walk the AST to assign or look up ids; it iterates [`ResolvedModule`'s id-indexed tables](../crates/phoenix-sema/src/resolved.rs) directly (see `register_declarations` in [`phoenix_ir::lower_decl`](../crates/phoenix-ir/src/lower_decl.rs)).  Because both sides agree by construction — IR doesn't have a registration walk order to drift from — `IrModule.functions[id.index()]` corresponds 1:1 with either `ResolvedModule.functions[id.index()]` (free function) or `ResolvedModule.user_methods[id.index() - user_method_offset]` (user method).  Synthesized callables (closures, generic specializations) are appended past the user-method range during lowering and monomorphization, and `IrModule.synthesized_start` / `IrModule.is_synthesized(id)` mark the boundary.

The previously-deferred Phase 2.6 follow-up landed in the same diff: user-declared methods now carry their own [`FuncId`]s in [`MethodInfo::func_id`](../crates/phoenix-sema/src/checker.rs) and live in [`ResolvedModule::user_methods`].  Built-in stdlib methods (`Option.unwrap`, `List.push`, `String.length`, …) carry `func_id: None` and live in [`ResolvedModule::builtin_methods`] — they have no IR function (the Cranelift backend inlines each one), so issuing `FuncId`s for them would be wrong.

**The journey of a `function f()` from source to IR.**  Useful as a contributor's first read:

1. Parser produces `Declaration::Function(FunctionDecl { name: "f", … })` in `program.declarations`.
2. Sema pre-pass A walks `program.declarations` once and allocates `FuncId(0..N)` to free functions in source order; pre-pass B does the same for user methods, allocating `FuncId(N..N+M)`.  Pre-pass results live in `Checker::pending_function_ids` / `pending_user_method_ids`.
3. Sema's main checking pass populates `Checker::functions[name] → FunctionInfo { func_id, params, return_type, … }` for each declaration; the `func_id` field adopts the pre-allocated id verbatim.
4. `phoenix_sema::checker::check` consumes the `Checker` (ownership move, then drop) and calls `build_from_checker` to flatten everything into `Analysis { module: ResolvedModule { functions: Vec<FunctionInfo>, user_methods: Vec<MethodInfo>, … }, diagnostics, endpoints, … }`.  The `Vec`s are indexed by id.
5. IR's `lower_decl::register_declarations` iterates `resolved.functions_with_names()` and `resolved.user_methods_with_names()` in id order and creates one `IrFunction` stub per entry at the matching `FuncId` slot.  No AST walk is involved in pass 1 — registration is driven entirely by sema's id tables.
6. IR's `lower_function_bodies` does walk the AST (because that's where the bodies live) and looks up the matching `FuncId` from `function_index` / `method_index` to attach each body to the right stub.

**`Checker` ownership model.**  `Checker` is a mutable, internal accumulator used during checking; it never appears in any consumer's API.  `phoenix_sema::checker::check(program) -> Analysis` constructs a `Checker`, runs the registration + checking passes, calls `build_from_checker(program, checker)` (which moves rather than clones), and drops the `Checker`.  Consumers (codegen, IR lowering, LSP, driver, bench) receive `Analysis` (or just `&Analysis::module`) — never `Checker`.  This is what enables the LSP to keep multiple `Analysis` snapshots in flight without sharing mutable state.

**Sema → IR consumer matrix.**  Who consumes which view of the sema product:

| Crate | Takes | Why |
|---|---|---|
| `phoenix-ir` | `&ResolvedModule` | Schema only — needs callable signatures, types, per-span maps. |
| `phoenix-ir-interp` | `&ResolvedModule` | Same as above; runs IR directly. |
| `phoenix-cranelift` | `&ResolvedModule` | Same as above; emits machine code from IR. |
| `phoenix-codegen` | `&Analysis` | Reads `endpoints` (auxiliary) plus `module.struct_by_name` etc. for body types. |
| `phoenix-lsp` | `&Analysis` | Reads `symbol_references` + `diagnostics` (auxiliary) plus `module` for hover/definition. |
| `phoenix-driver` | `&Analysis` | Routes `diagnostics` to user, threads `&module` to IR lowering. |
| `phoenix-bench` | `&Analysis` | Same shape as the driver. |

**Why two types instead of one.**  An earlier pass collapsed everything onto a single `ResolvedModule`, but that produced a schema-shaped name carrying schema-irrelevant data (semantic diagnostics on a "resolved" module reads contradictorily), forced IR lowering to take a 17-field god-struct when it only needed the schema slice, and made it ambiguous which fields a future addition belonged on.  The split keeps `ResolvedModule`'s contract clean ("the resolved schema; IR consumes this") and gives auxiliary outputs a dedicated home (`Analysis`) so adding new ones (e.g. macro-expansion traces, dependency edges) doesn't widen IR's parameter type.

**Reserved-id zones.**  `EnumId(0)` is built-in `Option`; `EnumId(1)` is built-in `Result`.  User-declared enums start at `EnumId(2)`.  These three positions are exposed as the named constants [`OPTION_ENUM_ID`](../crates/phoenix-common/src/ids.rs), [`RESULT_ENUM_ID`](../crates/phoenix-common/src/ids.rs), and [`FIRST_USER_ENUM_ID`](../crates/phoenix-common/src/ids.rs) — `phoenix_sema::resolved::build_from_checker` `assert_eq!`s the placed position against the constants so a future built-in addition surfaces as a deliberate code change in three places (the constants, the placement loop, and the cross-check).  No other id space (`FuncId`, `StructId`, `TraitId`) has a reserved zone.

**Pass-order invariant.**  `phoenix_sema::checker::Checker::check_program` runs three id-touching passes in strict order: pre-pass A (allocate `FuncId`s for free functions, AST order) → pre-pass B (allocate `FuncId`s for user methods, AST order, captured `user_method_offset = next_func_id` between the two) → registration (each `register_*` consumes the pending id from `pending_function_ids` / `pending_user_method_ids`).  A `debug_assert!` at the start of registration verifies `user_method_offset == pending_function_ids.len()` and that the boundary between A and B was captured correctly — a future refactor that reorders the passes fails loudly here instead of HashMap-index-panicking deep inside `register_function`.

**No placeholder slots in the resolved Vecs.**  `build_from_checker` builds `Vec<Option<FunctionInfo>>` and `Vec<Option<MethodInfo>>` first, populates each slot exactly once during the registration drain (a second write panics — the dedup contract above is the gate for that), and `unwrap`s every slot when collecting into the final `Vec<FunctionInfo>` / `Vec<MethodInfo>`.  An unwritten slot panics with a clear "FuncId(N) was pre-allocated but never registered" diagnostic.  The released `ResolvedModule` therefore has no sentinel values and no unfilled slots — IR lowering can index by id without a defensive check.  IR's own `IrModule.functions` follows a related but distinct pattern: it pre-sizes its function table with `IrFunction(FuncId(u32::MAX))` placeholders and a debug-only assertion at the end of `register_declarations` confirms every slot was written; an unwritten IR slot would indicate a sema↔IR alignment bug, not a sema-internal one.

**Final shape:**

`ResolvedModule` (taken by `phoenix-ir`, `phoenix-ir-interp`, `phoenix-cranelift`):

| Field | Type | Indexed by | Notes |
|---|---|---|---|
| `functions` | `Vec<FunctionInfo>` | `FuncId(0..N)` | Free functions in AST order |
| `function_by_name` | `HashMap<String, FuncId>` | name | O(1) name → id |
| `user_methods` | `Vec<MethodInfo>` | `FuncId(N..N+M) - user_method_offset` | User-declared methods in AST order |
| `user_method_offset` | `u32` | — | `= functions.len()` |
| `method_index` | `HashMap<String, HashMap<String, FuncId>>` | `type → method` | User methods only; nested so accessors borrow `&str` without allocating |
| `builtin_methods` | `HashMap<String, HashMap<String, MethodInfo>>` | `(type, method)` | Stdlib methods; no `FuncId` |
| `structs` / `enums` / `traits` | `Vec<…Info>` | `StructId` / `EnumId` / `TraitId` | `Option`/`Result` lead `enums` |
| `*_by_name` | `HashMap<String, *Id>` | name | O(1) name → id |
| `expr_types` / `call_type_args` / `var_annotation_types` / `lambda_captures` | `HashMap<Span, …>` | span | Pass-through from sema |

`Analysis` (taken by `phoenix-codegen`, `phoenix-lsp`, `phoenix-driver`, `phoenix-bench`):

| Field | Type | Notes |
|---|---|---|
| `module` | `ResolvedModule` | The IR-facing schema |
| `diagnostics` | `Vec<Diagnostic>` | Empty iff valid |
| `endpoints` | `Vec<EndpointInfo>` | `phoenix-codegen` |
| `symbol_references` | `HashMap<Span, SymbolRef>` | LSP |
| `trait_impls` | `HashSet<(String, String)>` | Sema-internal; preserved for tooling |
| `type_aliases` | `HashMap<String, TypeAliasInfo>` | LSP completion / hover |

**Keeping it a pure refactor.** No sema rule changes.  No IR op changes.  No ABI change.  The existing sema / IR / Cranelift test suites are the correctness bar — if any test behavior changes, the refactor regressed something.

**Rationale — why Phase 2.2, not Phase 2.6.** Phase 2.6's module-system work needs a stable post-sema ownership model to build on.  Landing `ResolvedModule` inside Phase 2.6 means two major redesigns land in the same phase (the handoff + modules), and their decisions tangle — visibility checks want to know about `FuncId`-vs-name keying; cross-file imports want to know where `function_by_name` lives.  Landing `ResolvedModule` first, as a refactor-only milestone inside Phase 2.2, gives Phase 2.6 a clean foundation.  The migration is mechanical across ~20 files; the design work is bounded.

**Alternatives considered:**
- **Thread the existing `CheckResult` further, keying by `FuncId` instead of name.** Rejected: keeps two sources of truth (sema's struct + IR's metadata), doesn't solve the LSP strain, forces "sema is a live dependency" to persist indefinitely.
- **Defer to Phase 2.6.** Rejected: tangles with module-system design.
- **Defer to LSP pressure (Phase 3.2).** Rejected: by then, every IR consumer has compounded the name-keyed lookup pattern; refactor cost scales with how many callers we wait to accumulate.

### Centralized layout for reference types

Fat-pointer layouts (e.g., `String` as `(ptr, len)`) and their slot counts were scattered across `phoenix-cranelift/src/types.rs`, `translate/helpers.rs`, and per-method files (`store_fat_value`, `slots_for_type`, etc.). Every new heap-backed type (`Bytes`, `BigInt`, `Date`, future `Vec` / `Buffer`, etc.) required touching each of them, and the invariants lived implicitly in whoever last edited the backend. This is a compiler-internals question, not a user-facing one.

**Decision:** Introduce a single `Layout` trait or registry that owns slot count, alignment, load/store codegen, and calling-convention handling per reference type. Each reference type becomes one entry in the abstraction; the scattered branches in the Cranelift backend become calls into it.
**Decided:** 2026-04-19
**Target phase:** Phase 2.2 (Cranelift native compilation, in flight)
**Status:** Implemented as `TypeLayout` in `phoenix-cranelift/src/translate/layout/`. Adding a new reference type is now a single match-arm edit in `TypeLayout::of`; load/store/sizing are data-driven from there.
**Rationale:** Scattered layout knowledge is not feasible long-term. Phase 4 will add several heap-backed types (`Date`, `Bytes`, regex values, etc.); without a central abstraction, each one touches 3–5 files and silently drifts the invariants. The monomorphization pass (above) and the `dyn Trait` vtable codegen (above) both need to reason about arbitrary-type layouts at codegen time — centralizing first means both passes consume the same abstraction rather than growing their own ad-hoc per-type dispatch.

**Alternatives considered:**
- **Status quo — keep layout scattered.** Rejected: every new heap-backed type compounds the scatter; invariants get easier to break silently; monomorphization and dyn-trait codegen would each need their own ad-hoc per-type dispatch.

### Numeric error semantics (division, overflow, integer edge cases)

Division by zero, integer overflow, and `i64::MIN` negation all terminate the process via `phx_panic` in the Cranelift backend and matching panics in the interpreter. There was no `checked_*` / `wrapping_*` / `Result`-returning variant, and no way for user code to recover from arithmetic errors gracefully. This looks like an implementation detail but is a language-semantics commitment — once users write `let x = a / b` expecting specific behavior, changing it breaks every program that relies on the old behavior.

**Decision:** Integer operators panic on divide-by-zero, overflow, and `i64::MIN` negation (ratifying current behavior). Stdlib provides `Int.checkedDiv`, `Int.checkedAdd`, `Int.checkedSub`, `Int.checkedMul`, `Int.checkedRem`, `Int.checkedNeg`, each returning `Option<Int>`, for user code that needs graceful recovery (validation paths, untrusted input). Floats follow IEEE 754 exactly — overflow produces `±Inf`, invalid operations produce `NaN`, divide-by-zero produces `±Inf` / `NaN` as IEEE 754 prescribes. No panics on Float arithmetic. Float validation uses predicates (`Float.isFinite()`, `Float.isNaN()`, `Float.isInfinite()`), not checked-arithmetic methods.
**Decided:** 2026-04-19
**Target phase:** Phase 2.2 for the semantics commitment (operators already panic; this ratifies it). Phase 4.1 (stdlib core-types expansion) for the `Int.checked*` family and Float predicates — can land incrementally without breaking existing code.
**Rationale:** Panicking operators keep `+` / `-` / `*` / `/` ergonomic in the 95% case where arithmetic is known-safe at write time; the explicit `checked*` methods give user code a real recovery path without paying verbosity cost on every expression. Float overflow is not a "recoverable error" in IEEE 754's model — `Inf` and `NaN` are *defined intermediate values* that numeric code relies on propagating (graphics, statistics, simulations). Forcing checked arithmetic on floats would punish every numeric kernel with early-exit logic that contradicts IEEE 754's whole point.

**Alternatives considered:**
- **Status quo — no checked methods at all.** Rejected: forces preflight checks everywhere untrusted input touches arithmetic; validation paths become awkward.
- **Checked-by-default with opt-in wrapping (Rust's model).** Rejected: Rust's debug-vs-release divergence on overflow is the known foot-gun we want to avoid, and `wrapping_*` / `saturating_*` / `overflowing_*` is a lot of surface area Phoenix doesn't need.
- **Result-returning arithmetic by default.** Rejected: every integer expression becomes `(a / b)?` — unreadable for chained arithmetic, forces the 99% safe case to pay for the 1% unsafe case.
- **Applying Option 4 to floats too.** Rejected: IEEE 754 already defines "what does this mean when it goes wrong" via `Inf` / `NaN`, and users rely on the propagation semantics. Predicates are the right validation tool for floats, not panicking operators.

**Follow-ups (not in scope here):**
- `Float.requireFinite() -> Option<Float>` as a convenience if the predicate pattern proves awkward. Optional; not blocking.

### GC strategy

Compiled Phoenix binaries allocate heap memory via `malloc` but never free it — every string, list, map, closure, struct, and enum variant accumulates for the lifetime of the process. Short-lived CLI programs are fine (the OS reclaims on exit); long-running processes (servers, daemons — Phoenix's primary target per the web-framework positioning) are not. The commitment to add a GC was already made; this question is which flavor.

**Decision:** Tracing GC. Start with a simple, correct mark-and-sweep baseline in Phase 2.3; leave room to evolve to a generational collector later without changing ABI. Do not ship generational in the first cut — keep Phase 2.3 tractable.
**Decided:** 2026-04-19
**Target phase:** Phase 2.3 (already scheduled as the home for GC).
**Rationale:** The deciding factor is **WASM alignment**. Phase 2.4 targets WebAssembly immediately after GC, and WASM GC is shipping in every major browser as of 2024–2025 and will be *the* natural WASM target by the time Phase 2.4 lands. Tracing GC maps onto WASM GC cleanly — same semantics, host VM does the collection, small binaries. RC would force "ship your own RC on linear memory" (heavier binaries, inconsistent with the WASM GC ecosystem) or "use WASM GC but ignore its collector" (defeats the purpose). Phoenix's web-framework positioning means a meaningful amount of user code will run in browsers / on edge runtimes, and a native-vs-WASM semantic mismatch is a real user-facing cost.

**Concurrency reinforces this.** Phase 4.3 (async runtime) is planned; tracing GCs have well-understood concurrent-collection stories, whereas RC under contention needs atomic refcount ops that are 10–100× slower than non-atomic ones. If Phoenix ever goes truly parallel, RC punishes that workload in a way tracing does not. Generational tracing is also the best fit for web request-handling workloads, which allocate many short-lived objects per request ("most objects die young" is exactly what generational is tuned for).

Without WASM and concurrency in the picture this would be a much closer call — RC is simpler to implement, composes with `Drop`-style cleanup, and avoids pause-time tuning. Those are real losses and the section below names them.

**Implied commitment: `defer` (or `using` / `with`) becomes required, not optional.** Tracing GC has no deterministic-destruction story, so `Drop`-style resource cleanup (file handles, database connections, mutex unlocks) is not viable via the GC. Phoenix needs an explicit scope-bound cleanup mechanism before Phase 4.3 lands — probably sooner, once user code starts dealing with file handles. This forces the resolution of the `defer` question (currently Tier 3 / open) and effectively collapses it into "yes, add it; pick syntax later."

**Drawbacks accepted:**
- **Implementation complexity if done well.** A simple mark-and-sweep is tractable, but a *good* tracing GC — generational, concurrent, low-pause — is a multi-year tuning problem. Phase 2.3 will ship the simple version; tuning continues indefinitely.
- **Pause times are an ongoing concern.** RC does not have this class of work. Tracing GC always will.

**Alternatives considered:**
- **Reference counting (with cycle collector).** Rejected despite real wins (simpler implementation, deterministic destruction enabling `Drop`, no pause-time tuning): forces an awkward WASM story (ship own RC on linear memory, or abandon deterministic destruction on WASM), and needs atomic refcounts under concurrency in Phase 4.3.
- **Hybrid (RC + cycle collector, or nursery + tracing old gen).** Rejected: doubles implementation cost for a pre-1.0 language without a correspondingly decisive win. Revisitable later if the simple tracing GC hits real limits.
- **Boehm conservative GC (`bdwgc`).** Considered for the baseline. Rejected: conservative scanning forces the rest of the runtime to live with false-retention (any stack/register word that happens to look like a heap pointer pins the object), forces foreign-code linkage (C library) at the exact moment we want to keep the runtime crate self-contained for future WASM-GC port, and brings none of the benefits of a precise collector that we already have the type information to build (`IrType::is_ref_type()` already classifies every SSA value).

### GC implementation: subordinate decisions

Sub-decisions made under the `### GC strategy` umbrella above, settled at the start of Phase 2.3. Decided 2026-05-04.

#### A. Root-finding: precise via shadow stack

A per-thread linked list of frames; each frame is a small array of root-pointer slots, pushed at function entry, popped at exit, written at every IR op that produces a ref-typed SSA value.

**Why not Cranelift user-stack-maps.** They exist (Cranelift 0.66+) but the API churns and the metadata is finite-lifetime: Phase 2.4 (WASM GC) replaces native root-finding entirely with WASM GC's typed references. Spending real effort on stack-map plumbing for code we'll throw away is a bad trade.

**Why not conservative.** Conservative scanning would force `i64`/heap-pointer disambiguation (NaN-boxing or low-bit tagging across the whole ABI). That's a much bigger change than precise tracking — and Phoenix already knows which `ValueId`s are references via `IrType::is_ref_type()` (`crates/phoenix-ir/src/types.rs`) and `FuncState.type_map` (`crates/phoenix-cranelift/src/translate/mod.rs`). The information is there; we just emit it.

**Cost accepted.** ~10–20% per-call overhead on ref-heavy code (one extra store per ref-typed assignment). Acceptable for a baseline.

#### B. Heap layout: segregated free lists by size class, single arena

**Original target.** Size classes 16/32/64/128/256/512/1024 bytes; large objects allocated individually. Mark-and-sweep returns blocks to the appropriate size class's free list. **No compaction** (compaction needs a moving GC + write barriers, both out of scope per "baseline only").

Bump-only without compaction fragments badly. Free-list with size classes is the standard mark-sweep baseline (Boehm, Go pre-1.5). Predictable, no relocation pressure on our missing write barriers.

**Adopted baseline (2026-05-04).** First cut uses Rust's global allocator + a `HashSet<*mut ObjectHeader>` registry — every allocation is registered, sweep walks the registry and frees unreachable headers via `dealloc`. Segregated free lists deferred to Phase 2.7 as a perf-tuning target; the global-allocator path is correctness-equivalent and avoids reimplementing arena management before we have a perf signal pointing at it.

**Evaluated 2026-05-12 (PR 6 of phase 2.7) — deferred again.** `docs/perf-baselines/allocation.md` shows `phx_gc_alloc` running at ~60–130 ns per call (vs. plain `malloc`'s ~10–30 ns), so the global-allocator + registry path does carry overhead — but no program in either the `compile_and_run` group of `docs/perf-baselines/pipeline.md` or the `bench-corpus` workloads at `docs/perf/phoenix-vs-go.md` has alloc throughput as its dominant cost. `compile_and_run/medium` at 1.23 ms could shed ~15 % wall-clock from a 30 % faster allocator; nothing else benefits more than single-digit %. (Math: a quick `perf record -F 4000` against the compiled binary produced from the `medium` fixture at [`crates/phoenix-bench/benches/fixtures/medium.phx`](../crates/phoenix-bench/benches/fixtures/medium.phx) — the executed-binary phase of the `compile_and_run/medium` bench in [`crates/phoenix-bench/benches/pipeline.rs`](../crates/phoenix-bench/benches/pipeline.rs) — attributes roughly half of wall-clock to `phx_gc_alloc` + GC bookkeeping at the chosen sample size. Small Phoenix programs allocate per-statement for intermediate values, so a 30 % allocator speedup translates to ~30 % × 50 % ≈ 15 % wall-clock. Back-of-envelope only; the `perf` output itself is **not captured in the repo**, and the real lift would need a refreshed `compile_and_run` baseline if size classes ever land. Anyone reopening this entry should rerun `perf record` against that same fixture before relying on the 50 % attribution.) The cross-language gap that actually motivated PR 6's expansion (1900× / 6900× ratios on `sort_ints` / `hash_map_churn`) is dominated by O(n²) immutable-container builds (decision F in [§Phase 2.7 benchmarking](#phase-27-benchmarking)), which the size-class arena does not address.

**Reopens when:** an alloc-throughput-dominated workload lands in the corpus — most likely once Phase 4's HTTP / JSON stdlib pieces ship and a real request-handler workload starts running through the alloc fast path on every request. Until then, the current `phx_gc_alloc` cost is acceptable.

**Out-of-scope changes preserved.** Phase 2.3's 8-spare-codepoint reservation in the object header (decision C below) still holds, so reinstating size classes later doesn't break ABI. The `GcHeap` trait abstraction (decision E below) means the swap-in stays behind a single Rust interface.

#### C. Object header: 8-byte per-object header

```
[ header: u64 ][ payload... ]
  bits  [0]      mark bit
  bits  [1..8]   7-bit type tag (Unknown, String, List, Map, Closure, Struct, Enum, Dyn)
  bits  [8..32]  reserved (forwarding pointer for future moving GC)
  bits  [32..64] payload size or trace-table index
```

The original design split bits 1..4 into a size-class field; that field is unused in the first cut (Decision B's adopted baseline does not maintain size classes), so bits 1..8 are all available to the type tag. Phase 2.7 evaluated the segregated allocator and deferred it (see decision B above); if a future reopen reinstates size classes the tag narrows back to 4 bits, which still fits today's 8 variants with **8 spare codepoints** — no ABI break. Anyone adding to `TypeTag` should treat that 16-variant ceiling as a hard budget; past it, either the size-class plan changes or the header redesigns.

Trace metadata lives in a side table indexed by type-tag for built-ins and by `StructInfo` / `EnumInfo` index for user types — keeps per-allocation IR small. Forwarding-pointer bits are reserved now so a future moving GC doesn't break ABI.

The typed C-ABI allocator is `phx_gc_alloc(size: usize, type_tag: u32) -> *mut u8`; the *Rust-side* `GcHeap` trait dispatches once inside that function.

**2026-05-12 update (PR 6 of phase 2.7).** The earlier untyped `phx_alloc(size) -> *mut u8` shim was retired in this PR — every codegen-emitted allocation now calls `phx_gc_alloc` directly with the appropriate `TypeTag`, and the typed runtime helpers (`phx_list_alloc`, `phx_map_alloc`, `phx_string_alloc`) bottom out there too. Any downstream test linker or FFI consumer that previously resolved `phx_alloc` must now call `phx_gc_alloc` with an explicit tag (`TypeTag::Unknown as u32` reproduces the prior conservative-scan behavior).

#### D. Safepoint placement: at allocation calls only

Single-threaded. GC fires only when `phx_gc_alloc` (and the typed helpers `phx_list_alloc`, `phx_map_alloc`, `phx_string_alloc` that bottom out in it) decide to collect (threshold-based — start at 1 MB allocated since last collection). Loop back-edges and function entry are deferred to Phase 4.3 when concurrency forces preemptive safepoints.

No Phoenix program today can run an allocation-free hot loop (string/list/map ops all allocate), so the "GC starves under hot non-allocating loop" pathology is theoretical until 4.3.

#### E. Allocator abstraction: `GcHeap` Rust trait, single impl in 2.3

`trait GcHeap { fn alloc(&mut self, size, type_tag) -> *mut u8; fn collect(&mut self, roots: &[*mut u8]); }` in `phoenix-runtime/src/gc/mod.rs`. Phase 2.3 ships `MarkSweepHeap` only. Phase 2.4 plugs in a WASM-GC-backed impl behind the same trait — port becomes mechanical instead of a rewrite.

The C-ABI symbol `phx_gc_alloc` does not change shape between impls — trait dispatch happens once, inside that C function.

#### F. Strings join the GC heap; no interning in 2.3

Every `leak_string()` call site in `phoenix-runtime/src/string_methods.rs` and the conversion functions in `lib.rs` switches to GC-allocated strings. The String type-tag's trace function is a no-op (strings hold no refs internally).

Without this, "no leaks under valgrind" cannot pass — strings are the dominant allocator in any non-trivial Phoenix program (every `+`, `toString`, `trim`, `replace`). Interning is a separate optimization, deferred to Phase 2.7+ benchmarks.

#### G. Scope-bound cleanup syntax: Go-style statement-level `defer`

`defer expr;` schedules `expr` to run at end of the enclosing block in reverse (LIFO) order, on every exit path including early `return` and panic. It's a statement, not a block — no new indentation level, no required protocol trait.

**Rejected: `using x = expr { ... }` block-binding (Java/Python/C# style).** Block-binding implies a `Closeable` / `Drop`-style trait that user types must implement. That's a non-trivial language design decision (auto-derive? multiple cleanup methods? interaction with traits?) we don't want to commit to early. `defer` sidesteps the trait commitment by letting the user write the cleanup expression themselves.

**Rationale for shipping plumbing in 2.3 even though no stdlib type uses it yet.** The GC decision *creates* this requirement (no deterministic destruction). Bundling the syntax decision with the GC context now is cheaper than reopening it later — and the IR/codegen plumbing for `defer` interacts with the function-exit emission for `phx_gc_pop_frame`, so it's natural to land them together (defers run *before* `pop_frame`).

#### H. FFI-boundary no-panic policy

Every `extern "C"` symbol in `phoenix-runtime` (panic strategy: `unwind`) must terminate via `runtime_abort` / `process::exit` rather than `panic!` / `assert!` / `expect`. Unwinding across the C ABI is undefined behavior; a panic that originates inside an `extern "C"` runtime helper would unwind through Cranelift-emitted compiled code that never compiled in unwinding support.

This invariant is load-bearing for any helper that brackets work between `phx_gc_push_frame` and `phx_gc_pop_frame`: a panic between the two would (a) trigger the UB above and (b) leak the frame. New helpers that need to push/pop a frame around a multi-step operation must therefore audit every function called between the push and the pop and confirm none of them panic. The current audit list, all routed through `runtime_abort`:

- `phx_gc_alloc` / `phx_string_alloc` — `runtime_abort` on layout failure, `handle_alloc_error` on OOM (which itself aborts).
- `phx_gc_set_root` / `phx_gc_pop_frame` — `runtime_abort` on bad indices / mismatch.
- `phx_gc_shutdown` — `lock_heap()` (`runtime_abort` on poison), `mem::replace`, then `Drop for MarkSweepHeap` which `runtime_abort`s on layout failure during dealloc. No `panic!`/`expect`/`assert!` reachable.
- `to_phx_string_from_str` — wraps `phx_string_alloc`, no panicking branch.
- `phx_list_alloc` — `runtime_abort` on negative inputs and on mul/add overflow.
- `phx_list_push_raw` — `runtime_abort` on length overflow.

The audit list above is intended to stay panic-free in its entirety: there is no "documented as unreachable" carve-out. If a future helper genuinely needs to surface failure to the caller, the response is to switch its declaration to `extern "C-unwind"` (and audit every Cranelift call site) — *not* to silently allow the panic. A `Drop`-guard approach is insufficient because the unwind-across-FFI is the deeper hazard than the leaked frame.

`runtime_abort` itself uses `eprintln!`, which allocates via Rust's global allocator. A nested OOM during the abort path falls through to Rust's panic-in-print handler, which itself aborts — so the FFI-no-panic invariant survives even in the most pathological case. The cost is one indirection past the documented exit; mentioning it here so future readers don't audit `runtime_abort` thinking it leaks an unwind path.

### Diagnostic builder pattern

Diagnostics are currently constructed inline everywhere via `self.error(format!(...), span)` — a message string plus a source span, built in one shot at the call site. This works for basic "X is wrong at line Y" errors but does not compose well with rich diagnostics (secondary spans, notes, suggestions / quick-fixes) of the kind Rust and Elm popularized. Every new rich-diagnostic feature adds either more arguments to `self.error(...)` or a parallel function, and the call sites multiply.

**Decision:** Introduce a fluent builder — `Diagnostic::error(span, msg).with_note(...).with_suggestion(...).with_label(...).emit()` — as the single construction API for all diagnostics. Every existing `self.error(...)` site migrates to the builder. The builder centralizes diagnostic shape so the rendering side (CLI display, LSP, future tooling) has one structure to consume.
**Decided:** 2026-04-19
**Target phase:** Phase 2.6 (module system). The refactor lands as a side-quest during 2.6 — module-system work itself benefits from rich multi-span diagnostics (e.g., "symbol X from module Y is private; it was defined here: [span in the other file]"), so the builder pays for itself during that phase. Must be complete before Phase 3.2 (LSP) begins.
**Rationale:** The real deadline is Phase 3.2 (LSP), not Phase 3.5 (Error Messages). LSP clients already render rich diagnostics (squiggly underlines, hover notes, quick-fix buttons) — those map directly onto the builder's fields. If LSP ships against the current thin diagnostic API, editors display weaker diagnostics than Rust's, and every subsequent enrichment means touching LSP *and* every error site — double the work. Getting the builder in place during Phase 2.6 means LSP connects once, and module-system diagnostics (a natural home for multi-span reporting) become the first consumer of the rich shape.

**Alternatives considered:**
- **Status quo — keep inline construction.** Rejected: every rich-diagnostic feature compounds the scatter; Phase 3.5 would start with "refactor everything first"; LSP ships weak.
- **Struct-populate API** (`Diagnostic { primary: ..., notes: vec![...], ... }.emit()`) instead of fluent builder. Rejected: slightly more verbose at call sites, and conditional-construction advantages don't outweigh the ergonomics of the fluent chain. Fluent matches what most mature compilers ship (Rust's `rustc_errors`, Swift's diagnostic API).
- **Phase 3.1 (package manager) as the landing slot.** Considered, valid alternative. Phase 2.6 preferred because module-system work itself exercises the rich-diagnostic surface; 3.1 would introduce the builder without an immediate consumer.
- **Phase 3.5 (Error Messages) — the doc's original suggestion.** Rejected: too late. Phase 3.2 (LSP) arrives first and would ship against the thin API.

### Interpreter-parser coupling via `Value::Closure` — REVERTED 2026-04-27

**Original premise (2026-04-19):** `Value::Closure` in `phoenix-interp` stores `phoenix_parser::ast::Block` directly. Switching to store IR blocks would let the interpreter walk IR like the Cranelift backend does, decoupling the interpreter from the parser AST.

**Reversal:** This was the wrong framing. `phoenix-interp` is intended to remain a fast AST tree-walking interpreter for debugging (`phoenix run`), kept deliberately separate from `phoenix-ir-interp` (`phoenix run-ir`). A tree-walking interpreter walking AST closure bodies is the correct shape for that role — the "coupling" to the parser AST is by design, not a vestigial defect. Forcing `phoenix-interp` to walk IR for closure bodies would either (a) make it depend on `phoenix-ir-interp` and invert the layering, or (b) duplicate the IR dispatcher — neither of which is justified by the original "decouple the interpreter from parser changes" rationale once we accept that the AST interpreter exists *because* it walks the AST.

**Decision:** Keep `Value::Closure { params, body: ast::Block, captures }` as-is. Treat any future parser-AST changes that affect closures as a normal multi-crate edit, not as a coupling defect. The two interpreters (`phoenix-interp` for AST tree-walking, `phoenix-ir-interp` for IR round-trip verification) are independent by design.

### Module system: discovery, root, `mod.phx`, and entry-point rules

Phase 2.6 introduces multi-file modules. Four interlocking sub-decisions land together because changing any one in isolation would force a redesign of the others. See [phase-2.md §2.6](phases/phase-2.md#26-module-system-and-visibility) for the user-facing surface.

**Decision (1) — Project root.** The project root is `dirname(canonicalize(entry_file))` — i.e., the directory of the `.phx` file passed to `phoenix run` / `phoenix build` / `phoenix run-ir`. No upward walk for a marker file; no `phoenix.toml` requirement. Imports resolve relative to this root.
**Decided:** 2026-04-27
**Why this and not a `phoenix.toml` marker:** The package manager (Phase 3.1) is the right moment to introduce `phoenix.toml`. Adding it now to solve project-root discovery means building marker-file machinery now and re-litigating it in 3.1. When 3.1 ships, `phoenix.toml`'s directory cleanly supersedes this heuristic without breaking anything compiled under the heuristic.

**Decision (2) — Discovery is lazy / import-driven.** Only files reachable from the entry's transitive import graph are parsed. Files in the project tree that are not imported are not parsed and produce no diagnostics.
**Decided:** 2026-04-27
**Why this and not eager scan:** Eager forces every `.phx` under the root (scratch files, archived experiments, dev-only scripts, codegen output) to parse-clean every build. Lazy matches the model used by Rust (`mod foo;`), Go (explicit imports), TypeScript, Python — reduces user surprise from cross-language transfer.
**Trade-off accepted:** A typo in an `import` path silently leaves the misnamed file uncompiled. Mitigated long-term by a future `phoenix check` whole-tree command (no commitment date). For the §2.6 multi-module test matrix, each fixture has an explicit entry point so the gate isn't affected.

**Decision (3) — `mod.phx` is optional.** A directory `models/` containing `models/user.phx` is importable as `models.user` whether or not `models/mod.phx` exists. If `mod.phx` exists, *it* is the `models` module (importable as bare `models`); sibling files remain independently importable as `models.<sibling>`.
**Decided:** 2026-04-27
**Why this and not "required to make a directory a module":** Forcing `mod.phx` for every directory adds bureaucracy without a corresponding semantic gain in a language without Rust-style attribute-on-mod-decl features. Optional lets users opt in to a directory-level module only when they actually have something to put in it.
**Resolution rule:** `import a.b.c` tries `<root>/a/b/c.phx` first, then `<root>/a/b/c/mod.phx`. Both existing is an `AmbiguousModule` error; neither existing is a `MissingModule` error. `mod.phx` is consulted *only at the terminal segment* of the path — intermediate directories are walked through as plain directories. Concretely: `import a.b` does not look at `<root>/a/mod.phx`; that file (if it exists) is the bare `a` module, independently importable as `import a { … }` without colliding with `a.b`.

**Decision (4) — `function main()` only in the entry module.** Phoenix's parser already rejects bare top-level statements (every program is `function main()`-rooted today), so the spec's "top-level statements only in entry file" rule reduces to: non-entry modules may not declare `function main()`. Imported modules may declare functions, types, traits, impls, type aliases, and imports — but not `main`.
**Decided:** 2026-04-27
**Why:** Multiple `main`s across imported modules would be ambiguous about which is the program entry. With per-module name mangling (`module_qualify`), sema's registration would happily accept both `main` (entry) and `<modpath>::main` (non-entry) under distinct keys — so the duplicate-name check no longer catches this on its own. Rejecting `main`-in-non-entry up front in `check_modules_inner` (before any registration runs) is therefore load-bearing, and as a bonus produces a clearer diagnostic — "`main` may only be declared in the entry module" — instead of leaving an executable-only-in-the-IR-stage `<modpath>::main` to confuse downstream passes. The FuncId allocator allocates one id per qualified key, so a stray non-entry `main` would also burn a callable slot if not rejected here.

**Why these four ship together:** Lazy discovery + dirname-as-root means `mod.phx` cannot be the *only* way to mark a module (or the entry directory's `main.phx` siblings would be unreachable without an `entry/mod.phx`); main-only-in-entry keeps the FuncId allocator stable across the resolver's deterministic emit order. Pulling any one decision in isolation would force the others.

**Scope deferred to follow-ups (not part of 2.6):** Explicit `public`/private on `impl` blocks (default for 2.6 = "impls are in scope iff both trait and type are visible"); re-exports (`public import a.b { Item }`); cross-package imports (Phase 3.1).

**Bundled scope:** The [closure capture type ambiguity bug](known-issues.md#closure-capture-type-ambiguity-with-indirect-calls) was originally tied to this refactor on the assumption that capture metadata would land in a unified IR closure representation alongside the AST-to-IR switch. With the reversal, the bug is fixed independently in IR + Cranelift via an env-pointer calling convention (closure functions take their environment pointer as the first arg and unpack captures from the heap object themselves; capture types never cross the indirect-call boundary, structurally eliminating the ambiguity). `phoenix-interp` is not touched by that fix.

### Endpoints are checked in the body-check pass, not the registration pass

`Declaration::Endpoint` arrives at sema's two-pass split (`register_decls` then `check_decl_bodies`) and could in principle be checked from either pass. Today it is checked from the body-check pass — `check_decl_bodies` calls `check_endpoint`, while `register_decls` matches `Endpoint(_)` and does nothing.

**Decided:** 2026-04-29
**Why:** Endpoints reference types and functions through their `body`, `response`, and `params` clauses, so they need name-resolution to have completed. Before Phase 2.6 the registration pass was *type names only* (function and method bodies hadn't been checked yet, but type tables were complete), so endpoint-checking could have run in either pass. After Phase 2.6, Phase B of module-scope construction also runs before registration, which means imports are resolved before any `register_*` call — so technically endpoint-checking could *now* move to `register_decls` without losing anything. We deliberately did not move it: keeping endpoints in the body pass means endpoint type resolution happens after every signature-level lookup the body pass does, with the same scope state. Moving it to the registration pass would be churn for no behavioral change, and would also make it harder to add endpoint diagnostics that depend on body-checked function signatures (e.g., handler return-type compatibility) in the future.

### Per-method `public` / private on inline struct/enum methods

Phase 2.6's module-system spec ([phase-2.md §2.6](phases/phase-2.md#26-module-system-and-visibility)) enumerates `public` rules for structs, struct fields, functions, enums, and traits — but is silent on methods. Today's parser stores `Visibility::Private` unconditionally on every inline method (`crates/phoenix-parser/src/parser.rs:663`, `:750`), and `MethodInfo` (`crates/phoenix-sema/src/checker.rs:178`) has no `visibility` field at all. The de-facto behavior is "methods inherit reachability from the containing type": if the type is public, every method on it is callable from importers; if the type is private, none are reachable (since the receiver cannot be named). This contradicts the spec's already-stated principle that *struct fields have independent visibility* — public types routinely need private helper methods, and the asymmetry between fields and methods has no documented justification.

**Decision:** Methods carry independent visibility, symmetric with fields. Inline methods (in `struct` / `enum` bodies and in `impl` blocks) accept an optional `public` modifier; without it, the method is module-private. Two structural rules apply:

1. **A public method on a private type is a sema error.** The modifier has no meaning when no importer can name the receiver, and accepting it silently teaches a wrong mental model. Reject with a diagnostic suggesting either making the type public or dropping the `public` from the method.
2. **A private method on a public type is allowed and is the encapsulation case.** Internal helpers on an exported type stay module-private even though the type itself is reachable.

Default visibility for methods is private, matching every other declaration form in Phoenix.

**Decided:** 2026-04-28
**Target phase:** Phase 2.6 — lands with the rest of the visibility surface. Cannot be deferred past 2.6 without shipping a half-done visibility model that no later phase can extend without a breaking change to method call-site resolution.

**Rationale:**
- **Symmetric with fields.** phase-2.md:201 already commits to *"A struct can be public while some fields are private."* Methods are the obvious sibling case; the asymmetry is unmotivated.
- **Standard precedent.** Rust, Swift, Kotlin, TypeScript, and C# all let public types have private methods. Cross-language transfer expects this; the current shape would surprise every user.
- **No regression path.** Without per-method visibility, there is no syntax for an internal helper on a public type. Authors are forced to either expose helpers as part of the public API or hoist them to module-private free functions — both are leaks of implementation detail into the API surface.
- **Why error on public-on-private-type rather than no-op:** A `public` modifier with no callable consequence is almost always a mistake (typo, half-finished refactor, copy-paste from a different type). A diagnostic catches the mistake at the point it was made; silently accepting it lets it rot.

**Implementation shape:**
- Parser: accept `public` before `function` inside `struct` / `enum` / `impl` bodies; thread the parsed `Visibility` into `FunctionDecl.visibility` instead of hardcoding `Private`. The two `// Inline method — methods do not carry independent visibility.` comments are removed.
- AST: no new fields — `FunctionDecl` already carries `visibility`.
- Sema: `MethodInfo` gains a `visibility: Visibility` field, populated during registration. `check_register` enforces rule 1 (public method on private type) at registration time so the diagnostic points at the method, not a downstream call site. Cross-module method-call resolution (the new Phase 2.6 visibility check) consults `MethodInfo.visibility` the same way it consults `FieldInfo.visibility` for field access.
- Existing single-module programs are unaffected: every method written today is parsed `private`, every call is intra-module, and intra-module privacy is permissive.

**Alternatives considered:**
- **Status quo (methods piggyback on type visibility).** Rejected: no way to encapsulate internal helpers on a public type; asymmetric with fields without justification; locks in a model that's harder to relax later than to get right now.
- **Public methods of public types only — no per-method modifier.** Rejected for the same reason as status quo: the encapsulation case is the whole point.
- **Allow `public` on methods of private types as a no-op (annotate-now-export-later).** Rejected: silent acceptance of a meaningless modifier is the worst of both worlds — it teaches the wrong mental model and leaves dead annotations that decay as code moves around.

**Interaction with the deferred `impl`-block visibility decision (2.6 follow-up, see above).** Today's deferred rule is *"impls are in scope iff both trait and type are visible."* Per-method visibility is orthogonal to that rule for inherent `impl` blocks (each method is checked independently). For trait `impl` blocks, the trait's method set is part of the trait's contract — a trait `impl` cannot have private methods (the trait already declared them public-by-virtue-of-being-on-a-public-trait). Concretely: `public` is rejected on methods inside a `impl Trait for Type` block (the trait controls visibility); per-method `public` is accepted on methods inside inherent `impl Type` blocks and inline struct/enum bodies. Trait method visibility itself is not in scope here and remains tied to the trait's own visibility.

**Follow-ups (not in scope here):**
- Revisit the deferred *"explicit `public`/private on `impl` blocks"* decision in light of this rule. The natural extension — *"an inherent `impl` block has no visibility of its own; each method's visibility stands alone"* — looks correct, but the decision lives in its own entry once 2.6's surface settles.

### Phase 2.7 benchmarking

Subordinate decisions for the Phase 2.7 benchmark suite. Each pins a contract that bench output (and any decision driven off that output) depends on; settling them before any bench code lands keeps the harness's assumptions reviewable and prevents the first numbers from shipping with implicit policy choices baked in. Phase-level scope and exit criteria live in [phase-2.md §2.7](phases/phase-2.md#27-benchmark-suite).

#### A. Baseline storage strategy: manual snapshot in `docs/perf-baselines/`

**Decided:** 2026-05-04
✅ **Implemented 2026-05-11**: `docs/perf-baselines/{allocation,collections,pause,pipeline}.md` populated; `phoenix-bench-diff update` writes; `phoenix-bench-diff diff` reads.
**Rationale:** committed numbers are visible in the repo and PR diffs catch obvious regressions. Cost is low (markdown table per phase) and the format stays human-readable.

**Format:** per-bench markdown table with columns `bench / parameters / mean / median / stddev / sample-size`. Refreshed at phase close and on intentional perf-affecting changes. Source files reference the baseline path so a maintainer who cuts a regression knows where to look.

**Alternatives considered:**
- *Criterion `--save-baseline` / `--baseline`* — files in `target/criterion/`, never committed; per-CI-host so cross-host comparison is meaningless without normalization. Useful for local before/after on a single machine but not for the cross-PR detection problem we have.
- *External service (bencher.dev or similar)* — durable and comparable but adds a third-party dependency on accounts, API tokens, and out-of-repo state. Revisit if the manual snapshot becomes a maintenance pain.

#### B. CI gating policy: post-merge on `main`

**Decided:** 2026-05-04
✅ **Implemented 2026-05-11**: [`.github/workflows/bench.yml`](../.github/workflows/bench.yml) runs `cargo bench` + `phoenix-bench-diff diff` on `push: main`; opens an issue tagged `bench-regression` when any bench exceeds the 20% threshold. Enforcement gated behind `BENCH_ENFORCE=1` until the noise floor is established (see `docs/perf-baselines/README.md`).
**Rationale:** middle ground between the two extremes. Per-PR gating with N% slack flakes too easily before we know how stable the numbers are. Informational-only is too easy to ignore — regressions can land unnoticed for weeks.

**Original design:** GitHub Actions workflow on `push: main` that runs `cargo bench`, parses criterion output, compares to the committed baseline, and opens an issue if any number regresses by more than 20%. Cross-language Go comparisons (decision E) are explicitly excluded from this CI loop — they run off-CI per phase-close. See the `**Implemented:**` line above for the as-built pointers.

**Alternatives considered:**
- *Informational only* — devs read the trend; no automated alerting. Lowest CI cost (~0 if not run on PR), zero flake risk, but regressions survive too long unnoticed.
- *Per-PR gating with N% slack* — fails CI if the new number is >N% slower than baseline. Catches regressions immediately but flakes when the runner has noisy neighbors. Reopen if post-merge gating ends up being too late.

#### C. Calibration and runner constraints

**Decided:** 2026-05-04
✅ **Implemented 2026-05-11**: recipe applied across the harness. The 5-run minimum is enforced by criterion's default sample size in `crates/phoenix-bench/benches/{allocation,collections,pipeline}.rs` and explicitly by `--runs 5 --warmup 1` in [`bench-corpus/run.sh`](../bench-corpus/run.sh) (decision E). The runner spec is rendered into each baseline file's `README.md`-referenced header and into [`docs/perf/phoenix-vs-go.md`](perf/phoenix-vs-go.md). Single-threaded scope is implicit in 2.7 (no multi-threaded benches exist). CPU-governor pin is **not applied** under WSL2 (no `cpufreq` exposure) or GitHub-hosted runners (shared-tenant VMs); the residual variance is documented per-page (`pause.md`, `phoenix-vs-go.md`) as the cost of decision B's deferred dedicated-runner work.
**Rationale:** pause-time numbers in particular are sensitive to glibc allocator behavior, NUMA, kernel page-fault costs. Without controls the numbers flake; documenting the recipe means a future "the numbers got worse" investigation can rule out runner drift before chasing a real regression.

**Recipe:**
- Pinned CPU governor (`performance`) when the runner permits.
- Minimum 5-run aggregate per bench, criterion default sample size unless variance is unworkable.
- Document the runner spec (CPU model, kernel version, glibc version, criterion version) in the baseline file's header.
- Single-threaded runs only in 2.7. Multi-threaded benches arrive with Phase 4.3 (async runtime) and need separate calibration.

**Fallback if the recipe still flakes in practice:** drop CI gating to informational-only (decision B) until the runner is fixed, rather than tolerate noisy alerts.

#### D. Aggregate choice

**Decided:** 2026-05-04
✅ **Implemented 2026-05-11**: throughput benches in `allocation.rs` / `collections.rs` / `pipeline.rs` report criterion defaults; `gc_pause` group emits P50/P95/P99/max via the JSON sidecar consumed by `phoenix-bench-diff`.
**Rationale:** different bench shapes need different summary stats. Switching aggregates mid-phase makes historical comparisons useless, so pick once and stick.

- **Throughput benchmarks** (allocation, collections, end-to-end compiled binary): mean / median / stddev — criterion's defaults; well-understood and adequate for steady-state work.
- **Pause-time benchmarks** (GC collection latency): P50 / P95 / P99 / max — need the tail to catch worst-case stalls; mean alone hides them.

#### E. Cross-language comparison scope: Go 1.22+ only

**Decided:** 2026-05-04
✅ **Implemented 2026-05-11**: `bench-corpus/` ships the four locked workloads with paired Phoenix and Go programs, a `run.sh` runner gated on hyperfine + Go 1.22+, and a published comparison page at [`docs/perf/phoenix-vs-go.md`](perf/phoenix-vs-go.md). Refresh cadence: per-phase close (decision E).
**Rationale:** Go is the closest comparison Phoenix has — GC'd, compiled, statically typed, web-server-friendly. Adding a second comparator multiplies workload-authoring effort for diminishing positioning value. Pick the one comparison that's most predictive of "would a user choose Phoenix over X" and stop there.

**Locked scope:**
- One comparator: Go 1.22+.
- Four workloads, each with paired Phoenix and Go implementations in `bench-corpus/<name>/{phoenix,go}/`: `sort_ints`, `hash_map_churn`, `alloc_walk_struct`, `fib_recursive`.
- Off-CI runner. Results published to `docs/perf/phoenix-vs-go.md`.
- **Informational only — not a regression gate.** Phoenix-vs-Phoenix numbers (decision B) stay the gating signal. Cross-language numbers are positioning awareness.
- Refresh cadence: per-phase close (2.7, 2.4, 2.5 each refresh once).
- The comparison page must document the "not benchmarked yet" gap (HTTP / JSON / concurrency — Phoenix's actual differentiators per the web-framework pitch — can't be compared until Phase 4 stdlib lands) so readers don't over-extrapolate from compute-only workloads.

**Alternatives considered (and explicitly rejected, so a future contributor doesn't quietly add another):**
- *JVM (Java / Kotlin)* — 25+ years of GC tuning ahead of us. Comparison would be more punishing than informative at this stage; revisit when Phoenix's GC has had real tuning passes.
- *.NET (C#)* — comparable in maturity to Go but less commonly the comp Phoenix users would be coming from. Dropped on positioning grounds, not technical.
- *Rust* — has no GC; the comparison would isolate to compute kernels and miss everything Phoenix's runtime does. Wrong axis for what we want to measure.
- *TypeScript / Node* — closer to Phoenix's UX pitch, but a totally different perf model (JIT) and different problem domain (browser-leaning). Different question, different bench.

**Why this entry is in design-decisions.md and not just phase-2.md.** A future contributor skimming the bench code and adding a Java workload "to be thorough" would trip the foreclosure here. The decision is durable across phases, not just a 2.7 implementation choice.

#### F. Mutable-builder API for `List` / `Map`: explicit types, not implicit linearity

**Decided:** 2026-05-12 (during PR 6 scoping; triggered by `phoenix-vs-go.md` showing 1900× / 6900× ratios on `sort_ints` / `hash_map_churn` against Go, dominated by O(n²) immutable-container build cost).

**Decision:** add `ListBuilder<T>` and `MapBuilder<K, V>` as new builtin generic types. Construction via `List.builder()` / `Map.builder()`; in-place mutation via `.push(v)` / `.set(k, v)`; hand-off to the immutable container via `.freeze()`. Total build cost across `n` mutations + one freeze is O(n) — `n` amortized-O(1) mutations + an O(n) finalize. The freeze step's concrete cost shape is an implementation detail covered in the status block below; the user-visible contract is the O(n) total, which is what unblocks the `sort_ints` / `hash_map_churn` bench cells from their prior O(n²) shape. Both builders are heap-allocated, GC-tracked, and carry their own `TypeTag` for typed-allocator threading.

**Rationale.** Three options were on the table:

1. **Explicit builder types** — the choice above.
2. **Closure-based `List.build(|b| { ... })`** — single builtin taking a closure that receives a transient handle. Cleaner call site, no risk of using a builder after freeze, but every build pays a lambda allocation and codegen for the handle's lifetime is trickier.
3. **Implicit / compiler-detected linearity** — no new API; the compiler proves a binding is the only reference and lowers `xs = xs.push(v)` to in-place mutation. Considered and rejected (see decision G).

Picked (1) over (2) because explicit builder values can be passed across function boundaries (helpers that take a `ListBuilder<T>` and add items), stashed in structs for staged construction, and composed with the rest of the type system. (2) keeps the builder lifetime locked inside one closure, which solves the common case but forecloses any helper-function decomposition.

Picked (1) over (3) for the reasons in **G** below: predictable perf > implicit free-with-caveats; tactical fix for a Phase 2.7 bench gap shouldn't commit the language to a linearity story.

**Surface area:**

```phoenix
let b: ListBuilder<Int> = List.builder()
for i in 0..n {
    b.push(i)              // amortized O(1) (2× grow on overflow)
}
let xs: List<Int> = b.freeze()   // O(n) memcpy; b is unusable after this

let mb: MapBuilder<Int, Int> = Map.builder()
for i in 0..n {
    mb.set(i, i * 7)       // amortized O(1); duplicate keys deduped at freeze
}
let m: Map<Int, Int> = mb.freeze()   // O(n) hash build via phx_map_from_pairs
```

**Use-after-freeze:** runtime-checked at the start of every builder method. The first call after `.freeze()` aborts via `runtime_abort` (FFI-safe — see GC subordinate decision H). Compile-time enforcement (linearity) is decision G's deferred work; the runtime check is the placeholder.

**Why not just keep persistent containers and add Clojure-style transients dynamically?** Transients in Clojure are dynamically-checked (calling a transient method on a non-transient is a runtime error). Phoenix is statically typed, so the static version of the same idea is two distinct types — `List<T>` and `ListBuilder<T>` — which is what's adopted here.

**Forward compatibility with G.** If Phoenix eventually grows ownership / linearity (decision G), the builder types become natural linear values: `.freeze()` is the consumption point. The runtime use-after-freeze check becomes a static error. No API rewrite required.

**Status: ✅ Implemented 2026-05-13** (PR 7 of phase 2.7). `ListBuilder<T>` / `MapBuilder<K, V>` live in `crates/phoenix-runtime/src/{list,map}_builder_methods.rs` with Cranelift codegen in `crates/phoenix-cranelift/src/translate/{list,map}_builder_methods.rs`; sema recognizes `List.builder()` / `Map.builder()` via `check_builtin_static_method` and the new `IrType::{ListBuilder,MapBuilder}Ref` variants thread through monomorphization and the type layout. Native-backend integration tests in `crates/phoenix-bench/tests/fixture_validity.rs` (`list_builder_native`, `map_builder_native`). Tree-walk + IR-interp don't yet evaluate builders — known limitation; programs using builders run under `phoenix build`, not `phoenix run` / `phoenix run-ir`. Bench-corpus `sort_ints` + `hash_map_churn` rewritten to use the builders; the published `docs/perf/phoenix-vs-go.md` ratios fell from 1900× → 5.4× and 6979× → 3.6×. Freeze is **O(n) memcpy** (not the O(1) pointer-swap the original decision text described); the build-phase win comes from `.push()` / `.set()` being O(1) amortized.

#### G. Linearity / ownership types: deferred to Phase 4+

**Decided:** 2026-05-12 (paired with decision F).

**Decision:** Phoenix does *not* adopt a linearity, affine-ownership, or uniqueness-types story in Phase 2. The question is reopened at Phase 4 (stdlib) or later, when there's enough perf-and-API data to evaluate it against alternatives.

**Rationale.** Linearity would solve more than just the bench-published O(n²) container-build problem — it would also cover deterministic resource cleanup (files, sockets, locks), concurrency safety (Phase 4.3), and pipeline-style intermediate-allocation elimination (`xs.map(f).filter(g)`). Real wins. But it's a substantial language-design decision with cascading consequences, and doing it as a tactical fix for one bench gap is the wrong forcing function.

Concretely, adopting linearity now would require resolving:

- **Closure capture semantics.** A closure capturing a linear value must itself be linear; what does that mean for `function(x: Linear<T>) -> ...`? How does it interact with Phoenix's current first-class-closure story?
- **Generic / polymorphism interaction.** Generic code over "linear or not" needs multiplicity polymorphism (Haskell's solution) or a `Copy`-style opt-in marker (Rust's). Either way, more surface in the type system.
- **GC × linearity boundary.** When does the collector scan into a linear value's interior? Does freezing change its trace treatment? Phoenix's tracing GC is single-generation today; linear values that hold GC refs are a real design question.
- **Defer / exception interaction.** `defer`-scheduled blocks running on early exit need to know whether a linear value is consumed or still owned. Pattern match arms that take different code paths similarly.
- **Existing stdlib rewrite.** Every function that takes `List<T>` becomes a decision: borrow, consume, require a fresh allocation? Existing Phoenix code may compile against the borrow signature, the consume signature, neither, or only with explicit `.clone()` insertions.

Each of these is a quarter-to-half of design + implementation work. Total ~1–2 quarters of senior engineering before any user-facing benefit lands. Not a Phase 2.7 scope.

**What's decided now:**

- Phase 2.7 ships **decision F** (explicit builders) as the tactical fix for the bench-published gap.
- Phase 3 and Phase 4 should accumulate observations about *where else* the immutability-without-linearity cost surfaces (pipeline-style code, struct updates, append-heavy patterns in web handlers) so the eventual decision is informed by the full picture, not just one bench workload.
- A "linearity / ownership" design exploration is the natural opening of Phase 4 (stdlib pass) or a dedicated Phase between 3 and 4. The exploration should evaluate at least: full linear (Clean / Linear Haskell), affine + borrow (Rust), uniqueness annotations as optimizer hints only (Clean's softer mode), and explicit-only solutions (status quo + decision F).

**Alternatives considered (and the reason for not picking them now):**

- *Full linearity now (Rust-style).* Wrong phase fit — too much language design for too narrow a Phase 2.7 motivation. The "fight the borrow checker" UX would also undermine Phoenix's "easy to use" pitch unless carefully scoped.
- *Uniqueness annotations as optimizer hints only.* Clean's softer model — user writes `unique List<T>`, compiler exploits it for in-place mutation but doesn't otherwise change the type system. Real candidate for Phase 4+. Not picked now because the implementation work is similar to full linearity and the same "do this well or not at all" applies.
- *Escape analysis in the compiler.* Compiler-only, no user-facing type-system change. Limited reach (only catches the proof-easy cases) but worth keeping on the table for Phase 3 (optimizer). Doesn't subsume decision F because users want explicit control over the builder pattern, not "hope the analysis fires."
- *Swift-style runtime COW (refcount + isUniquelyReferenced check).* See the "Design space explored" addendum below — added 2026-05-13 after the option was explored. Genuinely strong candidate for Phoenix's pitch; tracked there so the alternatives list stays scannable.

**Why this entry is in design-decisions.md and not just phase-2.md.** A future contributor proposing "let's just add linearity to fix problem X" needs to find the prior deliberation. Same logic as decision E: the decision is durable across phases, not just a 2.7 implementation choice.

##### Design space explored (2026-05-13 follow-up)

After Phase 2.7 closed, additional thought was put into the design space. Pinning the additional context here so the Phase-4+ reopen doesn't relitigate the same paths from scratch.

**Terminology — three closely-related framings of "no aliasing."**

- **Linear types** (Linear Haskell `-XLinearTypes`, Idris 2 quantitative types): a value must be used **exactly once**. Use-count constraint; no duplication, no silent drop.
- **Affine types** (Rust ownership): a value is used **at most once**. Drops allowed, duplication isn't. Rust's `move` is affine.
- **Uniqueness types** (Clean): the value is **the only reference** at this moment. Aliasing constraint, looked at from the other direction.

All three encode the same invariant from different angles; all three license the same in-place-mutation optimization. The terminological distinction matters less than the choice of *enforcement mechanism* (type system vs runtime check vs inference).

**What linearity would buy Phoenix — three pillars, not just mutation perf.**

1. **In-place mutation without aliasing risk.** The decision-F motivation. Generalizes beyond `xs.push(v)` loops — record updates, pipeline-style `.map(f).filter(g)` intermediate materialization, append-heavy web handlers all benefit.
2. **Deterministic resource cleanup.** If a value must be consumed exactly once and the "consume" operation is `.close()` on a file / socket / lock, the type system enforces "close exactly once" as a static property. Phoenix's `defer` does this dynamically (best-effort; doesn't fire on hardware traps — see `docs/known-issues.md`). Linearity makes resource discipline a *type-system property* instead of a discipline.
3. **Concurrency safety.** No aliases → safe to move across threads → no data races. Rust's `Send` / `Sync` traits are the canonical form. For Phase 4.3 async runtime this is load-bearing: request handlers owning their state are trivially parallelizable.

The Phase-4+ evaluation should weight all three, not just (1). The bench gap that produced decisions F and G is one piece of evidence; resource discipline and concurrency safety are the others. Reopening on (1) alone misses the point.

**Cost of linearity (what makes the "do well or not at all" framing real).** Every linear value's "exactly-once" constraint propagates through every API. A function taking a linear value consumes it; passing through a logger or assertion is a type error unless borrowing is introduced. Closures capturing linear values are themselves linear. Generics need *multiplicity polymorphism* (Linear Haskell) or a `Copy`-style opt-in marker (Rust). Rust's reputation for difficulty comes mostly from making this propagation visible everywhere.

**Fourth alternative: Swift-style runtime COW (copy-on-write with `isUniquelyReferenced`).** Not in the original decision-G alternatives list; surfaced 2026-05-13 and worth tracking explicitly.

- Every container carries a refcount. Every mutation runs `isKnownUniquelyReferenced(&self)` — a one-cycle branch. If refcount == 1, mutate in place; if > 1, copy first, then mutate the copy.
- No compile-time analysis. No user-facing annotations. No type-system burden. Works across function boundaries trivially because the refcount comes with the object, not the binding.
- Cost: a refcount field on each container + ~1 ns per mutation for the branch, paid whether or not the value is actually shared. Failure mode is observable ("shared instance triggered copy" is profileable), not user-confusing.
- Implementation cost in Phoenix is non-trivial because we already have a tracing GC. Refcounting on top means either (a) per-type refcount fields just for `List` / `Map` / future builders, or (b) full hybrid refcounted + cycle-detecting collector. (a) is the cheaper path — only add refcounts to types where the perf delta matters.
- Trade-off vs decision F's explicit builders: COW gives the perf win to *all* user code (not just code that reaches for the builder), at the cost of an always-on per-mutation branch. Decision F's builders are zero-cost where used, zero-coverage where the user doesn't reach for them.

**Inference vs explicit annotation tradeoffs.** Pure compile-time inference of uniqueness (Go-style escape analysis pushed further) was discussed and has known limits:

- **Local inference** within one function is cheap and reliable. Doesn't help for cross-function flows.
- **Closed-world whole-program inference** (MLton-style) is tractable in principle — call graph + fixed-point iteration over per-function uniqueness facts — but with caveats:
  - Hard cases: function values / closures (call site doesn't statically know which body executes), `dyn Trait` dispatch (receiver opaque), generics with multi-call-site dispatch (per-uniqueness-flavor monomorphization or conservative fallback).
  - **The hostile cases land exactly on Phoenix's pitched patterns** — `dyn Handler` / `dyn Middleware` trait-object pipelines are the design surface Phoenix encourages, and they're the surface inference can't see through.
  - Compile-time cost is super-linear in worst case. MLton compile times (tens of seconds for medium programs) are the reference; not blocking for batch builds, bad for hot-reload dev iteration.
  - Incremental compilation gets harder: per-function changes invalidate transitive caller uniqueness facts.
  - **The silent-failure problem doesn't go away — it moves.** Instead of "adding a `log()` call broke local inference," you get "adding a `dyn Handler` somewhere in the request pipeline silently broke uniqueness inference for body-building code three modules over." The blast radius shifts; the failure shape (perf collapse with no error and no visible code diff) stays.
- **Techniques to reduce inference cost** — known, not free:
  - *Modular summary analysis* (each function emits a boundary summary; callers use the summary). Conservative but composes well. State of the art is well-understood.
  - *Datalog-based incremental analysis* (Soufflé, Doop). Best incremental story; substantial infrastructure cost (a Datalog engine in the compiler).
  - *Fine-grained dependency tracking* (Rust borrow-check, GHC type inference style). Works but the dep graph through call sites is dense.
  - *Profile-guided / tiered* (LLVM LTO + PGO style). Skip inference at iteration time; run at release-build time. Trade: dev iteration fast, release builds slow, debug/release behavioral drift can hide bugs.

**Synthesis worth exploring at the Phase-4+ reopen: tiered dev-COW / release-inference.** Combines runtime COW and whole-program inference into a tiered design.

- **Dev build:** No uniqueness inference. Swift-style runtime COW handles correctness; mutations pay the ~1 ns refcount-check overhead. Compile time stays fast (no whole-program analysis). Perf is predictable — "good enough for hot-reload."
- **Release build:** Run whole-program uniqueness inference. Where it proves uniqueness statically, the refcount check is removed and mutation lowers to in-place. Where it can't prove (dyn-trait dispatch, higher-order calls), the COW fallback stays. Analysis cost is paid only when shipping.

Gives:
- Fast dev iteration (no whole-program analysis).
- Predictable perf in dev (COW handles it; no silent slowdowns).
- Excellent perf in release (inference reduces COW overhead where provable; the hostile-to-inference patterns fall back to COW, which still gets the answer right with a small constant cost).
- No type-system burden on users (no `unique` annotations, no `&mut`).

Costs:
- Two compilation paths (release-mode bugs may not reproduce in dev — known LLVM-style hazard).
- Runtime-COW infrastructure has to exist regardless.
- Whole-program-inference infrastructure has to exist for release builds (with the incremental-build cost issues above).

**For the Phase-4+ reopen, the three positions worth distinguishing are:**

1. **Full Rust-style affine ownership + borrow checker.** Maximum guarantees, maximum learning curve. Wrong for Phoenix's "easy to use" pitch unless carefully scoped.
2. **Hybrid: conservative whole-program inference where it's cheap + explicit annotations on hostile boundaries** (dyn-trait, pipeline closures, generics). The "Rust but with inference doing more of the work" position.
3. **Tiered dev-COW / release-inference** (the synthesis above). Zero user-facing complexity; the trade-off is implementation infrastructure cost (refcount layer + whole-program-inference pipeline).

Phoenix's "easy to use, web-framework-friendly GC'd lang" pitch argues hardest for (3). The bench gap that triggered F and G is real but it's one piece of evidence; resource discipline (Phase 4 stdlib opens files / sockets / DB connections) and concurrency safety (Phase 4.3 async) are the other two pillars to weight in the reopen.

**Where this lands as forward guidance.** None of this changes decision G's deferral — Phase 2.7 still ships decision F as the tactical fix. The Phase-4+ reopen should:

1. Survey real-world Phoenix code patterns to see which of the three pillars (mutation perf, resource discipline, concurrency safety) is the dominant pain point. The bench corpus weights (1); HTTP / JSON / async stdlib will reveal whether (2) and (3) are too.
2. Evaluate runtime COW seriously alongside the type-system approaches. The decision-F builders + Swift-style COW + targeted inference is a credible "no annotations" path that decision G's original framing didn't fully explore.
3. Not commit to whole-program inference as a standalone solution. The silent-failure shape is hostile to "easy to use" — either it's paired with a runtime safety net (COW) or the user-visible failure mode needs a different story than "your code got slow, no error, no diff."

### Phase 2.4 WebAssembly compilation

Subordinate decisions for the Phase 2.4 WASM backend. Each pins a scope contract before any code lands so the bench refresh, the matrix expansion, and the host-import shape don't drift mid-phase. Phase-level scope summary and exit criteria live in [phase-2.md §2.4](phases/phase-2.md#24-webassembly-target) (matching the location pattern used by §2.2 / §2.3 / §2.6 / §2.7).

#### A0. WASM emission tool: `wasm-encoder`, not Cranelift

**Decided:** 2026-05-15 (pivot from the original PR 2 framing, discovered before any wasm code landed).

**Context: how the decision was reached.** Before this decision, both [phase-2.md §2.4](phases/phase-2.md#24-webassembly-target) and the [GC strategy decision](#gc-strategy) referred to "Cranelift's `wasm32` support" / "Cranelift's built-in WASM support" as the planned emission tool. That framing was incorrect — it conflated Cranelift's role *consuming* WebAssembly (which it does, as the JIT backend for Wasmtime) with a role *emitting* WebAssembly (which it has never done and has no plans to do). The error sat in the planning docs from Phase 2.0 forward without being challenged until Phase 2.4 PR 2 implementation began. Recording the path so a future reader sees how the decision was actually reached, not just the outcome.

**Technical reason Cranelift cannot emit WebAssembly.** Cranelift's [`TargetIsa`](https://docs.rs/cranelift-codegen/latest/cranelift_codegen/isa/trait.TargetIsa.html) abstraction models a *machine architecture*: x86_64, aarch64, riscv64, s390x — register-based hardware where instruction selection lowers Cranelift IR (CLIF) to instructions over a general-purpose register file with an ABI-defined calling convention. WebAssembly is structurally a different kind of target:

- **No registers, just locals + an operand stack.** CLIF assumes a register allocator (`regalloc2`); WASM has typed indexed locals and an implicit value stack. Lowering CLIF SSA values to "operand stack vs local" is a separate scheduling problem ([stackification](https://kripken.github.io/talks/relooper.html)), unrelated to register allocation.
- **Structured control flow, not goto-CFG.** CLIF is a basic-block CFG with unrestricted branches; WASM has `block` / `loop` / `if` with branch-out-to-enclosing-label and **no general `goto`**. Going CLIF → WASM requires reconstructing structured control flow from an arbitrary CFG, which is its own algorithm (relooper / Stackifier). Irreducible CFGs (e.g. loops with multiple entry points) need a dispatch-table workaround.
- **Typed validation at every instruction.** WASM's binary format requires the operand-stack types match a declared signature at every point; CLIF doesn't carry validation in the same shape.

Cranelift's `cranelift-wasm` crate exists, but it goes the *other direction*: it parses a `.wasm` module and lowers its structured control flow *into* CLIF so the downstream native backends (x86, aarch64, …) can compile it. There is no `wasm32` `TargetIsa` in mainline Cranelift, no public proposal to add one, and no signal from the Bytecode Alliance that adding one is on any roadmap.

**Alternatives considered.** Recorded fully so a future contributor proposing "let's use X instead" finds the prior comparison rather than re-deriving it.

| Option | What it is | Pros | Cons | Verdict |
|---|---|---|---|---|
| **A. `wasm-encoder`** (chosen) | Bytecode Alliance's low-level binary writer. Version-locked with `wasmparser` (validator) and `wasmprinter` (disassembler) alongside Wasmtime. | Standard. Active. Pure Rust. WASM GC opcodes since 0.200+ — same tool serves PR 5's wasm32-gc variant. Wasmtime's own test suite pairs `wasm-encoder` emission with `wasmparser` validation on the canonical consumer side, so the encoder's output stays correct against the validator it ships next to. | We write the Phoenix IR → WASM translator ourselves (no reuse from `translate/`'s Cranelift IR work). Bytes-level API: we own LEB128 / section / index bookkeeping. | **Chosen.** |
| **B. `walrus`** | Higher-level WASM module builder/editor. Used historically by `wasm-bindgen`. | Slightly more ergonomic builder API (named locals, automatic ID handling). | Designed for *editing* existing modules more than emit-from-foreign-IR. Less actively maintained than `wasm-encoder`; rustwasm-org focus has shifted. WASM GC support lags. | Rejected: trades active maintenance + WASM-GC-readiness for marginal API ergonomics. |
| **C. `binaryen` Rust bindings** | Bindings to Binaryen (the C++ optimizer Emscripten uses). | Battle-tested. Includes a peephole optimizer. Used by AssemblyScript. | C++ dep behind FFI — breaks Phoenix's pure-Rust posture. Build-time pain across CI runners. WASM GC support lags upstream proposals. | Rejected: dep cost too high for the benefit Phoenix gets at its current scale. |
| **D. Hand-rolled WASM binary emitter** | Implement our own LEB128 + section encoder inside `phoenix-cranelift`. | Zero new deps. Maximum control. | Reinventing `wasm-encoder` worse. WASM binary format has enough fiddly bits (LEB128 widths, type-index/func-index dance, code-section size prefixes, validation rules) that a hand roll is real work for negative differentiation. | Rejected: no upside vs A. |
| **E. Emit C source, shell out to `clang --target=wasm32-wasi`** | Phoenix IR → C source → external `clang` invocation produces `.wasm`. | Leverages LLVM's industrial-strength wasm32 backend. Mature WASI handling. The smallest amount of *Phoenix* code (we'd just write a C emitter). | Massive toolchain dep — clang on every dev machine and every CI runner. Multi-step pipeline is fragile. We lose direct control over WASM ops — PR 5's WASM GC variant becomes painful because the C ABI lowers to linear-memory by default and clang doesn't expose WASM GC types as a C surface. Slow build times. | Rejected: tooling weight + PR-5 incompatibility outweigh "less Phoenix code." Honest second place. |
| **F. Switch native backend from Cranelift to LLVM** | Drop `cranelift-*`, use LLVM for both native and wasm32. | One toolchain covers both targets. LLVM's wasm32 backend is mature. | Enormous architectural change. LLVM dep is much heavier than Cranelift (build time, distribution size, complexity). The [Phase 2.2 framing](phases/phase-2.md#22-native-compilation-cranelift) explicitly chose Cranelift over LLVM on the "pure Rust dep, fast compile times" axis. Most of the ~6k LOC under [`phoenix-cranelift/src/translate/`](../crates/phoenix-cranelift/src/translate/) would be rewritten. | Rejected: out of proportion to the problem; abandons Cranelift investment. |
| **G. Defer Phase 2.4 until Cranelift adds wasm32** | Wait. | No code changes today. | Vaporware. No upstream signal that wasm32 emission is being added. Indefinite block on the rest of Phase 2.4. | Rejected: blocks the phase on something with no timeline. |
| **H. Implement a wasm32 `TargetIsa` for Cranelift ourselves** | Add an emission backend to Cranelift upstream (or as a side project). | Reuses Cranelift IR → wasm32 across all Cranelift consumers, not just Phoenix. | Research-grade project (CFG → structured control flow is its own algorithm). Months-to-years of work outside Phoenix scope. Doesn't help PR 5's WASM GC variant (managed refs don't map to CLIF). | Rejected for Phoenix. See [`design-decisions-appendix-a0-cranelift-wasm32-feasibility.md`](design-decisions-appendix-a0-cranelift-wasm32-feasibility.md) for the detailed scope analysis. |

**Versioning note.** `wasm-encoder` and `wasmparser` are pinned together at the same version because their codepoints (instruction encodings, type representations) are kept in lockstep by the Bytecode Alliance. Bumping one without the other is a known-bad pattern; the workspace `[dependencies]` block keeps them paired (currently both at `0.248`).

**What stays the same after the pivot:**
- The native (Cranelift) path is untouched. `Target::Native` still goes through `cranelift-object` + system linker.
- The `phoenix_cranelift::compile(&IrModule, Target)` surface stays the same; `Target::Wasm32Linear` / `Target::Wasm32Gc` now route to the wasm-encoder pipeline.
- Phoenix's `IrModule` is the single source of truth fed to either backend. Sema / lowering / verifier are codegen-neutral.

**What changes:**
- The `phoenix-cranelift` crate now contains a non-Cranelift backend. The crate name is mildly misleading but not load-bearing — a rename is a downstream cleanup tracked separately. The crate's package description is updated to spell out the dual-backend reality.
- PR 2's size estimate from the original plan ("Cranelift's wasm32 backend has rough edges; budget for shakedown") was wrong about the cause but right about the size — `wasm-encoder` is well-trodden, but Phoenix needs its own IR → WASM translator (a parallel of `phoenix-cranelift/src/translate/` rather than a thin wrapper around Cranelift), and that translator is the bulk of PR 2.
- The "verify via grep on emitted WAT" exit-criteria item (B in §Phase 2.4 below) was already factored to use `wasm-tools print` for disassembly; the upstream `wasm-encoder` emission doesn't change that step.

#### A. Dual backends in this phase: WASM GC primary, linear-memory fallback

**Decided:** 2026-05-15
**Rationale:** the [GC strategy decision](#gc-strategy) named WASM GC as the deciding factor for picking tracing GC, and [decision E (`GcHeap` trait abstraction)](#e-allocator-abstraction-gcheap-rust-trait-single-impl-in-23) was deliberately built single-impl in 2.3 so 2.4 could plug a second impl in behind it without touching the trait shape. Both backends ship together because (1) [phase-2.md §2.4](phases/phase-2.md#24-webassembly-target) explicitly frames linear-memory as the fallback, (2) running both verifies the abstraction holds, and (3) the runtimes-without-WASM-GC fallback path is small once the wasm32 codegen exists — most of the work overlaps.

**WASM GC variant:**
- WASM-managed struct/array refs back the heap; shadow-stack emission ([decision A](#a-root-finding-precise-via-shadow-stack)) is *replaced* by WASM GC's typed references, not ported. The "Phase 2.4 (WASM GC) replaces native root-finding entirely" clause in decision A is the explicit anticipation of this work.
- New `WasmGcHeap` impl of `GcHeap`. `alloc` returns an opaque managed ref; `collect` is a no-op (host VM handles it).
- Per-`TypeTag` mapping declares one WASM struct/array type at module emit time. Phoenix `StructRef(name)` → one WASM struct per Phoenix struct. The PR 5 design entry (added when it lands) pins the per-`IrType` → WASM type mapping.

**Linear-memory variant:**
- Existing `MarkSweepHeap` + shadow stack ports onto wasm32 with a no-std-compatible global allocator (recommend `dlmalloc`; `wee_alloc` is unmaintained).
- Shadow stack survives as today's linked-list-on-heap; the per-thread TLS counter degrades to a single static under single-threaded wasm32.
- `phx_gc_shutdown` runs on `proc_exit` so the leak-clean contract from the §2.3 valgrind gate stays valid for the WASM port (no valgrind equivalent under wasmtime, but registry-empty-at-exit can be asserted via a runtime hook).

**Alternatives considered:**
- *WASM GC only, defer linear-memory.* Rejected: phase-2.md §2.4 explicitly commits to "linear-memory WASM remains a fallback option for runtimes without WASM GC support." Shipping only WASM GC retracts that commitment without a new decision.
- *Linear-memory only, defer WASM GC.* Rejected: contradicts the [GC strategy decision](#gc-strategy) framing that picked tracing GC *for WASM alignment*. Doing wasm32 without WASM GC strands the alignment benefit one phase further out.

#### B. Exit-criteria runtime: wasmtime CLI

**Decided:** 2026-05-15
**Rationale:** wasmtime is the simplest CI integration (subprocess, same shape as `phoenix build && ./out`) and supports WASM GC as of 18.0 (Jan 2024). The four-and-then-five-backend matrix becomes one more column on `phoenix-driver/tests/backend_matrix.rs`. Browser execution is real positioning value but slots into Phase 2.5 (JS interop), not 2.4 — getting WASM modules to run on *something* is the 2.4 gate.

- Linux CI gate: `phoenix build --target wasm32-{linear,gc} <file>` then `wasmtime <out.wasm>` and assert stdout byte-equality with native.
- Skip-with-warning pattern mirrors the §2.3 valgrind gate: `PHOENIX_REQUIRE_WASMTIME=1` turns the skip into a hard failure so a misconfigured CI runner can't silently bypass the gate.
- Browser execution is *not* gated in 2.4. Phase 2.5 (JS interop) is the natural slot.

**Alternatives considered:**
- *Node.js with V8 WASM GC.* Rejected for 2.4: needs Node ≥21 and `--experimental-wasm-gc`; more browser-like but the CI dependency is awkward. Reopens in Phase 2.5 when JS interop work justifies the Node footprint.
- *Both wasmtime CI + browser docs.* Considered. Documenting the browser execution path is fine, but adding a browser test rig to the 2.4 matrix doubles CI complexity for no exit-criteria signal that wasmtime doesn't already give.

#### C. Host-import surface: WASI preview1 only

**Decided:** 2026-05-15
**Rationale:** `fd_write` to stdout and `proc_exit` for panic/exit are the *only* host imports a 2.4 compiled WASM module needs. Phoenix-defined custom imports (e.g. `phoenix.print_i64`) are the natural Phase 2.5 territory — once JS interop is on the table, a richer import surface lets browsers / Node hosts skip WASI shims. Locking 2.4 to standard WASI keeps the import schema portable across every WASI-aware runtime (wasmtime, Node `@bjorn3/browser_wasi_shim`, browser polyfills) for free.

- `phx_print_i64` / `phx_print_f64` / `phx_print_bool` / `phx_print_str` format the value into a stack buffer and route through `wasi_snapshot_preview1.fd_write` on fd 1.
- `phx_panic` writes the message via `fd_write` on fd 2 then calls `wasi_snapshot_preview1.proc_exit(1)`.
- Phoenix-defined imports are out of scope. The Phase 2.5 design block opens with WASI as the established floor and adds richer imports on top.

**Alternatives considered:**
- *Phoenix-defined custom imports (no WASI).* Rejected for 2.4: smaller binaries are real, but every host has to implement Phoenix's import schema before any WASM module runs. WASI shifts that cost to existing tooling.
- *WASI + Phoenix-specific extras (hybrid).* Considered. The "extras" set is empty for 2.4 (timing for benches sits naturally inside the bench harness, not the runtime). Reopens whenever Phase 2.5's JS interop work pulls a real Phoenix-specific import in.

#### D. Phase-close bench refresh scope: WASM vs native Phoenix only

**Decided:** 2026-05-15
**Rationale:** Phase 2.4 is structural (codegen + runtime abstraction), not perf-focused. The actionable datum from the close-refresh is "how much slower is `wasm32-{linear,gc}` than `native`?" — that signal drives every Phase 2.5 / Phase 4 decision about whether browser-served Phoenix workloads need a different optimization story than native. Comparing Go-as-WASM via tinygo would add a column readers misread (tinygo is not Go — different GC, different reflection, different syscall surface; a "Phoenix is slower than Go in WASM" comparison would be misleading).

- New `docs/perf/phoenix-wasm-vs-native.md` (or new columns in [`docs/perf/phoenix-vs-go.md`](perf/phoenix-vs-go.md) — file layout to be settled in PR 7) reports the four locked corpus workloads (`sort_ints`, `hash_map_churn`, `alloc_walk_struct`, `fib_recursive`) across `native` / `wasm32-linear` / `wasm32-gc`.
- Refresh cadence aligns with the [Phase 2.7 decision E (cross-language comparison)](#e-cross-language-comparison-scope-go-122-only) per-phase-close commitment — 2.4 close refreshes once.
- Adding tinygo or Go-as-WASM to the bench harness is explicitly out of scope. Future contributors proposing it should re-litigate this decision here.

**Alternatives considered:**
- *Full matrix (native Phoenix + WASM Phoenix + native Go + Go-via-tinygo-WASM).* Rejected: tinygo's semantic divergence from canonical Go would make the comparison misleading without a long footnote that readers skip.
- *Skip the refresh this phase.* Rejected: contradicts decision E's per-phase-close commitment. WASM-vs-native is the actionable cell; we have to publish it.

#### E. Target triple: `wasm32-wasip1`

**Decided:** 2026-05-15 (during PR 3 scope review).

**Rationale:** `wasm32-wasip1` (the renamed `wasm32-wasi`, WASI preview 1) gives us the full Rust standard library, a default global allocator (wasi-libc-provided), and working stdio routed through WASI for free. The existing `phoenix-runtime` Rust source compiles essentially unchanged. The target also matches our host-import surface ([decision C](#c-host-import-surface-wasi-preview1-only)), so we don't ship a runtime built for one ABI and emit user code expecting another.

- `phoenix build --target wasm32-linear` compiles user-emitted modules and the runtime against `wasm32-wasip1`.
- `phoenix-runtime` builds via `cargo build -p phoenix-runtime --target wasm32-wasip1 --release` once per environment; phoenix-cranelift discovers the artifact at codegen time ([decision F](#f-runtime-delivery-embed-and-merge)).
- `std` features used by `phoenix-runtime` (`HashSet`, `Mutex`, `OnceLock`, `thread_local!`, `eprintln!`, `process::exit`) all work on `wasm32-wasip1` without feature gates.

**Alternatives considered:**
- *`wasm32-unknown-unknown`.* Bare-metal target. No std, no allocator, no I/O. We'd hand-roll `dlmalloc` integration, feature-gate every `std::*` use in `phoenix-runtime`, and route panic/print through a Phoenix-defined extern surface. Rejected: nothing's asking for this target, and it'd retract the WASI preview1 commitment in decision C.

#### F. Runtime delivery: embed-and-merge

**Decided:** 2026-05-15 (during PR 3 scope review).

**Rationale:** On native, Cranelift emits a `.o` referencing `phx_*` symbols and `cc -lphoenix_runtime` resolves them at link time. WASM has no equivalent of `cc` + `libphoenix_runtime.a`, so we choose how the runtime's compiled wasm32 bytes get into the user-program `.wasm` module. **Embed-and-merge** keeps the pipeline pure-Rust and matches the wasm-encoder shape we already use:

1. `phoenix-runtime` is compiled once to a complete `.wasm` module via `cargo build --target wasm32-wasip1 --release`.
2. `phoenix-cranelift` discovers the resulting `phoenix_runtime.wasm` (env var `PHOENIX_RUNTIME_WASM` → search `target/wasm32-wasip1/{release,debug}/`).
3. At codegen time, `wasmparser` walks the runtime module; phoenix-cranelift splices its functions, data segments, and globals into the wasm-encoder output, fixing up function / type / memory / global indices to live in one flat index space with the user code.
4. The user's Phoenix `main` calls into runtime symbols (`phx_gc_alloc`, `phx_print_i64`, `phx_gc_push_frame`, etc.) by their post-merge function index — the same shape PR 2 used for the imported WASI functions, just sourced from the runtime module rather than the WASI host.

**Pros:** pure-Rust pipeline; no external linker dep; merge logic is bounded (~few hundred LOC) and reused for PR 5's WASM GC variant; future-proof against runtime growth (any Rust the runtime crate adds just compiles to more WASM functions that get merged through the same path).

**Cons:** index fix-up is fiddly (every function call, every global reference, every memory access has to be rewritten); the merge logic is its own surface to maintain; debug info is dropped on the floor in the first cut.

**Alternatives considered:**
- *`wasm-ld` (LLVM linker).* Compile both `phoenix-runtime` and our wasm-encoder output as relocatable wasm32 objects, link via `wasm-ld`. Native-shape. Rejected: adds LLD as a build-time dependency on every dev / CI machine; the wasm-encoder side gets significantly more complex (linking metadata sections, relocations); we lose direct control over the final module's section layout — which matters for PR 5's WASM GC custom type-section ordering.
- *Pre-bake runtime bytes via `include_bytes!`.* Variant of embed-and-merge — the runtime `.wasm` builds at workspace-build time and embeds directly into `phoenix-cranelift`. Rejected for PR 3 in favor of the runtime-discovery shape that mirrors native's `find_runtime_lib`; revisit if the manual `cargo build -p phoenix-runtime --target wasm32-wasip1` step proves persistently annoying.

**Memory-layout coordination.** The runtime allocator wants ownership of linear memory above its data section. The merged runtime brings its own iovec staging in its data section, so the wasm32-linear backend has no separate per-call scratch region. The layout invariants the merge pipeline maintains:

- The merged module declares an initial memory of `max(MIN_INITIAL_PAGES, runtime_min_pages)` 64-KiB pages (currently `MIN_INITIAL_PAGES = 17`, ~1 MB) so the GC has somewhere to allocate. The runtime's own page floor wins when it's larger. A runtime-declared `maximum` (sandbox cap) is propagated through verbatim; if the runtime's cap is lower than our floor we drop the cap (and emit a warning) rather than emit an invalid `minimum > maximum` memory type.
- The runtime's data segments are merged at their compiled-in offsets (de-conflicted by virtue of the runtime owning `[0, runtime_data_end)`).
- `__heap_base` global is initialized by the runtime's compiled image to the post-runtime-data offset; the runtime allocator reads it on first allocation.
- PR 3a emits no *user* data. PR 3b will: when it starts appending user data segments above the runtime's, the bytes would land in the heap region unless `__heap_base` is bumped to the new post-user-data offset. PR 3b is therefore responsible for rewriting the `__heap_base` global initializer (the surface for this is `module_builder::globals` + the merge's `global_remap`). Surfacing the constraint here so it's visible when PR 3b lands rather than discovered as a corruption bug.

#### G. Control-flow translation: loop+switch dispatch, relooper deferred

**Decided:** 2026-05-15 (during PR 3 scope review).
**Implemented:** ✅ PR 3b. `wasm/translate.rs::translate_multi_block` emits the loop+switch dispatcher described below; `wasm/translate.rs::translate_terminator` routes `Jump` / `Branch` through it. The fibonacci.phx round-trip test (`fibonacci_runs_under_wasmtime`) pins correctness against the AST interpreter.

**Rationale:** WASM has no general `goto` — only structured `block` / `loop` / `if` with branch-out-to-enclosing-label. Phoenix's IR is a basic-block CFG. The translation has two practical shapes; we pick the simpler one for PR 3b and reopen if benchmarks demand the tighter one.

**Chosen: loop+switch dispatch** (the "irreducible-CFG fallback" pattern used by LLVM's wasm backend when relooper fails):

- Each function body is wrapped in `(loop $L)`.
- Each basic block gets a contiguous integer ID (matching its [`BlockId`]).
- A function-local i32 holds "next block ID" — default-initialized to 0 (= entry block) so no explicit init instruction is needed.
- The loop body opens with `br_table` dispatching on that local to a labeled `block` per basic block. Nesting is deepest-first (`$bb_0` innermost), so `br_table 0 1 … N-1 0` matches block-ID-to-label-depth.
- Each basic block's terminator sets the next-ID local and `br <depth_to_loop>`s back to `$L` (or emits `return` for `Terminator::Return`).
- Block parameters get fresh WASM locals at dispatcher-construction time; `Jump` / `Branch` terminators copy SSA-value locals into the target block's param locals before re-entering the dispatch.

Correctness is unconditional — any CFG, including loops with multiple entry points, lowers cleanly. Output quality is "fine, not great" — extra `br_table`s and locals that Wasmtime / V8's optimizers mostly clean up at JIT time.

**Deferred: relooper / Stackifier.** The published algorithm that reconstructs structured control flow (LLVM's `WebAssemblyCFGStackify.cpp`, ~1.5k LOC) produces tighter output but is a project unto itself. Phase 2.4 doesn't gate on output quality; the [phase-close bench refresh (decision D)](#d-phase-close-bench-refresh-scope-wasm-vs-native-phoenix-only) is the right place to discover whether control-flow overhead is a real signal in WASM-vs-native, and a relooper PR can land in Phase 2.5+ if it is.

**Tripwire for revisiting:** if the phase-close `wasm32-linear` numbers on `fib_recursive` or `sort_ints` are >3× slower than native specifically because of the `br_table` dispatch (verified via wasmtime profiling), the relooper pass becomes a real follow-up. Otherwise the deferral holds indefinitely.

**Alternatives considered:**
- *Relooper up front.* Rejected per the scope argument above: ~1.5k LOC of intricate algorithm on top of everything else PR 3b is doing.
- *Stackifier (LLVM's successor).* Same reasoning, plus the algorithm is even more complex.
- *Cranelift IR's loop analysis as a substrate.* Rejected: would tie the WASM emitter to Cranelift's pass infrastructure, undoing decision A0's separation.

#### H. String-literal materialization: data-section borrowed pointers

**Decided:** 2026-05-15 (during PR 3c scope review).
**Implemented:** 2026-05-18 (PR 3c).

**Context.** PR 3b's first attempt at `Op::ConstString` placed string bytes in a user data segment above the runtime's data section. The runtime's allocator clobbered those bytes because its compiled `__heap_base` (the offset where dlmalloc starts serving free memory) sits at the end of the runtime's own data section, with no exported global to override. PR 3b deferred strings to PR 3c with two candidate fixes; this decision picks between them.

**Investigation findings (PR 3c scope review).** Direct inspection of `phoenix_runtime.wasm` (the merged-source artifact) confirmed:
- The only export is `memory`; **no `__heap_base` global is exposed.** Rustc bakes `__heap_base` into the allocator's init code as an i32 constant, not as a wasm global. There is no merge-time hook to bump it.
- The single declared global is `__stack_pointer` (mutable i32, initialized to `1048576`). The runtime's stack grows down from offset 1048576; the rest of the first 1 MB `[0, 1048576)` is the stack region. Runtime data segments start at 1048576 and grow upward.

**Chosen: option (e), data-section borrowed pointers.**

- String literals are placed in user data segments at offsets `[16, ~1024)` (well below the stack's typical excursion for current fixtures). Offset 0 is reserved as a NULL sentinel — we don't write there.
- `Op::ConstString("s")` emits `(i32.const offset, i32.const len)` — a 2-slot WASM fat pointer that points *directly* at the data section. No `phx_string_alloc` call, no memcpy, no shadow-stack rooting.
- The runtime's `phx_print_str` / `phx_str_concat` / `phx_str_eq` / etc. treat their fat-pointer arguments as **borrowed slices** — they `slice::from_raw_parts(ptr, len)` and read the bytes without writing or freeing. A data-section pointer works identically to a heap-allocated `phx_string_alloc` pointer for these inputs.
- Runtime ops that **produce** new strings (`phx_str_concat`, `phx_i64_to_str`, etc.) still heap-allocate via `phx_string_alloc`. Those results need shadow-stack rooting (PR 3c's other deliverable); literal-string fat pointers don't, because the data section is permanent.

**Pros:**
- Simplest implementation — single `reserve_user_data` call per `Op::ConstString`, no codegen indirection.
- Zero per-use overhead for literals (no alloc, no memcpy).
- No shadow-stack rooting for literals — the GC doesn't track them, which matches reality (they live in the data section forever).
- Heterogeneous-origin fat pointers (data-section + heap-allocated) compose uniformly through the runtime's `phx_str_*` surface.

**Cons:**
- **Bounded stack-collision risk.** The runtime's stack grows down from offset 1048576. Programs with deep enough recursion could push the stack pointer below the data section, overwriting literal bytes. For the current fixture set this is *believed* safe — none of the fixtures recurse deeply enough to plausibly threaten offset 16 — but the safety margin is not measured anywhere; we infer it from the shallow call shapes (`fibonacci.phx`'s recursion is at most ~10 deep and each frame is dozens of bytes, leaving an order-of-magnitude headroom). The tripwire today is a runtime crash with corrupted string output. Documented in `wasm/translate.rs` at the `Op::ConstString` lowering site.
  - **Concrete follow-up trigger.** During PR 4's four-backend matrix run, instrument the wasm32-linear `_start` shim to read `__stack_pointer` at end-of-program and log its low-water mark. If any fixture's low-water mark drops below `STACK_REGION_BASE / 2` (consumes more than half of the stack region) — or below `USER_DATA_LIMIT` at any point — file a Phase 2.5 follow-up to either (a) bump `STACK_SAFETY_MARGIN`, (b) split user data and stack into disjoint memory ranges via a runtime-coordinated `__heap_base` rewrite, or (c) revisit option (c) "runtime alloc + memcpy" from the alternatives list. The numeric values for `STACK_REGION_BASE`, `USER_DATA_LIMIT`, and `STACK_SAFETY_MARGIN` are defined in `crates/phoenix-cranelift/src/wasm/module_builder.rs` — that's the single source of truth; this doc references them by name so they don't drift if the margin is retuned. Until the trigger fires, the heuristic margin stands.
- Heterogeneous lifetimes for string fat pointers. Mostly invisible at the IR-translator level (every runtime op accepts both), but a future pass that *frees* data based on inspecting the fat pointer's lifetime would need to distinguish.
- String literals waste no heap space; that's a feature, but it also means `phx_gc_shutdown`'s leak-detection won't report literals — which is correct but worth noting.

**Alternatives considered:**

- **Option (c), runtime alloc + memcpy.** For each `Op::ConstString` site, call `phx_string_alloc(len)` then memcpy bytes from a source region into the heap. Strings live on the GC heap with shadow-stack rooting — a single ownership model. Rejected: still needs the source bytes in low-offset data segments (so the stack-collision concern doesn't go away), pays an alloc + memcpy per use, and the uniformity benefit is moot because runtime ops already treat fat pointers uniformly via borrowed slices.
- **Option (b), bump `__heap_base` at merge time.** Impossible — the runtime exports no `__heap_base` global. Rustc bakes it as a constant.
- **Option (d), module-init via `_initialize`.** Allocate every string at module load, store fat pointers in WASM globals consulted at each use site. Rejected: globals would hold GC-managed pointers but the runtime has no `phx_gc_register_global_root` API for permanent roots, so the strings would be reclaimed at the first collection. Adding such an API is a runtime change outside PR 3c's scope.

##### PhxFatPtr layout contract (sub-decision under H)

The wasm32-linear backend's *sret* call sequences hand-roll `i32.load` instructions at offsets `0` (ptr) and `4` (len) to read `PhxFatPtr` out of caller-allocated stack space — see `translate_to_string_builtin` in `crates/phoenix-cranelift/src/wasm/translate.rs`. Those offsets become wrong if the struct grows, reorders, or drops `#[repr(C)]`, and the resulting bug would be a silent miscompile (wasmtime would happily execute and produce garbage strings).

To make any layout change a build break instead, `crates/phoenix-runtime/src/lib.rs` pins three invariants in a `const _: () = { ... };` block immediately after the `PhxFatPtr` declaration:

1. `offset_of!(PhxFatPtr, ptr) == 0` — ptr at offset 0.
2. `offset_of!(PhxFatPtr, len) == size_of::<usize>()` — len follows ptr with no padding.
3. `size_of::<PhxFatPtr>() == 2 * size_of::<usize>()` — total size is exactly two words.

All three are target-independent (they pin structural invariants, not absolute byte counts). On wasm32 `usize` is 4 bytes (struct is 8 bytes total); on x86_64 it's 8 (struct is 16). Each backend reads at offsets matching its own target's word size, so the wasm32 backend's "offset 4" hand-rolled loads are correct iff `size_of::<usize>() == 4` *and* the len-follows-ptr-with-no-padding invariant holds — which the second assert pins.

#### I. wasm32-gc runtime architecture: codegen-emitted helpers, no Rust runtime crate

**Decided:** 2026-06-04

**Context.** PR 5 (the WASM GC backend) needed to settle how the existing `phoenix-runtime` crate — built around dlmalloc + a side-registry tracing GC + a flat `phx_*` symbol surface — interacts with the new target. Three candidate architectures were considered: (1) codegen-only, no runtime crate; (2) hybrid, runtime for WASI stubs only; (3) recompile the full runtime for wasm32-gc.

**Structural blocker for "recompile the runtime".** Rust's wasm32 toolchains (`wasm32-unknown-unknown`, `wasm32-wasip1`, etc.) compile to **linear memory only.** There is no Rust target that emits `struct.new` / `array.new` — Rust's `Box<T>` lowers to `dlmalloc(sizeof(T)) → *mut T`, never to a `(ref (struct …))` managed reference. The literal interpretation of "recompile the runtime" would keep the Rust runtime running in linear memory; user code holds WASM-GC managed references; every runtime call therefore marshals **WASM-GC array → linear-memory ptr+len → runtime computation → linear-memory ptr+len → freshly-allocated WASM-GC object** at the FFI boundary — and the WASM-GC-side allocation on the return path has to be emitted by *codegen*, not by Rust, because Rust can't emit `struct.new`. That's strictly more work than option 1, not less.

**Chosen: Codegen-emitted, no Rust runtime crate for wasm32-gc.**

- The wasm32-gc codegen emits `struct.new` / `array.new` / `array.copy` / `call_ref` inline for all allocation, structure access, and dispatch.
- Genuinely complex helpers — the hash-table probing/grow/tombstone logic of `phx_map_*`, the formatting of `phx_i64_to_str` / `phx_f64_to_str`, the string transform surface (`phx_str_trim` / case mapping) — ship as **wasm-encoder-emitted WASM functions synthesized from the codegen crate itself**, not as a separate Rust runtime artifact. Same shape as the synthesis of `_start` and the WASI print stubs before `phx_print_*` were merged in via `phoenix-runtime`: codegen owns its own helpers, written in WASM bytecode rather than Rust.
- No `phoenix-runtime` recompile for wasm32-gc. No `embed-and-merge` step. No `phx_gc_alloc` / `phx_gc_set_root` calls — the host VM's GC handles tracing; shadow-stack emission is suppressed entirely on this target (already pinned by §2.3 decision A: *"Phase 2.4 (WASM GC) replaces native root-finding entirely with WASM GC's typed references."*).

**Pros:**
- No marshaling overhead at the runtime FFI boundary — user values stay as WASM-GC references the whole way through.
- No duplicated allocator / GC implementation. The host's GC is the only memory manager.
- Helpers grow incrementally and locally to the codegen crate; complexity sits where the layout decisions live.
- Shadow-stack suppression is automatic (the runtime that *provides* `phx_gc_push_frame` etc. simply doesn't exist on this target — the call sites can't be emitted, so the missing helpers can't be the source of a regression).

**Cons:**
- Codegen carries more bytecode-emission work than the linear-memory backend. Hash tables and number formatting in particular are non-trivial wasm-encoder programs. The PR 5 MVP scope (decision J below) defers most of this; PR 6 grows helpers as fixtures demand them.
- Two code paths for any helper that conceptually overlaps with the linear-memory runtime (string concat, list push, hash insert). Acceptable because the target ABIs diverge so completely (linear-memory + tagged GC vs. typed managed refs + host GC) that even a shared Rust implementation would have to fork at every meaningful method.

**Alternatives considered (and rejected):**

- **Hybrid runtime, WASI stubs only.** A tiny `phoenix-runtime-gc` crate that exposes only WASI print helpers (`phx_print_str` → `fd_write` wrapper), with all allocation inline. Rejected: the WASI helpers are themselves trivial wasm-encoder programs (memory.fill + fd_write); a whole crate-level dependency for ~30 lines of WASM bytecode is structurally heavier than emitting them from codegen.
- **Recompile `phoenix-runtime` for wasm32-gc.** Rejected per the structural blocker above. Even if every internal Rust data structure stays in linear memory, the WASM-GC-side alloc on the return path of every runtime call still has to be emitted by codegen — so this option is "option 1 + extra marshaling + a runtime crate to maintain," strictly worse than chosen option alone.

**File:** the wasm32-gc codegen lives under `crates/phoenix-cranelift/src/wasm/wasm_gc/` (decision K below); WASM-bytecode helpers ship as functions emitted by that module's own builder pass, not as a separate compilation unit.

#### J. wasm32-gc MVP scope: hello + fibonacci + one struct

**Decided:** 2026-06-04

**Chosen.** PR 5 ships the minimum representative slice that proves the wasm32-gc pipeline end-to-end. To keep each merge small and reviewable, PR 5 is delivered in slices, each growing the op surface and carrying its own tests; the source-tree comments use the same "slice N" labels:

- **Slice 1 (`tests/fixtures/hello.phx`)** — the current landed slice. The fixture is `let x: Int = 42; print(x)`, so it pins `Op::ConstI64` + the immutable-`let` lowering (`Op::Alloca` / `Op::Store` / `Op::Load`), `Op::BuiltinCall("print", Int)` routed through the inline WASI print synthesis from decision I (the synthesized `phx_print_i64` digit-conversion helper), and the `_start` / `main` plumbing. `Op::ConstString` materialization (data segment + `array.new_data`) and `print(String)` are deferred to the String slice, not exercised by `hello.phx`.
- **Slice 2 (`tests/fixtures/fibonacci.phx`)** — pins direct function calls, integer arithmetic, control flow (loop+switch dispatch already shared with the linear backend per §2.4 decision G), value-returning user functions, and (via a negative result) the `phx_print_i64` sign path that slice 1 cannot yet reach.
- **Slice 3 (one struct fixture** — `tests/fixtures/features.phx` subset, or a focused new fixture if `features.phx` reaches for ops beyond MVP scope) — pins `Op::StructAlloc` / `Op::StructGetField` / `Op::StructSetField` lowered as `struct.new` / `struct.get` / `struct.set`, and **the first concrete WASM-struct-type-per-Phoenix-struct decision** (a sub-decision under K below — every Phoenix struct gets its own named WASM struct type with named fields, mirroring the layout that's monomorphized into the IR module's `struct_layouts`). This is also the first slice that emits any WASM-GC type at all; slices 1–2 are structurally linear modules that merely validate under the GC proposal.

**Out of MVP scope, deferred to PR 6:** closures, lists, maps, enums (including `Option`/`Result`), `dyn Trait` dispatch, builder types. Each carries its own type-mapping decision (closure-as-`(struct (ref func) cap0 …)` vs. via `funcref`; `(array T)` vs. struct-wrapped; etc.) that benefits from being settled one slice at a time inside PR 6 rather than locked in batch up front. The four-backend matrix expansion in PR 6 surfaces the corner cases each mapping has to handle.

**Why not more?** Type-mapping decisions for collections / closures / `dyn` are independent of the core pipeline scaffolding (module builder, type interner, `_start` synthesis, struct ops). Surfacing them one at a time in PR 6 keeps each merge reviewable and lets the matrix gate each addition; locking them in at PR 5 design time would either be premature (we'd choose without the matrix's adversarial pressure) or would slip PR 5 by weeks.

#### K. wasm32-gc codegen layout: parallel `wasm/wasm_gc/` module tree

**Decided:** 2026-06-04

**Chosen.** A new `crates/phoenix-cranelift/src/wasm/wasm_gc/` directory contains the wasm32-gc translator, mirroring the layout of the existing wasm32-linear tree (`crates/phoenix-cranelift/src/wasm/`). Slice 1 lands `mod.rs`, `module_builder.rs`, and `translate.rs`; the struct slice (decision J slice 3) adds `heap_layout.rs` once there is a concrete WASM-struct-type-per-Phoenix-struct layout to own. Shared scaffolding lives in the parent `wasm/` namespace and is imported by both targets:

- `wasm/type_interner.rs` — function-signature dedup is target-independent.
- `wasm/runtime_discovery.rs` and `wasm/runtime_merge.rs` — irrelevant for wasm32-gc per decision I (no runtime to merge), but unused imports are not added; wasm32-gc just doesn't reference these.

Each target's `module_builder.rs` owns its own section-state struct because the section pipelines diverge (wasm32-gc declares type-section GC types; wasm32-linear merges a runtime). Each target's `translate.rs` owns its own per-op match.

**Why not target-dispatched lowering inside `wasm/translate.rs`?** The two backends' allocation primitives (`phx_gc_alloc` vs. `struct.new` / `array.new`), heap representations (raw byte offsets + TypeTag tracking vs. typed managed refs), and signature shapes (env-pointer ABI through `closure_target_slot` vs. WASM GC's `funcref` / `call_ref`) diverge enough that target-dispatched `if/else` chains would dominate every match arm. Each arm of `Op::StructAlloc`, `Op::CallIndirect`, `Op::DynAlloc`, etc., would split into two essentially-disjoint emission blocks. Parallel trees keep each target's code linear-readable and let the file structure mirror the per-target mental model. The ~60% shared scaffolding (type interner, basic constants, error type) lives in the parent `wasm/` namespace where both trees can import it directly.

**Why not a separate `phoenix-cranelift-wasm-gc` crate?** Crate boundaries are appropriate when the dependency graph diverges. The two WASM backends both depend on `phoenix-ir`, `phoenix-common`, and `wasm-encoder`; they share most of the type-mapping helpers; the only real divergence is the body emission and module-builder layout. A new crate adds Cargo manifest, workspace dependency wiring, and re-export plumbing for negligible isolation benefit.

**File:** new module `crates/phoenix-cranelift/src/wasm/wasm_gc/mod.rs` with the parallel tree underneath. `crates/phoenix-cranelift/src/lib.rs::compile`'s `Target::Wasm32Gc` arm dispatches to `wasm_gc::compile_wasm_gc` (mirroring the existing `Target::Wasm32Linear` → `wasm::compile_wasm_linear` dispatch).

#### K.1. wasm32-gc struct-type mapping: one nominal WASM struct type per Phoenix struct

**Decided:** 2026-06-05 (sub-decision under K, locked alongside PR 5 slice 3 — the first slice that emits a `(struct …)` type-section entry).

**Context.** Slice 3 (decision J) is the first slice that has to commit to a concrete IR-type → WASM-type mapping. Slices 1–2 emitted structurally linear modules whose validity under `-W gc=y` was trivial (no GC types declared); slice 3's `Op::StructAlloc` / `Op::StructGetField` / `Op::StructSetField` cannot lower without a WASM struct type to reference. The sub-decision picks among the candidate mappings before any code lands so the question doesn't reopen each PR-6 slice that grows the surface (closures, lists, maps, enums each carry their own analogous sub-decision when they land).

**Chosen.** Each Phoenix struct (post-monomorphization name — e.g. `Point`, `Container__i64`) gets one nominal WASM struct type, declared once in the type section before any function signature references it. Fields are declared in Phoenix source declaration order (matching `IrModule::struct_layouts`'s ordered `Vec<(String, IrType)>`), all marked mutable (Phoenix supports `p.x = 5` and has no syntax to declare a field immutable). Phoenix `IrType::StructRef(name, _)` lowers to `(ref null $name_struct_idx)`:

- **Nullable** so `Op::Alloca(StructRef(...))`'s WASM-local default state (which WASM zero-initializes) is well-typed without an explicit init instruction. `Op::Store` then writes the post-`struct.new` non-nullable reference into the same slot (subtype: `(ref $T) <: (ref null $T)`).
- **Field types**: Phoenix `Int` → WASM `i64`, `F64` → `f64`, `Bool` → `i32`.
  - **String fields lifted 2026-06-12**: `StringRef` → `(ref null $string)`, enabled by flipping the type-section order so `$bytes`/`$string` (which reference no other type) are declared *before* the structs — the field encodes a concrete index that now exists. `scan_helper_needs` backstops the helper scan from both layout tables: a String field in a non-template struct layout, or in any variant of an enum instantiation that will be declared, forces the string types in even when no function body touches a string. This unblocked `traits_static.phx` and three multi-module matrix fixtures.
  - **Generic templates skipped, same date**: `struct_layouts` retains the generic *template* (`Container` with a `TypeVar("T")` field) alongside its monomorphized instances; the struct passes skip any template (via `concrete_struct_names`) rather than declaring it — concrete code only references the instances, mirroring how the native backend resolves layouts by concrete name on demand and how K.4's enum declaration treats templates. Templates are identified by their `IrModule::struct_type_params` entry (registered for every generic declaration, never cleared), not by scanning fields for placeholders: a phantom param (`struct Tag<T> { id: Int }`) leaves no placeholder in any field but is still a template. This unblocked `generics.phx` (whose instances' fields are `i64` / `StringRef`, both supported).
  - **Reference-typed fields lifted 2026-06-15 (K.11)**: every reference-typed field — nested `StructRef`, `EnumRef`, `ListRef`, `MapRef`, `ClosureRef`, `DynRef` — now lowers to `(ref null $T)`, enabled by the K.10 rec-group machinery (reserve struct indices early, define bodies late). See K.11. The per-field "not yet supported" diagnostic this note described is gone.

`struct.get`/`struct.set` resolve the WASM struct-type index by reading the *receiver value's* binding `ValType` (which carries `HeapType::Concrete(idx)`), so the get/set ops don't have to thread the struct name themselves — the type that the receiver was bound with is authoritative. Equivalent to threading the name, but avoids a parallel `ValueId → struct_name` map.

**Why nominal (one WASM struct per Phoenix struct), not structural sharing.** WASM-GC's type system is nominal — even if two structs declare the same field shape, they are distinct types and a `(ref $A)` is not assignable to a `(ref $B)`. That property maps directly onto Phoenix's nominal structs (`Point { x: Int, y: Int }` and `Pixel { x: Int, y: Int }` are distinct in Phoenix too — assigning a `Point` into a `Pixel`-typed slot is a sema error). A structural-sharing scheme — "one WASM struct per distinct field layout, multiple Phoenix structs alias the same WASM type" — would be smaller (one declaration where two would be), but it'd require runtime tag bytes to distinguish the Phoenix-level types when they meet a `dyn Trait` or a pattern match, re-inventing the tagged-union machinery that the WASM GC target was supposed to elide. The size savings are real but small for MVP fixtures (one struct per fixture).

**Why declare-before-any-function-signature.** Function signatures that take or return struct refs encode the struct's WASM type index inside their `ValType::Ref { heap_type: HeapType::Concrete(idx) }`. The WASM type section is a flat, position-indexed list; resolving the index requires the struct's declaration to precede the function signature in section order. The pipeline *reserves* every struct index up front (`reserve_phoenix_structs`, K.11) before any function signature is interned; the struct bodies are filled later (`define_phoenix_structs`) but the index a signature encodes already exists.

**Alternatives considered:**

- **Shared "boxed" struct (`(struct (field anyref … anyref))`)**. One WASM struct type with N anyref slots; every Phoenix field stored as boxed-anyref. Rejected: `i64`/`f64`/`bool` fields would need boxing/unboxing on every access — exactly the overhead WASM-GC's typed fields exist to eliminate. The "no tagged-union machinery" advantage above evaporates.
- **i31ref tagging (one WASM type, distinguish Phoenix types via i31ref tag).** Rejected: same boxing overhead as the shared-boxed variant, plus an extra runtime tag check.
- **Lazy declaration (declare a struct the first time `Op::StructAlloc(name)` is translated).** Considered. Would let us declare only structs actually instantiated. Rejected: function signatures may reference struct refs in params/returns before any allocation site is translated, so the index has to exist at signature-interning time anyway. Declaring eagerly from `struct_layouts` is simpler and emits at most a handful of dead types. One eager-declaration caveat: a struct that is *declared but never instantiated* is still walked, so if such a struct carries a field type the current slice doesn't support yet (a nested struct, list, string, etc.), `declare_phoenix_structs` errors even though no live code path touches it. For slice 3 that was the intended fail-closed behavior (an explicit per-field diagnostic beats a silently malformed module), but it meant a dead struct was "free" only when all its fields were on the supported primitive surface — not unconditionally. Lazy declaration would have sidestepped that, at the cost of the signature-ordering problem above. **Since K.11 (2026-06-15)** every field type is supported, so this caveat is moot — a declared-but-uninstantiated struct with reference-typed fields now declares fine.

**Implementation pointers:**
- WASM struct declarations: [`crates/phoenix-cranelift/src/wasm/wasm_gc/module_builder.rs::reserve_phoenix_structs` / `define_phoenix_structs`](../crates/phoenix-cranelift/src/wasm/wasm_gc/module_builder.rs) (K.11 reserve/define split).
- `IrType::StructRef` → `ValType::Ref` mapping: [`crates/phoenix-cranelift/src/wasm/wasm_gc/translate.rs::wasm_valtypes_for`](../crates/phoenix-cranelift/src/wasm/wasm_gc/translate.rs).
- `Op::StructAlloc` / `Op::StructGetField` / `Op::StructSetField` lowering: same `translate.rs`, under the `translate_instruction` match.
- Fixture: `tests/fixtures/wasm_gc_struct.phx` (a focused fixture rather than carving down `features.phx`; per decision J slice 3, "or a focused new fixture if `features.phx` reaches for ops beyond MVP scope" — `features.phx` exercises strings, methods, enums, while-loops, for-loops, all of which are out of slice-3 scope, so a focused fixture is the right call).

#### K.2. wasm32-gc string mapping: three-field struct over a mutable byte array

**Decided:** 2026-06-05 (sub-decision under K, locked alongside PR 6's first slice — strings on wasm32-gc).

**Context.** PR 6 (the WASM-GC matrix expansion, per [decision J](#j-wasm32-gc-mvp-scope-hello--fibonacci--one-struct)) opens with the String slice because strings are pervasive across the fixture corpus (most printing fixtures interpolate or compare strings) and the print-Bool / print-Float / print-String carve-outs in the existing wasm32-gc test surface (`print_bool_is_rejected_until_a_later_slice` and friends) cannot lift without a `IrType::StringRef` → WASM-type mapping in place. As with K.1, the sub-decision is recorded *before* code lands so the representation choice isn't relitigated each time a follow-up slice grows the string op surface (concat is slice 1; substring, trim, case mapping, format, etc. each land in their own slice — and each reads bytes from whatever shape this decision locks).

**The "what about substring / string builder" pivot.** An earlier draft of this decision recommended a bare `(ref (array (mut i8)))` representation on the grounds that WASM-GC arrays already track their length intrinsically, so wrapping them in a struct just to "have a String type" duplicates information. That framing was structurally incomplete: it treated zero-copy substring and zero-copy `StringBuilder.finalize()` as hypothetical future requirements when they are in fact both on the Phoenix roadmap (substring is a core string operation in every language Phoenix benchmarks against, and a string builder is the Phase 2.7 [`ListBuilder` / `MapBuilder` analog](#phase-27-benchmarking) that Phoenix will need once string-heavy workloads come into perf scope). Once those requirements are first-class, the bare-array shape forces both operations to be O(n) (substring copies bytes, builder finalize copies the assembled payload into a right-sized array). The right shape supports both as O(1) view-style operations — and that shape has to be chosen *now*, because changing the WASM type of `IrType::StringRef` later would require rewriting every concat / equality / format / interpolation / print site that emits string ops, and the language ecosystem (any downstream WASM-GC consumer that links against a Phoenix module's exports) would face an ABI break.

**Chosen — the three-field shape, labelled (b') below** (a prime extension of the two-field alternative (b), adding an explicit `$offset`; the per-op cost table and the alternatives list further down both reference this label). `IrType::StringRef` lowers to `(ref null $string)` where `$string` is the nominal WASM-GC type:

```
(type $bytes  (array (mut i8)))
(type $string (struct
  (field $data   (ref $bytes))
  (field $offset i32)
  (field $len    i32)))
```

Three fields:

- **`$data`** — a non-null reference to the underlying mutable byte array. The array's WASM-level length (`array.len $data`) is the *capacity* — for an immutable Phoenix-level string it equals `$len`, but for a `StringBuilder.finalize()`-derived string it may exceed `$len`. For a substring view it may exceed `$offset + $len` by an even larger margin.
- **`$offset`** — the byte index inside `$data` where this string starts. For a freshly-allocated string (`Op::ConstString`, `Op::StringConcat`) `$offset` is `0`. For a substring view it is the start index passed to `substring(s, start, end)`. Carrying `$offset` explicitly is what makes substring an O(1) struct.new rather than an O(n) array allocation + copy.
- **`$len`** — the byte length of this logical string. `String.length()` is a single `struct.get $string $len` (no helper needed, no walk of the bytes).

The `$bytes` array is declared with **mutable** cells (`(array (mut i8))`) — Phoenix-level immutability is enforced by sema (no IR op writes to `$bytes` after the string-producing op finishes), but the WASM-level mutability is required so the eventual `StringBuilder` can grow its array in place (append-byte path) without needing a separate mutable array type that wouldn't be assignable to `$string`'s `$data` field. The WASM-GC type system has no array-mutability subtyping that would let a `(array i8)` flow into a `(ref (array (mut i8)))` slot; choosing `(mut i8)` uniformly avoids the migration.

**`IrType::StringRef` → WASM `ValType`.** Nullable concrete ref `(ref null $string)`, same nullability rationale as K.1's `StructRef` mapping: `Op::Alloca(StringRef)` slots default to `ref.null` so the WASM-local zero-init is well-typed; the first `Op::Store` writes a non-nullable `(ref $string)` from `Op::ConstString` / `Op::StringConcat` / etc. into the slot (subtype `(ref $T) <: (ref null $T)`).

**Op-by-op cost under (b'):**

| Op | Lowering | Cost vs. bare-array `(a)` |
|---|---|---|
| `Op::ConstString("hi")` | `array.new_data $bytes $data_seg <off> <len>`; `i32.const 0`; `i32.const len`; `struct.new $string` | +1 alloc (struct), +2 const pushes |
| `String.length()` | call to `phx_str_length` helper (code-point-start walk) | One Call vs. one `struct.get` |
| `Op::StringConcat(a, b)` | Read each side's `$len` + `$offset` via `struct.get`; `array.new_default $bytes (a_len + b_len)`; two `array.copy` honoring source `$offset`; `struct.new $string` wrapping with `offset=0` | +1 alloc (struct), ~6 extra instructions for offset arithmetic |
| `Op::StringEq(a, b)` | Length check via `struct.get $len`; loop reading `array.get_u $bytes (offset + i)` on each side | +1 `i32.add` per byte iteration |
| `print(String)` | `phx_print_str` helper copies `$len` bytes starting at `$offset` from `$data` into the linear-memory iovec staging area, then `fd_write` | +1 `i32.add` per byte in the copy loop |
| **Future** `Op::Substring(s, start, end)` | `struct.new $string s.$data (s.$offset + start) (end - start)` — zero bytes copied | O(1) vs. O(n) under (a) |
| **Future** `StringBuilder.finalize()` | `struct.new $string builder.$data 0 builder.$len` — zero bytes copied | O(1) vs. O(n) under (a) |

The per-op cost is a small bounded constant in the present, and the substring / builder savings are unbounded by string length in the future. The asymmetry is what makes (b') the right tradeoff.

**Why the array stays `(array (mut i8))`, not a packed-bytes structural alternative.** WASM-GC offers `array i8` (packed i8 storage, accessed via `array.get_s` / `array.get_u`) but not a "packed bytes array of fixed length" type. The `(array i8)` form is the only available shape. The mutability question is orthogonal — chosen `mut` for StringBuilder reuse per above.

**Alternatives considered:**

- **(a) Bare `(ref null (array (mut i8)))`**. Simplest type declaration (1 entry), `length` is `array.len`. Rejected for the substring / builder asymmetry above — both operations would be O(n) under this shape, and migrating later would re-cost every string-touching op site.
- **(b) Two-field struct `(struct (ref $bytes) (field $len i32))`**. Length-separation enables zero-copy StringBuilder finalize but NOT zero-copy substring (substring needs `$offset` as well). Rejected: the increment from (b) to (b') is one extra field for a substantial future benefit; if we're paying for any struct wrapping at all, paying for the three-field shape is dominantly cheaper than the two-then-three migration cost.
- **(c) Single-field wrapper `(struct (ref $bytes))`**. Nominal wrapper as a forward-compatibility stepping stone to (b) or (b'). Rejected: the migration to (b') is a localized codegen change in any starting shape (one helper rewrite + per-op-site updates), so the wrapper's "easier migration" benefit is small. Meanwhile (c) pays an extra `struct.get $data` on every byte access today with no current upside.
- **`String` as `(ref null (array i8))` with `$bytes` immutable.** Rejected: forces StringBuilder to use a different array type internally (a mutable `(ref (array (mut i8)))`) with no WASM-GC casting path that would let the builder's array flow into the finalized String's `$data` slot. Builder finalize would have to allocate a fresh immutable array and copy — defeating the purpose.
- **`i31ref`-encoded short strings + heap fallback for long ones.** Would let strings ≤ 4 bytes live in 32-bit values without allocation. Rejected for slice 1: real complexity (every read site has to test the i31 tag), unclear payoff under realistic workloads, and the dispatch tax is paid on *every* string operation forever. Reopens as a Phase 4-ish perf optimization if profiling identifies short-string allocation as a bottleneck.

**PR 6 slice 1 op surface (locked alongside this decision):**

- `Op::ConstString` — data-segment materialization + `array.new_data` + `struct.new`.
- `Op::BuiltinCall("print", String)` — synthesized `phx_print_str` helper that copies `$len` bytes from `$data + $offset` into the linear-memory iovec staging area, appends a newline, and calls `fd_write`. Mirrors the existing `phx_print_i64` synthesis pattern (see `module_builder::synthesize_print_i64_helper`).
- `Op::StringConcat` — synthesized `phx_str_concat` helper that allocates `$bytes` of combined length, `array.copy` from each operand honoring its `$offset`, then `struct.new` the result.
- `Op::StringEq` / `Op::StringNe` — synthesized `phx_str_eq` helper (length-equal check + byte loop with offset arithmetic on both sides). `StringNe` lowers as `phx_str_eq` + `i32.eqz`.
- `Op::BuiltinCall("String.length", _)` — calls a synthesized `phx_str_length` helper that walks code-point starts. **Correction note (2026-06-05):** an earlier draft of this entry claimed length lowered as a single `struct.get $string $len` and called it "1 instruction." That was structurally incorrect — Phoenix's `String.length()` returns the **char count** (code-point count), not the byte count, as established by [`phoenix-runtime/src/string_methods.rs::phx_str_length`](../crates/phoenix-runtime/src/string_methods.rs)'s `s.chars().count()` shape. Returning `$len` (the byte length) would silently diverge from every other backend's semantics on any non-ASCII string. Slice 1 shipped with the byte-length bug latent (its fixture was pure-ASCII so the bug didn't surface in the tests); slice 2 ships the corrected helper. The K.2 cost table above is updated alongside this note.

**Deferred to follow-up slices:** substring (carries the substring decision K.3, locked when the slice lands), `String.trim()` / case mapping (each its own helper), interpolation (already lowers to `Op::StringConcat`-chains today, but multi-arg concat may want its own optimized helper rather than N-1 chained `phx_str_concat` calls — open question for the slice), `print(Bool)` / `print(Float)` (separate primitive-to-string conversion helpers), `Op::StringLt` / `Op::StringLe` / `Op::StringGt` / `Op::StringGe` (lexicographic comparison helpers).

**Implementation pointers (slice 1):**
- `$string` and `$bytes` declarations: `crates/phoenix-cranelift/src/wasm/wasm_gc/module_builder.rs::declare_string_types` (new) — called in `compile_wasm_gc` before `reserve_phoenix_structs` (since the 2026-06-12 string-field lift, so struct fields can encode the `$string` index) and before `declare_imports`, so the type-section indices are stable for any declaration that mentions `StringRef`.
- `IrType::StringRef` → `ValType::Ref` mapping: `crates/phoenix-cranelift/src/wasm/wasm_gc/translate.rs::wasm_valtypes_for`, alongside the K.1 `StructRef` arm.
- Synthesized helpers (`phx_print_str`, `phx_str_concat`, `phx_str_eq`): `crates/phoenix-cranelift/src/wasm/wasm_gc/string_helpers.rs` (each helper's instruction emission), dispatched by `module_builder::ModuleBuilder::declare_string_helpers`, which decides which to emit and records their indices. Mirrors the `synthesize_print_i64_helper` pattern in `module_builder.rs`.
- Fixture: `tests/fixtures/wasm_gc_string.phx` — focused fixture exercising literal printing, concat (interpolation), equality (both directions), and length.

#### K.3. wasm32-gc substring lowering: O(char_count) view via a `phx_str_substring` helper

**Decided:** 2026-06-05 (sub-decision under K.2, locked alongside PR 6 slice 2 — substring + lex compare + print(Bool) on wasm32-gc).

**Context.** K.2 sold the three-field `$string` shape on the premise that substring is "O(1) struct.new — zero bytes copied." That framing was structurally incomplete: Phoenix's existing `substring` semantics, established in [the substring-clamps decision](#substring-clamps-out-of-range-indices-silently) and implemented in `phoenix-runtime/src/string_methods.rs::phx_str_substring`, are **char-indexed, not byte-indexed**. `"héllo".substring(1, 3)` returns `"él"` (two code points), whose bytes happen to form a contiguous slice of the parent (`é` is 2 bytes, `l` is 1, total 3 bytes starting at byte offset 1) — but *finding* those byte boundaries requires walking the parent's UTF-8 bytes counting code-point boundaries until `start` chars are consumed, then `(end - start)` more. The walk is unavoidable; the language semantic dictates it.
The "O(1) substring" promise from K.2 therefore softens to:

- **For pure-ASCII strings** (which both byte-indexed and char-indexed semantics treat identically), substring on wasm32-gc IS O(1) on the byte-walk side — `start_byte = start_char`, `end_byte = end_char`, no walk needed in principle. We still walk in the helper to keep one code path; a fast-path could be added later if profiling identifies it as a bottleneck.
- **For UTF-8 strings with multi-byte chars**, substring is O(char_count) for the walk plus O(1) for the `struct.new`. The byte-array is still shared with the parent — zero bytes are copied — so the substring's runtime cost is *bounded by char count*, not byte count, and dominated only by the walk itself.

The byte-array sharing is what survives from K.2. The "O(1) substring" claim was the part that needed correction. `StringBuilder.finalize()`'s O(1) promise from K.2 is unaffected — the builder produces ASCII / known-boundary output and hands its byte array directly to the finalized `$string` wrapper without a walk.

**Chosen.** A single synthesized `phx_str_substring(s: (ref null $string), start: i64, end: i64) -> (ref $string)` helper. Mirrors the synthesis pattern used by `phx_str_concat` and `phx_str_eq`. The helper:

1. Walks bytes from `s.$offset` counting UTF-8 code-point starts (a byte is a code-point start iff its top two bits aren't `0b10` — i.e., `byte & 0xC0 != 0x80`) until `start` code points are consumed. Records the resulting byte offset as `byte_start`.
2. Continues walking, counting `(end - start)` more code-point starts. Records the byte offset as `byte_end`.
3. Clamps both bounds: `start.max(0).min(char_count)` and `end.max(start).min(char_count)`. Clamping happens *during* the walk — when the walk hits `s.$offset + s.$len` (end of the parent's logical bytes), it stops, and any remaining "advance" requests degenerate to no-ops, naturally producing the clamped result.
4. Returns `struct.new $string s.$data (s.$offset + byte_start) (byte_end - byte_start)` — a view into the parent's byte array.

Empty-result fast-path (`start >= end` after clamp) is the same `struct.new` with `$len = 0`; we don't bother with the runtime's `empty_phx_str` static-pointer optimization because wasm32-gc has no equivalent of a process-static byte address and a 16-byte `struct.new` allocation is cheap.

**Why a helper, not inline lowering.** UTF-8 boundary detection is ~10 instructions of WASM (loop header, byte fetch, mask, conditional decrement of remaining-count, advance), and the loop wraps a clamping check on each iteration. Inlining at every call site would duplicate that block per call — call density isn't huge today, but each duplication is real bytes in the module. The helper keeps the surface bounded and matches how the existing slice-1 string helpers are organized.

**Why not byte-indexed substring.** Considered briefly: byte-indexed substring is O(1) in both walk and struct.new (you can directly index into `$data`). Rejected because it diverges from the existing language semantics that every other backend (native, wasm32-linear, interpreter) already implements char-indexed, and changing the semantic would (a) break user programs that depend on it and (b) require updating the wasm32-linear backend's runtime call too. A wasm32-gc-only divergence is the worst of both worlds: silent divergence between backends. If Phoenix later decides byte-indexed substring is the right language choice, that's its own design-decision pivot and the helper here gets simplified to a single `struct.new`.

**Lex compare (`Op::StringLt` / `StringLe` / `StringGt` / `StringGe`)** is bundled into the same slice (not a separate sub-decision because the design space is narrow): a single `phx_str_cmp(a: ref $string, b: ref $string) -> i32` helper returns negative / zero / positive (lexicographic byte compare on offset-adjusted spans). Each of the four ops then dispatches as `Call $phx_str_cmp` followed by `i32.const 0` and the corresponding signed i32 cmp (`i32.lt_s` / `le_s` / `gt_s` / `ge_s`). One helper rather than four parallel ones because the body is identical except for the final comparison.

**print(Bool)** lowers inline, not as a helper. Two **active** data segments — `"true\n"` (5 bytes) and `"false\n"` (6 bytes) — are emitted at fixed linear-memory offsets above `PRINT_STR_BUF_END`, and the WASM module instantiation auto-copies them into memory at module load. Each `print(Bool)` site emits a 5-instruction `if/else` that stages the iovec at one of the two pre-populated offsets and calls `fd_write`. No new function declaration, no `phx_print_bool` helper. The trade-off here is "module size grows ~11 bytes once (the segment payloads) + ~10 instructions per call site" vs. "one helper function + one Call per site"; for the realistic call density (a `print(true)` here and there) the inline shape is smaller.

**Slice 2 op surface (locked alongside this decision):**

- `BuiltinCall("String.substring", [s, start, end])` — calls `phx_str_substring`.
- `Op::StringLt` / `Op::StringLe` / `Op::StringGt` / `Op::StringGe` — each calls `phx_str_cmp` + signed-i32 cmp against 0.
- `BuiltinCall("print", Bool)` — inline two-segment if/else (no helper).

**Deferred to follow-up slices:** `print(Float)` (its own slice with the f64-formatter design — Ryu vs. lossy fixed-precision vs. host-delegated), `String.trim()` / case mapping / `replace` (each carries its own follow-up — `trim` walks both ends for whitespace and produces a view; case mapping must allocate; `replace` is its own algorithm).

**Implementation pointers:**
- `phx_str_substring` synth: `crates/phoenix-cranelift/src/wasm/wasm_gc/string_helpers.rs::synthesize_str_substring`.
- `phx_str_cmp` synth: same file, `synthesize_str_cmp`.
- Bool data segments: new constants `BOOL_TRUE_OFFSET` / `BOOL_FALSE_OFFSET` in `crates/phoenix-cranelift/src/wasm/wasm_gc/module_builder.rs`, written via active data segments declared by `declare_bool_data`.
- `Op::StringLt`/`Le`/`Gt`/`Ge` lowering: `crates/phoenix-cranelift/src/wasm/wasm_gc/translate.rs`, under `translate_instruction`.
- `BuiltinCall("String.substring")` lowering: same file, `translate_string_substring`.
- Fixture additions: `tests/fixtures/wasm_gc_string.phx` extended (or a sibling fixture) to cover substring on both ASCII and multi-byte UTF-8 strings, lex compare across `<` / `<=` / `>` / `>=`, and `print(true)` / `print(false)`.

#### K.4. wasm32-gc enum mapping: subtype hierarchy (parent + per-variant subtypes)

**Decided:** 2026-06-05 (sub-decision under K, locked alongside PR 6 slice 3 — enums on wasm32-gc).

**Context.** Phoenix enums are pervasive — `Option<T>` and `Result<T, E>` appear in nearly every non-trivial fixture (collection methods return `Option`, parse returns `Result`, error propagation runs through `Result`), plus user-defined enums show up in pattern-matched AST shapes, state machines, and bench fixtures. The representation choice ripples through every enum-touching site (alloc, discriminant read, field read, recursive references) and is hard to revisit later because changing it would re-cost the entire matrix.

Phoenix's IR carries three enum ops, all surfaced by `lower_match` / `lower_expr`:

- `Op::EnumAlloc(name, variant_idx, fields)` — construct an instance of variant `variant_idx` with `fields` payload values.
- `Op::EnumDiscriminant(value)` — return the variant index as `Int` (i64). Emitted at every match-arm dispatch site.
- `Op::EnumGetField(value, variant_idx, field_idx)` — read field `field_idx` from variant `variant_idx`. Always emitted *inside* a match arm that has already confirmed the variant via discriminant test, so it's safe to assume the variant.

Match expressions lower to `Op::EnumDiscriminant` + chained `Op::IEq` + `Terminator::Branch` against the discriminant — *not* `Terminator::Switch` — so this slice doesn't need to extend the terminator surface beyond what slices 1–2 already covered.

**Alternatives considered (full comparison locked here so a future contributor proposing a switch doesn't re-derive the analysis).**

| Property | A. Subtype (chosen) | B. Flat-max | C. Tagged-outer + payload |
|---|---|---|---|
| Type declarations per Phoenix enum | N+1 (parent + N variant subtypes) | 1 (single struct with max-arity fields) | N+1 (outer + N variant structs) |
| `EnumAlloc` allocations | 1 `struct.new` | 1 `struct.new` (plus boxing allocations for heterogeneous fields) | 2 `struct.new`s (variant payload + outer wrapper) |
| `EnumDiscriminant` cost | `struct.get $parent 0` — 1 instruction, no cast | `struct.get $enum 0` — 1 instruction | `struct.get $enum 0` — 1 instruction |
| `EnumGetField` cost | `ref.cast (ref $variant)` + `struct.get $variant (i+1)` — 2 instructions | `struct.get $enum (i+1)` if homogeneous; `ref.cast` + unbox if heterogeneous — 1–4 instructions | `struct.get $enum 1` (payload) + `ref.cast` + `struct.get $variant i` — 3 instructions |
| Memory: `Option<Int>.None` | header + tag (4B) | header + tag + 8B unused `Some` slot | 2 headers + tag + payload-ref + empty payload struct |
| Memory: `Result<Int, String>.Ok(42)` | header + tag + 8B `i64` | header + tag + boxed `i64` (separate heap alloc for the box, since `Int` doesn't fit in `i31ref` for values ≥ 2³¹) | 2 headers + tag + payload-ref + 8B `i64` |
| Heterogeneous-variant handling (`Result<T, E>` with `T ≠ E`) | natural — each variant typed independently | forces field-slot type to `anyref` plus per-access boxing/unboxing | natural — each variant struct typed independently |
| Industry precedent | canonical WASM-GC sum-type shape (OCaml-on-WASM-GC, Scheme-to-WASM-GC, Kotlin/WASM) | uncommon | seen in some early ports, less common today |

**Why B (flat-max) was the closest runner-up but still rejected.** The flat-max layout looks attractive because it produces a single WASM type per Phoenix enum (smaller type section) and skips the `ref.cast` on homogeneous-variant field reads. But realistic Phoenix programs lean heavily on heterogeneous variants — `Result<Int, String>`, AST node enums where `BinOp(Expr, Expr)` and `Literal(i64)` coexist, error enums where each variant carries a different payload — and under B, every one of those pays a permanent boxing tax. The boxing tax compounds: every primitive field in a heterogeneous slot needs `ref.i31` (for small ints) or a heap box (for `Int` values ≥ 2³¹ and for `Float`); every read needs the inverse unwrap. For a `Result<Int, _>` returned from a hot-path function, that's two boxing operations per call. The B-vs-A type-section savings (one type per enum vs. N+1) are real but small for the realistic enum count in a Phoenix module (tens of enums, not thousands).

**Why C (tagged outer + payload) was rejected.** C's main appeal is conceptual cleanness — separating "enum-level identity (tag)" from "variant payload (data)" — and it would let Phoenix mutate a value's variant in place (`e = e.with_other_variant()`) by replacing only the payload pointer. Phoenix has no such pattern today and no roadmap for one. The structural cost — two heap allocations per `EnumAlloc`, four instructions per `EnumGetField` — is paid forever for a feature the language doesn't use.

**Chosen.** For each Phoenix enum (post-monomorphization name — e.g. `Color`, `Option__i64`, `Result__i64__StringRef`), declare:

- One **parent** struct type `(sub (struct (field $tag i32)))` — *not* final, so variants can subtype it. The parent holds only the discriminant. `IrType::EnumRef(name, _)` lowers to `(ref null $enum_parent)` — every SSA enum value flows through the parent type at locals, function params, block params, struct/list/enum fields.
- For each variant in declaration order: one **variant** struct type `(sub $enum_parent (struct (field $tag i32) (field $f0 …) (field $f1 …) …))` — final. The variant struct's first field is `$tag` (required by WASM-GC, which mandates subtypes start with all the supertype's fields in order). Subsequent fields are the variant's payload in Phoenix declaration order.

**Op lowering.**

- **`Op::EnumAlloc(variant_name, fields)`**:
  ```
  i32.const <variant_idx>
  <each field value pushed in order>
  struct.new $enum_VariantName
  ```
  Produces `(ref $enum_VariantName)` which upcasts to `(ref null $enum_parent)` via subtype subsumption for storage into a Phoenix enum-typed slot. One heap allocation.

- **`Op::EnumDiscriminant(value)`**:
  ```
  local.get value
  struct.get $enum_parent 0    ;; reads $tag via the parent type
  i64.extend_i32_u             ;; widen to Phoenix Int
  ```
  No `ref.cast` — reading through the parent type is well-typed because every concrete variant IS-A parent. This is the key property the subtype hierarchy buys: the discriminant test that drives every match dispatch costs nothing.

- **`Op::EnumGetField(value, variant_idx, field_idx)`**:
  ```
  local.get value
  ref.cast (ref $enum_VariantName)
  struct.get $enum_VariantName (field_idx + 1)
  ```
  The `+ 1` offset accounts for the `$tag` field occupying slot 0 of the variant struct. The `ref.cast` is structurally cheap in production WASM-GC VMs (wasmtime, V8) — it's an inline check against the runtime type tag carried in the object's header, a few cycles.

**Heterogeneous variants are naturally handled.** For `Result<Int, String>`:
```
(type $Result_int_string      (sub (struct (field $tag i32))))
(type $Result_int_string_Ok   (sub $Result_int_string (struct (field $tag i32) (field $f0 i64))))
(type $Result_int_string_Err  (sub $Result_int_string (struct (field $tag i32) (field $f0 (ref null $string)))))
```
`Ok.f0` is `i64`, `Err.f0` is a string ref. No boxing — each variant carries its field at its natural WASM type.

**Recursive enums** (e.g. `enum Tree { Node(Tree, Tree), Leaf(Int) }`) work without special handling. The parent type is declared first; variants reference `(ref null $Tree_parent)` for `Tree`-typed fields. Forward reference is safe because all enum parent types are declared before any variant struct is declared.

**Field-type restriction for slice 3.** Variant fields can be: `Int` (i64), `F64` (f64), `Bool` (i32), `StringRef` (`(ref null $string)`), `StructRef` (`(ref null $struct_idx)`), `EnumRef` (`(ref null $other_enum_parent)` — including self-recursive). Lists, maps, closures, and `dyn Trait` as variant fields error with a per-slice diagnostic — each lands in the slice that pins its own type mapping (K.5 / K.6 / etc.).

**Generic monomorphization at codegen time.** Phoenix's IR does *not* monomorphize enum layouts — `enum_layouts` stores templates with `__generic` placeholder fields (e.g., `enum_layouts["Option"] = [("Some", [StructRef("__generic")]), ("None", [])]`), and concrete type arguments live only on `EnumRef(name, args)` at use sites. The wasm32-linear backend handles this dynamically per call site (no static enum types). For wasm32-gc with statically-declared WASM types, the layouts have to be fully concrete at declaration time, so the type-decl pass *itself* runs a codegen-time monomorphization step:

1. **Collect.** Walk every function (signatures, block params, instruction `result_type`s) and every struct/enum field type recursively, collecting every distinct `EnumRef(name, args)` tuple. Each tuple is one concrete instantiation that needs its own WASM enum declaration.
2. **Substitute.** For each `(template_name, type_args)` tuple, take the template's `enum_layouts` entry and substitute `__generic` placeholders in field-type position using the position-counting heuristic: walk all variant fields in declaration order, treating each `__generic` placeholder as consuming the next slot of `type_args`. This matches the substitution wasm32-linear's `lower_match_enum` already uses to type field accesses at match sites, and works correctly for `Option<T>` (one type param, one placeholder) and `Result<T, E>` (two type params, one placeholder per variant in Phoenix-declaration-order).
3. **Declare.** For each concrete instantiation, declare a parent + per-variant subtypes as described above, indexed by the `(name, args)` tuple. `IrType::EnumRef(name, args)` → `(ref null $parent_for_that_tuple)`. The same enum template with different type args yields *different* WASM enum types — `Option<Int>` and `Option<String>` are separate parent + variant subtypes in the type section, no shared declarations.

**Known limitation (inherited from the wasm32-linear lowering).** The position-counting substitution heuristic is correct iff every type parameter of a generic enum appears at most once across all variants combined, in the same Phoenix declaration order as the type-parameter list. For Phoenix's stdlib generic enums:

- `Option<T>` — one type param `T`, one placeholder in `Some([T])` — correct.
- `Result<T, E>` — two type params in order `[T, E]`, one placeholder each in `Ok([T])` and `Err([E])`, variants declared in `Ok, Err` order — correct.

A *user-defined* generic enum like `enum Pair<T, U> { Both(T, U), Single(T) }` would mis-substitute: `Both` consumes type_args at positions 0 and 1 (correct: `T`, `U`); `Single` consumes position 2 (out of range) instead of position 0 (`T`). The same limitation is flagged in `crates/phoenix-ir/src/lower_match.rs` for the match-side path — fixing it properly requires storing per-placeholder type-parameter-name metadata in `enum_layouts` (architecture item A5 in that file's notes). For PR 6 slice 3, both wasm32 backends carry the same limitation; user-defined generic enums where type params repeat across variants error with a clear per-slice diagnostic when the position counter runs past `type_args.len()`. Non-generic user-defined enums and the stdlib `Option` / `Result` work correctly.

**Type-section ordering.** All enum *parent* types must be declared before any variant struct (so variants can subtype them) and before any function signature touching `IrType::EnumRef` (so signatures can encode the parent index). Variant structs are declared right after the parents, in (enum name × variant index) sorted order for determinism. The pipeline:

1. `declare_string_types` (K.2) — first since the 2026-06-12 string-field lift, so struct fields can reference `$string`
2. `reserve_phoenix_dyn` (K.10) + `reserve_phoenix_structs` (K.1 / K.11) — type-section *indices* reserved early so later types can reference a `$dyn_T` / a struct; bodies are filled in steps below
3. `declare_phoenix_enums` (K.4) — parents first, then variants
4. `declare_phoenix_lists` (K.7) / `declare_phoenix_closures` (K.8) / `declare_phoenix_maps` (K.9)
5. `declare_phoenix_dyn` (K.10, define) + `define_phoenix_structs` (K.11) — fills the reserved `$dyn_T` / struct slots, now that every referenced type exists
6. `close_type_rec_group` (K.10) — seals steps 1–5 into one `(rec …)` group, legalizing the forward references between them
7. `declare_imports` / `declare_print_helper` / `declare_string_helpers`
8. `declare_phoenix_functions`
9. `declare_start`
10. emission

**Why parent-first within K.4.** A variant references its parent by type-section index in its `supertype_idx`; the parent must already exist. Within the parents-then-variants order, both passes iterate enums in sorted name order for deterministic byte output.

**Implementation pointers:**
- Enum type declarations: `crates/phoenix-cranelift/src/wasm/wasm_gc/module_builder.rs::declare_phoenix_enums` (new).
- Parent/variant index lookup: `require_enum_parent_idx(name)` / `require_enum_variant_idx(name, variant_idx)` accessors.
- `IrType::EnumRef` → `ValType::Ref` mapping: `wasm_valtypes_for` in `translate.rs` (alongside K.1 / K.2 arms).
- Op arms (`EnumAlloc` / `EnumDiscriminant` / `EnumGetField`): `translate.rs::translate_instruction`.
- `TypeInterner::declare_subtype_struct(fields, super_idx) -> u32` — new method that emits a `SubType` with `supertype_idx = Some(super_idx)`, alongside the existing `declare_struct` (which is the non-subtype form used by K.1).
- Fixtures: `tests/fixtures/wasm_gc_enum.phx` exercising Option, Result (heterogeneous), and a custom 3-variant enum (covers nullary variants, multi-field variants, and the multi-arity case); `tests/fixtures/wasm_gc_enum_nested.phx` exercising reference-typed variant fields — a `StructRef` payload and a self-recursive `EnumRef` payload. Both are loaded via `include_str!` from `compile_wasm_gc.rs`.
- Nested-generic guard: `wasm_enum_field_type_for` rejects any variant field that still contains a `__generic` placeholder (an `EnumRef`/`StructRef` whose type args are unresolved, e.g. `enum Wrapper<T> { W(Option<T>) }`) with the Known-limitation diagnostic below, rather than letting it fall through to a misleading "missing struct" error. `collect_enum_instantiations` likewise skips placeholder-bearing `EnumRef`s so no junk parent types are emitted.

**Deferred to follow-up slices:** list/map/closure/`dyn` as variant field types (each needs its own type-mapping decision), pattern matching with struct destructuring (orthogonal to enum representation).

**`Option`/`Result` builtin methods landed** (`.unwrap()` / `.map()` / `.andThen()` in the K.8 closure slice; `mapErr` / `orElse` / `unwrapOrElse` / `okOr` / `filter` / `ok` / `err` / the `is*` predicates completing the combinator family on 2026-06-15) — they share this same enum lowering. See `crates/phoenix-cranelift/src/wasm/wasm_gc/option_result.rs`, pinned by `tests/fixtures/option_result.phx` + `option_result_combinators.phx`.

#### K.5. wasm32-gc Float scalar ops: arithmetic + comparison only, `print(Float)` deferred

**Decided:** 2026-06-05 (sub-decision under K, locked alongside PR 6 slice 4 — Float scalar ops on wasm32-gc).

**Context.** Float values have been carved out of wasm32-gc since slice 1 (the `print_float_is_rejected_until_a_later_slice` test pins this). The wider Float surface — constants, arithmetic, comparison, printing — has two mostly-orthogonal halves: the *scalar ops* (mechanical, ~30 lines of new code) and the *formatter* (Ryu f64-to-shortest-decimal, ~300 lines of intricate bytecode plus correctness risk). This slice ships the easy half; the formatter gets its own slice with its own design decision.

**Chosen.** Slice 4 ships the scalar Float surface:

- `Op::ConstF64(v)` → `f64.const v` (one instruction).
- `Op::FAdd` / `Op::FSub` / `Op::FMul` / `Op::FDiv` → `f64.add` / `f64.sub` / `f64.mul` / `f64.div`.
- `Op::FNeg` → `f64.neg` (one instruction, not the `0 - x` workaround the INeg path uses for Int).
- `Op::FMod` (Float `%`) is **not** in this slice. It's the one float-arithmetic op without a one-instruction lowering: WASM has no `f64.rem`, so `%` needs an `fmod` runtime helper (sign-of-dividend remainder) rather than a direct opcode. The frontend already lowers `Float % Float` → `Op::FMod` (`lower_expr.rs`), so the wasm32-gc backend rejects it with a specific diagnostic (naming the missing `f64.rem`) rather than the generic catch-all; it lands with the rest of the Float runtime surface.
  - **✅ Landed 2026-06-10 (slice 6).** `Op::FMod` lowers to a call to a synthesized `phx_fmod` helper — a port of musl's `fmod` (`src/math/fmod.c`, MIT — upstream copyright notice preserved in `THIRD-PARTY-NOTICES.md` at the repo root, since MIT requires notice retention for substantial portions; this stays within the MIT-only policy that K.6's tables were computed-from-definitions to honor) into wasm-encoder bytecode in `float_helpers.rs`, the same technique as the K.6 Ryu port. The algorithm is pure integer manipulation of the f64 bit pattern (exponent alignment + repeated mantissa subtraction + renormalization) with no rounding step, so it is *exact* — bit-identical to native Rust's `f64 % f64` (which lowers to the platform `fmod`) on every finite result, since the true remainder is always representable and unique; that includes the sign-of-dividend rule for ±0 results and subnormal operands/results. The NaN cases (`x % 0`, `inf % y`, `% NaN`) agree by class — NaN exactly when Rust `%` yields NaN — though NaN payload bits aren't pinned to any platform's. Synthesized only when an `Op::FMod` site exists (`HelperNeeds::fmod`); no `fd_write` dependency. Pinned by four differential tests in `compile_wasm_gc.rs` against Rust `%`: a 15-pair branch corpus (`float_fmod_matches_native`), hand-picked subnormal operand/result pairs covering both mantissa-normalize loops and the subnormal scale-down re-encoding (`float_fmod_subnormals_match_native` — the random sweep provably hits none of those paths), the IEEE special cases computed at runtime (`float_fmod_special_cases_run_under_wasmtime_gc`), and 100 deterministic random finite operand pairs (`float_fmod_random_bits_match_native`). The pay-per-use claim itself is pinned structurally by `fmod_free_module_carries_no_fmod_helper` — two fixtures identical except `-` vs `%` must differ by exactly one function — the K.5 analogue of K.6's `float_free_module_carries_no_ryu_tables` size pin (function count rather than byte size, because the helper is far smaller than the Ryu tables).
- `Op::FEq` / `Op::FNe` / `Op::FLt` / `Op::FGt` / `Op::FLe` / `Op::FGe` → `f64.eq` / `f64.ne` / `f64.lt` / `f64.gt` / `f64.le` / `f64.ge`. WASM `f64.<cmp>` already returns i32 0/1 — exactly Phoenix's `Bool` representation — so no widen/narrow is needed and the existing comparison-binop emit helpers carry over verbatim.

`print(Float)` keeps its carve-out from the `print_float_is_rejected_until_a_later_slice` test — the formatter slice (TBD K-number) picks among Ryu / integer-fast-path + lossy fallback / host-delegated approaches. Decoupling lets Float-arithmetic-only programs run on wasm32-gc immediately; Float-printing programs stay carved out cleanly.

**Why not include the formatter now.** The Phoenix runtime's `format_f64` calls Rust's `f64::to_string()` which delegates to Ryu/Grisu3 internally — porting that to ~300 lines of WASM bytecode is the bulk of the print-Float surface and carries real correctness risk (precomputed power-of-10 tables, shortest-decimal reduction, NaN/Infinity edge cases). A lossy fixed-precision alternative would diverge from the native backend on any high-precision value, breaking matrix consistency on every fixture that prints a non-integer Float. The right move is to land the formatter as its own slice with its own design decision and adversarial verification against the native fixture corpus.

**Matrix impact.** Any Float-arithmetic-only fixture (no `print(Float)`, no Float interpolation into `print(String)`) runs on wasm32-gc after this slice. Any Float-printing fixture stays carved out until the print-Float slice lands.

**Implementation pointers:**
- `Op::ConstF64` / F-arithmetic / F-comparison lowering: `crates/phoenix-cranelift/src/wasm/wasm_gc/translate.rs::translate_instruction` — add arms alongside the existing Int arithmetic and comparison families.
- Helper reuse: the comparison helper was generalized to `emit_cmp` and reused verbatim for f64 — every comparison family (i64, f64, i32-Bool) consumes its operands off the stack and pushes the same i32 0/1, so only the WASM instruction differs and the i32 result type is fixed in the helper. A parallel `emit_f64_binop` (f64 result) carries the `emit_i64_binop` shape over for F-arithmetic.
- `IrType::F64` → `ValType::F64` mapping: already wired in `wasm_valtypes_for` since slice 1 (unchanged).
- Fixture: extend an existing fixture or add `tests/fixtures/wasm_gc_float.phx` exercising `+`/`-`/`*`/`/`/`neg` and all six comparisons, with results funneled through `print(Bool)` so the execution tier sees them.

**Deferred to the print-Float slice:** the formatter design decision (Ryu vs integer-fast-path + lossy vs host-delegated), the K-number it lands under, the matching `phx_print_f64` helper synthesis. `Op::BuiltinCall("Float.toString", _)` (if Phoenix ever wires it as a builtin) shares the same formatter; that's the natural pair-up for the slice.

#### K.6. wasm32-gc Float-print formatter: synthesized inline Ryu, no runtime embed; both backends emit Ryu's scientific format

**Decided:** 2026-06-05 (original module-size decision).
**Amended:** 2026-06-09 (output-format pivot — see "Amendment: output format" below).

**Primary reason: module size.** This decision was specifically chosen over the "embed-and-merge the runtime" alternative on module-size grounds. Embedding `phoenix-runtime` brings ~50KB of compiled WASM into every wasm32-gc module — even a "hello world" that never prints a Float — because the merge is whole-module: dlmalloc, the Phoenix mark-sweep GC, every linear-memory string/list/map/builder runtime helper, panic infrastructure. A synthesized inline `phx_print_f64` adds ~9.6KB of precomputed power-of-5 tables (active data segments; the as-built figure — the original estimate said ~5KB, assuming ryu's "small" reconstructed-table variant, but the implementation uses ryu's full-table constants — trimmed to the f64-reachable index ranges — for bit-for-bit fidelity and zero runtime reconstruction code) plus ~1,100 instructions of bytecode across `phx_print_f64` and its `phx_ryu_*` sub-helpers — *only when the module actually calls* `print(Float)`. As measured 2026-06-10: a module whose only statement is `print(1.0)` compiles to 12,270 bytes / 1,161 instructions, vs. 269 bytes / 86 instructions for `print(1)` — so against the ~50KB embedded runtime the ratio is ~4× on a Float-printing module, and two orders of magnitude on a Float-free one (which carries neither tables nor helper; pinned structurally by `float_free_module_carries_no_ryu_tables`). Module-size matters in the WASM-GC use cases Phoenix targets (browser delivery, embedded VMs, edge runtimes) — every kilobyte adds startup latency and bandwidth.

**Supporting reasons (in order of weight):**

1. **Most of the runtime would be dead code on wasm32-gc.** Decision I's framing — "host VM handles GC; codegen emits inline" — means wasm32-gc never calls `phx_gc_alloc` / `phx_gc_set_root` / `phx_list_alloc` / `phx_map_alloc` / `phx_string_alloc` / `phx_str_*` (our strings are WASM-GC `(ref $string)`, not linear-memory `PhxFatPtr`). Embedding the runtime puts ~95% of its bytes into a module that never calls them. Synthesized inline avoids this dead-code tax.
2. **Avoids amending [decision I](#i-wasm32-gc-runtime-architecture-codegen-emitted-helpers-no-rust-runtime-crate).** Decision I's "codegen-emitted helpers, no Rust runtime crate" was deliberate. Walking it back partway ("OK for scalar helpers; not OK for GC ops") muddies a clean architectural story and would invite future creep ("if scalar print is OK, why not scalar string ops too?"). Keeping the boundary at "no runtime, period" is easier to defend.
3. **Avoids two memory models cohabiting.** Today wasm32-gc has a clean separation: WASM-GC managed heap for objects (host VM traces), ~4KB of linear memory for WASI iovec staging. Under embed-and-merge, programs would have both the host VM's managed heap *and* a linear-memory dlmalloc heap from the runtime. Two distinct allocators for one program; conceptually muddier; the host VM's GC can't trace through linear-memory references.
4. **Wider runtime-merger reuse isn't actually there.** The runtime's string / list / map / builder helpers operate on linear-memory `PhxFatPtr` / typed pointers, not WASM-GC `(ref $type)`. Calling them from wasm32-gc would require extract-marshal-allocate-copy roundtrips that defeat the K.2 / K.4 zero-copy promises. The only runtime helpers we could *actually* use are the by-value scalar ones (`phx_print_i64`, `phx_print_f64`, `phx_print_bool`) — and one of those (`phx_print_i64`) is already synthesized inline cheaply. The runtime-merge code-reduction argument shrinks to "replace 80 LOC of `phx_print_i64` synth" — not worth the 50KB module-size hit.

##### Amendment: output format (2026-06-09)

The original 2026-06-05 decision said "synthesized inline Ryu" but **did not pin down the output format** the helper would target. An empirical probe (2026-06-09, during PR 6 slice 5 Phase 2 implementation) surfaced that Rust's `f64::to_string()` — which `phoenix-runtime::format_f64` calls for non-integer values — emits **fixed-point notation, always**, with no scientific shortcut. So `(1e100).to_string()` is a 101-character string (`"10000…000"`), `(5e-324).to_string()` is a 325-character string (`"0.000…0005"`), `f64::MAX.to_string()` is 309 digits. The `ryu` crate, by contrast, picks scientific when the magnitude is large or small (`"1e100"`, `"5e-324"`) — the same *convention* used by Python `repr`, Go `fmt`, ECMAScript, and the Ryu paper itself, though not the same *bytes* (those languages emit `1e+100` with an explicit `+`, and ECMAScript prints `5.0` as `"5"`). The single source of truth for Phoenix's *runtime print/`Float→String` format* is the `ryu` crate's `Buffer::format` output, pinned byte-for-byte by `format_f64_pins_ryu_output` in `phoenix-runtime`; the wasm32-gc port targets those bytes, not any other language's. Out of scope: `phoenix-codegen`'s emission of Float default values and literals into generated Go/TypeScript/Python source (Rust `Display` today) and generated Go's wire serialization (`strconv.FormatFloat(_, 'f', -1, 64)`, fixed-point) — those produce source literals and serialized values for other languages' parsers, not Phoenix `print` output, so matrix consistency doesn't constrain them. Whether they should converge on ryu's bytes is an open question for a codegen slice, not this one.

A Ryu port that targets Rust's fixed-point output would need a ~340-byte scratch buffer (worst case `5e-324`) plus a custom fixed-point emission stage; a Ryu port that targets ryu-the-crate's scientific output needs a ~24-byte buffer and matches the published algorithm verbatim. The native-side question — which format is the right native default — surfaced at the same time: every other major language defaults to scientific for extreme magnitudes; Rust's fixed-point Display for f64 is widely criticized as a footgun. Forcing wasm32-gc to mirror it would propagate that footgun, and bloat the helper.

**Resolution:** switch **both** backends to Ryu's scientific format.

- Native `phoenix-runtime::format_f64` is rewritten to `ryu::Buffer::new().format(val).to_string()`. The integer fast-path (`val.fract() == 0.0 && in i64 range → (val as i64).to_string()`) is removed; the `-0.0`/NaN/inf branches were already handled by ryu (ryu emits `"-0.0"`, `"NaN"`, `"inf"`, `"-inf"`).
- Wasm32-gc `phx_print_f64` ports the `ryu` crate's `d2s` algorithm verbatim — including the scientific-vs-fixed dispatch heuristic. No integer fast-path. No `-0.0` special case (ryu's algorithm handles it).

**User-visible native output changes** (compared to the pre-amendment behavior):

| Input | Was | Now |
|---|---|---|
| `print(5.0)`, `print(-7.0)` | `5`, `-7` | `5.0`, `-7.0` |
| `print(0.0)`, `print(-0.0)` | `0`, `-0` | `0.0`, `-0.0` |
| `print(1e100)` | 101-char fixed-point | `1e100` |
| `print(5e-324)` | 325-char fixed-point | `5e-324` |
| `print(f64::MAX)` | 309 digits | `1.7976931348623157e308` |
| `print(0.1)`, `print(3.14)`, `print(0.30000000000000004)` | unchanged | unchanged |
| `print(NaN)`, `print(inf)`, `print(-inf)` | unchanged | unchanged |

The fixture/test impact is bounded to expectations that print *integer-valued* Floats — the only finite class whose output changes inside the fixed-point range: driver tests in `crates/phoenix-driver/tests/` — `basic.rs` (`float_addition`, `float_multiplication`, `negative_float_modulo`: `"4"`/`"10"`/`"-2"` → `"4.0"`/`"10.0"`/`"-2.0"`), `traits.rs` (`25` → `25.0`), `types_and_structs.rs` (Float struct fields), `enums_and_matching.rs` (Float enum-variant fields) — plus the `large` fixture expectations in `crates/phoenix-bench/tests/fixture_validity.rs` (tree-walk and IR-interp copies) and one native assertion in `crates/phoenix-cranelift/tests/compile_result_methods.rs` (`unwrap_or` yielding `0.0`). Tests asserting non-integer values (`78.53975`, `3.14`, …) are unchanged — ryu and Rust std agree there. Three Phase-1 wasm32-gc tests are reworked accordingly: the integer fast-path test is removed (no fast-path under ryu), the special-cases test drops its `-0.0 → "-0"` line, and the general-case trap test expands to also pin `print(5.0)`, `print(-0.0)`, and the out-of-range integer-valued case as traps until Phase 2 lands.

**Known asymmetry: print output is not re-parseable as a Phoenix literal.** The lexer has no exponent syntax — `1e100` is not a valid Float literal (the wasm trap tests write it longhand) — so scientific-notation output from `print` cannot be pasted back into Phoenix source. Pre-amendment fixed-point output didn't have this asymmetry. Not a blocker for this slice, but it creates a standing motivation for exponent literals in a future lexer slice; if that lands, it should follow ryu's no-`+` form (`1e100`, `5e-324`) so literals round-trip with output.

##### Algorithm (post-amendment)

```text
phx_print_f64(v: f64):
  Step 1: Special cases (early returns)
    - v is NaN: print "NaN\n"
    - v is +Infinity: print "inf\n"
    - v is -Infinity: print "-inf\n"
      (-0.0 is NOT special-cased — ryu's algorithm prints it as "-0.0".
       Letting it flow into d2s matches ryu's behavior verbatim.)
  Step 2: Ryu d2s (general case — finite, non-NaN, non-±inf)
    - extract sign / mantissa / exponent from f64 bit pattern
    - compute (digit_string, decimal_exponent) via ryu's mulShift on
      precomputed POW5 / POW5_INV tables, plus the shortest-decimal
      reduction loop
    - emit digits + decimal point + (optional 'e' + signed exponent)
      using ryu's scientific-vs-fixed dispatch heuristic
```

**No integer fast-path.** The pre-amendment integer fast-path delegated `5.0` to `phx_print_i64`, producing `"5"`. Under ryu, `5.0` produces `"5.0"`; the fast-path is removed (it would now produce a different output from the general case). Removing the fast-path also removes the `-0.0`-loses-its-sign-via-i64-cast surprise: under ryu, `-0.0` flows through d2s naturally and emits `"-0.0"`.

**Precomputed tables.** Two active data segments holding ryu's full-table constants, **computed from their mathematical definitions** in `ryu_tables.rs` at codegen time (a ~150-line limb bignum; runs once per process, microseconds). Two as-built corrections to the original sketch here: (1) the sketch assumed ryu's "small" feature tables — 26 reconstructed-at-runtime entries; the full tables cost more bytes but need no runtime reconstruction bytecode to port or get wrong. (2) The first implementation copied `d2s_full_table.rs` verbatim from the `ryu` crate, which would have pulled ryu's Apache-2.0/BSL-1.0 terms into the repo as a source-form redistribution; computing the entries from the published definitions (Adams, PLDI 2018) keeps the repo MIT-only — the constants are mathematical facts, and bit-equality with ryu is enforced by test (see the per-exponent sweep below) instead of by provenance:

- **`DOUBLE_POW5_INV_SPLIT`** — 291 entries × 128 bits (16 bytes) = ~4.5 KiB. Indexed for binary exponents ≥ 0; entry q is `floor(2^(bitlen(5^q) − 1 + 125) / 5^q) + 1`.
- **`DOUBLE_POW5_SPLIT`** — 325 entries × 128 bits = ~5.1 KiB. Indexed for binary exponents < 0; entry i is the top 125 bits of `5^i`.

Both segments are trimmed to the index ranges f64 inputs can actually reach (0..=290 and 1..=325; entries are bit-identical to ryu's where reachable). The reference implementation's tables carry 342 and 326 entries — sized for the algorithm family, not the f64 input range — and shipping the unreachable ~0.8 KiB in every Float-printing module would cut against this decision's whole premise. The pow5 segment starts at index 1, so `phx_ryu_d2d` addresses it through a base constant one entry-width below the segment (`RYU_POW5_SPLIT_INDEX_BASE`), keeping the index arithmetic in the bytecode unchanged.

The tables are embedded as *active* segments at fixed linear-memory offsets (`RYU_POW5_INV_SPLIT_OFFSET` / `RYU_POW5_SPLIT_OFFSET` in `module_builder.rs`'s layout map) — active rather than passive because they're plain constant loads at fixed addresses, with no `array.new_data` consumer needing a segment index. They're emitted only when the module actually prints a Float — modules that don't carry no table overhead (pinned structurally by `float_free_module_carries_no_ryu_tables`, which asserts a Float-free module is smaller than the table payload alone).

**128-bit multiplication.** WASM's i64 multiply is 64×64 → 64 (low bits only — no native widening multiply). For Ryu's `mulShift`, we need 64×64 → 128. The standard composition: split each i64 into two 32-bit halves, do four widening multiplications, sum with carries. As built this is a `phx_ryu_umul128_hi` helper function called from a `phx_ryu_mul_shift_64` helper (three calls per d2s invocation — ryu's `mul_shift_all_64` computes vr/vp/vm) rather than inlined per site: helpers keep the hand-written bytecode auditable against the ryu source, and `phx_print_f64` is cold code where call overhead is irrelevant.

**Scratch buffer.** Ryu's f64 output is bounded by 24 characters. The worst case is a negative small-magnitude value whose exponent needs 4 chars: `-2.2250738585072014e-308` (= `-f64::MIN_POSITIVE`) — note it is *not* the large-magnitude `-1.7976931348623157e308` (23 chars), because the negative exponent costs one extra character. Ryu's own `Buffer` is `[u8; 24]`. The wasm32-gc helper reserves 32 bytes (`PRINT_F64_BUF_END - PRINT_F64_BUF_START`) — the 24-char worst case plus the trailing `\n` is 25, leaving 7 bytes of headroom. The pre-amendment 64-byte buffer is reduced to 32; the Phase-1 short-literal helper (`emit_print_literal`) still fits.

**Adversarial test corpus.** Five tests in `compile_wasm_gc.rs` compare wasm32-gc's `print(Float)` output against the `ryu` crate (= native `format_f64`'s bytes):

- `float_print_matches_native` — 29 pinned literals covering every formatter branch and the positional/scientific dispatch boundaries (1e15/1e16, 1e-5/1e-6), 2^53, classic non-terminating binaries, negatives.
- `float_print_extremes_match_native` — f64::MAX and the smallest subnormal (5e-324), fed as longhand decimal literals (Phoenix's lexer takes arbitrary-length digit strings; its parser delegates to correctly-rounded `str::parse`).
- `float_print_computed_values_run_under_wasmtime_gc` — runtime-computed `-0.0`, `0.1 + 0.2`, `1.0 / 3.0`.
- `float_print_random_bits_match_native` — 200 deterministic pseudo-random (SplitMix64, pinned seed) IEEE-754 bit patterns, uniform over exponents. Strictly deterministic, so not "fuzzing" — but it covers table-index and rounding territory a hand-picked corpus can't.
- `float_print_every_binary_exponent_matches_native` — one value per IEEE binary exponent (0..=2046, SplitMix64 mantissas). The d2s table index is a function of the binary exponent alone, so this sweep exercises **every reachable entry of both power-of-5 tables** end-to-end against the oracle — it is the test that licenses computing the tables instead of copying them.

NaN / ±inf are covered separately by `print_float_special_cases_run_under_wasmtime_gc` (they short-circuit before d2s).

**Alternatives considered:**

- **Embed-and-merge the `phoenix-runtime` `phx_print_f64`.** Rejected per the primary-reason analysis above: module-size hit too large; ~95% dead code on wasm32-gc.
- **Narrow extractor pulling just `phx_print_f64` + transitive deps.** Rejected: the transitive-dep walk through Rust std's float formatter pulls in formatting machinery, integer-to-string helpers, and panic landing pads; the extractor itself is delicate code (~300 LOC) for a one-function payload.
- **Compile-time const-fold only.** Rejected as a *sole* approach: leaves runtime-computed Floats unprintable. Could be added as an optimization atop the synthesized helper in a follow-up — but not in place of it.
- **Lossy fixed-precision fallback.** Rejected: byte-for-byte divergence from native on any non-integer Float breaks matrix consistency on every fixture that prints one.
- **Mirror Rust std's fixed-point format on both sides (Option A from the 2026-06-09 review).** Rejected per the Amendment: requires ~340-byte buffer and a custom fixed-point emission stage on wasm32-gc; propagates Rust std's widely-criticized fixed-point default into a target where every other Phoenix backend would have to match.
- **Defer print(Float) entirely (Option C from the 2026-06-09 review).** Rejected: a target that traps on `print(3.14)` is shippably broken; deferring leaves slice 5 half-complete with no exit story.

**Implementation pointers** (numbering matches `float_helpers.rs`'s module-level breakdown):

- Phase 1: native `format_f64` rewrite to ryu; wasm32-gc `phx_print_f64` NaN/±inf inline branches. *Landed 2026-06-09 (reworking the obsolete 2026-06-08 fast-path version).*
- Phase 2: port ryu's d2s digit-finding + positional/scientific emission into wasm-encoder bytecode; emit the POW5 / POW5_INV tables as active data segments; replace the Phase-1 trap. *Landed 2026-06-09.*
- Phase 3: adversarial test corpus (see above) in `crates/phoenix-cranelift/tests/compile_wasm_gc.rs`. *Landed 2026-06-09.*

Code locations:
- `phx_print_f64` + `phx_ryu_*` helper synth: `crates/phoenix-cranelift/src/wasm/wasm_gc/float_helpers.rs`; table computation: `ryu_tables.rs` alongside it; linear-memory table offsets: `module_builder.rs` layout map.
- `translate_print`'s Float arm: dispatch to `phx_print_f64` when arg is `ValType::F64`. *Landed.*
- `HelperNeeds::print_f64` flag: scanner picks up `BuiltinCall("print", args)` whose `args[0]` is `IrType::F64`. *Landed.*
- Native `format_f64`: `crates/phoenix-runtime/src/lib.rs`.
- `ryu` crate dep: `crates/phoenix-runtime/Cargo.toml` (workspace dep declared in root `Cargo.toml`; also a `phoenix-cranelift` dev-dependency as the Phase 3 test oracle). Linking the crate imposes no source-form obligations on this repo; only copying its source would have (see the `ryu_tables.rs` module doc).

#### K.7. wasm32-gc `List<T>` / `ListBuilder<T>`: length-carrying wrapper struct over a shared mutable array; zero-copy freeze

**Decided 2026-06-10 (PR 6 slice 7 design lock).**

**Chosen shape.** One pair of WASM-GC types per distinct concrete element type `T` (codegen-time monomorphization, mirroring K.4's `EnumInstantiationKey` pattern), plus a builder type when the module uses `ListBuilder<T>`:

```
$arr_T     = (array (mut T_wasm))
$list_T    = (struct (field $len i64)
                     (field $data (ref null $arr_T)))
$builder_T = (struct (field $len (mut i64))
                     (field $frozen (mut i32))
                     (field $data (mut (ref null $arr_T))))
```

- `T_wasm` is the existing `IrType` → `ValType` mapping: `i64` / `f64` / `i32` for Int/Float/Bool; `(ref null $string)` / `(ref null $struct)` / `(ref null $enum_parent)` for reference types; `(ref null $list_U)` for nested lists. Reference elements are **nullable** because builder buffers need a defaultable element type for `array.new_default` (the capacity slack is null-initialized); sema guarantees no null element is ever read, the same invariant K.1/K.2 lean on for struct fields. The `$data` field is nullable for the same convention (and so intermediate values round-trip through nullable locals without casts); the array ops trap on null, which never fires.
- The array is `mut` and Phoenix-level list immutability is a **sema invariant, not a WASM one** — exactly K.2's `$bytes` precedent. No list op emits `array.set` against a frozen list's array.
- `$len` is i64 to match `List.length`'s IR result type (no width conversion on the hot read); the data array's own length is the **capacity** and may exceed `$len`.

**Why a wrapper struct (not a bare array).** A bare `(array T)` would be one allocation and the simplest lowering, but it forces `length == array.len` — which forces `ListBuilder.freeze()` to copy into an exact-size array (O(n)) and leaves no room for `$len < capacity`. The wrapper costs a second allocation and one `struct.get` per access, and buys:

- **Zero-copy `freeze()`** — `freeze` sets `$frozen = 1` and returns `struct.new $list_T($len, $data)` sharing the builder's buffer: **O(1)**, vs. native's O(n) memcpy (decision F). Behavior is identical — the frozen flag (runtime-checked, as on native) blocks all further builder mutation, so the shared buffer is never written again; only the cost model improves. The trade: up to 2× growth slack stays live until the frozen list is collected.
- Consistency with K.2's `String` shape (wrapper struct + backing array), and room for future O(1) `take`/`drop` views if a later slice wants them (today both copy, matching native's clamped-copy semantics).

**Semantics mapping (native parity).**

- `List.get` with `index < 0 || index >= len` **traps** (`unreachable` after an unsigned i64 compare — negative indices wrap to huge unsigned values, one check covers both). Native prints `runtime error: list index … out of bounds` and exits 1; wasm32-gc follows the established trap precedent (divide-by-zero, K.3-era) — non-zero exit, no message, until the panic-routing slice gives traps a `proc_exit` + stderr story.
- `List.push` copies: `array.new_default` at `len + 1`, `array.copy`, `array.set`, fresh `$list_T` — O(n), matching native immutability.
- `List.take(n)` / `List.drop(n)`: a **negative `n` is a runtime error** (interpreters abort with `take()/drop() argument must be non-negative`; native aborts in `phx_list_take`/`phx_list_drop`; wasm32-gc traps); `n > len` clamps to `len` ("take/skip at most n") and copies. **Divergence resolved 2026-06-10:** implementing this slice surfaced that the backends disagreed — both interpreters errored on negative `n` while the native runtime silently clamped to 0 (`n.max(0)`), a divergence no fixture exercised. The error semantic won (loud failure matches `List.get`'s OOB philosophy); the native clamp was removed in the same slice and pinned by `take_negative_aborts` / `drop_negative_aborts` in phoenix-runtime plus `list_take_drop_negative_traps_under_wasmtime_gc` in compile_wasm_gc.rs.
- `List.contains` equality per element type, matching native's `elements_equal`: i64/i32 compare for Int/Bool, `f64.eq` for Float (IEEE — `NaN != NaN`, `-0.0 == 0.0`), `phx_str_eq` for String (byte equality), and `ref.eq` for struct/enum elements (native compares the stored 8-byte pointer — identity, not structural — so `ref.eq` is the exact analogue).
- `ListBuilder.push` grows the buffer 2× (saturating, min 1) when `$len == array.len($data)`, like native; push or freeze on a frozen builder **traps** (native aborts with `builder was already frozen`).
- For-in iteration needs no new machinery — the frontend lowers it to `List.length` + `List.get`.

**Scope (slice 7).** `Op::ListAlloc` (literals, via `array.new_fixed`), `List.length` / `get` / `push` / `contains` / `take` / `drop`, for-in, `ListBuilder.alloc` / `push` / `freeze`. Element types: Int, Float, Bool, String, structs, enums, and nested `List<U>` (instantiations declared inner-before-outer). **Deferred:** `String.split` (returns `List<String>`; small follow-up), and **list-typed struct/enum fields** (the slice-3 field restriction stands — lifting it is its own slice because it makes type-section declaration order a dependency sort across structs/enums/lists).

**Landed 2026-06-15.** `first`/`last` (closure-free but return `Option<T>`) and every closure-taking method (`map`/`filter`/`reduce`/`flatMap`/`sortBy`/`find`/`any`/`all`) are lowered on wasm32-gc. The query methods that return `Option<T>` share the `list_option_info` helper (variant indices + valtype from the result `EnumRef`); `find`/`any`/`all` short-circuit via a `Br` out of `emit_count_loop`, matching native evaluation order. Pinned five-backend by `tests/fixtures/list_query_methods.phx` (with a side-effecting predicate proving identical short-circuit behavior) — see `crates/phoenix-cranelift/src/wasm/wasm_gc/lists.rs`.

**Alternatives rejected:**

- **Bare `(array T)` as the list.** One allocation and `array.len` as the length, but forces O(n) freeze and admits no metadata. Rejected with user 2026-06-10.
- **Wrapper + explicit capacity field** (the original PR 5 sketch `(struct len, cap, data)`). Capacity is the data array's own length — a stored copy is dead weight on every immutable list. Rejected.
- **Copying `freeze()`** (exact-size array, native cost model). No behavioral difference from zero-copy, strictly worse constant factor; the slack-retention trade was judged acceptable. Rejected with user 2026-06-10.

Code locations: `crates/phoenix-cranelift/src/wasm/wasm_gc/lists.rs` (the whole K.7 surface: instantiation collection + type declaration mirroring `enums.rs`, plus the `Op::ListAlloc` / `List.*` / `ListBuilder.*` lowering helpers), `translate.rs` (just the dispatch arms routing into `lists.rs`), `module_builder.rs` (declaration pipeline ordering: strings → structs → enums → lists, per the 2026-06-12 string-field lift).

#### K.8. wasm32-gc closures: per-signature subtype hierarchy over typed function references (`call_ref`)

**Decided 2026-06-12 (closure design lock); ✅ implemented 2026-06-12 (same day), exactly as locked.** Code: `crates/phoenix-cranelift/src/wasm/wasm_gc/closures.rs` (collection + declaration + the three op lowerings, mirroring `lists.rs`); the dead template-copy closures are skipped in lockstep by `declare_phoenix_functions` / `emit_phoenix_bodies` (`is_dead_placeholder_closure`); both wasmtime harnesses now pass `-W function-references=y,gc=y`. Matrix: `closures`, `closures_ambiguous_captures`, `closures_over_generic`, `closures_over_generic_cross_width`, and `defer_closure` run five-backend byte-identical.

**Context.** Phoenix's IR closure ABI is the env-pointer calling convention (locked during the Phase 2.4 closure-capture-ambiguity fix): a closure value's heap object *is* the environment; `Op::CallIndirect(closure, args)` passes the closure verbatim as the callee's first argument; the callee reads captures via `Op::ClosureLoadCapture(env, idx)` against its `capture_types`. Call sites never know capture layouts — that is what lets two closures with the same user signature but different captures unify through a phi. The wasm32-gc mapping must preserve exactly that property. There is no capture-store op: captures are by-value and immutable once allocated.

**Chosen shapes.** One *function type* + one open *parent struct* per distinct closure **signature** `(param_types, return_type)` (codegen-time collection over `ClosureRef` types and `ClosureAlloc` sites, mirroring K.4/K.7; declared inner-first by closure-nesting depth so higher-order signatures — a closure taking or returning a closure — resolve their nested parent refs, the K.7 list-depth pattern; signatures still carrying generic placeholders are skipped — they belong to dead template copies, same guard as K.4). One final *site subtype* per `ClosureAlloc` **target function**, carrying that closure's capture fields:

```
$fn_SIG   = (func (param (ref null struct))      ;; env — abstract, see below
                  (param P_wasm ...)
                  (result R_wasm))
$clo_SIG  = (sub (struct (field $code (ref $fn_SIG))))
$site_F   = (sub final $clo_SIG
              (struct (field $code (ref $fn_SIG))
                      (field $cap0 T0_wasm) ...))   ;; immutable capture fields
```

- `IrType::ClosureRef { .. }` → `(ref null $clo_SIG)`.
- `Op::ClosureAlloc(F, caps)` → `ref.func $F` + capture values + `struct.new $site_F`. (`ref.func` requires the target in an `(elem declare func …)` segment; the collection pass emits one.)
- `Op::CallIndirect(clo, args)` → `struct.get $clo_SIG $code` + `call_ref $fn_SIG`, passing `clo` as the env argument — statically signature-checked, no table, no runtime signature test.
- `Op::ClosureLoadCapture(env, idx)` → `ref.cast $site_F` + `struct.get` field `idx + 1` — the K.4 enum parent→variant get-field pattern reapplied. The cast target is known statically: the op only occurs inside `F`'s own body.

**Env parameter is abstract `(ref null struct)`, not `(ref null $clo_SIG)`.** The precise typing would make `$fn_SIG` and `$clo_SIG` mutually recursive, requiring `(rec …)` group emission in the type interner. The abstract typing breaks the cycle with no interner changes, and loses nothing real: the callee must `ref.cast` the env down to its concrete `$site_F` either way (parent → site), and every *user-controlled* param/result stays precisely typed, so `call_ref` still statically checks everything a caller can get wrong. Revisit-trigger: if the interner grows rec-group support for another reason (struct↔enum field cycles are the likely customer), tightening the env type is a mechanical follow-up.

**Dispatch mechanism: `call_ref`, not a funcref table.** Verified empirically (2026-06-12) on the pinned wasmtime v45: `call_ref` validates and runs with `-W function-references=y,gc=y` and is rejected under `-W gc=y` alone — wasmtime gates the two proposals separately even though GC formally builds on function-references. The slice therefore adds `function-references=y` to every wasmtime invocation that runs wasm32-gc modules (the backend-matrix harness, the `compile_wasm_gc.rs` harness; CI inherits both). The rejected alternative — module-wide funcref table + `call_indirect` — works under today's flags but re-implements wasm32-linear's machinery (table, element segments, index bookkeeping, per-call runtime signature checks) inside the backend whose type system exists to make that unnecessary.

**Alternatives rejected** (2026-06-12, with user): funcref table + `call_indirect` (above); uniform boxed env `(struct funcref (ref array anyref))` — every scalar capture boxes on alloc and unbox-casts on read, exactly the overhead typed fields eliminate; precise env typing via rec groups (deferred, not rejected outright — see revisit-trigger).

**Scope.** The K.8 slice ships the core only: `ClosureAlloc` / `CallIndirect` / `ClosureLoadCapture` + the `ClosureRef` type mapping — unblocking `closures.phx`, `closures_ambiguous_captures.phx`, `defer_closure.phx`, and the generic-closure fixtures (monomorphization already clones closure bodies per specialization at the IR level; the backend sees concrete types). **Follow-up slices:** Option/Result method builtins (`option_result.phx`) — ✅ **landed 2026-06-12** as `option_result.rs`: `map` / `andThen` (closure-taking, reusing K.8's `emit_closure_call`), `unwrap` / `unwrapOr`, and the `isOk` / `isErr` / `isSome` / `isNone` predicates, all lowered in terms of the K.4 enum representation (no new declarations). The slice also added **partial-generic enum resolution** (next paragraph), which unblocked `defer_try.phx` as a bonus. The **List closure methods** — ✅ **landed 2026-06-12** in `lists.rs`: `map` / `filter` / `reduce` / `flatMap` / `sortBy`, each walking the receiver's `$data` array and calling a user closure per element via `emit_closure_call` (no GC rooting — the host VM traces; no runtime merge — synthesized inline, unlike wasm32-linear which calls embedded-runtime helpers). `sortBy` originally shipped here as a **stable insertion sort** (matching what wasm32-linear and the interpreters carried at the time: `cmp <= 0` keeps ties in input order), trading native's O(n log n) for O(n²) to favor structured `block`/`loop`s over merge sort's many-edged CFG. **Upgraded to bottom-up iterative merge sort in the Phase 2.4 close (2026-06-17)** — once the bench corpus exercised 100k-element sorts (`sort_ints`), the O(n²) cost was ~1000× native, so both WASM backends were ported to the same O(n log n) merge sort native and the interpreters use. Output stays byte-identical for any total order under the shared `cmp <= 0` stability rule (the merge favors the left run on ties). `filter` relies on the K.7 `$len < capacity` invariant (sizes the array at the input length, reports the kept count); `flatMap` reallocates to exactly `out_len + sub_len` per sublist (O(n²), fine for the small lists the method sees, and self-contained — needs only `$arr_U`/`$list_U`). This unblocked the five sortBy matrix fixtures; `collections.phx` exercises `map`/`filter`/`reduce`/`flatMap` too but stays matrix-skipped on its last blocker, `Map` (the maps slice).

**Partial-generic enum resolution (2026-06-12).** A `Result`-returning function whose `Err` branch leaves the `T` slot unconstrained widens its Ok/Err join block param to `Result<__generic, E>` (and symmetrically `Result<i64, __generic>` when the `Ok` branch is the unpinned one) — a type with no declared nominal WASM identity, since the K.4 collection skips placeholder-bearing instantiations. At runtime such a value *is* the unique concrete `Result<T, E>` the program declares with matching non-placeholder slots. `ModuleBuilder::canonical_enum_key` resolves it: same name and arity, every non-placeholder slot equal, exactly one concrete sibling → use it; zero or multiple → leave unchanged and let `require_*` error. Applied inside `require_enum_parent_idx` / `require_enum_variant_idx`, so every enum lookup (block params, `EnumAlloc`, the Option/Result builtins) flows through it. This was the K.4 known limitation that previously forced `option_result.phx` (via its `divide` helper) and `defer_try.phx` onto the skip list; both now run five-backend.

#### K.9. wasm32-gc `Map<K,V>`: ordered association over parallel arrays, not a hash table

**Decided 2026-06-12 (map design lock); ✅ implemented 2026-06-14, exactly as locked.** Code: `crates/phoenix-cranelift/src/wasm/wasm_gc/maps.rs` (collection + `$map_KV` declaration + the literal and seven method lowerings, mirroring `lists.rs`); `collections.phx` (now exercising the K.8 list closure methods five-backend at last) and `map_hash_many_keys.phx` run byte-identical. **Cross-backend divergence resolved alongside:** a map literal with duplicate keys (`{"a":1,"a":3}`) dedups **last-wins, first-position-kept** in the compiled backends (native / wasm32-linear via `phx_map_from_pairs`, and now wasm32-gc) but the two interpreters previously kept all entries — a map can't hold two same-key entries, so the interpreters were wrong. Both now dedup at literal construction; pinned five-backend by the new `map_duplicate_keys.phx` matrix fixture. No fixture had exercised it, so the matrix hadn't caught it — same latent shape as the take/drop divergence the lists slice surfaced. **Float key equality unified at the same time:** the interpreters previously compared map keys with their IEEE `==` (`±0.0` equal, `NaN` never-equal), diverging from the **byte-wise** float key comparison native and wasm32-gc use (see *"`Map<Float,V>` uses byte-wise key comparison"* above). Both interpreters now route *every* map-key comparison — literal dedup **and** `get` / `contains` / `set` / `remove` — through a `map_key_eq` helper (`crates/phoenix-{interp,ir-interp}/src/value.rs`) that compares float keys by bits, so `Map` semantics are byte-identical across all five backends; pinned by `map_key_eq_is_byte_wise_for_floats` unit tests in each crate. (This closes only the *map-key* slice of the broader float-equality drift flagged earlier in this doc — `==` in expressions and `List.contains` are still IEEE and out of scope here.) **Native `Bool`-key bug fixed alongside:** adding Bool-key matrix coverage surfaced that native (and codegen generally) stored a `Bool` with a 1-byte `i8` write into its full 8-byte container slot, leaving 7 uninitialized bytes that the type-erased map runtime then hashed — so every `Map<Bool,_>` lookup and `List<Bool>.contains` missed on native. `TypeLayout::store` now zero-extends sub-slot scalars; pinned five-backend by `map_bool_keys.phx`. See [phase-2.md §Bugs closed in this phase](phases/phase-2.md#bugs-closed-in-this-phase).

**Context.** Native's `Map<K,V>` (`phoenix-runtime/src/map_methods.rs`) is an FNV-1a open-addressing hash table with linear probing, tombstones, 70%-load rehashing, *plus* a parallel `u32` insertion-order array so `keys()`/`values()` iterate in first-insertion order (the contract Phoenix shares with Python/JS dicts). The crucial observation: **nothing about the hash table is observable.** The only observable surface is (a) key-equality lookup (`get` / `contains` / `set` / `remove`), (b) `length`, and (c) **insertion-order** `keys()`/`values()`. The hash table is purely a lookup-speed optimization.

**Chosen representation — ordered association via parallel arrays.** One pair of WASM-GC types per distinct concrete `(K, V)` (codegen-time collection, mirroring K.4/K.7):

```
$arr_idx = (array (mut i32))   // shared; declared once
$map_KV  = (struct (field $len  i64)
                   (field $keys (ref null $arr_K))
                   (field $vals (ref null $arr_V))
                   (field $idx  (ref null $arr_idx)))
```

`$arr_K` / `$arr_V` are **the same array types K.7 declares for `List<K>` / `List<V>`** — `keys()` / `values()` therefore just `struct.new $list_K($len, $keys)` (wrap the existing array as a list, an O(1) view since the arrays are immutable). The map collection pass ensures `List<K>` and `List<V>`'s array+struct types exist (they're needed by `keys()`/`values()` anyway). Entries are stored in **insertion order** directly in the arrays, so order preservation is structural — no separate order array, no rehash to keep ordered.

The 4th field `$idx` (added at the Phase-2.4 close, driven by the `hash_map_churn` bench — originally ~380× slower than native: 100k builder inserts + 200k lookups ran in ~34s) is an open-addressing hash *index* over a single shared `$arr_idx = (array (mut i32))` (declared once), each slot holding a *slot index* into the still-insertion-ordered `$keys`/`$vals` (or `-1` for empty), sized to a power of two ≥ `max(8, 2*len)` (≤50% load). It is a pure, **non-observable** acceleration structure: it makes `get`/`contains` **O(1)** and literal/`set`/`remove`/`MapBuilder.freeze` construction-dedup **O(n)** (hash-insert) rather than the O(n)/O(n²) a linear scan would cost. The hash is *not* matched to native's FNV-1a — equality, not hash, is the contract, so any consistent function works (Int/Bool/Float mix the i64/bits; String FNV-1a's the `$bytes` window; Float still routes equality byte-wise). The arrays stay insertion-ordered, so `keys()`/`values()`/`length` and all output remain **byte-identical** across all five backends — only speed changes (`hash_map_churn` now ~0.08s). Code: `crates/phoenix-cranelift/src/wasm/wasm_gc/map_hash_index.rs` (the `emit_map_hash` / `emit_index_lookup` / `emit_index_insert` primitives) + `crates/phoenix-cranelift/src/wasm/wasm_gc/maps.rs` (the lowerings).

**Operation semantics (all observably identical to native; lookup/construction are O(1)/O(n) via the `$idx` index):**

- `Op::MapAlloc(pairs)` (literal) — build `$keys`/`$vals` of the literal's length, then insert each pair with **last-wins-on-duplicate, first-position-kept** (probe the `$idx` hash index; if present, overwrite the value in place; else append and index it). Matches native's `from_pairs`. Final `$len` may be < the array length when the literal had duplicate keys (the K.7 `$len < capacity` invariant).
- `Map.get(k) -> Option<V>` — `$idx` index probe; `Some(vals[i])` at the key-equal slot `i`, else `None`. (Native returns ptr-or-null wrapped to Option; wasm32-gc builds the `Option<V>` enum directly via K.4, with K.4 partial-generic resolution covering the result type.)
- `Map.contains(k) -> Bool`, `Map.length() -> Int` — index probe / `$len`.
- `Map.set(k, v) -> Map` / `Map.remove(k) -> Map` — copy-on-write into fresh arrays (immutable API), rebuilding a fresh `$idx` over the result; `set` overwrites in place if present (position kept) else appends; `remove` drops the entry, compacting order.
- `Map.keys() -> List<K>` / `Map.values() -> List<V>` — `struct.new $list_K($len, $keys)` (and `$list_V`).

**Key equality dispatch** (on `K`'s WASM ValType, matching native's `keys_equal` → `elements_equal`): `Int` → `i64.eq`; `Bool` → `i32.eq`; `Float` → `i64.reinterpret_f64` + `i64.eq` (**byte-wise**, per the existing *"`Map<Float,V>` uses byte-wise key comparison"* decision — `NaN == NaN` same bits, `-0.0 ≠ +0.0`, deliberately *not* IEEE `f64.eq`); `String` → the `phx_str_eq` helper (content equality). Ref-typed keys (struct/enum) error with a per-slice diagnostic until a fixture needs them (the identity-vs-structural choice is deferred with them).

**Why not the faithful hash table.** Porting FNV-1a + open addressing + linear probing + tombstones + the order array + rehashing into hand-written bytecode is several hundred intricate, bug-prone instructions for **zero observable benefit** at fixture scale — and the O(1) advantage is invisible to the matrix (`map_hash_many_keys.phx`'s 100 inserts are trivial either way). The ordered-association form is observably byte-identical, reuses K.7 wholesale, and makes insertion-order preservation structural rather than a rehash-invariant to maintain.

**Scope.** Core `Map`: literal (`Op::MapAlloc`), `get` / `contains` / `length` / `set` / `remove` / `keys` / `values`, key types Int/Float/Bool/String — unblocking `collections.phx` (its last blocker; also exercises the K.8 list closure methods five-backend at last) and `map_hash_many_keys.phx`. **Deferred:** `MapBuilder` (`alloc`/`set`/`freeze` — no matrix fixture; bench-corpus-only) and ref-typed keys, each to the slice that needs it. Code: `crates/phoenix-cranelift/src/wasm/wasm_gc/maps.rs` (collection + declaration + lowerings, mirroring `lists.rs`).

#### K.10. wasm32-gc `dyn Trait`: per-trait typed-funcref vtable struct, trampolines, `call_ref`

**Decided 2026-06-14 (dyn design lock). Implemented 2026-06-15 — five of the six `traits_dyn*.phx` fixtures landed immediately; the sixth (`dyn` in a *struct field*) followed the same day once K.11 generalized reference-typed struct fields. All six now pass five-backend byte-for-byte.**

**Context.** The IR pre-resolves each `dyn` method call to a slot index (`Op::DynCall(trait, slot, recv, args)`) and registers a per-`(concrete, trait)` vtable in `IrModule::dyn_vtables` as `Vec<(method_name, FuncId)>` in trait-declaration order (slot = index). Native emits a rodata function-pointer table; wasm32-linear a data-section i32-index table dispatched via `call_indirect`. Both reach the concrete methods — but a `dyn` call site only knows the *abstract* receiver, while the concrete methods are typed `self: Circle`. So a uniform-signature bridge is unavoidable on any backend. K.8 already established typed `call_ref` + the `function-references` feature + the abstract-receiver-cast pattern, which this reuses wholesale.

**Chosen shapes.** Per trait `T` with methods `m0..mₙ` (collected from the `dyn_vtables` keys; one set of types per trait that's actually coerced to `dyn`):

```
$dynfn_T_i = (func (param (ref null struct))   ;; abstract self
                   (param P_i ...) (result R_i))   ;; one func type per method slot
$vtable_T  = (struct (field $m0 (ref $dynfn_T_0)) ;; non-null typed funcrefs,
                     (field $m1 (ref $dynfn_T_1)) ;; heterogeneous (m0→String, m1→Int…)
                     ...)
$dyn_T     = (struct (field $data (ref null struct))
                     (field $vt   (ref null $vtable_T)))
```

`$dynfn_T_i`'s `self` is the **abstract** `(ref null struct)` — exactly K.8's env typing, and for the same reason: the dispatched function must accept any concrete receiver. (`struct` is the abstract top of the GC struct hierarchy, covering every dyn-able concrete: structs, K.4 enum parents, K.7 lists, K.9 maps, strings — primitives can't be `dyn`, sema enforces.) `IrType::DynRef(T)` → `(ref null $dyn_T)` — a single-slot ref, so it slots uniformly into params / returns / locals / struct fields / list elements with no special-casing.

**Trampolines bridge abstract→concrete.** A `dyn` value's data is `(ref null struct)`, but `Circle.draw` expects `(ref $Circle)`. So per `(trait T, concrete C, slot i)` the backend synthesizes a trampoline `tramp_T_C_i(self: (ref null struct), args…) -> Rᵢ { ref.cast self to (ref $C); <push args>; call C.mᵢ }` — its `ref.func` fills the vtable. (Identical in spirit to a closure's `$site` cast in `ClosureLoadCapture`.) Concrete `C` resolves to its K.1 struct index (`ref.cast` target); a non-struct concrete (an enum `impl`) errors until a fixture needs it.

**One shared vtable instance per `(trait, concrete)`, via a global.** The vtable for a `(T, C)` pair is identical for every value, so it's a WASM global `(ref null $vtable_T)` built once — `(global $vt_T_C (ref null $vtable_T) (struct.new $vtable_T (ref.func tramp_T_C_0) …))` — and reused: a `List<dyn Shape>` of 100 elements allocates the vtable once, not 100×. (`struct.new` in a global init expression validates under `-W function-references=y,gc=y` on the pinned wasmtime — verified 2026-06-14.) The trampoline `ref.func`s join the K.8 `(elem declare func …)` segment.

**Lowering.**
- `Op::DynAlloc(T, C, val)` → `struct.new $dyn_T(val, global.get $vt_T_C)` (the concrete `val` upcasts implicitly to `(ref null struct)`).
- `Op::DynCall(T, slot, recv, args)` → push `recv.$data`, push `args`, push `struct.get $vt_T_C? $m{slot}` (read the funcref from `recv.$vt`), `call_ref $dynfn_T_slot`. Statically type-checked; no table, no runtime signature test.
- `Op::UnresolvedDynAlloc` never reaches the backend (monomorphization resolves every one to a concrete `DynAlloc` — verified).

**Section ordering.** Trampolines are *deferred-body* functions (they `call` user `FuncId`s, whose indices exist only after `declare_phoenix_functions`): their signatures are declared right after the user functions and their bodies emitted right after the user bodies, keeping the function/code section parallelism `ModuleBuilder::finish` guards. The vtable globals (which `ref.func` the trampolines) are emitted before the function bodies — globals need only the trampoline indices, which exist after `declare_dyn_trampolines` — so each `Op::DynAlloc`'s `global.get` resolves when its body is translated.

**Type declaration: one rec group + `$dyn_T` index reservation (implementation, 2026-06-15).** `dyn` is the first wasm32-gc feature whose types genuinely *cycle* across the declaration order: a `dyn` method returning a `List` needs lists declared *before* `$dynfn` (`traits_dyn_ret`), while a `List<dyn T>` element needs `$dyn_T` declared *before* lists (`traits_dyn_list`) — no single linear order satisfies both across the independently-compiled fixtures. This is the "rec group customer" K.8 anticipated. Resolved in two parts:
- **All wasm32-gc types emit as one explicit `(rec …)` group.** `TypeInterner` gains an opt-in *buffered* mode (wasm32-gc only; wasm32-linear keeps its byte-identical immediate path and interns no GC types): every struct / array / subtype / `$fn_SIG` / `$dynfn` declaration buffers as a `SubType`, and `close_rec_group()` — called once after the last GC-type pass and **before** the WASI `fd_write` import type is interned — flushes them as a single rec group. A rec group makes every member's forward references legal. The import (and the user / helper / trampoline / `_start` signatures interned afterwards) emit *standalone*, because a func type canonicalized as a member of the big rec group is not type-compatible with the host's standalone `fd_write`. Type indices are tracked by an explicit counter, not `TypeSection::len()` (which counts a flushed rec group as one entry, undercounting its members).
- **`$dyn_T` indices are *reserved* before structs / lists.** A rec group legalizes a forward reference in the *encoding*, but the referring type still needs the referent's *index* at construction time. So `reserve_types` runs early (right after string types, before structs) and reserves one type-section slot per trait for `$dyn_T`, recording it so the `IrType::DynRef` → valtype mapping resolves during struct / list declaration; `define_types` later fills those slots (and declares `$dynfn` / `$vtable_T` at fresh indices) once method param/return types are available. `$dyn_T` (low index) points at `$vtable_T` (high index) as a forward reference — legal inside the group.

**Why not a funcref table + `call_indirect`** (the wasm32-linear shape). It still needs the same trampolines (`call_indirect` also requires a uniform signature), and re-introduces a function table + element segments + per-slot interned types + index bookkeeping that `call_ref` makes unnecessary. The typed-funcref vtable is GC-native, statically checked, and consistent with K.8 (same feature flag, same cast pattern). Rejected with user 2026-06-14.

**Scope (as shipped).** The dyn core (`$dynfn`/`$vtable`/`$dyn` types, trampolines, vtable globals, `DynAlloc`, `DynCall`, `DynRef` valtype) + `DynRef` wired into function params / returns / locals (the dyn value crossing a `call` boundary) and **list elements** (K.7) — landing five of the six `traits_dyn*.phx` fixtures five-backend: `traits_dyn` (single-method), `traits_dyn_multi` (multi-method, Int/String args), `traits_dyn_ret` (Void + `List<Int>` return), `traits_dyn_factory` (dyn return position), `traits_dyn_list` (`List<dyn>` + `sortBy`). Code: `crates/phoenix-cranelift/src/wasm/wasm_gc/dyn_trait.rs` (collection + reserve/define type declaration + trampoline/global declaration + `DynAlloc`/`DynCall` lowering, mirroring `closures.rs`); rec-group machinery in `crates/phoenix-cranelift/src/wasm/type_interner.rs`.

**`dyn` as a *struct field* (`traits_dyn_field`) — resolved 2026-06-15 by K.11.** This fixture was initially shipped skipped, not for any dyn-specific reason but because reference-typed *struct fields* were not lowered on wasm32-gc at all (a plain `struct Outer { inner: Inner }` was rejected identically). K.11 generalized struct fields to every reference type using this entry's reserve/define rec-group machinery; `traits_dyn_field` unskipped with it. The `dyn` ABI needed no further work — `IrType::DynRef(T)` → `(ref null $dyn_T)` already slots into a field like any other single-slot ref.

---

#### K.11. wasm32-gc reference-typed struct fields: reserve struct indices early, define bodies late

**Decided & implemented 2026-06-15 (full generalization over a dyn-field-only).**

**Context.** K.1 declared each Phoenix struct as a nominal WASM-GC `(struct …)` but supported only scalar (`Int`/`Float`/`Bool`) and, later, `String` fields; every reference-typed field (nested `StructRef`, `EnumRef`, `ListRef`, `MapRef`, `ClosureRef`, `DynRef`) errored with a per-field diagnostic. The blocker was never the field encoding — `Op::StructAlloc` / `StructGetField` / `StructSetField` lower as plain `struct.new` / `struct.get` / `struct.set` by field index and are already valtype-agnostic, and `struct.get` binds its result through the standard `single_slot` mapping — but the field *type declaration*: a struct field referencing another struct (or a list / map / enum / closure / dyn type) needs that type's section index at field-build time, and the declaration passes ran structs *before* enums / lists / maps / closures / dyn, so the index didn't exist yet. Cross-struct references are moreover cyclic (a struct field holding a list whose element is a struct).

**Chosen.** Struct fields map through the *same* `wasm_valtypes_for` single-slot mapping as every other position — a reference field is `(ref null $T)`, mutable (K.1's all-fields-mutable rule). No field-specific type logic survives; the scalar-only `wasm_field_type_for` is replaced by a delegation to `single_slot`. The declaration-order cycle is dissolved with the K.10 rec-group machinery:

- **Reserve struct indices early.** `reserve_phoenix_structs` runs right after the string types — before enums / lists / maps / closures / dyn — reserving one type-section slot per concrete (non-template) struct and recording its `name → idx` and field count. Every later type that references a struct (an enum variant payload, a `List<MyStruct>` element, a `dyn` method param) therefore resolves a real index.
- **Define struct bodies late.** `define_phoenix_structs` runs after `declare_phoenix_dyn` (so enums / lists / maps / closures / dyn all exist) and fills each reserved slot with its field types via `single_slot`. A field referencing a *later*-indexed type (e.g. a struct holding a `List` whose `$list_T` index is higher) is a forward reference — legal because the whole GC type graph emits as one rec group (K.10).

Type *discovery* needed no change: `collect_list_elems` / `collect_enum_instantiations` / `collect_map_kvs` / the closure-signature collector already walk `struct_layouts.values()`, so a list / map / enum / closure type that appears *only* as a struct field was already being declared — it just couldn't be consumed.

**Scope.** All six reference-field kinds land together (struct / enum / list / map / closure / dyn), unblocking `traits_dyn_field` plus nested-struct / list / map / enum / closure fields. The two `struct_with_*_field_is_rejected_until_a_later_slice` guard tests become positive round-trips. Code: `reserve_phoenix_structs` / `define_phoenix_structs` in `crates/phoenix-cranelift/src/wasm/wasm_gc/module_builder.rs`; pipeline reorder in `wasm_gc/mod.rs`; the reserve/define interner primitives are the K.10 ones in `wasm/type_interner.rs`.

---

#### K.12. Phantom-parameter enum inference: expected-type pinning + a partial-generic IR verifier invariant

**Decided & implemented 2026-06-15 (root-cause fix in sema over backend-side or monomorphization repair; cross-backend output determinism declared non-negotiable).**

**Problem.** A constructor with a *phantom* type parameter — one not determined by its arguments — leaves that parameter unbound: `Ok(99)` fixes `Result`'s `T` but not `E`; `None` fixes neither of `Option`'s. In a *concrete* (non-generic) function, sema's bottom-up checker recorded e.g. `Result<Int, free-var>`, which monomorphization erases to `Result<Int, __generic>`. The native and tree-walk/IR interpreters tolerate it (their enums are structural), but wasm32-gc enums are *nominal* (K.4): when two instantiations of a template coexist (`Result<Int,String>` and `Result<Int,Int>`), a `Result<Int, __generic>` reference can't be pinned to a unique nominal type, and the build fails. So the *same program* produced output on some backends and a compile error on others — a determinism break the user ruled non-negotiable.

**Why sema, not the backend or a mono pass.** The defect is that `Ok(99)`-in-a-`Result<Int,String>`-context is *typed wrong* — the context is right there and a complete checker uses it. Fixing it at the wasm32-gc backend (or in a post-hoc monomorphization repair pass) would let the wrong type into the IR and patch it downstream, leaving sema's `expr_types` — consumed by every backend, the IR interpreter, and future tooling — inaccurate. The root-cause fix is expected-type propagation, which is also reusable type-system infrastructure (it subsumes the existing `None`/empty-literal special-casing and is the foundation a future full bidirectional checker builds on).

**Chosen: scoped expected-type pinning, not full bidirectional checking.** `check_expr` stays bottom-up (synthesizing). At each boundary where an expected type is known — `let` with annotation, explicit `return`, lambda/implicit return, function/method call arguments, struct/enum constructor arguments, collection elements, `if`/`match` branches, and *nested* constructor arguments — `pin_inferred_type_to_annotation` refines the expression's recorded type toward the concrete expected type. It only fills type-var holes (`Result<Int, free-var>` → `Result<Int, String>`); it is a no-op on already-concrete types and never changes diagnostics or what the checker accepts/rejects — so it is far lower-risk than threading an expected type through `check_expr`. It recurses structurally (collection elements *and the container itself*, `if`/`match` arms *and* the branch expression itself, enum- and struct-constructor arguments via the declared type's variant/field types) so deeply-nested phantoms (`Ok(None): Result<Option<String>, String>`) resolve. The container must be pinned alongside its elements because `check_list_literal`/`check_map_literal` record the container as `List<first_element_type>` / `Map<first_key, first_value>` with no cross-element unification — so a phantom-typed *first* element (`[None, Some(1)]: List<Option<Int>>`) leaves the container `List<Option<?>>`, and `lower_list_literal` uses that recorded type as the `ListAlloc` result type the verifier checks. The struct-constructor case is what resolves a *sole-phantom-field* generic struct (`Box(None)` where `Box<T> { v: Option<T> }`, against `Box<Int>`): the param `T` can't be recovered from the argument's own type, so it must come from the outer declared type — exactly what this propagation supplies. A subtlety fixed alongside: `check_block_type` re-ran `infer_expr_type` on a `return` expression *after* `check_return` had pinned it, clobbering the pin — it now reads the already-recorded type instead.

**The IR verifier invariant (the determinism guarantee).** `phoenix_ir::verify` rejects any concrete function whose value types contain `__generic` *as an enum type argument* (`Result<Int, __generic>`, `Option<__generic>`, `List<Option<__generic>>` — recursing through containers). This is the postcondition of complete inference and a hard, backend-agnostic error: a program either type-resolves fully (and runs identically everywhere) or is rejected everywhere — never one backend's output vs another's compile error. It also subsumes the K.4 nested-generic-variant limitation (`enum Wrapper<T> { W(Option<T>) … }`), which now surfaces here rather than deep in wasm32-gc codegen.

**Why scoped to *enum* arguments.** A *bare* `__generic`, or one in a list/map *element* (`List<__generic>`), a struct argument, or a closure parameter/return, comes from **inert** sources — a dead generic-closure copy's erased capture, an unconstrained empty literal no nominal codegen consumes — that run identically on every backend. A blanket "no nested `__generic`" invariant rejected those working programs (verified: it broke `closures_over_generic`, `list_of_options`, et al.). Enum arguments are precisely where phantom-parameter constructors create the nominal-ambiguity divergence, so the invariant targets them and recurses *through* the other containers to catch an enum nested inside.

**Code.** `pin_inferred_type_to_annotation` and its `if`/`match`/constructor recursion (the enum/struct field-type lookup is `constructor_field_types`) in `crates/phoenix-sema/src/check_stmt.rs`; boundary call sites in `check_stmt.rs` / `checker.rs` / `check_expr.rs` / `check_expr_call.rs`; the clobber fix in `checker.rs::check_block_type`; `IrType::contains_placeholder_in_enum_arg` in `crates/phoenix-ir/src/types.rs`; `verify_no_partial_generic_types` in `crates/phoenix-ir/src/verify.rs`. **Tests.** Cross-backend behavior is pinned by two matrix fixtures: `tests/fixtures/partial_generic_enum_inference.phx` (the phantom-constructor boundaries — `let`/explicit-return/lambda/function-level implicit-return/positional-and-named-call-arg/method-arg/nested-constructor/collection-element/`if`-`else-if`-`match`-arm/non-generic-struct-field — including bare implicit-return `bareNone`/`bareOk` that guard the `check_block_type` clobber fix) and `tests/fixtures/generic_annotated_empty_collections.phx` (the guard's *type-var-annotation* branch: `let xs: List<T> = []` stays unpinned and lowers to an inert `List<__generic>`, including the `sortBy`-on-empty case that exercises the wasm32-gc `__generic` ref branch). The verifier walk and the inert/flagged distinction have unit coverage in `verify.rs` (`partial_generic_type_tests`) and `types.rs`; a sema-level differential `expr_types` harness remains future work (see below).

**Toward full bidirectional inference (recorded prerequisites).** The scoped pinning above *is* partial bidirectional inference, applied at concrete boundaries. Going to **full** bidirectional checking — threading an `expected: Option<&Type>` through `check_expr` so every expression is checked against its context — is the long-term direction but needs test-coverage investment *first*, because the current suite cannot catch its regressions:
- **Error-message coverage.** Only ~5 golden diagnostic snapshots exist. Bidirectional checking routinely changes which error fires, where, and how it reads; build a broad error-message snapshot corpus before the rewrite or diagnostic regressions ship silently.
- **Valid-program acceptance breadth.** ~150 `assert_no_error` tests + the matrix fixtures sample a finite slice of a combinatorial space. Full bidirectional flips behavior for *every* expression-in-context (nested generics, container literals, branch unification, closure-return inference); expand the accepted-program corpus to cover that tail.
- **Differential inference harness.** Nothing today diffs inferred types before/after a change. Add a harness that snapshots every fixture's `expr_types` so a diff shows exactly what inference changed — the safety net the boundary-by-boundary approach lacked.

To promote the verifier invariant from *enum arguments* to *all* `__generic` (a true "no unresolved inference reaches a backend" guarantee), two inert sources must first be eliminated rather than tolerated: dead generic-closure copies that reach codegen with `__generic` captures, and unconstrained empty-literal placeholders; plus the K.4 nested-generic-variant *monomorphization* limitation (Phase 4) must be lifted so those programs resolve instead of being rejected.

---

### Phase 2.5 JavaScript interop

Subordinate decisions for the Phase 2.5 JS-interop layer. Each pins a scope or ABI contract before any code lands so the marshalling model, the glue-artifact shape, and the test-host surface don't drift mid-phase. Phase-level scope summary and exit criteria live in [phase-2.md §2.5](phases/phase-2.md#25-javascript-interop) (matching the location pattern used by §2.3 / §2.4 / §2.6 / §2.7).

The framing that produced these: Phase 2.4's [decision B (wasmtime exit runtime)](#b-exit-criteria-runtime-wasmtime-cli) and [decision C (WASI-only host surface)](#c-host-import-surface-wasi-preview1-only) both explicitly named Phase 2.5 as the slot where browser/Node execution and Phoenix-defined custom imports arrive. 2.5 cashes that in. Three assumptions baked into the original [phase-2.md §2.5](phases/phase-2.md#25-javascript-interop) stub turned out not to hold: it depended on a package manager (Phase 3.1) that does not exist; its example used `async`/`await` that the language does not have until Phase 4.3; and an early draft scoped `extern js` as WASM-only, which has been revised.

#### A0. Parity model: extern functions are a uniform host-FFI boundary

**Decided:** 2026-06-17 (revises an early WASM-only scoping).

**Context.** Every prior Phase 2 feature is pure computation, so byte-identical stdout across all backends falls out for free — that is what made the §2.4 five-backend matrix possible. `extern js` is the first feature whose *purpose* is to reach outside the program into a host environment, and a native binary / pure-Rust interpreter has no JavaScript engine. "Parity" therefore has to be defined; two readings:

- *Mechanism parity* — every backend uniformly declares / type-checks / lowers / **calls** extern functions through a host-binding layer; only the binding differs per environment.
- *Effect parity* — `alert` literally pops a dialog on every backend. Only achievable by embedding a JS engine (V8 / QuickJS) into native binaries *and* both interpreters — abandons Phoenix's pure-Rust posture, bloats every binary, and runs browser APIs where no browser exists.

**Decision: mechanism parity via a uniform host-FFI boundary.** `Op::ExternCall` is a generic host-call op (not WASM-tagged). Each backend binds it differently:

- **wasm32-linear / wasm32-gc** — the generated JS glue is the host (real JS). See decisions B–G.
- **AST + IR interpreters** — a registerable Rust host-function table keyed by `(module, name)`; the embedder / test harness registers Rust closures (e.g. `alert` → append to a buffer).
- **native (Cranelift)** — each distinct extern lowers to a call of a C-ABI symbol (`phx_extern_<module>__<name>`); the standalone binary resolves it from a linked host shim.

**Default when an extern is unbound at runtime** (no host closure registered / no shim symbol linked): a clear runtime error naming the missing `(module, name)` — never a silent no-op.

**Consequence for the matrix.** Interop fixtures whose host functions are stubbable (log-append, scalar transforms) register identical stubs on every backend and **rejoin the five-backend byte-identical matrix** — the §2.4 parity discipline is preserved, not excepted. Genuinely DOM-only fixtures stay in the browser tier (decision I) because the effect they assert only exists in a browser.

**Residual (stated honestly).** Mechanism parity is not effect parity: a DOM-mutating extern called from a native binary hits its host-shim binding, which on a non-browser host can only stub or error — it cannot mutate a DOM that does not exist. That is a property of the deployment target, not a backend gap.

**Alternatives considered:**
- *WASM-only with clean rejection* (the early draft). Honest about the host dependency and simplest, but native + both interpreters would reject interop programs at compile time and interop fixtures would live outside the five-backend matrix.
- *Embed a JS engine on every backend* (effect parity). Rejected: abandons the pure-Rust posture, bloats every native binary, and the payoff — browser APIs running where there is no browser — is illusory.

#### A. Host set & gating: Node always-on, browser gated

**Decided:** 2026-06-17

**Rationale:** wasmtime (the 2.4 exit gate) is WASI-only and cannot host JS, so interop fixtures need a real JS host. Node is the lightest always-on gate — npm-native, already provisioned in CI's `gen-checks` job, and a subprocess shape identical to the existing `run_with_wasmtime` harness. The browser is where the "DOM access" promise is actually verifiable, but a headless-browser rig (driver install, flakier, heavier CI) is wrong as an always-on gate; it runs as a gated DOM-verification tier.

- Node gate: `PHOENIX_REQUIRE_NODE=1` turns the skip-when-absent into a hard failure, mirroring `PHOENIX_REQUIRE_WASMTIME` (§2.3 valgrind-gate shape).
- Browser gate: `PHOENIX_REQUIRE_BROWSER=1` likewise; soft-skip with a visible warning where no browser is provisioned.

**As-built — browser tier is two sub-tiers.** The DOM tier ([decision I](#i-dom-type-coverage-curated-hand-declared-subset)) ships as *two* runners over the **same** fixtures, host stubs, and baselines (`tests/fixtures/interop/dom/<name>/`), in `tests/interop-browser/`:
- a **jsdom smoke** (no real browser; always-on wherever the harness's npm deps are installed — it soft-skips a fresh checkout until `npm ci`, see below) that loads the generated glue against a jsdom `document` under Node — it verifies the DOM-host marshalling and the retained-event-handler path (decision G) at the API level. The `click_handler` fixture genuinely stresses the pin: after registration it churns enough throwaway allocations to trip the GC threshold several times over *before* the click fires, so a regression that failed to pin the host-retained closure would sweep it and diverge from the baseline — rather than passing trivially because no collection ever ran; and
- the **Playwright tier** (gated by `PHOENIX_REQUIRE_BROWSER=1`) that loads the page in real headless Chromium and dispatches a real click — it catches real-engine behavior jsdom cannot.

The harness's npm deps (`jsdom`, `playwright-core` — the latter brings no bundled browser) are not committed; a fresh checkout runs `npm ci` in `tests/interop-browser/`. Two new gates keep this from breaking a CI that hasn't provisioned them yet: `PHOENIX_REQUIRE_BROWSER_DEPS=1` (hard-fail if the npm deps are missing) and `PHOENIX_REQUIRE_BROWSER=1` (hard-fail if no browser is launchable); both soft-skip otherwise. CI wiring (`npm ci` + `playwright install chromium` + setting the gates) lands in PR 17.

**Alternatives considered:**
- *Node only.* Rejected: leaves the DOM story — the whole point of browser interop — unverified by any test.
- *Browser only.* Rejected: makes every interop test depend on a headless-browser rig; too heavy for the always-on gate and overkill for non-DOM marshalling fixtures.
- *Deno.* Rejected: less ubiquitous than Node as the ecosystem default; npm compat is good but not the assumption Phoenix users start from.

#### B. WASM host bindings: BOTH wasm32-linear and wasm32-gc ship

**Decided:** 2026-06-17

Scopes the two **WASM** host bindings of [decision A0](#a0-parity-model-extern-functions-are-a-uniform-host-ffi-boundary) (the interpreter and native bindings are covered by A0 + decisions E/G). Both WASM backends ship interop in 2.5.

**Rationale:** WASM-GC is the strategic browser backend (§2.4 [decision A](#a-dual-backends-in-this-phase-wasm-gc-primary-linear-memory-fallback) frames it as primary, linear as the fallback), and the Phase 5 reactivity/frontend endgame lives in the browser. The decisive technical point: `externref` is the *native* model for holding a JS value, so on WASM-GC `JsValue` is a host-VM-traced managed reference — which means a host-retained Phoenix **callback is traced automatically**, dissolving the retained-callback leak that the linear backend has to manage with manual rooting + explicit free. Shipping both now avoids designing two marshalling models at different times. The cost is bounded because the **user-facing surface is backend-neutral and designed once** — `extern js` grammar, `Type::JsValue`, marshallability rules, and `Op::ExternCall` are shared (PRs 1–3); only the per-backend lowering / marshalling / glue differs. A `.phx` program written against linear interop keeps compiling unchanged on WASM-GC, so this is not a user-visible ABI fork.

- Linear: wasm-bindgen-style copy-marshalling across linear memory (the proven path). Ships and reaches its gated harness first.
- WASM-GC: additive `externref` layer on the shared front-end + glue core + harness. Introduces `externref` into a backend that today uses only `HeapType::Concrete`.

**Alternatives considered:**
- *Linear only, defer WASM-GC (the original recommendation).* Defensible — the front-end is shared so WASM-GC stays a pure-additive follow-on, and linear-memory WASM runs in browsers fine — but it ships interop on the "fallback" backend first and leaves the externref-based callback-lifetime win on the table.
- *WASM-GC only.* Rejected: linear is the proven marshalling path and de-risks the language-level design; dropping it also strands runtimes without WASM-GC.

#### C. Glue-artifact shape: paired sidecar `.js`

**Decided:** 2026-06-17

**Rationale:** A JS host cannot instantiate a Phoenix `.wasm` that imports custom externs without glue that *satisfies* those imports (instantiation wiring + per-extern marshalling thunks). Emitting a paired `app.js` next to `app.wasm` is the wasm-bindgen-proven shape and keeps the wasm artifact itself host-agnostic. The glue is generated from the **same extern table the import section was built from**, so import names/signatures on the two sides cannot drift.

- One shared marshalling core; Node and browser **entry variants** over it; per-backend (linear / WASM-GC) encode-decode plugins.
- Emitted by default for a WASM target once `extern js` declarations are present (suppressible).

✅ **Implemented 2026-06-22 (PR 15, completing the WASM-GC binding).** The shared core (`wasm/glue.rs`) owns the WASI preview1 shim, the `instantiate` frame, the thunk/factory *structure*, the JS-string escaper, and the generated-file marker; a `GlueBackend` plugin supplies only the value-ABI marshalling (per-type encode/decode expressions + the JS helper blocks they call). The two plugins are `wasm/js_glue.rs` (`LinearGlue` — the i32 handle table, `phx_string_alloc` string builder, pinned/`FinalizationRegistry` callback retention) and `wasm/wasm_gc/js_glue.rs` (`GcGlue` — `externref`-direct `JsValue` with no handle table, the scratch-region String helpers, closures held by the JS wrapper and traced by the host VM with no pin). The two targets do *not* share a glue file — each emits its own, and they're not interchangeable (linear's handle table / `phx_string_alloc` vs. gc's scratch helpers); the build dispatches to `wasm_linear_js_glue` vs. `wasm_gc_js_glue` per target. What they share is the trampoline *naming* scheme, so the core generator needs no per-backend branch and the same fixture's expectations hold on both. Each target's single glue file is then correct in either *host environment*: the Node entry is the `instantiate({ wasm, host, writeStdout })` ESM export both produce; the browser entry is the same ESM run with `fetch` + a DOM host (no separate file — the glue is environment-agnostic). The interop fixture family runs byte-identically on both targets under Node, and the DOM family under jsdom + (gated) Playwright on both targets — the same fixtures, host stubs, and baselines, proving the two plugins marshal equivalently. `phoenix build --target wasm32-gc` emits its paired `.js` (`wasm_gc_js_glue`), alongside the existing `--target wasm32-linear` path.

**Alternatives considered:**
- *Embed the glue as a custom section in the `.wasm`.* Rejected: hosts would need a Phoenix-specific extraction step before instantiation; a plain `.js` file is directly `import`-able / `require`-able.
- *Hand-written per-project glue.* Rejected: drifts from the import section the moment an extern signature changes; the generator is the drift-proof source of truth.

#### D. `JsValue` representation: per-backend, same user-facing type

**Decided:** 2026-06-17

**Rationale:** `JsValue` is the opaque handle for a JS value Phoenix holds but never inspects. Its lowering is necessarily different per backend — linear memory has no notion of a host reference, while WASM-GC has `externref` precisely for this — but the user-facing type is identical. `Type::JsValue` is a pre-registered synthetic primitive, modeled on the Option/Result pre-registration in `resolved.rs::build_from_checker`.

- Linear: an `i32` handle into a JS-owned handle table the glue manages; Phoenix never dereferences it, only passes it back to externs.
- WASM-GC: an `externref` passed directly; the host VM owns and traces it (no handle table). **Foundation landed PR 12:** `IrType::JsValue` lowers to `(ref null extern)` (`ValType::EXTERNREF`) in the gc backend's type mapping, and `Op::ExternCall` lowers to a `call` of a custom per-extern import (one import per distinct `(module, name)`, declared before any local function). PR 12 covers scalar + `JsValue` externs; `String` (PR 13) and closures (PR 14) are rejected at import declaration; the JS glue that satisfies these gc imports (and passes real host values as `externref`) is PR 15.

**Alternatives considered:**
- *A uniform i32-handle model on both backends.* Rejected: throws away WASM-GC's externref tracing (the callback-lifetime win in decision B/G) to make the two backends superficially identical.

#### E. Extern-call ABI: per-backend marshalled signatures

**Decided:** 2026-06-17

Per the [A0 host-FFI model](#a0-parity-model-extern-functions-are-a-uniform-host-ffi-boundary), each backend binds `Op::ExternCall` differently. The marshalled signature per binding:

- **WASM (linear):** each distinct extern `(module, name)` becomes one custom WASM function import (declared via the documented `merge_func_import` extension). `Int`→`i64`, `Float`→`f64`, `Bool`→`i32`, `String`→ the existing 2-slot `(i32 ptr, i32 len)` fat pointer, `JsValue`→`i32` handle, closure→ a single `i32` env pointer (**as built in PR 8** — refined from the `(i32 fn_idx, i32 env_ptr)` pair sketched here; the fn-table index is `env[0]`, so the exported trampoline reloads it. See [decision G's as-built note](#g-closures-as-callbacks-lifetime-per-backend)).
- **WASM (gc):** one import per extern (declared via the GC `ModuleBuilder`); scalars as above; `String` copied across the GC backend's small linear-memory scratch region; `JsValue`→`externref`; closure→ a managed ref. **String marshalling landed PR 13:** a `String` crosses as its concrete `(ref null $string)` import type (so the lowering is unchanged), and the module exports the helper(s) the glue (PR 15) calls to copy bytes through the linear-memory scratch region — `phx_extern_str_to_scratch` (`$string`→scratch, String-OUT) and `phx_extern_str_from_scratch` (scratch→`$string`, String-IN; bytes copied, never shared, per decision F). Only the helper for each direction actually used is emitted (a param-only extern exports just String-OUT, a return-only one just String-IN), so the export surface matches what the glue calls. The scratch is the existing print buffer (reused safely — the glue marshals each string serially); strings over its 4095-byte cap trap ([known-issues.md](known-issues.md#wasm32-gc-extern-js-strings-are-capped-at-4095-bytes)).
- **Interpreters (AST + IR):** no marshalling — extern calls dispatch on `(module, name)` to the registered Rust host closure with `Value` arguments directly; `JsValue` is an opaque interpreter-side handle the host table owns.
- **Native (Cranelift):** ✅ **Implemented 2026-06-19.** Each distinct extern lowers to a call of a C-ABI symbol `phx_extern_<module>__<name>` with the native value ABI (`i64`/`f64`/`i8`/string-fat-pointer `(ptr, len)` two-register); `JsValue`→ an opaque `i64` handle owned by the linked host shim. The compiler emits the call and the symbol reference; the host shim provides the body.

  **As-built.** For each called extern the compiler emits a **weak** (`Linkage::Preemptible`) default definition of the symbol whose body calls `phx_extern_unbound(module, name)` (a runtime helper that aborts naming the missing binding). A host that links a **strong** definition of `phx_extern_<m>__<n>` overrides the weak default: for the fully static link this backend drives (`cc app.o shim.o libphoenix_runtime.a`) that's the plain strong-beats-weak rule resolved at link time, independent of link order; in a dynamically linked image the same override happens via PLT interposition at load time (the native backend is position-independent, so a call to a preemptible symbol routes through the PLT). The public link path gained `link_executable_with_objects` so a host-shim object can be linked alongside the program + runtime. This realizes the A0 "clear runtime error when unbound, never a silent no-op": an interop program *links and runs* with no host and aborts the instant it calls an unbound extern. **Platform note:** weak-symbol override (strong-beats-weak at static link, PLT interposition when dynamic) is the ELF/Mach-O model; Windows/COFF native interop is out of scope for this phase (the wasm32 + interpreter bindings cover Windows hosts) — see [known-issues.md](known-issues.md#native-extern-js-interop-is-elfmach-o-only-no-windowscoff-weak-override).

This is an internal compiler↔host contract, not a user-visible ABI — which is what lets the bindings differ without forking the language surface.

#### F. String ownership across the boundary: copied, never shared

**Decided:** 2026-06-17

**Rationale:** Sharing a Phoenix GC string's bytes with the JS engine (or vice versa) would couple two independent garbage collectors' lifetimes across the boundary — a correctness hazard with no upside at 2.5's scale. Strings are copied at the crossing on both backends: out via `TextDecoder` over the staged bytes (the `phx_print_str` scratch-copy pattern), in via `phx_string_alloc` (linear) / GC `$string` allocation (WASM-GC) into a Phoenix-owned string. The GC owns the Phoenix side; the JS engine owns the JS side; neither aliases the other.

#### G. Closures-as-callbacks lifetime: per-backend

**Decided:** 2026-06-17

A Phoenix closure passed to a host crosses through each backend's binding (per [A0](#a0-parity-model-extern-functions-are-a-uniform-host-ffi-boundary)). On WASM the module exports a `__phoenix_invoke_closure` trampoline (a `call_indirect` over the existing function table) that the glue wraps in a JS callable. Lifetime management differs per binding:

- **WASM (linear):** ✅ **Implemented 2026-06-19 (PR 8).** The glue registers the wrapped callable in a retention table and the Phoenix side roots the closure (`gc_roots`) so the GC can't collect a host-retained callback. Freeing is **explicit** (a drop extern / `FinalizationRegistry` tie-in) — callbacks-only async has no `Promise` to anchor lifetime. The host-never-released path is a linear-only leak filed in [`known-issues.md`](known-issues.md#a-retained-extern-js-callback-is-pinned-for-the-programs-life-on-wasm32-linear) as a *forward* deferral, not a 2.5 blocker.

  **As-built (PR 8).** The persistent root is a process-global pin set in the runtime (`phx_gc_pin` / `phx_gc_unpin`, scanned by the mark phase alongside the shadow stack) — the shadow stack is frame-scoped and cannot root a callback that outlives the extern call that handed it over. The glue owns the pin lifecycle: it pins when it wraps a crossing closure and unpins on release — explicitly via the wrapper's `release()` or via a `FinalizationRegistry` when the wrapper is collected — so the "Phoenix side roots the closure" is realized by the glue calling the runtime's pin hook (both exported from the module, gated on the program actually handing a closure to a host). The module exports one trampoline **per distinct callback signature** (`__phoenix_invoke_closure_<param-codes>_to_<ret-code>`, e.g. `__phoenix_invoke_closure_i_to_v`), since `call_indirect` is statically typed. **ABI refinement of [decision E](#e-extern-call-abi-per-backend-marshalled-signatures):** a closure crosses as a *single* `i32` env pointer, not the `(i32 fn_idx, i32 env_ptr)` pair the E sketch named — the fn-table index always lives at `env[0]`, so the trampoline reloads it rather than marshalling a redundant second slot. Functionally equivalent; it keeps the closure's boundary representation identical to its in-program one (a GC-pointer = one `i32`) and the import/export signatures derive from the shared `wasm_valtypes_for`.
- **WASM (gc):** ✅ **Implemented 2026-06-21.** The glue holds the closure ref via `externref`/`funcref`, so the host VM GC traces a host-retained callback automatically — **no manual rooting, no explicit-free leak.** **As-built:** a Phoenix closure crosses as its concrete `(ref null $clo_SIG)` managed ref (the closure's signature-parent struct; a single slot, so the `Op::ExternCall` lowering is unchanged), and the compiler exports one `__phoenix_invoke_closure_<sig>` trampoline per distinct callback signature — the *same* name the linear binding uses (both WASM bindings share one generated glue, decision C), but the body `call_ref`s the closure's funcref (loaded from `$clo_SIG` field 0) rather than `call_indirect`-ing through an env pointer. The wasm32-gc module exports **no** `phx_gc_pin`/`phx_gc_unpin` (the linear binding's manual-rooting machinery) — the JS reference the glue holds keeps the closure alive, and dropping it lets the host VM reclaim it. The glue that marshals callback args/results and holds the ref lands in PR 15.
- **Interpreters:** the host table receives the Phoenix `Value::Closure` directly and invokes it via the interpreter's normal call path; the interpreter's own GC/ownership keeps it alive for as long as the host table holds it.
- **Native:** ✅ **Implemented 2026-06-19.** The closure crosses to the host shim as its `i64` env pointer; the compiler exports one `phx_invoke_closure_<sig>` trampoline per distinct callback signature (the function pointer lives at `env[0]`, which the trampoline reloads and `call_indirect`s — the same single-env-pointer simplification as the WASM binding, not a separate `(fn_ptr, env_ptr)` pair). The shim calls back through the trampoline. Retention/rooting mirrors the linear contract: a synchronous callback is already rooted by the calling frame's shadow stack, and a shim that *retains* a callback past the call must hold the env pointer rooted via the runtime's `phx_gc_pin` / `phx_gc_unpin` (the same pin set the linear glue uses) — with the same host-never-released leak as linear.

#### H. Async model: callbacks-only

**Decided:** 2026-06-17

**Rationale:** The language has no async/await/`Promise` (the async runtime is Phase 4.3). Introducing an async-shaped interop surface now would commit to a model the real runtime must later reconcile with. JS async APIs (`fetch`, `setTimeout`) are modeled with Phoenix closures passed as callbacks. The [phase-2.md §2.5](phases/phase-2.md#25-javascript-interop) `async function main()` sketch is revised to the callbacks-only form. Ergonomic `await` over JS Promises rides Phase 4.3 + a later interop slice.

**Alternatives considered:**
- *A minimal `JsPromise`/thenable bridge in 2.5.* Rejected: more ergonomic for `fetch`, but introduces an async-shaped type ahead of the runtime that must own async semantics — a reconciliation cost for a phase whose async story isn't ready.

#### I. DOM type coverage: curated hand-declared subset

**Decided:** 2026-06-17

The browser tier verifies DOM interop against a **curated, hand-declared** `extern js` subset (e.g. `setText`, an event-handler registration), not generated or imported DOM typings. Generated typings depend on the same package/typings-resolution machinery as the deferred npm slice and ride with Phase 3.1.

#### J. npm package slice deferred to Phase 3.1

**Decided:** 2026-06-17

**Rationale:** `import js "pkg" { ... }` string-source imports, `[js-dependencies]` in `phoenix.toml`, and bundler/npm resolution all depend on a package manager that does not exist yet (today `phoenix.toml` carries only `[gen]`, and `phoenix-modules` resolves filesystem-relative imports with no registry). Building a package system inside 2.5 would balloon the phase; 2.5 ships hand-declared `extern js` host/browser APIs only, and the npm slice rides with / after Phase 3.1. The existing dotted-identifier `import a.b.c { ... }` grammar is untouched — the JS string-source form is not landed in 2.5.

**Alternatives considered:**
- *A minimal bundler shim in 2.5* (lean on an existing esbuild/Vite + the host's `node_modules`, no Phoenix package manager). Rejected: adds bundler-integration scope to a phase that is already large, and pre-commits to a resolution model the real package manager should own.
- *Pull Phase 3.1 forward into 2.5.* Rejected: largest scope; reorders the roadmap to satisfy a slice that is cleanly separable.

#### K. Extern declarations are signature-only; the host is supplied separately (no inline JS bodies)

**Decided:** 2026-06-20

**Context.** `extern js { function alert(message: String) }` declares a *signature*; the implementation is provided per backend (the JS glue/host object on WASM, a linked C-ABI shim on native, a registered Rust closure in the interpreters). A natural question is why the body can't be written *inline* in the `.phx` source — `extern js { function alert(m: String) { console.log(m) } }` — instead of living in a separate host artifact. It is technically feasible: the generated WASM glue thunk already wraps a host call with marshalling (`alert(p0, p1) { ...; return host.alert(readString(p0, p1)); }`), and an inline body would simply be spliced in place of the `host.alert(...)` call. Emscripten's `EM_JS` does exactly this (inline JS bodies inside C).

**Decision: signature-only.** The body is **not** written in the `.phx` source; the host is supplied by the embedding environment (`instantiate({ host })`) or the linked binding. Inline JS bodies are **rejected** as the default form.

**Rationale.**
- **`extern js` is a backend-neutral host-call, not "a JS function" ([decision A0](#a0-parity-model-extern-functions-are-a-uniform-host-ffi-boundary)).** The *same* declaration binds to four backends; an inline **JS** body is meaningful only for the two WASM backends — a native binary and the pure-Rust interpreters have no JS engine. Baking JS into the source would make a `.phx` file non-portable across exactly the five-backend matrix the phase is built around. Emscripten can do `EM_JS` precisely because it only ever targets WASM+JS; Phoenix targets five backends from one source, so host-language source in the `.phx` is meaningless for three of them.
- **The primary use case has no body to author.** Most `extern js` binds host APIs that *already exist* — `document.getElementById`, `fetch`, `localStorage`, an npm export. You declare the shape and the binding wires the call to the real thing; there is no body to write. Inline bodies would only serve the minority "tiny helper" case.
- **The host is environment-specific, and that is a feature.** `alert` pops a dialog in a browser, logs under Node, and cannot run server-side. Supplying the host at instantiation lets the *same* `.wasm` run under a browser, Node, Deno, or a worker unchanged; an inline body hard-codes one host environment into the artifact. (The `host.mjs` files under `tests/fixtures/interop/` are *test* hosts — in real use the embedder provides the host.)
- **Source purity / analysis.** Keeping `.phx` free of embedded host-language source preserves "a `.phx` file is pure Phoenix the toolchain can analyze," rather than carrying opaque JS blobs the compiler emits verbatim.

**Not foreclosed.** If the "quick inline JS helper" ergonomic proves to matter, it can be added later as *explicit, opt-in, WASM-only* sugar — e.g. a distinct `extern js inline { ... }` form the WASM backends splice in and the native/interpreter backends reject (or require a separate binding for). That keeps the portable signature-only default clean while offering an escape hatch; it is a deliberate future language-design decision, not a default. The planned [npm slice (decision J)](#j-npm-package-slice-deferred-to-phase-31) already covers "I want real JS dependencies" the portable way.

**Alternatives considered:**
- *Inline JS bodies as the default (the `EM_JS` model).* Rejected: couples `extern js` to JS-hosted backends, breaking A0's uniform boundary and the five-backend byte-identical matrix; serves only the minority case (the common case binds existing host APIs); and hard-codes one host environment into the artifact.
- *Inline bodies as opt-in WASM-only sugar, in 2.5.* Deferred, not rejected — a reasonable future ergonomic, but it adds a second extern form and a per-backend "this form is unsupported here" rule to a phase that is already large; revisit if demand appears.

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
   binding) will no longer parse. Accepted pre-1.0; no in-tree fixture used the
   name (verified by running the full suite when the keyword landed). Each entry:
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
     TS camelCase) and aliases to the wire name — exactly the pattern the Python
     `Query(alias=...)` fix established, now generalized to headers.

3. **Request headers** behave like query params per target: client method input,
   sent via the framework's header API (`req.Header.Set` / `headers={}` /
   fetch `headers`), parsed server-side (`r.Header.Get` / FastAPI `Header(alias=)`
   / express `req.header(...)`) into the handler signature.
   - **Scalar wire encoding (cross-language contract).** Non-string headers are
     stringified for the wire identically across every target so a header
     round-trips regardless of which client talks to which server. In particular
     `Bool` is always lowercase `true`/`false` (Go `strconv.FormatBool`, TS
     `String(bool)`, Python an explicit `"true"/"false"`) and is read back with a
     lowercase `== "true"` / `ParseBool` check. Python's `str(True)` → `"True"`
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
   (TS/Python/Go/OpenAPI), proven by BOTH harnesses (compile-and-lint +
   round-trip). Constraint/validation behavior on headers is out of scope for v1
   (no `where`); auth remains middleware-shaped per roadmap §4 — headers are the
   transport, not an auth model.

7. **Framework-managed response headers (caveat).** Some server frameworks set
   transport headers automatically (Express's auto-`ETag`, `Content-Length`,
   etc.). Because HTTP header names are case-insensitive, a response header whose
   wire name collides with one of these (e.g. an `etag` header → `Etag`) can be
   overwritten by the framework's value before the client reads it. The generated
   code emits only a router/handler factory, not the full app, so disabling the
   framework default is the caller's responsibility (e.g. `app.set("etag", false)`
   for Express — the round-trip harness does exactly this). Documented here so the
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
   Void), recognized as a built-in type name via `Type::from_name`
   (`phoenix-sema/src/types.rs`). It needs the usual lexer token + parser
   type-name recognition + `Display` (`"File"`).

2. **Implicit multipart — no new body grammar.** A multipart/binary body is a
   normal struct that *contains* a `File` field; there is NO dedicated
   `multipart { }` block. `struct AvatarUpload { avatar: File  caption: String }` +
   `body AvatarUpload` IS the multipart upload. Rationale: matches the type-driven
   model of every target (OpenAPI `format: binary`, FastAPI `UploadFile`, Go
   `multipart.FileHeader`), composes with the existing derived-body machinery
   (omit/pick/partial, the `resolve_derived_type` struct path — bodies are always
   struct-derived today), and adds the minimum to the frozen grammar. The
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
   lowered to IR — confirmed: `Declaration::Endpoint` is a no-op in
   interpreter/IR/cranelift/wasm), `File` never reaches the execution pipeline;
   `lower_type` gets a `Type::File => unreachable!("File is endpoint-transport-only")`
   arm, mirroring the existing `Type::Error` unreachable arm.
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
   (uploads) + binary response bodies (downloads), all four targets, proven by
   BOTH harnesses (compile-and-lint + round-trip). The round-trip contract gains
   non-JSON-body support (a small binary fixture). If downloads prove to carry
   enough per-target streaming nuance to balloon the slice, they split into a
   clean follow-up — flagged at that point, not assumed.

### Sema enforcement of the `File` scope rules (implementation)

The restrictions above are enforced in `phoenix-sema` via a context-flag
mechanism threaded through the single type-resolution choke point
(`Checker::resolve_type_expr`), rather than a separate post-registration walk —
`let`-binding and lambda-param types are resolved in function bodies, never
registered, so only the resolver sees every `File`-bearing position.

- `Checker::file_field_allowed` (default `false`) is set `true` *only* while
  resolving a struct field's direct type annotation (`register_struct`). The
  resolver clears it before recursing into generic/function arguments, so
  `File` is accepted *only* as a direct struct field and is rejected in
  function params/returns, `let`/variable types, query params, headers, enum
  variant payloads, type-alias targets, and nested generics.
- **`Option<File>` is ALLOWED as a struct field** (optional file upload): the
  resolver propagates the field allowance into `Option`'s single argument only.
  **`List<File>`, `Map<String, File>`, and every other generic over `File` are
  REJECTED** — multiple-file arrays add per-target complexity and are deferred
  (known limitation; liftable later). `StructInfo::is_file_bearing` and the
  `body_is_multipart` flag both treat `Option<File>` as carrying a `File`.
- **Transitive body-only rule.** A struct with a `File` (or `Option<File>`)
  field is flagged `StructInfo::is_file_bearing`. `Checker::file_bearing_struct_allowed`
  (default `false`) gates use of such a struct by name; it is set `true` *only*
  at the endpoint `response`-resolution site, and applies only to the **direct**
  response type. The resolver clears it before recursing into generic/function
  arguments (mirroring `file_field_allowed`), so `response List<Doc>` /
  `response Option<Doc>` (where `Doc` is file-bearing) are rejected — a `File`
  cannot be JSON-serialized inside a list/option. The request `body` path
  resolves its struct by name through `resolve_derived_type` (never
  `resolve_type_expr`), so it is accepted without the flag; every other position
  (function param/return, `let`, nested struct field, enum payload, generic arg,
  type alias) rejects a file-bearing struct.
- **Direction asymmetry.** Request bodies may mix `File` + scalar fields
  (multipart). A `File`-bearing *response* struct must contain exactly one
  field, of type `File`, and nothing else (pure binary download) — checked at
  the response-resolution site in `check_endpoint`.
- **Binary download excludes response headers.** A binary download's response
  body is the raw file stream; there is no `<Endpoint>Result` envelope to carry
  typed response-header fields (every target returns a stream/blob/`Response`
  for it). A binary-download endpoint that also declares `headers { … }` on its
  response therefore has no coherent generated shape, so `check_endpoint`
  rejects the combination rather than letting it reach the per-target codegen
  (where it would otherwise silently drop the headers or emit contradictory
  code).
- **Multipart fields are scalar-or-file.** A `multipart/form-data` part is text
  (or a file) on the wire, so every *non-file* field of a multipart request body
  must be a scalar (`Int`/`Float`/`Bool`/`String`) or `Option<scalar>`. A
  `List`, `Map`, nested struct, or enum field has no form encoding and is
  rejected in `check_endpoint` (`Checker::is_multipart_field_type`) rather than
  emitted as broken client/server code. (A non-multipart JSON body keeps its
  full type freedom — this rule fires only once a body contains a `File`.)
- **Codegen-facing flags.** `EndpointInfo` gains `body_is_multipart: bool`
  (request body, after omit/pick/partial, contains a `File` field) and
  `response_is_binary: bool` (response is a single-`File` struct), computed in
  `check_endpoint` and consumed by the per-target multipart/download codegen.
- **OpenAPI `required` now excludes `Option<T>` body fields (behavior change).**
  While wiring multipart schemas, `derived_type_to_schema` started excluding
  `Option<T>` fields from a body schema's `required` array (previously only the
  `partial`-derived `optional` flag did). This also corrects *plain JSON* bodies:
  an `Option<String>` body field is no longer emitted as `required`. The fix is
  correct (an optional field is not required) but it changes pre-existing
  JSON-body OpenAPI output — a consumer that relied on the old (incorrect)
  `required` set will see the field drop out. Covered by
  `derived_type_body_option_field_not_required`.

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
   response-envelope precedent from headers. Proven by BOTH harnesses
   (compile-and-lint + round-trip; the round-trip asserts the handler-supplied
   metadata round-trips through the body). OpenAPI emits the `<Endpoint>Page`
   object schema as the 200 response body.

9. **Route-ordering fix (surfaced by this slice, not pagination-specific).** The
   pagination round-trip exposed a latent bug in the TypeScript (Express) and
   Python (FastAPI) server generators: both frameworks match routes
   **first-registered-wins**, and the generators emitted routes in schema source
   order, so a parametric route (`/api/posts/{id}`) declared before a static
   sibling (`/api/posts/paged`) **shadowed** the static one — the static path was
   captured as `id = "paged"` and dispatched to the wrong handler. Fix: both
   generators now register routes **most-specific (most-static) first** via a
   `route_specificity_key` (per-segment static-before-`{param}` ordering, stable
   for equal specificity), matching the most-specific-wins semantics Go's
   `net/http.ServeMux` (1.22+) already provides — Go needed no change, OpenAPI has
   no routing. This is a general correctness fix (it also covers e.g.
   `/users/me` vs `/users/{id}`), found because the round-trip suite executes the
   generated server rather than only checking it compiles.

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
   the struct machinery in every target — Python in particular emits
   `T.model_validate(...)` / `body.model_dump_json()`, which only exist on
   pydantic models, so a non-struct `T` would generate code that fails at
   runtime. Relaxing this later (e.g. via pydantic `TypeAdapter`) is additive.
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

6. **Sema data model.** `EndpointInfo` keeps `response: Option<Type>` and gains
   `response_statuses: Vec<ResponseStatusInfo>`. A bare `response Type` leaves
   `response_statuses` EMPTY — `response` stays the single source of truth, so
   every existing endpoint's generated output is byte-identical (no churn for
   the non-multi-status case, mirroring how headers/pagination left non-using
   endpoints unchanged). A `response { }` block populates `response_statuses`
   (its non-emptiness is the multi-status signal to codegen) and mirrors the
   shared body type `T` back into `response` so downstream "what is the success
   body type" reads keep working. (An earlier sketch instead lowered the bare
   form to an implicit single-200 entry; the empty-for-bare representation was
   chosen because it needs no special-casing to keep existing output
   unchanged.)

7. **Scope of this slice:** multi-status success responses (shared body type +
   typeless statuses), all four targets, proven by BOTH harnesses
   (compile-and-lint + round-trip; the round-trip asserts the handler-chosen
   status + body round-trip). Content negotiation (multiple content-types per
   status, union returns, Accept dispatch) is OUT.

### Deferred — content negotiation (the other half of the roadmap sketch)
Multiple content-types at one status (`200 json: User`, `200 text: String`) with
Accept-header dispatch and union return types is deferred indefinitely. It is the
high-complexity / low-frequency half: it forces runtime client dispatch on the
response `Content-Type` and a sum/union return type that Go cannot express
idiomatically (only `interface{}` or a generated discriminated wrapper), which
would undercut the "idiomatic per-target output" quality bar. Revisit only if real
demand appears; the multi-status grammar above leaves room to add a per-status
content-type qualifier later without breaking existing schemas.

### Body-identifier collision fix (surfaced by this slice)
The fixture's multi-status endpoint has BOTH a request `body` and a multi-status
envelope, which exposed a latent TypeScript-generator bug: the client method
declared a local `let body` for the parsed response body, colliding with the
`body: <T>Body` request-body **parameter** (TS2300 duplicate identifier; also a
type error). Fix: the TS client now names the response-body local `responseBody`
(returning `{ status, body: responseBody }`). Python had already avoided this
(its agent named the local `response_body`); Go uses struct fields so never
collided. Found because the compile-and-lint harness ran `tsc` over a fixture
combining a request body with a multi-status response — the inline generator
tests used bodyless endpoints and missed it.

### Generated-type-name collision check (closed 2026-06-10)
An endpoint declaration synthesizes up to five type names in the generated
output: the envelopes `<Endpoint>Result` / `<Endpoint>Page` / `<Endpoint>Response`
(mutually exclusive), plus the request-body types `<Endpoint>Body` (any `body`
clause; combinable with the envelopes) and `<Endpoint>ClientBody` (Go only,
multipart bodies). Multi-status made a collision materially more likely —
`Response` is a natural user struct name. In Go/TS a collision is a loud
generated-code compile error, but in Python a duplicate `class X(BaseModel)` is a
**silent redefinition** (last wins) — a quiet miscompile. Closed with a sema check
(`check_endpoint`): when an endpoint synthesizes one of those names, a
user-defined struct or enum of that exact name is rejected with a clear message.
The check only fires when the feature is actually declared (a like-named struct
alongside a plain endpoint is fine — no false positives). Five follow-on
hardenings landed with it: (1) **endpoint-vs-endpoint name collisions** are
rejected at the *exported-name* level (`check_exported_name_collision`):
endpoint names are unique only case-sensitively, so `getUser` and `GetUser` are
both distinct names — but Go builds the client method, server method, and
handler-interface method from `capitalize(name)`, so that pair emits two
`GetUser` methods on one struct, a Go compile error **regardless of what else
the endpoints declare** (TS/Python keep the name as written and are
unaffected, but sema is target-agnostic, matching how `ClientBody` is reserved
on every target; surfaced in review of this slice). The predicate is
exported-name equality, not full case-insensitivity — `getUser` / `getuSer`
export as distinct Go methods and stay legal. Because every generated type
name is `exported + suffix`, this name-level check subsumes all *same-stem*
type collisions, leaving one endpoint-vs-endpoint type case live: the
cross-stem suffix overlap. `"ClientBody"` ends with `"Body"`, so `upload`
(multipart) and `uploadClient` (any body) both generate `UploadClientBody`
despite distinct stems — the only suffix-overlap pair among the five, caught
by a `generated_type_names` claim map mirroring `route_signatures` (all five
names still claim entries, so the map self-defends if a future suffix
introduces a new overlap). This case is worst for `Body` and `ClientBody`:
codegen's `emitted_derived_types` dedupe is first-wins in **every** backend,
so without the check the second endpoint silently bound to the first one's
struct. (2) The capitalization rule the generated names are built with moved to
`phoenix_common::idents::capitalize`, shared by sema and every codegen backend
(Go's `to_pascal_case` delegates to it), so the check and the generators cannot
silently diverge. (3) Diagnostic discipline: a duplicated endpoint name or a
colliding endpoint-name pair is one mistake and gets one diagnostic — per
*pair*, since one endpoint can legitimately collide with two different
endpoints (same-stem on the exported name, cross-stem on `ClientBody`) and
those are two mistakes; an exported-name collision seeds the type check's
per-pair suppression so the same-stem type hits don't double-report.
(4) The fixed-name multipart helper `FileUpload` (Go; emitted once,
shared by every multipart endpoint) is reserved too — a user type of that name
duplicates the declaration in generated Go. Sharing across multipart endpoints
is by design, so it bypasses the endpoint-vs-endpoint reporting: the first
multipart endpoint claims the name and reports the user-type clash once
(surfaced in review of this slice). (5) Deliberate cascade, pinned by test:
when the only multipart endpoint carries a duplicated name, its `FileUpload`
clash is suppressed with its other diagnostics and surfaces on the recompile
after the rename. Known scope limit: the user-type lookup
resolves in the endpoint's module scope while the claim maps are global — if
endpoints ever live in non-entry modules, a same-named type in a sibling module
would be missed (flagged in a code comment at the check). The checks live
in `check_exported_name_collision` and `check_generated_type_collisions`,
extracted alongside their structural sibling `check_route_collision`. Covered
by the `envelope_collision_*`, `body_collision_*`, `exported_name_*`, and
`file_upload_*` sema tests, plus
`collision_with_user_type_declared_after_endpoint_rejected`, which pins the
two-pass guarantee (registration before endpoint checking) that makes the
lookup order-independent.

### Other drive-by hardening (surfaced by this slice)
Three pre-existing gaps fixed while building this slice — the first two because
the new code needed the same machinery, the third found while reviewing the new
`response { }` block against its `error { }` sibling:
- **TS client — unconsumed response bodies.** Every client path that never reads
  the body now cancels it with `await response.body?.cancel()`: void-response
  endpoints, the new all-typeless multi-status envelope, and the error path of
  endpoints without an `error { }` block (which throws without reading the
  body; the `error { }` path already consumes it via `response.text()`).
  Previously the unconsumed fetch body held the underlying connection until GC;
  cancelling releases it for reuse immediately. This changed the existing
  void-response and error-path client snapshots.
- **Go server — `(nil, nil)` handler returns.** The binary-download and
  response-header server paths now guard `result == nil` with a 500 ("handler
  returned nil result") before dereferencing, matching the guard the new
  multi-status path ships with. A `(nil, nil)` return is a handler bug Go's type
  system can't prevent; previously it panicked the route (`io.Copy` from a nil
  reader / a nil-envelope field read).
- **Parser — `error { }` malformed-entry hang; comma separators.** The
  `error { }` variant loop had no recovery advance, so ANY malformed variant —
  including the natural comma-separated spelling
  `error { NotFound(404), Conflict(409) }`, which two of this repo's own doc
  comments used — re-examined the same token forever and hung the compiler.
  Fixed with a consumed-nothing guard (skip one token when a variant parse
  consumes nothing). And because the comma spelling is clearly the habit users
  will bring (the roadmap's own sketch used it), both `error { }` and the new
  `response { }` block now accept an optional comma after each entry, matching
  the forgiving `omit { a, b }` field-list style. Endpoint sections remain
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

**Harness wiring status (green; gate removed).** All six fixtures parse and check
clean — the parse/sema-clean invariant is guarded by
`crates/phoenix-driver/tests/gen_schema_fixtures.rs`, which runs `phoenix check`
over each fixture — and they now run through the full compile-and-lint harness on
all four targets, **unconditionally** (under the same `PHOENIX_GEN_E2E` gate as
the inline schemas). Getting there was a stage-by-stage bug hunt: the dense
fixtures surfaced a Go `q`-param collision, then a Go wrapped-doc-comment `gofmt`
issue once the build reached `gofmt`; a TS optional-before-required (TS1016)
order bug, then two TS `prettier` divergences (redundant parens around a negated
method-call constraint; response-header-envelope long-line breaking) once TS1016
let the TS leg reach `prettier`; and a Python `black`/`ruff` pair (an unwrapped
multi-status `return` line and a case-sensitive import sort). All are fixed, each
with a regression test, and the full harness passes (`go build`/`gofmt`/
`golangci-lint`; `tsc`/`eslint`/`prettier`; `black`/`ruff`/`mypy`;
`redocly lint`). The `PHOENIX_GEN_FIXTURE_LIB` opt-in gate that held the library
out while those bugs were open has been deleted — `compiles_and_lints.rs` carries
a `FILE_FIXTURES` const of `include_str!` pairs and loops over it in each of the
four target tests directly. (Note: a run must NOT impose a tight `ulimit -v` —
redocly's WASM runtime needs a large address space and OOMs under a 6 GB cap, a
false failure unrelated to the generated specs.) The generated specs are also
warning-clean: unreferenced component schemas are pruned, so `redocly lint`
reports zero `no-unused-components` warnings.

**Bugs closed (known-issues.md entries deleted in this slice).** The Gen track
has no `docs/phases/phase-N.md` of its own, so this is its closure record — each
deleted entry maps to the regression test that keeps it closed (the same "delete
the stub, point at a test" discipline the language phases use):

- **Parser error-recovery OOM** (three malformed-input triggers) — bounded now;
  `gen_schema_fixtures.rs::{poisoned_keyword_field_rejected,
  poisoned_doc_comment_in_query_rejected, poisoned_response_projection_rejected}`
  (run un-ignored; each must exit non-zero with a diagnostic, not die by rlimit).
- **Go generated-local collision** — `q` was one instance of a class: every
  function-scoped *local* that shares a method/handler scope with the user's
  parameters could redeclare one. Generalized on both sides — the client locals
  (`u`/`q`/`req`/`resp`/`result`/`data`/`buf`/`writer`) and the server's
  handler-result local — each derived via `pick_free_local` to dodge the
  parameter names. The client's one *fixed* identifier, the receiver `c`, is
  uniquified the same way (a `c` cursor/count param would otherwise shadow
  `c.BaseURL`). `go_tests.rs::{query_param_named_q_does_not_collide_with_builder,
  generated_locals_dodge_colliding_param_names, client_receiver_dodges_colliding_param_name}`
  (the locals one verified end-to-end with `go build`/`go vet`/`gofmt`/`golangci-lint`
  on a collision schema). **Known remaining edge:** the server closure's fixed
  identifiers — `w`/`r`/`h`/`mux` — are not yet uniquified, so a *param* named
  `w`/`r`/`h`/`mux` still breaks the generated server (deferred until a real
  schema needs it; tracked in `emit_server_route`'s scope note).
- **Go wrapped-doc-comment `gofmt` rewrite** — continuation indentation is
  stripped per line; `go_tests.rs::render_line_comment_strips_continuation_indentation`.
- **TS optional-before-required param (TS1016)** — a fully-optional bag now
  renders `= {}`-defaulted, not `?:`; the three regenerated client snapshots
  (`*get_with_query_all_optional_client`, `*multi_status_inputs_client`,
  `*request_header_optional_client`) plus the end-to-end `tsc` leg of
  `compiles_and_lints.rs`.
- **TS `prettier` redundant parens on a negated constraint** —
  `typescript_tests.rs::struct_validation_bare_method_call_constraint_has_no_redundant_parens`.
- **TS `prettier` response-header-envelope long-line break** —
  `typescript/format.rs::{emit_object_property_breaks_call_args_not_after_colon,
  split_breakable_call_matches_only_call_shaped_values}`.
- **Python multi-status `return` not `black`-wrapped** —
  `python_tests.rs::multi_status_long_response_name_wraps_client_return`.
- **Python case-sensitive import sort (`ruff` I001)** —
  `python/format.rs::format_from_import_orders_names_case_insensitively`.

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
are now format-checked against `uuidRe` inline → 400, matching TS/Python. (Struct
`List<Uuid>`/`Map<String, Uuid>` *field* elements in `Validate()` remain
unchecked — a separate weak link, see the `Money` entry.) See *"Phoenix Gen —
tighten scalar query/header `Uuid`/`Decimal` validation on Go (2026-06-18)"*
below.

**Verification (DateTime).** Two harnesses, both green. (1) Compile-lint: a
comprehensive `DATETIME_SCHEMA` (field/`Option`/`List`/`Map`/nested/query/req+resp
header/pagination-items, plus BARE scalar/`List`/`Map` responses) runs through all
four targets — Go `go build`/`gofmt`/`golangci-lint`, TS `tsc`/`eslint`/`prettier`
(incl. the `revive*` pass), Python `black`/`ruff`/`mypy`, OpenAPI `redocly lint`.
(2) Bespoke wire round-trips (`tests/roundtrip/datetime/*`, separate from the
contract-driven `gen_api` suite) for Go/TS/Python assert body
(required/`Option`/`List`/`Map`)/query/response-header (required *and* optional)
AND bare scalar/`List`/`Map` response instants survive RFC 3339 in both directions.
The round-trip caught three bugs the lint pass could not (all
produce *valid* code that serializes *wrong* or crashes at runtime): the TS server
set response headers via `String(date)` (locale form, not ISO) — now
`.toISOString()`; the Python client sent bodies via `model_dump()` (leaves
`datetime` objects httpx's `json.dumps` rejects) — now `model_dump(mode="json")`
when the body carries a `DateTime`; and the Python client decoded EVERY bare
response with the object-only `Type(**response.json())` form, which crashes on a
scalar/scalar-collection response (`datetime(**"…")`) — now decoded by type
(`datetime.fromisoformat(...)` for a scalar, comprehensions for `List`/`Map`,
`Model(**…)` only for structs; see `py_decode_expr`). That last fix was a
pre-existing, type-agnostic gap (`response String` → `str(**…)` was equally
broken) that the missing-`datetime`-import diagnostic surfaced. Two ruff/eslint
wrinkles also surfaced: ruff's B008 FastAPI exemption is type-aware (recognizes
`str`/`int` `Header()`/`Query()` defaults but not `datetime`), fixed by adding the
standard FastAPI `extend-immutable-calls` to the Python scaffold; and Go
optional-deref `*x.Format(...)` needed parenthesizing. One TS layout wrinkle: a
bare `Map` response revival (`Object.fromEntries(…)`) overflows the 80-col print
width, so the client return is emitted through `emit_return`'s Prettier-style call
wrapping.

Per-file special-scalar import detection in Python is split across
position-specific walkers (`request_input_uses`, `response_header_uses`,
`bare_response_uses`, plus the envelope/page coverage in `models_use`), each
generic over a `fn(&Type) -> bool` type test so the SAME position logic serves
both `datetime` (`from datetime import datetime`) and `Uuid`
(`from uuid import UUID`) — evaluated once per scalar. An unused import trips ruff
F401 and a missing one trips F821/mypy, so each file imports a scalar iff some
position it actually renders names it. The bare-response walker is parameterized
by whether to exclude response-header endpoints — the client/handler render the
named `<Endpoint>Result` envelope there, while the server still returns the bare
`result.body`. The same generalization is true in TS: `Uuid` reuses DateTime's
revival machinery (`ts_type_needs_revival`/`ts_revive_expr`/`revive<Struct>`) —
`Uuid` → `parseUuid` slots in exactly where `DateTime` → `new Date` does.

**Verification (UUID).** Same two harnesses, both green. (1) Compile-lint: a
comprehensive `UUID_SCHEMA` (field/`Option`/`List`/`Map`/nested/query/req+resp
header + bare scalar/`List`/`Map` responses) through all four targets. (2) Bespoke
round-trips (`tests/roundtrip/uuid/*`) for Go/TS/Python assert body/query/
response-header/bare-response uuids survive the wire AND that the validating
decode paths (Python `UUID(...)`, TS `parseUuid`, Go `Validate()`'s `uuidRe`)
accept valid input. Two import gaps surfaced (both fixed): TS `handlers.ts` named
`Uuid` (query/header params, bare response) without importing it; and the TS
reviver-function signature overflowed 80 cols for a long body type
(`reviveCreateAccountBody`) — now wrapped Prettier-style via
`push_reviver_signature` (a latent DateTime bug too, just not triggered by shorter
names). All 10 round-trips (4 base + 3 DateTime + 3 UUID) and all four compile-lint
targets are green.

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

**Implementation note: generalized from UUID, not duplicated.** `Decimal` and
`Uuid` are the same shape (branded validated string), so the UUID-specific
machinery was generalized to serve both rather than copied: TS has
`ts_branded_scalars()` / `branded_scalar()` (a small `[(Type, alias, parse fn)]`
table) driving `type_to_ts`, `ts_revive_expr`, the coercions, the one-time
alias/`parse*` emission, and the per-file import collection; the pure helpers
became `type_mentions(ty, target)` / `leaf_is(ty, target)`. Go's `Validate()`
machinery moved from a single `uuidRe` bool to a `types_regex_vars:
BTreeSet<&str>` with a `regex_scalar(ty) -> (var, label)` lookup (`uuidRe` /
`decimalRe`), each var emitted with its pattern from `go_regex_pattern`. Python's
per-file detection was already predicate-generic (`*_uses(fn)`); `Decimal` just
adds `type_uses_decimal[_deep]` and a `needs_decimal` import flag. Adding a fourth
branded scalar later is now a table entry plus a regex, not a fresh copy.

**Verification.** Two harnesses, both green. (1) Compile-lint: a comprehensive
`DECIMAL_SCHEMA` (field/`Option`/`List`/`Map`/nested/query/req+resp header + bare
scalar/`List`/`Map` responses) through all four targets — Go
`build`/`gofmt`/`golangci-lint`, TS `tsc`/`eslint`/`prettier`, Python
`black`/`ruff`/`mypy`, OpenAPI `redocly lint`. (2) Bespoke round-trips
(`tests/roundtrip/decimal/*`) for Go/TS/Python assert body/query/response-header/
bare-response decimals survive the wire AND that the validating decode paths
(Python `Decimal(...)`, TS `parseDecimal`, Go `Validate()`'s `decimalRe`) accept
valid input and reject a malformed body decimal — plus the (since-closed)
TS-validates-query / Go-accepts-query divergence (each driver's query case now
asserts rejection on all three targets; Go updated 2026-06-18, see below).
All 13 round-trips (4 base + 3 DateTime + 3 UUID + 3 Decimal) and all four
compile-lint targets are green; 238 codegen lib tests show no snapshot drift from
the UUID→branded-scalar generalization.

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
`enum`). `iso4217::tests::iso_4217_codes_are_well_formed` guards authoring
mistakes (all `^[A-Z]{3}$`, unique, ascending, plausible count); a differential
test against an MIT reference set is a possible future hardening. Scope: active
national/supranational transaction currencies (incl. `EUR`/`XOF`/`XAF`/`XPF`/
`XCD`/`XDR`); precious-metal/fund/test/no-currency `X` codes are excluded.

**Implementation note: generalized, not duplicated.** `Money` reused the
scalar machinery wherever it fit — the TS revival pipeline (`ts_revive_expr`/
`ts_type_needs_revival`/`leaf_struct_reviver` gained `Money` arms; a `Money`-using
schema also forces the `Decimal` helpers since `reviveMoney` calls `parseDecimal`),
the Go `Validate()` emitter (a new `money_fields` list calling each field's own
`Validate()`), and Python's `py_decode_expr` / model_dump gate (`Money` ⟹ JSON
mode, since it carries a `Decimal`). Per-file `Money` import detection mirrors the
scalars (a bare-`Money` response imports the `Money` type + its reviver; a
struct-field `Money` rides the containing struct's import).

**Deferred (documented).** Money *arithmetic* (add/multiply across currencies)
stays out — transport-only, like `Decimal`; it arrives with the Go/TS decimal-lib
opt-in. Currency-aware rounding/scale per ISO-4217 minor units is future. A
`Money` field in a multipart body is rejected (same `is_multipart_field_type`
deferral as the scalars). Go's `Validate()` recurses into direct
`Money`/`Option<Money>` fields but NOT into `List<Money>`/`Map<String, Money>`
elements (Python/TS do validate those) — the documented weak link parallel to the
regex scalars; see [known-issues.md](known-issues.md).

**Verification.** Both harnesses green. (1) Compile-lint: a `MONEY_SCHEMA`
(field/`Option`/`List`/`Map`/nested-struct + bare scalar/`List`/`Map` responses)
through all four targets — incl. gofmt/golangci on the `currencyCodes` map, black/
ruff/mypy on the model + `_CURRENCY_CODES` set (the after-imports blank-line and
the `field_validator` import were the calibration points), tsc/eslint/prettier on
the interface + `CURRENCY_CODES` set, and redocly on the `Money` component +
currency `enum`. A `MONEY_ONLY_SCHEMA` (bare-`Money` response, no user structs)
covers the case where the generated `Money` definition is the file's last thing —
which surfaced and pinned a trailing-blank-line fix in Python's `models.py` tail
(the "emit once" spacer blanks leaked with no following user model). (2) Bespoke
round-trips (`tests/roundtrip/money/*`) for Go/TS/Python assert a `Money` survives
body/nested/bare-response positions and that the recursive/validator decode
accepts valid input and rejects both a bad amount and an unknown currency. (3)
Sema position-restriction tests (`check_endpoint`): a `Money` (or `Option`/`List`
of it) in query-param or header position is rejected; struct/body/response use is
accepted. All 16 round-trips (4 base + 3 DateTime + 3 UUID + 3 Decimal + 3 Money)
and all four compile-lint targets are green; 239 codegen lib tests show no drift.

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

**Two formatter fixes surfaced by the new code paths** (both were dead branches
until a real fixture exercised them, both are width-conditional so no snapshot
drifted):
- **Python `emit_py_assignment`** (response-header decode): an over-88-col
  assignment whose RHS is a single call (`expires_at =
  datetime.fromisoformat(raw or "…")`, from `internal_admin`'s now-`DateTime`
  `X-Expires-At` header) must explode the *call's* parens, not wrap the RHS in
  extra invisible parens — black does the former for a call, the latter only for
  ternaries/binary expressions. Added `single_outer_call` to distinguish them.
- **TypeScript server body revival**: `const body =
  reviveXBody(validateXBody(req.body));` overflows 80 cols once a body needs
  revival (Money/Uuid in `payments`' Customer/Charge create bodies) and the
  endpoint name is long; made it break one-arg-per-line with a trailing comma, the
  way prettier does.

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

**Sema.** A new `DefaultValue::Enum(String)`; `extract_default_value` maps a bare
identifier default to it; a shared `check_enum_param_default` (used by query +
request/response header resolution) enforces the simple-enum / no-struct
restriction on the (option-unwrapped) named type and validates the enum-variant
default against the declared enum.

**Cross-target client encoding.** TS sends `String(enumValue)` (the union value);
Go `string(v)` / `fmt.Sprint`; Python sends `.value` (NOT `str(enum)`, which would
emit `Color.Red`). The Python enum **member** is the SCREAMING_SNAKE form
(`Color.RED`), but its **value** is the bare variant (`"Red"`), so defaults render
`Color.RED` while the wire stays `Red`. The SCREAMING_SNAKE conversion now lives
in the shared `phoenix_common::idents::to_screaming_snake` (so the Python member
name and the TS `<ENUM>_VALUES` const name cannot drift). It is acronym-aware
(`HTTPError` → `HTTP_ERROR`, `RED` → `RED`), unlike the prior Python-local helper
which split before every uppercase char (`RED` → `R_E_D`). This only changes the
Python enum **member identifier** for acronym/all-caps variants — the `.value`
and wire string are unaffected, so it is a self-consistent improvement, not a
wire-format change.

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

**Import wiring.** Enum param types are named in client/handler/server signatures
(and the client casts response-header enums on read), so all three generators'
import collectors now walk query/request-header (and, for the client,
response-header) param types — `collect_import_names` / `collect_python_imports`
already add user `Named` types, so this is just extending which positions are
scanned.

**Out of scope (deferred).** `List<Enum>` in query (part of the separate
list-valued-query-param slice). The pre-existing `Uuid`/`Decimal` query params
remain unchecked on the Go server (they pass the malformed value through to the
handler rather than 400 — a divergence from enums, which validate); unifying
param-validation→400 across all branded types is a possible future cleanup.
**Done 2026-06-18:** that cleanup landed — Go now format-checks scalar (and
`List`-element) query/request-header `Uuid`/`Decimal` against `uuidRe`/`decimalRe`
→ 400, matching enums and TS/Python. See *"tighten scalar query/header
`Uuid`/`Decimal` validation on Go (2026-06-18)"* below.

**Verification.** `ENUM_PARAM_SCHEMA` (Option/defaulted enum query params, required
+ Option enum request headers, required + Option enum response headers, an
endpoint with an `error {}` block and one without) is wired into the compile-lint
harness on all four targets and both server frameworks — green (`tsc`/`eslint`/
`prettier`, `go build`/`gofmt`/`golangci-lint`, `black`/`ruff`/`mypy`, `redocly`).
Sema rejection tests cover the tagged-enum, struct (query/request/response
header), and bad-default cases; 244 codegen lib (incl. `float_and_enum_query_params`,
now with a Go `types` snapshot pinning the emitted `Valid()` method), 469 sema, and
the integration suites are green; clippy clean. Runtime interop round-trips
(`ENUM_RT_SCHEMA` + bespoke `enum/go`, `typescript/enum-driver.ts`,
`python/enum_driver.py`) drive the generated client against the generated server
and assert enum query/header values survive the wire (required + Option + a
server-applied default) AND that an unknown query/header variant is rejected (Go
`Valid()`→400, TS `parse<Enum>`→400, Python FastAPI→422) — 19 round-trips total
(the prior 16 + 3 enum) green across go/ts/python.

## Phoenix Gen — inline response projection (2026-06-17)

Lets a `response` reference an existing struct narrowed by `pick`/`omit`/`partial`
instead of declaring a dedicated read-model struct (the fixture pain points: social
`PublicProfile`, file_storage `BucketUsage`). Decisions: support
the **full `pick`/`omit`/`partial` chain** (same as `body`); scope includes the
**bare response, `List<Struct pick…>`, and paginated projected items** (plus
projection with response headers).

**Grammar (least-invasive).** A `NamedType` gained an optional
`modifiers: Vec<TypeModifier>`; `parse_type_expr` consumes a trailing
`omit`/`pick`/`partial` chain onto a bare `Named`, so a projection is accepted
wherever a named type appears — crucially **inside `List<…>`** (`List<User pick
{…}>`) without any generic-grammar change. Existing `TypeExpr::Named(n)` matches
keep compiling (they ignore `modifiers`); only the 5 `NamedType` literal sites
needed the new field. `parse_body_type` now pulls the chain off the parsed `Named`
into its `DerivedType` (one shared modifier parser, `parse_type_modifiers`).

**Sema.** `resolve_type_expr` errors on a `Named` carrying modifiers (projection
misplaced — only `body`/`response` handle them, via `resolve_derived_type`). A new
`resolve_response_projection` detects the projection (bare or `List` element),
resolves it through the generalized `resolve_derived_type_in` into a
`ResolvedDerivedType`, points the resolved response `Type` at a reference to the
generated `<Endpoint>Response` struct (bare → `Named`, list → `List<Named>`), and
stores the field set in the new `EndpointInfo.response_projection`. The
`<Endpoint>Response` name is reserved by the generated-type collision check — it
reuses the multi-status envelope's name slot, but the two never co-occur (block
form has no bare response to project), and it composes with a `Result`/`Page`
envelope (which wrap the projected struct).

**Codegen.** Each generator emits `<Endpoint>Response` from `response_projection`,
mirroring `<Endpoint>Body`: Go struct (no `Validate()` — responses are outbound),
pydantic model (shared `emit_pydantic_model` helper extracted from the body path),
TS `type` alias, OpenAPI component schema. Everything downstream
(response/list/pagination/response-header handling, imports) then treats it as an
ordinary `Named` struct. The TS revival fixed-point (`compute_revivable_structs`)
now includes projected response structs, and a new reviver block emits
`revive<Endpoint>Response` so the client revives a projected `DateTime`/`Uuid`/
`Decimal`/`Money` (incl. paginated items, with a width-aware `.map((x) => …)`).

**Bugs surfaced.** Python: a response-header endpoint's handler imported the bare
projected `<Endpoint>Response` (unused → ruff F401) — the handler returns the
`<Endpoint>Result` envelope, so the bare-response import is now gated on no response
headers. TS: the paginated-items revival line overflowed 80 cols for a long
`revive<Endpoint>Response` — made width-aware (prettier's arrow-break).

**Out of scope (deferred).** Projection on a multi-status `response { 200: Struct
pick }` (the block form), an `Option<Struct pick>` response, and a `Map<_, Struct
pick>` value projection — all need the projection to nest in those positions (the
grammar already permits it syntactically, but sema/codegen don't wire those
shapes). A projection nested in one of these unwired response shapes resolves
through `resolve_type_expr` and hits the misplaced-projection error, whose message
names the SUPPORTED shapes ("only allowed directly on a `body` base type or a
`response` type, optionally as the element of a `List<…>`") rather than claiming
the position isn't a response — so `response Option<User pick …>` isn't
misdescribed. `partial` on a response is allowed (optional fields) per the
modifier-parity decision.

A projection that **picks a `File`-typed field** off the base struct is **rejected**
(not deferred): because `resolve_response_projection` resolves before (and instead
of) the normal response path, it bypasses the file/multipart response validation
(`file_bearing_struct_allowed` + the single-`File` binary-download rule). Rather
than silently emit a `File`-bearing `<Endpoint>Response` with no multipart handling,
the resolver scans the projected field set (`field_carries_file`, covering `File`
and `Option<File>`) and errors. Supporting projected file responses later would mean
running the same file/multipart checks the bare-response path does.

**Verification.** `PROJECTION_SCHEMA` (bare `pick`/`omit`, `omit … partial`, bare
`List<pick>`, paginated `List<pick>`, projection + response headers, every
projection carrying a `DateTime`/`Uuid`) is wired into the compile-lint harness on
all four targets and both server frameworks — green. `PROJECTION_RT_SCHEMA` +
bespoke `projection/go`, `typescript/projection-driver.ts`,
`python/projection_driver.py` assert a bare projected response, a `List<…>` of them,
a `partial` projection (every field optional), and an `omit` projection (the
complementary selector) round-trip the wire and that the TS client revives the
projected `createdAt` into a real `Date` (incl. through the reviver's optional-field
wrapping path for the `partial` case) — **22 round-trips total** (the prior 19 + 3
projection). Dedicated sema unit tests cover the new response path directly: bare-
and `List`-projection resolution (asserting the `response_projection` field set and
the `<Endpoint>Response` reference), the response-context `unknown struct`/bad-field
errors, the picked-`File`-field rejection (direct and `Option<File>`), and the
misplaced-projection error in four illegal positions (struct field, query param,
`response Option<Struct pick …>`, and `response Map<_, Struct pick …>`). Parser,
sema, codegen lib tests + integration suites green; clippy clean.

## Phoenix Gen — list-valued query/header params (2026-06-17)

Allows `List<T>` in query params and request headers (the batch-endpoint gap from
the fixture audit), where `T` is a permitted scalar (`Int`/`Float`/`Bool`/`String`/
`DateTime`/`Uuid`/`Decimal`) or a simple enum. `List<Money>`/`List<struct>`/
`List<tagged-enum>`/nested `List`/`List<Map>`, `Option<List<…>>`, and a default on
a list are all rejected by sema (`check_list_param`).

**Wire format** Query params
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

**Per target.** Client: append one query value per element (Go `url.Values.Add`,
TS `params.append`, Python list value via httpx) / comma-join the encoded header
elements into one value. Server: query reads all values for the key and coerces
each (Go `r.URL.Query()[k]`, TS `toStringArray(...).map`, Python FastAPI native
`list[T] = Query(default_factory=list)`); a list request header is received as a
raw `str`/joined value and split+trimmed+coerced (Go inline split, TS
`splitHeaderList`, Python a `str` Header param split into `<name>_items` in the
route body before the handler call — FastAPI can't split a comma header into
`list[T]` natively). Enum list elements are validated per element (Go `Valid()`→400,
TS `parse<Enum>`→400; Python query elements via FastAPI→422, header elements
construct the enum → ValueError→500, a documented minor divergence). The shared
`param_enum_names` now unwraps `List<…>` so a `List<Enum>` element gets its
validator. A `Uuid`/`Decimal` element is format-checked per element on the server
(Go against the shared `uuidRe`/`decimalRe` matcher → 400, TS `parseUuid`/
`parseDecimal` → 400; Python *query* elements via FastAPI `list[UUID]`/`Decimal`
coercion → 422, but a `Uuid`/`Decimal` *header* element — coerced manually with
`UUID(...)`/`Decimal(...)` in the route body, exactly like the numeric/enum header
elements below — raises → 500, the same documented header divergence, not 422); a
plain `String` (Go `string`) appends unconverted to dodge `unconvert`. (The Go
*scalar* `Uuid`/`Decimal` query/header path was format-lenient when this slice
landed; it was tightened the next day so a list element and a scalar validate
identically — see *"tighten scalar query/header `Uuid`/`Decimal` validation on
Go (2026-06-18)"* below.)

The same query-vs-header status divergence applies to malformed *numeric*
elements: a bad `List<Int>`/`List<Float>` query element is dropped (Go/TS scalar
leniency) or 422'd (Python FastAPI), whereas a bad numeric **header** element
raises on coercion (Python `int(...)`→500; Go/TS keep the scalar-header leniency
and skip it). This matches the existing scalar-header behavior and is accepted as
a minor divergence, not a defect.

**Out of scope (deferred).** Response-header lists (the server-write/client-read
paths don't handle them — sema rejects `List` response headers).

**Verification.** `LIST_PARAM_SCHEMA` (`List` of `Uuid`/`String`/`Int`/`DateTime`/
enum query params + a `List<String>` and a `List<enum>` request header) is wired
into the compile-lint harness on all four targets and both server frameworks —
green. `LIST_RT_SCHEMA` + bespoke `list/go`, `typescript/list-run.ts` (shared by
`list-driver.ts`/`list-driver-fastify.ts`), `python/list_driver.py` assert multiple
elements (and the empty list) round-trip in both directions. Query params AND
request headers each carry EVERY permitted element type
(`String`/`Int`/`Uuid`/`Status`/`Float`/`Bool`/`DateTime`/`Decimal`), because the
query and header paths diverge per target — most sharply in Python, where a query
`list[T]` rides FastAPI's native parsing + the `py_list_query_value` client encoders
while a header is split + coerced manually in the route body — so each path needs
its own typed element per type or those branches go untested. The `List<Status>`
(simple enum) and branded/format-checked `List<Uuid>` query elements also drive the
per-element reject path (Go `Valid()`/`uuidRe`→400, TS `parseStatus`/`parseUuid`→400,
Python FastAPI→422). The TS round-trip runs against BOTH Express and Fastify (list
query params arrive as a repeated-key array via a framework-specific parser, so both
must be driven to prove the array shape `toStringArray` normalizes arrives) — **25
round-trips total** (the prior 22 + 3 list). Dedicated sema accept/reject
tests cover every element-type rule (simple-enum/scalar accept; `Option<List>`,
default-on-list, `List<struct>`, `List<tagged-enum>`, nested-collection, and
list-response-header reject); parser 208, sema 488, codegen 244 lib tests +
integration suites green; clippy clean.

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

**Change.** `emit_query_param_parse` / `emit_header_param_parse` now format-check a
`Uuid`/`Decimal` param against the shared `uuidRe`/`decimalRe` matcher var,
rejecting a malformed value with 400 — the required branch also rejects an absent
(empty) value, matching the enum required path. Both Go `string`, so the value is
read without a conversion (no `unconvert`). This brings the **scalar** path in line
with the `List`-element path tightened the day before (`go_list_elem_append`), so a
`Uuid`/`Decimal` validates identically whether it rides as a scalar param or a
`List`-valued param element, across all three targets. The matcher var lives in
types.go (composed before the server routes that reference it), so the pre-scan in
`generate` that registers it now unwraps a single `Option<…>`/`List<…>` to catch
every position.

**TypeScript (same slice).** TS already ran `parseUuid`/`parseDecimal` on each
query/header `Uuid`/`Decimal` (scalar or `List` element), but those threw a plain
`Error`, which the route's catch never matched against the `ValidationError → 400`
guard — so a malformed value surfaced as a catch-all **500** (e.g. a batch
`GET ?ids=<bad-uuid>` 500'd), diverging from the 400 an enum param gave and from
Go's new 400. Fixed by making `parse*` throw `ValidationError` (the class is now
emitted wherever a branded scalar is used) and extending the route's guard
predicate (`emit_route_error_catch`) + the `ValidationError` server import to fire
for a `Uuid`/`Decimal` query/header param. So a malformed `Uuid`/`Decimal` now
rejects with **400** on both Go and TS (Python: 422 from FastAPI), regardless of
what else the endpoint carries. (Side effect, intended: `parse*` failures elsewhere
— a constrained body's branded field server-side, or a malformed branded value in a
*response* client-side — now also throw `ValidationError`, an `Error` subclass, so
no caller breaks; a constrained body's bad branded field now 400s too, instead of
500.)

**Was a weak link, now closed (separate code path).** At the time of this slice
Go's struct `Validate()` still did not recurse into `List<Uuid>`/`Map<String, Uuid>`
(or `List<Money>`) **field** elements — the general-nested-validation feature,
orthogonal to this query/header-param slice. That gap was **closed 2026-06-20** —
see "Go nested `Validate()` recursion" below.

**Verification.** Compile-lint: `UUID_SCHEMA`/`DECIMAL_SCHEMA` gained `Option<Uuid>`/
`Option<Decimal>` query + request-header params so all four parse branches (scalar/
`Option` × query/header) are exercised; all four targets green. The Go UUID and
Decimal wire round-trips **flip** their former accept assertion to a reject: a
malformed `ref`/`minAmount` query value now 400s (non-nil client error). The TS
UUID/Decimal drivers (and the list driver's `List<Uuid>` element) now issue a raw
`fetch` and assert the exact **400** — pinning the plain-`Error`→500 fix, not just
"client threw" — so Go and TS agree on 400 (Python 422). Clippy clean; 244 codegen
lib tests green; 25 round-trips green.

## Phoenix Gen — URL & bytes scalar types (2026-06-19)

Adds two scalars the fixture library wanted: `Url` (a validated URL string) and
`Bytes` (a first-class binary value). The user chose the semantics up front: `Bytes`
is a **real binary value** at runtime (`Uint8Array` / `[]byte` / `bytes`) carried as
base64 on the JSON wire — not a string the caller has to encode themselves; `Url` is
a **branded + validated** string (the `Uuid`/`Decimal` model), validated everywhere
but **never normalized**, so it round-trips byte-for-byte.

**Type plumbing (both).** New `Type::Url` / `Type::Bytes` variants + `from_name` /
`Display` / `object_safety` (not object-safe) / IR `lower_type` (`StringRef`
placeholder — never executed, codegen-only types). Each of the four generators maps
the type, (de)serializes it, and threads its import/helper through.

**`Url` — validated, never normalized.** All three targets validate the SAME way —
**scheme presence only** — so the servers agree on which strings are valid. A fuller
parse (`URL.canParse`, `net/url.ParseRequestURI`) was rejected precisely because the
language URL libraries disagree at the edges, which would mean a value one generated
server accepts and another rejects — breaking the whole-stack contract guarantee.
(One residual edge: Python's `urlparse` strips embedded `\t`/`\r`/`\n` before
parsing, so a URL containing a raw tab/newline can validate in Python while the
Go/TS anchored scheme regex rejects it — pathological input that would never
round-trip identically anyway, accepted as out of scope.)
- **TS:** a branded `Url = string & {…}` with `parseUrl`, folded into the regex-based
  `ts_branded_scalars` machinery (revival + query/header coercion +
  `has_validated_param`) exactly like `Uuid`/`Decimal` — a shared `URL_RE`
  (`/^[a-zA-Z][a-zA-Z0-9+.-]*:/`, the same scheme regex as Go's `urlRe`). Added
  `Type::Url` to every hardcoded `Uuid | Decimal` coercion arm. (An earlier draft used
  `URL.canParse`, which validated *more* than the other two and so was inconsistent —
  replaced with the scheme regex.)
- **Go:** a `string` format-checked by a shared `urlRe` (`^[a-zA-Z][a-zA-Z0-9+.-]*:`
  — requires a scheme) in struct `Validate()` and, **this slice**, in the scalar
  query/header param branches (`Uuid | Decimal | Url`) and the `List`-element branch
  — so a single `Url` query/header param now 400s on a malformed value, matching the
  `List<Url>` element path and the TS/Python behavior. (Before this, a single `Url`
  query/header reached the handler unvalidated, the same gap the prior slice closed
  for `Uuid`/`Decimal`.)
- **Python:** `Url = Annotated[str, BeforeValidator(_validate_url)]` where
  `_validate_url` rejects a value whose `urlparse(...).scheme` is empty; value stays
  `str` so it round-trips exactly; FastAPI runs the validator on query/header params.
- **OpenAPI:** `{type: string, format: uri}`.

**`Bytes` — real binary, base64 wire.**
- **TS:** runtime `Uint8Array`. Because `JSON.stringify` turns a `Uint8Array` into an
  index object and `res.json`/`fetch` take no replacer, a deep-walk `encodeBytes`
  helper rewrites every `Uint8Array` to its base64 string before serialization (on
  the client request body and the server response), guarding `Date` (so `Date.toJSON`
  still runs) and primitives. The decode/revival path uses `bytesFromBase64` (`atob`).
- **Go:** `[]byte` — `encoding/json` already base64s it both ways, no extra machinery.
- **Python:** the original `Base64Bytes` choice was **behaviorally wrong** and the
  round-trip caught it: `Base64Bytes` treats *all* construction input as base64 to
  decode, so a caller passing raw `bytes` (or the echo handler re-wrapping the
  decoded value) got corrupted output. Replaced with a custom alias
  `Bytes = Annotated[bytes, BeforeValidator(_bytes_from_b64), PlainSerializer(_bytes_to_b64, return_type=str)]`:
  the validator decodes a base64 *string* but passes raw `bytes` through unchanged
  (so a caller works with binary directly), and the serializer base64-encodes on
  dump. Unlike `datetime`/`UUID`/`Decimal`, `Bytes` does **not** join the client's
  `model_dump(mode="json")` gate: `PlainSerializer` runs with the default
  `when_used="always"`, so a `Bytes` field is already a base64 string under the plain
  `model_dump()` — it never leaves a non-JSON-safe value that `json.dumps` would
  reject.
- **OpenAPI:** `{type: string, contentEncoding: base64}` (the spec is 3.1 /
  JSON Schema 2020-12, where this — not the 3.0 `format: byte` — is the idiomatic
  base64 representation; `format: byte` would be only an ignored annotation under 3.1).

**Position restrictions (sema).** `Bytes` is body/struct/response-only — rejected in
query/header/path via a new `Type::mentions_bytes()` predicate (a binary value is not
a URL/header-encodable scalar), the binary analogue of the existing `Money` rejection.
`Url` is allowed everywhere (it is a validated string).

**Verification.** Compile-lint: a new `URL_BYTES_SCHEMA` (struct fields + `Option` +
`List` + a `Map<String, Bytes>` field for both types, plus `Url` query / `List<Url>`
query / `Url` header, and a multi-status `replace` whose shared body carries `Bytes`)
across all four targets, green. Behavioral round-trips: a new `url_bytes` driver per
target (Go/TS/Python) asserts (a) `Bytes` survives as raw binary including **non-UTF-8
bytes** (0x00/0xFF/0xFE/0x80) through body/`Option`/`List`/`Map<String, Bytes>`/response
— the server stub asserts it received a real `Uint8Array`/`bytes`, not the base64
string — and (b) `Url` round-trips **byte-for-byte** (query string + fragment +
non-lowercased host all preserved) through body/`Option`/`List`, a query param, a
`List<Url>` query param, and a request header, plus the malformed-`Url` reject path
(TS/Go 400, Py 422). The `Map<String, Bytes>` exercises the only remaining combinator
over `Bytes` (TS `encodeBytes` deep-walk over a `Record` + `Object.fromEntries`
revival; Go `map[string][]byte`; the dict-valued pydantic alias). A multi-status
`replace` endpoint additionally round-trips a `Bytes`-bearing shared body through the
`{ status, body }` response envelope, behaviorally exercising the `encodeBytes` wrap on
the server's `result.body` branch and the client's revival of the envelope body
(compile-lint alone cannot catch a missing wrap/revival there). A bare `Bytes` response
leaf (not struct-wrapped) is decoded inline by the client — TS imports `bytesFromBase64`,
and **Python** decodes via `base64.b64decode` in `py_decode_expr` (its own `Bytes` arm,
plus an `import base64`), because a non-struct leaf never runs the pydantic `Bytes`
alias (only `Model(**…)` does); Go gets it free from `encoding/json`. This is covered by
per-generator unit tests (TS + Python) and a `raw`/`rawList` (`Bytes`/`List<Bytes>`)
endpoint in `URL_BYTES_SCHEMA` so all four targets compile-lint the inline path — the
behavioral fixtures only ever wrap `Bytes` in a struct (whose reviver/alias handles it).
Bytes-position rejection (query/header/response-header) has parallel sema tests; the
list-element diagnostic for a `List<…>` response header is intentionally kept generic
(one error per span — the list-ness, not the element, is the reported reason).

**TS body-revival reject → 400 (gap closed in this slice).** A `Url` (or `Uuid`/
`Decimal`/`Money`) **body field** validates on `revive<Endpoint>Body` (`parse*` /
`reviveMoney` throws `ValidationError`) even with no `@`-constraint. The TS server's
`ValidationError → 400` guard was previously gated on `has_body_constraints ||
has_validated_param`, so a body whose *only* validation is a branded-scalar field —
and no validated query/header param — let the throw fall through to the catch-all
**500**, diverging from Go's body `Validate() → 400`. Added a `body_has_validated`
gate (`type_reaches_validated`, transitive through `Named` struct fields, mirroring
`type_reaches_bytes`) to `validates` and to the `ValidationError` server-import
condition, so such a body now 400s in both frameworks. (The `URL_BYTES` schemas
masked this because they always pair a `Url` body with a `Url` query param;
regression unit tests now cover a `Url`-only and a `Money`-only body.) The
behavioral drivers also pin the `Url` reject paths to the exact status (Go/TS raw
requests → **400**, Python → **422**, instead of "client errored" / "≥ 400", which a
500 regression would have passed) and add a malformed `List<Url>` query-element
reject.

Clippy clean; 513 sema + 250 codegen lib + 85 integration green; 28 round-trips green
(25 prior + 3 new).

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

**Change (Go target only).** `render_validate_fn` (`phoenix-codegen/src/go.rs`) no
longer takes flat `regex_fields` / `money_fields` lists. It takes one `nested_fields`
list — every field that [`type_is_validatable`] — and emits each via a new recursive
`emit_value_validate`, which descends the type:
- a regex scalar (`Uuid`/`Decimal`/`Url`) → `if !<re>.MatchString(v) { … }`;
- a `Money` or a validatable named struct → `if err := v.Validate(); err != nil { return fmt.Errorf("<field>: %w", err) }`;
- `List<T>` / `Map<String, V>` → `for _, eN := range … { <recurse on the element> }` (the map key is always `String`, never validatable);
- `Option<T>` → folded into a Go `*T` pointer with a `!= nil` nil-guard, then recursion on the pointee.

So `List<Money>`, `Map<String, Uuid>`, `List<NestedStruct>`, and arbitrarily nested
combinations now validate every element. Two new predicates back this and keep the
emit decision honest: `type_is_validatable` (does a value of this type need any
`Validate()` work, recursing through collections and named structs) and
`struct_needs_validate` (does a named struct get a `Validate()` method at all — it
does iff it has a constrained field or a validatable field; cycle-guarded with a
`visited` set). The single source of truth `type_is_validatable` now drives all three
gates that must agree — the source-struct `Validate()` emit gate, the derived-body
`Validate()` emit gate, and the server-side `body.Validate()` *call* gate — so a
`Validate()` is generated iff it has a body and called iff it was generated. A
**struct that previously got no `Validate()`** (e.g. one whose only validatable
content is a `List<Uuid>`) now gets one. Generated code stays gofmt-clean and the
value-receiver `Validate()` is callable on a `*T` element, so a pointer element needs
only the nil-guard.

**Verification.** The Go compile-lint harness (incl. the realistic fixture library —
`payments`, etc. — which carries `List`/`Map` of validatable types) compiles +
`gofmt`-clean. The Money round-trip already carried `List<Money>` / `Map<String,
Money>` / `List<LineItem>` fields; its **three** drivers now each assert the
nested-element reject path (bad currency in a `List<Money>`, bad amount in a
`Map<String, Money>` value, bad currency in a `List<LineItem>`'s nested `Money`) so
all three servers are proven to agree — Go's new recursion matches Python's pydantic
recursion and TS's revive walk. The existing 250 codegen lib snapshots are unchanged
(none exercised nested-collection validation), and a **new** snapshot
(`go_validate_nested_collections_types`, `validate_nested_collections` in
`go_tests.rs`) pins the previously-unpinned generated Go for the non-`Money` shapes
the behavioral drivers don't cover as source text: a regex scalar inside a `List`
(`List<Uuid>`) and a `Map` value (`Map<String, Decimal>`), an `Option`-wrapped
collection (`Option<List<Url>>` — nil-guard + range), a direct nested-struct field
(`primary: Address`), and a `List` of that struct (`List<Address>`). A second new
snapshot (`go_validate_recursive_struct_types`, `validate_recursive_struct`) pins a
self-referential struct (`Tree { id: Uuid, children: List<Tree> }`) to prove the
`visited` cycle-guard terminates and that a `Type::Named` element emits a single
`e0.Validate()` call (finite generated code; the recursion is the runtime data walk).
clippy clean; 28 round-trips green. **Bug closed:** the `known-issues.md` entry "`Money` element
validation inside `List`/`Map` is skipped in the Go target" (which also covered the
regex-scalar and nested-struct element cases) is removed.

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

**Python.** The multipart route already binds each scalar as `name: T = Form(...)`.
FastAPI's `Form(...)` accepts the same validation kwargs as pydantic `Field(...)`
(`min_length`/`max_length`/`ge`/`le`/`gt`/`lt`), so the fix is to feed the existing
`constraint_to_field` extraction into the `Form(...)` call —
`caption: str = Form(..., min_length=1)`, `rank: int = Form(..., ge=1)`. A violation is
a 422 (FastAPI's validation status), identical to the JSON pydantic path. No new
constraint-translation code; the JSON and multipart paths share `constraint_to_field`,
so they cover exactly the same subset (see the residual note below).

**TypeScript.** The generated `validate<Endpoint>Body` already (a) is emitted whenever
the body has a constrained field — including a multipart body — and (b) **safely
ignores `File` fields**: `ts_typeof_of(File)` is `None` and a `File` field carries no
constraint, so `emit_validation_body` emits neither a `typeof` check nor a constraint
guard for it. So the fix is simply to call it: the multipart route now assembles the
body inside `validate<Endpoint>Body({ … })` (the file fields pass through untouched,
the scalar fields' coerced values are `typeof`-checked and constraint-checked). The
validator throws `ValidationError`, which the route's existing catch maps to 400.
Two import gates were adjusted so the validator is imported (and the bare body *type*
is not, since the validator's return now supplies it) for constrained multipart bodies.

**Verification.** Compile-lint green on all four targets (the `file_storage` fixture's
`ObjectUpload` carries constrained multipart scalars). A new contract round-trip case
`uploadAvatar_multipart_constraint_empty_caption` (empty `caption`, violating
`self.length > 0`) drives all three servers and asserts the handler is **not** called
and the client sees the reject — 400 on Go/TS, 422 on Python — proving the gap is
closed end-to-end. (The drivers needed no changes: each dispatches `kind: constraint`
generically — invoke-by-endpoint, catch, assert status.) Two focused unit tests lock
the generated-output shape the round-trip exercises only behaviorally:
`multipart_constrained_scalar_validates_server_side` (TS — the `validate<Endpoint>Body({…})`
wrapping, the validator import, and the dropped bare-type import, plus a new server
snapshot) and `multipart_scalar_where_constraint_binds_form_kwarg` (Python — the
`Form(..., min_length=…)`/`Form(..., ge=…)` binding, with the `File` field taking no
kwarg). Pre-existing codegen lib snapshots unchanged; clippy clean; 31 round-trips green
(the new `uploadAvatar_multipart_constraint_empty_caption` case brings the suite to 31).

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
to the generators is unchanged, so generated output is byte-identical (256 codegen
snapshots unchanged).

**Verification.** Thirteen new sema regression tests: a typo'd `.length` is rejected;
`.length` on `String`/`Option<String>`/`List`/`Option<List>` is valid; `.length` on
`Int`/`Map`/`Bytes` is rejected; numeric and equality comparisons on `Option<Int>`
are valid; a presence-plus-length idiom (`self.isSome() && self.length > 0`) on
`Option<String>` is valid; the two residuals are LOUD — a `.contains` method call and
a struct-field access (`self.zip`) on an `Option<_>` field each produce a real
diagnostic; `self.isSome()` on `Option<Int>` still valid (pre-existing test, unchanged). 524 sema lib tests green; the Go compile-lint
harness (which runs every realistic fixture — all of which use `Option<String>
where self.length > 0`-style constraints — through the full pipeline) green; the
constraint-heavy `gen_api.phx` round-trips green; clippy clean. The whole driver
test suite passes uncapped (the `matrix_*` failures seen under `ulimit -v` are
wasmtime `mmap` reservations starved by the cap, unrelated).

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
(`.length`, numeric, `.contains`, `.isSome()` all work). The method-dispatch block
in `check_method_call` was extracted into a `dispatch_builtin_method(mc, ty)` helper;
when no path resolves the method on the `Option` itself, the constraint context
retries that helper on the unwrapped inner type **as a last resort** — placed *after*
every `Option`-level path (built-in `Option` methods, user-method table, trait
bounds), so a real `Option` method like `isSome` still resolves on the `Option` and
is never shadowed. `check_option_method` returns `None` for an unrecognized method
without side effects, so the retry neither double-checks args nor double-reports.

The single remaining residual is now just struct-field access on an `Option<Struct>`
field (`self.zip` on `Option<Address>`) — a separate path (the struct branch looks
up the outer type), loud, no fixture hits it; tracked in
[known-issues.md](known-issues.md). Verified by sema regression tests
(`.contains`/`.isSome() && .length` on `Option<String>` valid; the struct-field case
still rejected, naming the inner type); 527 sema lib tests green; 256 codegen
snapshots unchanged (sema-only, output byte-identical); Go compile-lint + driver
suite (uncapped) green; clippy clean.
