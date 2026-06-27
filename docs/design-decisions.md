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

**Decided 2026-06-10.** Every named, typed field in the language declares as `name: Type`: struct fields (`x: Int`, with `public`, `where` constraints, and doc comments unchanged), endpoint `query` parameters (`page: Int = 1`), and endpoint `headers` entries (`rateLimit: String as "X-RateLimit-Limit" = default`). The previous type-first form (`Int x`) was the lone holdout against the rest of the language — function parameters, return annotations, `let` bindings, and map literals all already used colon syntax — and it actively trapped users: writing the natural `x: Int` in a struct didn't error, it **hung the parser** (see the Phase 2.4 "Bugs closed" entry). The old form is a hard parse error with a targeted migration diagnostic ("write `x: Int`, not `Int x`"); no dual-syntax transition period (pre-1.0, with no schemas in the wild to migrate — dual grammar would be pure debt against the consistency goal). The phase-4 `schema`/`table` DSL sketch was updated to match so the future column grammar starts consistent. Out of scope: enum variants stay positional (`Circle(Float)` — they mirror constructor calls, not field declarations).

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
**Target phase:** Phase 2.2 (Cranelift native compilation)

**Rationale:** Phoenix is positioned as a compiled language with native performance, and the stdlib generics are already hand-monomorphized in the Cranelift backend. Any other strategy creates a two-tier world where stdlib is fast and user generics are slow. Monomorphization also stacks cleanly with a future vtable ABI for `dyn Trait`. The compile-time fan-out cost is real but mitigable (shared specialization where layouts match, incremental caching of instantiations) and only bites meaningfully once the stdlib grows in Phase 4.

**Alternatives considered:**
- **Uniform boxed representation (Go / Java-erasure style)** — one compiled copy, values passed as type-tagged pointers. Rejected: contradicts the native-performance positioning, forces the hand-monomorphized stdlib to either be rewritten boxed or live in a different world than user generics, and overlaps awkwardly with the planned `dyn Trait` vtable ABI.
- **Hybrid (monomorphize hot, box the rest)** — two ABIs plus a heuristic. Rejected: adds per-function uncertainty ("will this be fast?") without a clear rule, and the compile-time / binary-size savings don't matter at Phoenix's current scale.

### Dynamic dispatch via `dyn Trait`

Traits in Phoenix are statically dispatched only today — every trait use must be monomorphized at a concrete type via a generic bound, and a grep for `dyn` / `vtable` / `witness` / `trait_object` in `phoenix-sema`, `phoenix-ir`, `phoenix-cranelift` finds nothing. The consequence is that heterogeneous collections of trait implementors are unexpressible: a web router cannot hold `List<Handler>` where each element is a different concrete handler type. This question is whether and how to add runtime-dispatched trait objects.

**Decision:** Add `dyn Trait` with a vtable ABI. A `dyn Trait` value is a fat pointer `(data_ptr, vtable_ptr)` — the same shape as Phoenix's existing fat-pointer conventions (e.g., `String` is `(ptr, len)`). Static dispatch remains the default; users pay the indirection only when they explicitly write `dyn`. This is the Rust-style model, not Java's virtual-by-default.
**Decided:** 2026-04-19
**Target phase:** Phase 2.2 (Cranelift native compilation)
**Rationale:** Phoenix is AOT-compiled via Cranelift with no JIT planned. Java-style virtual-by-default only works because the JVM's JIT devirtualizes hot call sites at runtime; without that, virtual-by-default is just "slow by default" and would contradict the monomorphization decision made above. Every AOT language without a JIT (C++, Swift, Rust) has landed on opt-in dynamic dispatch for the same reason. The vtable ABI is the long-lived commitment that must be set before Cranelift's calling conventions solidify; object-safety rules can be tightened over time.

**Alternatives considered:**
- **Stay fully static.** Rejected: fights Phoenix's web-framework positioning — middleware chains, route handlers, plugin APIs all want heterogeneous collections. Forcing enums (closed sets) for cross-type polymorphism rules out extension patterns entirely.
- **Existential types / `impl Trait` in return position only.** Rejected on its own as too narrow — does not solve heterogeneous collections. Could be layered on top of the chosen option later as a sugar for hiding concrete return types, but not as a replacement.

**Follow-ups (not in scope here):**
- Object-safety rules (which traits are dyn-compatible). Can evolve after the ABI lands.
- Sugar for bare trait names in type position (`List<Handler>` auto-meaning `List<dyn Handler>`). Pure ergonomic layer; revisit if explicit `dyn` feels noisy in practice.

**Why explicit `dyn` (vs. implicit `Drawable` as a dynamic-dispatch type)** (decided 2026-04-20): bare `Drawable` as a type remains a compile error. Users must write `dyn Drawable` for runtime dispatch or `<T: Drawable>` for static. Reasons: (a) Phoenix already has static-dispatch generic bounds, so implicit-dyn would create a subtle perf gotcha where `foo<T: Drawable>(x: T)` and `foo(x: Drawable)` look similar but compile very differently; (b) explicit `dyn` makes runtime cost visible (indirect call, no inlining); (c) leaves syntactic room for future `impl Trait` / existential return types; (d) follows Rust 2018 / Swift 5.6 precedent — both started implicit and added explicit markers after user confusion about performance. The tradeoff accepted: one more keyword to learn, in exchange for a clearer distinction between Phoenix's two trait-dispatch modes.

**Deferred follow-ups.** Each carries a phase target; see known-issues.md for the concrete tripwires and workarounds.

| Follow-up | Target | Summary |
|---|---|---|
| Multi-bound trait objects (`dyn Foo + Bar`) | Phase 3 | Requires deciding whether bounds must be object-safe individually or only in combination, and whether the vtable is merged-method or multi-pointer. |
| Supertraits (`trait Sub: Super { ... }`) | Phase 3 | Affects trait-declaration syntax, `dyn Sub → dyn Super` coercion, and vtable layout. Sema doesn't model supertrait relations today. |
| `where Self: Sized` method carve-outs | Phase 3+ | Rust's mechanism for "mostly object-safe" traits. Open whether Phoenix needs it or users should split the trait. |
| Heterogeneous list literals (`[Circle(1), Square(2)]` typed `List<dyn Drawable>`) | Phase 3 | Blocked on bidirectional type inference in list-literal checking. The previously suggested `push()` workaround does not work today (sema rejects `let xs: List<dyn Trait> = []` because the empty literal types as `List<T>`). See [known-issues.md](known-issues.md#listdyn-trait-literal-initialization-in-compiled-mode). |

### Default-argument lowering strategy

Phoenix supports default parameter values: `function render(title: String = "untitled") -> String`.  Sema accepts the declaration and the AST interpreter evaluates defaults at call time, but when Cranelift compilation landed, a design question surfaced: *where* does the default expression get materialized?  Two plausible sites exist — at the caller's call site (inline the default once per omitted slot) or at the callee's entry block (synthesize the default once, guarded by a "this slot was omitted" flag passed from every caller).

**Decision:** Caller-site materialization.  `FunctionInfo` on the sema side carries `default_param_exprs: HashMap<usize, Expr>` — the full parsed expression cloned at function registration — and `merge_call_args` in `phoenix-ir/src/lower_expr.rs` lowers each missing slot's default into the caller's IR at the call site, before `coerce_call_args` runs.  No ABI change; no fill-mask; every `Op::Call` is emitted with a complete argument vector.
**Decided:** 2026-04-24
**Target phase:** Phase 2.2

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
**Target phase:** Phase 2.2.  Pure refactor, no semantic change.  Ships ahead of Phase 2.6 so the module-system work can build on a settled sema→IR boundary rather than redesigning it mid-phase.
**Why two types instead of one.**  An earlier pass collapsed everything onto a single `ResolvedModule`, but that produced a schema-shaped name carrying schema-irrelevant data (semantic diagnostics on a "resolved" module reads contradictorily), forced IR lowering to take a 17-field god-struct when it only needed the schema slice, and made it ambiguous which fields a future addition belonged on.  The split keeps `ResolvedModule`'s contract clean ("the resolved schema; IR consumes this") and gives auxiliary outputs a dedicated home (`Analysis`) so adding new ones (e.g. macro-expansion traces, dependency edges) doesn't widen IR's parameter type.

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
**Target phase:** Phase 2.2 (Cranelift native compilation)
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

This invariant is load-bearing for any helper that brackets work between `phx_gc_push_frame` and `phx_gc_pop_frame`: a panic between the two would (a) trigger the UB above and (b) leak the frame. New helpers that need to push/pop a frame around a multi-step operation must therefore audit every function called between the push and the pop and confirm none of them panic.

The audited helpers are intended to stay panic-free in their entirety: there is no "documented as unreachable" carve-out. If a future helper genuinely needs to surface failure to the caller, the response is to switch its declaration to `extern "C-unwind"` (and audit every Cranelift call site) — *not* to silently allow the panic. A `Drop`-guard approach is insufficient because the unwind-across-FFI is the deeper hazard than the leaked frame.

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

**Decision (5, Phase 4) — namespace imports.** Alongside the brace forms (`import a.b { Foo }`, `import a.b { * }`), `import a.b` and `import a.b as c` (no braces) bind the *module itself* as a qualified namespace under its last path segment (or the explicit alias), enabling `c.func(...)` access. The brace must hug the path on the same line — `import a.b` followed by a brace on the next line is the named form mis-typed and is rejected with a dedicated diagnostic, not silently read as a namespace import. The single-segment form (`import json`) is the idiomatic stdlib shape.
**Landing in three steps:** (1) the *syntax* (parser + AST `ImportItems::Namespace`) landed first; (2) *binding + `ns.func(...)` sema dispatch* landed next — a namespace import binds the target under its local name in a dedicated per-module `namespaces` map (kept out of `visible_symbols`, so a namespace is neither a value nor a type — it only legalizes qualified calls), and `ns.func(...)` dispatches at the call site against the target module's public functions, honoring 2.6 visibility per-call (sema records the resolved qualified callee in `ResolvedModule::namespace_call_targets`); (3) *execution* landed across all five backends — IR lowering resolves the recorded target to a `FuncId` and emits an ordinary `Op::Call` (so the IR interpreter, native, and both wasm backends dispatch it like any call), and the tree-walk interpreter dispatches to the target function in its owning module. Generics flow through unchanged: a generic function called via a namespace threads its inferred type args through the existing span-keyed monomorphization path. A local binding of the same name (`let json = …`) shadows the namespace and routes back to the value-receiver path. A multi-file fixture (`tests/fixtures/multi/namespace_import`) pins byte-identical output across all five backends.

**Duplicate imports are rejected.** Two imports in the same module that bind the same local name — across *any* forms (named, wildcard, namespace, intrinsic), e.g. `import a.user` + `import b.user`, or `import a { foo }` + `import b { foo }` — are a hard error (first import wins, the duplicate is not bound) rather than silent last-wins shadowing. The escape hatch is `as`: rebind one import under a distinct name. The check tracks only import-introduced names, kept apart from own-module declarations and builtins (which legitimately populate `visible_symbols`).

**Reserved intrinsic namespaces.** A single-segment import naming a *compiler-intrinsic* namespace (`import json`) binds an intrinsic whose members the compiler synthesizes, rather than resolving to a `.phx` file on disk — the resolver skips file resolution and adds no module-graph edge for it. Reserving the name means a project cannot shadow it with a top-level source module of the same name (the same trade-off as a reserved keyword), keeping the stdlib import shape stable. Today the only intrinsic is `json`; its `encode`/`decode` members are synthesized by the JSON serialization work (Phase 4.6), so until then a `json.*` call binds but reports "not available yet", and destructuring/wildcard forms of an intrinsic are rejected (use `import json` + `json.encode(...)`).

**Bundled scope:** The closure capture type ambiguity bug (since fixed) was originally tied to this refactor on the assumption that capture metadata would land in a unified IR closure representation alongside the AST-to-IR switch. With the reversal, the bug is fixed independently in IR + Cranelift via an env-pointer calling convention (closure functions take their environment pointer as the first arg and unpack captures from the heap object themselves; capture types never cross the indirect-call boundary, structurally eliminating the ambiguity). `phoenix-interp` is not touched by that fix.

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
**Rationale:** committed numbers are visible in the repo and PR diffs catch obvious regressions. Cost is low (markdown table per phase) and the format stays human-readable.

**Format:** per-bench markdown table with columns `bench / parameters / mean / median / stddev / sample-size`. Refreshed at phase close and on intentional perf-affecting changes. Source files reference the baseline path so a maintainer who cuts a regression knows where to look.

**Alternatives considered:**
- *Criterion `--save-baseline` / `--baseline`* — files in `target/criterion/`, never committed; per-CI-host so cross-host comparison is meaningless without normalization. Useful for local before/after on a single machine but not for the cross-PR detection problem we have.
- *External service (bencher.dev or similar)* — durable and comparable but adds a third-party dependency on accounts, API tokens, and out-of-repo state. Revisit if the manual snapshot becomes a maintenance pain.

#### B. CI gating policy: post-merge on `main`

**Decided:** 2026-05-04
**Rationale:** middle ground between the two extremes. Per-PR gating with N% slack flakes too easily before we know how stable the numbers are. Informational-only is too easy to ignore — regressions can land unnoticed for weeks.

**Original design:** GitHub Actions workflow on `push: main` that runs `cargo bench`, parses criterion output, compares to the committed baseline, and opens an issue if any number regresses by more than 20%. Cross-language Go comparisons (decision E) are explicitly excluded from this CI loop — they run off-CI per phase-close.

**Alternatives considered:**
- *Informational only* — devs read the trend; no automated alerting. Lowest CI cost (~0 if not run on PR), zero flake risk, but regressions survive too long unnoticed.
- *Per-PR gating with N% slack* — fails CI if the new number is >N% slower than baseline. Catches regressions immediately but flakes when the runner has noisy neighbors. Reopen if post-merge gating ends up being too late.

#### C. Calibration and runner constraints

**Decided:** 2026-05-04
**Rationale:** pause-time numbers in particular are sensitive to glibc allocator behavior, NUMA, kernel page-fault costs. Without controls the numbers flake; documenting the recipe means a future "the numbers got worse" investigation can rule out runner drift before chasing a real regression.

**Recipe:**
- Pinned CPU governor (`performance`) when the runner permits.
- Minimum 5-run aggregate per bench, criterion default sample size unless variance is unworkable.
- Document the runner spec (CPU model, kernel version, glibc version, criterion version) in the baseline file's header.
- Single-threaded runs only in 2.7. Multi-threaded benches arrive with Phase 4.3 (async runtime) and need separate calibration.

**Fallback if the recipe still flakes in practice:** drop CI gating to informational-only (decision B) until the runner is fixed, rather than tolerate noisy alerts.

#### D. Aggregate choice

**Decided:** 2026-05-04
**Rationale:** different bench shapes need different summary stats. Switching aggregates mid-phase makes historical comparisons useless, so pick once and stick.

- **Throughput benchmarks** (allocation, collections, end-to-end compiled binary): mean / median / stddev — criterion's defaults; well-understood and adequate for steady-state work.
- **Pause-time benchmarks** (GC collection latency): P50 / P95 / P99 / max — need the tail to catch worst-case stalls; mean alone hides them.

#### E. Cross-language comparison scope: Go 1.22+ only

**Decided:** 2026-05-04
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
| **H. Implement a wasm32 `TargetIsa` for Cranelift ourselves** | Add an emission backend to Cranelift upstream (or as a side project). | Reuses Cranelift IR → wasm32 across all Cranelift consumers, not just Phoenix. | Research-grade project (CFG → structured control flow is its own algorithm). Months-to-years of work outside Phoenix scope. Doesn't help PR 5's WASM GC variant (managed refs don't map to CLIF). | Rejected for Phoenix — a research-grade effort outside its scope. |

**Versioning note.** `wasm-encoder` and `wasmparser` are pinned together at the same version because their codepoints (instruction encodings, type representations) are kept in lockstep by the Bytecode Alliance. Bumping one without the other is a known-bad pattern; the workspace `[dependencies]` block keeps them paired.

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

**Rationale:** On native, Cranelift emits a `.o` referencing `phx_*` symbols and `cc -lphoenix_runtime` resolves them at link time. WASM has no equivalent of `cc` + `libphoenix_runtime.a`, so we choose how the runtime's compiled wasm32 bytes get into the user-program `.wasm` module. **Embed-and-merge** keeps the pipeline pure-Rust and matches the wasm-encoder shape we already use: `phoenix-runtime` is compiled once to a complete wasm32 module, and at codegen time phoenix-cranelift splices its functions, data segments, and globals into the wasm-encoder output so user code and runtime live in one flat index space.

**Pros:** pure-Rust pipeline; no external linker dep; merge logic is bounded (~few hundred LOC) and reused for PR 5's WASM GC variant; future-proof against runtime growth (any Rust the runtime crate adds just compiles to more WASM functions that get merged through the same path).

**Cons:** index fix-up is fiddly (every function call, every global reference, every memory access has to be rewritten); the merge logic is its own surface to maintain; debug info is dropped on the floor in the first cut.

**Alternatives considered:**
- *`wasm-ld` (LLVM linker).* Compile both `phoenix-runtime` and our wasm-encoder output as relocatable wasm32 objects, link via `wasm-ld`. Native-shape. Rejected: adds LLD as a build-time dependency on every dev / CI machine; the wasm-encoder side gets significantly more complex (linking metadata sections, relocations); we lose direct control over the final module's section layout — which matters for PR 5's WASM GC custom type-section ordering.
- *Pre-bake runtime bytes via `include_bytes!`.* Variant of embed-and-merge — the runtime `.wasm` builds at workspace-build time and embeds directly into `phoenix-cranelift`. Rejected for PR 3 in favor of the runtime-discovery shape that mirrors native's `find_runtime_lib`; revisit if the manual `cargo build -p phoenix-runtime --target wasm32-wasip1` step proves persistently annoying.

#### G. Control-flow translation: loop+switch dispatch, relooper deferred

**Decided:** 2026-05-15 (during PR 3 scope review).

**Rationale:** WASM has no general `goto` — only structured `block` / `loop` / `if` with branch-out-to-enclosing-label. Phoenix's IR is a basic-block CFG. The translation has two practical shapes; we pick the simpler one for PR 3b and reopen if benchmarks demand the tighter one.

**Chosen: loop+switch dispatch** — the "irreducible-CFG fallback" pattern used by LLVM's wasm backend when relooper fails. Correctness is unconditional — any CFG, including loops with multiple entry points, lowers cleanly. Output quality is "fine, not great" — extra `br_table`s and locals that Wasmtime / V8's optimizers mostly clean up at JIT time.

**Deferred: relooper / Stackifier.** The published algorithm that reconstructs structured control flow (LLVM's `WebAssemblyCFGStackify.cpp`, ~1.5k LOC) produces tighter output but is a project unto itself. Phase 2.4 doesn't gate on output quality; the [phase-close bench refresh (decision D)](#d-phase-close-bench-refresh-scope-wasm-vs-native-phoenix-only) is the right place to discover whether control-flow overhead is a real signal in WASM-vs-native, and a relooper PR can land in Phase 2.5+ if it is.

**Tripwire for revisiting:** if the phase-close `wasm32-linear` numbers on `fib_recursive` or `sort_ints` are >3× slower than native specifically because of the `br_table` dispatch (verified via wasmtime profiling), the relooper pass becomes a real follow-up. Otherwise the deferral holds indefinitely.

**Alternatives considered:**
- *Relooper up front.* Rejected per the scope argument above: ~1.5k LOC of intricate algorithm on top of everything else PR 3b is doing.
- *Stackifier (LLVM's successor).* Same reasoning, plus the algorithm is even more complex.
- *Cranelift IR's loop analysis as a substrate.* Rejected: would tie the WASM emitter to Cranelift's pass infrastructure, undoing decision A0's separation.

#### H. String-literal materialization: data-section borrowed pointers

**Decided:** 2026-05-15 (during PR 3c scope review).

**Context.** PR 3b's first attempt at `Op::ConstString` placed string bytes in a user data segment above the runtime's data section. The runtime's allocator clobbered those bytes because its compiled `__heap_base` (the offset where dlmalloc starts serving free memory) sits at the end of the runtime's own data section, with no exported global to override. PR 3b deferred strings to PR 3c with two candidate fixes; this decision picks between them.

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
- Heterogeneous lifetimes for string fat pointers. Mostly invisible at the IR-translator level (every runtime op accepts both), but a future pass that *frees* data based on inspecting the fat pointer's lifetime would need to distinguish.
- String literals waste no heap space; that's a feature, but it also means `phx_gc_shutdown`'s leak-detection won't report literals — which is correct but worth noting.

**Alternatives considered:**

- **Option (c), runtime alloc + memcpy.** For each `Op::ConstString` site, call `phx_string_alloc(len)` then memcpy bytes from a source region into the heap. Strings live on the GC heap with shadow-stack rooting — a single ownership model. Rejected: still needs the source bytes in low-offset data segments (so the stack-collision concern doesn't go away), pays an alloc + memcpy per use, and the uniformity benefit is moot because runtime ops already treat fat pointers uniformly via borrowed slices.
- **Option (b), bump `__heap_base` at merge time.** Impossible — the runtime exports no `__heap_base` global. Rustc bakes it as a constant.
- **Option (d), module-init via `_initialize`.** Allocate every string at module load, store fat pointers in WASM globals consulted at each use site. Rejected: globals would hold GC-managed pointers but the runtime has no `phx_gc_register_global_root` API for permanent roots, so the strings would be reclaimed at the first collection. Adding such an API is a runtime change outside PR 3c's scope.

#### I. wasm32-gc runtime architecture: codegen-emitted helpers, no Rust runtime crate

**Decided:** 2026-06-04

**Context.** PR 5 (the WASM GC backend) needed to settle how the existing `phoenix-runtime` crate — built around dlmalloc + a side-registry tracing GC + a flat `phx_*` symbol surface — interacts with the new target. Three candidate architectures were considered: (1) codegen-only, no runtime crate; (2) hybrid, runtime for WASI stubs only; (3) recompile the full runtime for wasm32-gc.

**Structural blocker for "recompile the runtime".** Rust's wasm32 toolchains (`wasm32-unknown-unknown`, `wasm32-wasip1`, etc.) compile to **linear memory only.** There is no Rust target that emits `struct.new` / `array.new` — Rust's `Box<T>` lowers to `dlmalloc(sizeof(T)) → *mut T`, never to a `(ref (struct …))` managed reference. The literal interpretation of "recompile the runtime" would keep the Rust runtime running in linear memory; user code holds WASM-GC managed references; every runtime call therefore marshals **WASM-GC array → linear-memory ptr+len → runtime computation → linear-memory ptr+len → freshly-allocated WASM-GC object** at the FFI boundary — and the WASM-GC-side allocation on the return path has to be emitted by *codegen*, not by Rust, because Rust can't emit `struct.new`. That's strictly more work than option 1, not less.

**Chosen: Codegen-emitted, no Rust runtime crate for wasm32-gc.** The codegen emits allocation, structure access, and dispatch inline; genuinely complex helpers (hash tables, number formatting, string transforms) ship as WASM functions synthesized from the codegen crate itself, not as a separate Rust runtime artifact. There is no `phoenix-runtime` recompile, no embed-and-merge step, and no shadow-stack emission — the host VM's GC handles tracing (already pinned by §2.3 decision A: *"Phase 2.4 (WASM GC) replaces native root-finding entirely with WASM GC's typed references."*).

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

#### J. wasm32-gc MVP scope: hello + fibonacci + one struct

**Decided:** 2026-06-04

**Chosen.** PR 5 ships the minimum representative slice that proves the wasm32-gc pipeline end-to-end. To keep each merge small and reviewable, PR 5 is delivered in slices, each growing the op surface and carrying its own tests: slice 1 (hello — constants, immutable `let`, `print(Int)`), slice 2 (fibonacci — function calls, integer arithmetic, control flow, value-returning functions), and slice 3 (one struct — the first slice that emits any WASM-GC type, carrying the first concrete WASM-struct-type-per-Phoenix-struct decision under K below).

**Out of MVP scope, deferred to PR 6:** closures, lists, maps, enums (including `Option`/`Result`), `dyn Trait` dispatch, builder types. Each carries its own type-mapping decision (closure-as-`(struct (ref func) cap0 …)` vs. via `funcref`; `(array T)` vs. struct-wrapped; etc.) that benefits from being settled one slice at a time inside PR 6 rather than locked in batch up front. The four-backend matrix expansion in PR 6 surfaces the corner cases each mapping has to handle.

**Why not more?** Type-mapping decisions for collections / closures / `dyn` are independent of the core pipeline scaffolding (module builder, type interner, `_start` synthesis, struct ops). Surfacing them one at a time in PR 6 keeps each merge reviewable and lets the matrix gate each addition; locking them in at PR 5 design time would either be premature (we'd choose without the matrix's adversarial pressure) or would slip PR 5 by weeks.

#### K. wasm32-gc codegen layout: parallel `wasm/wasm_gc/` module tree

**Decided:** 2026-06-04

**Chosen.** A new `crates/phoenix-cranelift/src/wasm/wasm_gc/` directory contains the wasm32-gc translator, mirroring the layout of the existing wasm32-linear tree, with the ~60% shared scaffolding (type interner, basic constants, error type) living in the parent `wasm/` namespace where both trees import it directly.

**Why not target-dispatched lowering inside `wasm/translate.rs`?** The two backends' allocation primitives (`phx_gc_alloc` vs. `struct.new` / `array.new`), heap representations (raw byte offsets + TypeTag tracking vs. typed managed refs), and signature shapes (env-pointer ABI through `closure_target_slot` vs. WASM GC's `funcref` / `call_ref`) diverge enough that target-dispatched `if/else` chains would dominate every match arm. Each arm of `Op::StructAlloc`, `Op::CallIndirect`, `Op::DynAlloc`, etc., would split into two essentially-disjoint emission blocks. Parallel trees keep each target's code linear-readable and let the file structure mirror the per-target mental model. The ~60% shared scaffolding (type interner, basic constants, error type) lives in the parent `wasm/` namespace where both trees can import it directly.

**Why not a separate `phoenix-cranelift-wasm-gc` crate?** Crate boundaries are appropriate when the dependency graph diverges. The two WASM backends both depend on `phoenix-ir`, `phoenix-common`, and `wasm-encoder`; they share most of the type-mapping helpers; the only real divergence is the body emission and module-builder layout. A new crate adds Cargo manifest, workspace dependency wiring, and re-export plumbing for negligible isolation benefit.

#### K.1. wasm32-gc struct-type mapping: one nominal WASM struct type per Phoenix struct

**Decided:** 2026-06-05 (sub-decision under K, locked alongside PR 5 slice 3 — the first slice that emits a `(struct …)` type-section entry).

**Context.** Slice 3 (decision J) is the first slice that has to commit to a concrete IR-type → WASM-type mapping. Slices 1–2 emitted structurally linear modules whose validity under `-W gc=y` was trivial (no GC types declared); slice 3's `Op::StructAlloc` / `Op::StructGetField` / `Op::StructSetField` cannot lower without a WASM struct type to reference. The sub-decision picks among the candidate mappings before any code lands so the question doesn't reopen each PR-6 slice that grows the surface (closures, lists, maps, enums each carry their own analogous sub-decision when they land).

**Chosen.** Each Phoenix struct (post-monomorphization name — e.g. `Point`, `Container__i64`) gets one nominal WASM struct type, declared once in the type section before any function signature references it. Fields are declared in Phoenix source declaration order, all marked mutable (Phoenix supports `p.x = 5` and has no syntax to declare a field immutable). Phoenix `IrType::StructRef` lowers to a nullable concrete reference so that an uninitialized slot is well-typed via WASM zero-init.

**Why nominal (one WASM struct per Phoenix struct), not structural sharing.** WASM-GC's type system is nominal — even if two structs declare the same field shape, they are distinct types and a `(ref $A)` is not assignable to a `(ref $B)`. That property maps directly onto Phoenix's nominal structs (`Point { x: Int, y: Int }` and `Pixel { x: Int, y: Int }` are distinct in Phoenix too — assigning a `Point` into a `Pixel`-typed slot is a sema error). A structural-sharing scheme — "one WASM struct per distinct field layout, multiple Phoenix structs alias the same WASM type" — would be smaller (one declaration where two would be), but it'd require runtime tag bytes to distinguish the Phoenix-level types when they meet a `dyn Trait` or a pattern match, re-inventing the tagged-union machinery that the WASM GC target was supposed to elide. The size savings are real but small for MVP fixtures (one struct per fixture).

**Why declare-before-any-function-signature.** Function signatures that take or return struct refs encode the struct's WASM type index, and the WASM type section is a flat, position-indexed list — so the struct's declaration must precede any function signature that references it in section order.

**Alternatives considered:**

- **Shared "boxed" struct (`(struct (field anyref … anyref))`)**. One WASM struct type with N anyref slots; every Phoenix field stored as boxed-anyref. Rejected: `i64`/`f64`/`bool` fields would need boxing/unboxing on every access — exactly the overhead WASM-GC's typed fields exist to eliminate. The "no tagged-union machinery" advantage above evaporates.
- **i31ref tagging (one WASM type, distinguish Phoenix types via i31ref tag).** Rejected: same boxing overhead as the shared-boxed variant, plus an extra runtime tag check.
- **Lazy declaration (declare a struct the first time `Op::StructAlloc(name)` is translated).** Considered. Would let us declare only structs actually instantiated. Rejected: function signatures may reference struct refs in params/returns before any allocation site is translated, so the index has to exist at signature-interning time anyway. Declaring eagerly from `struct_layouts` is simpler and emits at most a handful of dead types.

#### K.2. wasm32-gc string mapping: three-field struct over a mutable byte array

**Decided:** 2026-06-05 (sub-decision under K, locked alongside PR 6's first slice — strings on wasm32-gc).

**Context.** PR 6 (the WASM-GC matrix expansion, per [decision J](#j-wasm32-gc-mvp-scope-hello--fibonacci--one-struct)) opens with the String slice because strings are pervasive across the fixture corpus (most printing fixtures interpolate or compare strings) and the print-Bool / print-Float / print-String carve-outs in the existing wasm32-gc test surface (`print_bool_is_rejected_until_a_later_slice` and friends) cannot lift without a `IrType::StringRef` → WASM-type mapping in place. As with K.1, the sub-decision is recorded *before* code lands so the representation choice isn't relitigated each time a follow-up slice grows the string op surface (concat is slice 1; substring, trim, case mapping, format, etc. each land in their own slice — and each reads bytes from whatever shape this decision locks).

**The "what about substring / string builder" pivot.** An earlier draft of this decision recommended a bare `(ref (array (mut i8)))` representation on the grounds that WASM-GC arrays already track their length intrinsically, so wrapping them in a struct just to "have a String type" duplicates information. That framing was structurally incomplete: it treated zero-copy substring and zero-copy `StringBuilder.finalize()` as hypothetical future requirements when they are in fact both on the Phoenix roadmap (substring is a core string operation in every language Phoenix benchmarks against, and a string builder is the Phase 2.7 [`ListBuilder` / `MapBuilder` analog](#phase-27-benchmarking) that Phoenix will need once string-heavy workloads come into perf scope). Once those requirements are first-class, the bare-array shape forces both operations to be O(n) (substring copies bytes, builder finalize copies the assembled payload into a right-sized array). The right shape supports both as O(1) view-style operations — and that shape has to be chosen *now*, because changing the WASM type of `IrType::StringRef` later would require rewriting every concat / equality / format / interpolation / print site that emits string ops, and the language ecosystem (any downstream WASM-GC consumer that links against a Phoenix module's exports) would face an ABI break.

**Chosen — the three-field shape, labelled (b') below** (a prime extension of the two-field alternative (b), adding an explicit `$offset`; the alternatives list further down references this label). A Phoenix string lowers to a nominal WASM-GC struct of three fields: a reference to the underlying mutable byte array, a byte `$offset` into that array, and a byte `$len`. The array's own length is the *capacity* and may exceed `$len`. Carrying `$offset` explicitly is what makes substring an O(1) view rather than an O(n) copy; `$len` makes length a single field read. The byte array is mutable (Phoenix-level immutability is a sema invariant) so the eventual `StringBuilder` can grow it in place and hand it to the finalized string without a separate array type or a copy.

**Alternatives considered:**

- **(a) Bare `(ref null (array (mut i8)))`**. Simplest type declaration (1 entry), `length` is `array.len`. Rejected for the substring / builder asymmetry above — both operations would be O(n) under this shape, and migrating later would re-cost every string-touching op site.
- **(b) Two-field struct `(struct (ref $bytes) (field $len i32))`**. Length-separation enables zero-copy StringBuilder finalize but NOT zero-copy substring (substring needs `$offset` as well). Rejected: the increment from (b) to (b') is one extra field for a substantial future benefit; if we're paying for any struct wrapping at all, paying for the three-field shape is dominantly cheaper than the two-then-three migration cost.
- **(c) Single-field wrapper `(struct (ref $bytes))`**. Nominal wrapper as a forward-compatibility stepping stone to (b) or (b'). Rejected: the migration to (b') is a localized codegen change in any starting shape (one helper rewrite + per-op-site updates), so the wrapper's "easier migration" benefit is small. Meanwhile (c) pays an extra `struct.get $data` on every byte access today with no current upside.
- **`String` as `(ref null (array i8))` with `$bytes` immutable.** Rejected: forces StringBuilder to use a different array type internally (a mutable `(ref (array (mut i8)))`) with no WASM-GC casting path that would let the builder's array flow into the finalized String's `$data` slot. Builder finalize would have to allocate a fresh immutable array and copy — defeating the purpose.
- **`i31ref`-encoded short strings + heap fallback for long ones.** Would let strings ≤ 4 bytes live in 32-bit values without allocation. Rejected for slice 1: real complexity (every read site has to test the i31 tag), unclear payoff under realistic workloads, and the dispatch tax is paid on *every* string operation forever. Reopens as a Phase 4-ish perf optimization if profiling identifies short-string allocation as a bottleneck.

Note: Phoenix's `String.length()` returns the **char count** (code-point count), not the byte count, so it cannot be the byte-`$len` field read — returning byte length would silently diverge from every other backend's semantics on any non-ASCII string.

**Deferred to follow-up slices:** substring (carries the substring decision K.3, locked when the slice lands), `String.trim()` / case mapping, interpolation (already lowers to `Op::StringConcat`-chains today), `print(Bool)` / `print(Float)`, and the lexicographic comparison operators.

#### K.3. wasm32-gc substring lowering: O(char_count) view via a `phx_str_substring` helper

**Decided:** 2026-06-05 (sub-decision under K.2, locked alongside PR 6 slice 2 — substring + lex compare + print(Bool) on wasm32-gc).

**Context.** K.2 sold the three-field `$string` shape on the premise that substring is "O(1) struct.new — zero bytes copied." That framing was structurally incomplete: Phoenix's existing `substring` semantics, established in [the substring-clamps decision](#substring-clamps-out-of-range-indices-silently) and implemented in `phoenix-runtime/src/string_methods.rs::phx_str_substring`, are **char-indexed, not byte-indexed**. `"héllo".substring(1, 3)` returns `"él"` (two code points), whose bytes happen to form a contiguous slice of the parent (`é` is 2 bytes, `l` is 1, total 3 bytes starting at byte offset 1) — but *finding* those byte boundaries requires walking the parent's UTF-8 bytes counting code-point boundaries until `start` chars are consumed, then `(end - start)` more. The walk is unavoidable; the language semantic dictates it.
The "O(1) substring" promise from K.2 therefore softens to:

- **For pure-ASCII strings** (which both byte-indexed and char-indexed semantics treat identically), substring on wasm32-gc IS O(1) on the byte-walk side — `start_byte = start_char`, `end_byte = end_char`, no walk needed in principle. We still walk in the helper to keep one code path; a fast-path could be added later if profiling identifies it as a bottleneck.
- **For UTF-8 strings with multi-byte chars**, substring is O(char_count) for the walk plus O(1) for the `struct.new`. The byte-array is still shared with the parent — zero bytes are copied — so the substring's runtime cost is *bounded by char count*, not byte count, and dominated only by the walk itself.

The byte-array sharing is what survives from K.2. The "O(1) substring" claim was the part that needed correction. `StringBuilder.finalize()`'s O(1) promise from K.2 is unaffected — the builder produces ASCII / known-boundary output and hands its byte array directly to the finalized `$string` wrapper without a walk.

**Chosen.** A single synthesized `phx_str_substring` helper walks the parent's UTF-8 bytes counting code-point starts, clamps the bounds during the walk, and returns a struct sharing the parent's byte array (a view, zero bytes copied).

**Why not byte-indexed substring.** Considered briefly: byte-indexed substring is O(1) in both walk and struct.new (you can directly index into `$data`). Rejected because it diverges from the existing language semantics that every other backend (native, wasm32-linear, interpreter) already implements char-indexed, and changing the semantic would (a) break user programs that depend on it and (b) require updating the wasm32-linear backend's runtime call too. A wasm32-gc-only divergence is the worst of both worlds: silent divergence between backends. If Phoenix later decides byte-indexed substring is the right language choice, that's its own design-decision pivot.

**Lex compare** (`Op::StringLt` / `StringLe` / `StringGt` / `StringGe`) is bundled into the same slice (the design space is narrow): a single byte-compare helper returns negative / zero / positive, and each of the four ops compares its result against zero. **print(Bool)** lowers inline via two pre-staged data segments (`"true\n"` / `"false\n"`) rather than a helper.

**Deferred to follow-up slices:** `print(Float)` (its own slice with the f64-formatter design — Ryu vs. lossy fixed-precision vs. host-delegated), `String.trim()` / case mapping / `replace` (each carries its own follow-up — `trim` walks both ends for whitespace and produces a view; case mapping must allocate; `replace` is its own algorithm).

#### K.4. wasm32-gc enum mapping: subtype hierarchy (parent + per-variant subtypes)

**Decided:** 2026-06-05 (sub-decision under K, locked alongside PR 6 slice 3 — enums on wasm32-gc).

**Context.** Phoenix enums are pervasive — `Option<T>` and `Result<T, E>` appear in nearly every non-trivial fixture (collection methods return `Option`, parse returns `Result`, error propagation runs through `Result`), plus user-defined enums show up in pattern-matched AST shapes, state machines, and bench fixtures. The representation choice ripples through every enum-touching site (alloc, discriminant read, field read, recursive references) and is hard to revisit later because changing it would re-cost the entire matrix.

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

**Op lowering.** The discriminant read goes through the parent type with no `ref.cast` (every concrete variant IS-A parent), so the discriminant test that drives every match dispatch costs nothing — the key property the subtype hierarchy buys. A field read needs one `ref.cast` to the variant subtype. Heterogeneous variants are handled naturally: each variant struct carries its payload fields at their natural WASM types with no boxing, and recursive enums work via the forward reference from a variant to its already-declared parent type.

**Generic monomorphization at codegen time.** Phoenix's IR does *not* monomorphize enum layouts — it stores templates with `__generic` placeholder fields, and concrete type arguments live only on the use site. For wasm32-gc's statically-declared WASM types, the type-decl pass runs a codegen-time monomorphization step that collects every distinct concrete instantiation and declares a parent + per-variant subtypes for each. The same enum template with different type args yields *different* WASM enum types — `Option<Int>` and `Option<String>` are separate declarations, no shared types. (The K.4 known limitation — generic enums whose type params repeat across variants mis-substitute — is now owned by K.12, which makes it a backend-agnostic verifier error rather than a wasm32-gc-only failure.)

**Deferred to follow-up slices:** list/map/closure/`dyn` as variant field types (each needs its own type-mapping decision), pattern matching with struct destructuring (orthogonal to enum representation). `Option`/`Result` builtin methods share this same enum lowering and landed across the K.8 closure slice and the 2026-06-15 combinator completion.

#### K.5. wasm32-gc Float scalar ops: arithmetic + comparison only, `print(Float)` deferred

**Decided:** 2026-06-05 (sub-decision under K, locked alongside PR 6 slice 4 — Float scalar ops on wasm32-gc).

**Context.** Float values have been carved out of wasm32-gc since slice 1 (the `print_float_is_rejected_until_a_later_slice` test pins this). The wider Float surface — constants, arithmetic, comparison, printing — has two mostly-orthogonal halves: the *scalar ops* (mechanical, ~30 lines of new code) and the *formatter* (Ryu f64-to-shortest-decimal, ~300 lines of intricate bytecode plus correctness risk). This slice ships the easy half; the formatter gets its own slice with its own design decision.

**Chosen.** Slice 4 ships the scalar Float surface: constants, the four arithmetic ops, negation, and all six comparisons all map to direct `f64` opcodes (and WASM's `f64.<cmp>` already returns the i32 0/1 Phoenix uses for `Bool`). `Op::FMod` (Float `%`) is **not** in this slice: WASM has no `f64.rem`, so `%` needs an `fmod` helper (sign-of-dividend remainder) and lands with the rest of the Float runtime surface — until then the backend rejects it with a specific diagnostic rather than the generic catch-all.

`print(Float)` keeps its carve-out from the `print_float_is_rejected_until_a_later_slice` test — the formatter slice (TBD K-number) picks among Ryu / integer-fast-path + lossy fallback / host-delegated approaches. Decoupling lets Float-arithmetic-only programs run on wasm32-gc immediately; Float-printing programs stay carved out cleanly.

**Why not include the formatter now.** The Phoenix runtime's `format_f64` calls Rust's `f64::to_string()` which delegates to Ryu/Grisu3 internally — porting that to ~300 lines of WASM bytecode is the bulk of the print-Float surface and carries real correctness risk (precomputed power-of-10 tables, shortest-decimal reduction, NaN/Infinity edge cases). A lossy fixed-precision alternative would diverge from the native backend on any high-precision value, breaking matrix consistency on every fixture that prints a non-integer Float. The right move is to land the formatter as its own slice with its own design decision and adversarial verification against the native fixture corpus.

**Matrix impact.** Any Float-arithmetic-only fixture (no `print(Float)`, no Float interpolation into `print(String)`) runs on wasm32-gc after this slice. Any Float-printing fixture stays carved out until the print-Float slice lands.

**Deferred to the print-Float slice:** the formatter design decision (Ryu vs integer-fast-path + lossy vs host-delegated), the matching `phx_print_f64` helper synthesis.

#### K.6. wasm32-gc Float-print formatter: synthesized inline Ryu, no runtime embed; both backends emit Ryu's scientific format

**Decided:** 2026-06-05 (original module-size decision).
**Amended:** 2026-06-09 (output-format pivot — see "Amendment: output format" below).

**Primary reason: module size.** This decision was specifically chosen over the "embed-and-merge the runtime" alternative on module-size grounds. Embedding `phoenix-runtime` brings ~50KB of compiled WASM into every wasm32-gc module — even a "hello world" that never prints a Float — because the merge is whole-module: dlmalloc, the Phoenix mark-sweep GC, every linear-memory string/list/map/builder runtime helper, panic infrastructure. A synthesized inline `phx_print_f64` adds ~9.6KB of precomputed power-of-5 tables plus the formatter bytecode — *only when the module actually calls* `print(Float)`; a Float-free module carries neither tables nor helper. Module-size matters in the WASM-GC use cases Phoenix targets (browser delivery, embedded VMs, edge runtimes) — every kilobyte adds startup latency and bandwidth.

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

The fixture/test impact is bounded to expectations that print *integer-valued* Floats — the only finite class whose output changes inside the fixed-point range. Tests asserting non-integer values are unchanged — ryu and Rust std agree there.

**Known asymmetry: print output is not re-parseable as a Phoenix literal.** The lexer has no exponent syntax — `1e100` is not a valid Float literal (the wasm trap tests write it longhand) — so scientific-notation output from `print` cannot be pasted back into Phoenix source. Pre-amendment fixed-point output didn't have this asymmetry. Not a blocker for this slice, but it creates a standing motivation for exponent literals in a future lexer slice; if that lands, it should follow ryu's no-`+` form (`1e100`, `5e-324`) so literals round-trip with output.

The precomputed power-of-5 tables are **computed from their mathematical definitions, not copied from the `ryu` crate**, so the repo stays MIT-only (the constants are mathematical facts; bit-equality with ryu is enforced by an adversarial test sweep rather than by provenance).

**Alternatives considered:**

- **Embed-and-merge the `phoenix-runtime` `phx_print_f64`.** Rejected per the primary-reason analysis above: module-size hit too large; ~95% dead code on wasm32-gc.
- **Narrow extractor pulling just `phx_print_f64` + transitive deps.** Rejected: the transitive-dep walk through Rust std's float formatter pulls in formatting machinery, integer-to-string helpers, and panic landing pads; the extractor itself is delicate code (~300 LOC) for a one-function payload.
- **Compile-time const-fold only.** Rejected as a *sole* approach: leaves runtime-computed Floats unprintable. Could be added as an optimization atop the synthesized helper in a follow-up — but not in place of it.
- **Lossy fixed-precision fallback.** Rejected: byte-for-byte divergence from native on any non-integer Float breaks matrix consistency on every fixture that prints one.
- **Mirror Rust std's fixed-point format on both sides (Option A from the 2026-06-09 review).** Rejected per the Amendment: requires ~340-byte buffer and a custom fixed-point emission stage on wasm32-gc; propagates Rust std's widely-criticized fixed-point default into a target where every other Phoenix backend would have to match.
- **Defer print(Float) entirely (Option C from the 2026-06-09 review).** Rejected: a target that traps on `print(3.14)` is shippably broken; deferring leaves slice 5 half-complete with no exit story.

#### K.7. wasm32-gc `List<T>` / `ListBuilder<T>`: length-carrying wrapper struct over a shared mutable array; zero-copy freeze

**Decided 2026-06-10 (PR 6 slice 7 design lock).**

**Chosen shape.** One pair of WASM-GC types per distinct concrete element type `T` (codegen-time monomorphization, mirroring K.4): a length-carrying wrapper struct (an i64 `$len` plus a backing array) for the list, and a separate builder struct (length, frozen flag, growable array) when the module uses `ListBuilder<T>`. The backing array is mutable — Phoenix-level list immutability is a sema invariant, not a WASM one (K.2's `$bytes` precedent) — and its own length is the *capacity*, which may exceed `$len`.

**Why a wrapper struct (not a bare array).** A bare `(array T)` would be one allocation and the simplest lowering, but it forces `length == array.len` — which forces `ListBuilder.freeze()` to copy into an exact-size array (O(n)) and leaves no room for `$len < capacity`. The wrapper costs a second allocation and one `struct.get` per access, and buys:

- **Zero-copy `freeze()`** — `freeze` sets `$frozen = 1` and returns `struct.new $list_T($len, $data)` sharing the builder's buffer: **O(1)**, vs. native's O(n) memcpy (decision F). Behavior is identical — the frozen flag (runtime-checked, as on native) blocks all further builder mutation, so the shared buffer is never written again; only the cost model improves. The trade: up to 2× growth slack stays live until the frozen list is collected.
- Consistency with K.2's `String` shape (wrapper struct + backing array), and room for future O(1) `take`/`drop` views if a later slice wants them (today both copy, matching native's clamped-copy semantics).

**Semantics mapping (native parity).** All list operations match native semantics: out-of-bounds `List.get` traps, `push` copies (immutability), and `ListBuilder.push`/`freeze` on a frozen builder traps. Two cross-backend semantics were resolved in this slice: **negative `take`/`drop` `n` is a runtime error** on every backend (a divergence resolved 2026-06-10 — the native runtime had silently clamped to 0; the loud-failure semantic won, matching `List.get`'s OOB philosophy, and the native clamp was removed), and **`List.contains` uses identity (pointer) equality for struct/enum element types**, matching native's stored-pointer comparison (scalars and strings compare by value/bytes). For-in iteration needs no new machinery — the frontend lowers it to `List.length` + `List.get`.

**Alternatives rejected:**

- **Bare `(array T)` as the list.** One allocation and `array.len` as the length, but forces O(n) freeze and admits no metadata. Rejected with user 2026-06-10.
- **Wrapper + explicit capacity field** (the original PR 5 sketch `(struct len, cap, data)`). Capacity is the data array's own length — a stored copy is dead weight on every immutable list. Rejected.
- **Copying `freeze()`** (exact-size array, native cost model). No behavioral difference from zero-copy, strictly worse constant factor; the slack-retention trade was judged acceptable. Rejected with user 2026-06-10.

#### K.8. wasm32-gc closures: per-signature subtype hierarchy over typed function references (`call_ref`)

**Decided 2026-06-12 (closure design lock).**

**Context.** Phoenix's IR closure ABI is the env-pointer calling convention (locked during the Phase 2.4 closure-capture-ambiguity fix): a closure value's heap object *is* the environment; `Op::CallIndirect(closure, args)` passes the closure verbatim as the callee's first argument; the callee reads captures via `Op::ClosureLoadCapture(env, idx)` against its `capture_types`. Call sites never know capture layouts — that is what lets two closures with the same user signature but different captures unify through a phi. The wasm32-gc mapping must preserve exactly that property. There is no capture-store op: captures are by-value and immutable once allocated.

**Chosen shapes.** One *function type* plus one open *parent struct* per distinct closure **signature** `(param_types, return_type)` (codegen-time collection, mirroring K.4/K.7), and one final *site subtype* per `ClosureAlloc` target function carrying that closure's immutable capture fields. `IrType::ClosureRef` lowers to the nullable parent-struct ref; a call reads the function reference out of the closure struct and dispatches via `call_ref`, passing the closure itself as the env argument.

**Env parameter is abstract `(ref null struct)`, not `(ref null $clo_SIG)`.** The precise typing would make `$fn_SIG` and `$clo_SIG` mutually recursive, requiring `(rec …)` group emission in the type interner. The abstract typing breaks the cycle with no interner changes, and loses nothing real: the callee must `ref.cast` the env down to its concrete `$site_F` either way (parent → site), and every *user-controlled* param/result stays precisely typed, so `call_ref` still statically checks everything a caller can get wrong. Revisit-trigger: if the interner grows rec-group support for another reason (struct↔enum field cycles are the likely customer), tightening the env type is a mechanical follow-up.

**Dispatch mechanism: `call_ref`, not a funcref table.** Verified empirically (2026-06-12) on the pinned wasmtime v45: `call_ref` validates and runs with `-W function-references=y,gc=y` and is rejected under `-W gc=y` alone — wasmtime gates the two proposals separately even though GC formally builds on function-references. The slice therefore adds `function-references=y` to every wasmtime invocation that runs wasm32-gc modules (the backend-matrix harness, the `compile_wasm_gc.rs` harness; CI inherits both). The rejected alternative — module-wide funcref table + `call_indirect` — works under today's flags but re-implements wasm32-linear's machinery (table, element segments, index bookkeeping, per-call runtime signature checks) inside the backend whose type system exists to make that unnecessary.

**Alternatives rejected** (2026-06-12, with user): funcref table + `call_indirect` (above); uniform boxed env `(struct funcref (ref array anyref))` — every scalar capture boxes on alloc and unbox-casts on read, exactly the overhead typed fields eliminate; precise env typing via rec groups (deferred, not rejected outright — see revisit-trigger).

**Scope.** The K.8 slice ships the core (closure alloc / call / capture-load + the `ClosureRef` type mapping). Follow-up slices added the Option/Result and List closure-taking method builtins, all lowered in terms of the K.4 enum and K.7 list representations. (`sortBy` was later upgraded from a stable insertion sort to an O(n log n) merge sort once the bench corpus exercised 100k-element sorts; output stays byte-identical under the shared `cmp <= 0` stability rule.) The slice also added **partial-generic enum resolution**: a `Result`-returning function whose join block leaves one type slot unconstrained produces a `Result<__generic, E>` with no declared nominal WASM identity, which is resolved to the unique concrete sibling the program declares with matching non-placeholder slots — closing the K.4 known limitation for the stdlib cases.

#### K.9. wasm32-gc `Map<K,V>`: ordered association over parallel arrays, not a hash table

**Decided 2026-06-12 (map design lock).** Map literals dedup duplicate keys **last-wins, first-position-kept**, and **float keys compare byte-wise** (`NaN == NaN` for identical bits, `-0.0 ≠ +0.0`) — both unified across all five backends (the bug closures live in [phase-2.md §Bugs closed in this phase](phases/phase-2.md#bugs-closed-in-this-phase)).

**Context.** Native's `Map<K,V>` (`phoenix-runtime/src/map_methods.rs`) is an FNV-1a open-addressing hash table with linear probing, tombstones, 70%-load rehashing, *plus* a parallel `u32` insertion-order array so `keys()`/`values()` iterate in first-insertion order (the contract Phoenix shares with Python/JS dicts). The crucial observation: **nothing about the hash table is observable.** The only observable surface is (a) key-equality lookup (`get` / `contains` / `set` / `remove`), (b) `length`, and (c) **insertion-order** `keys()`/`values()`. The hash table is purely a lookup-speed optimization.

**Chosen representation — ordered association via parallel arrays.** One pair of WASM-GC types per distinct concrete `(K, V)` (codegen-time collection, mirroring K.4/K.7). Keys and values are stored in insertion order in parallel arrays — the *same* array types K.7 declares for `List<K>` / `List<V>`, so `keys()` / `values()` are O(1) views and insertion-order preservation is structural (no separate order array, no rehash to keep ordered). A non-observable open-addressing hash *index* (added at the Phase-2.4 close, driven by a churn bench that ran ~380× slower than native) accelerates lookup to O(1) and construction-dedup to O(n) without affecting any output: it is *not* matched to native's hash function (equality, not hash, is the contract), and the arrays stay insertion-ordered, so all observable results remain byte-identical across all five backends — only speed changes.

**Operation semantics.** All map operations are observably identical to native: map literals dedup last-wins / first-position-kept, `get` returns `Option<V>`, `set`/`remove` are copy-on-write (immutable API), and `keys()`/`values()` wrap the existing arrays as O(1) list views. Key equality dispatches on the key's type, matching native: by value for Int/Bool, **byte-wise** for Float (per the *"`Map<Float,V>` uses byte-wise key comparison"* decision — `NaN == NaN` for identical bits, `-0.0 ≠ +0.0`, deliberately not IEEE `f64.eq`), and content equality for String. Ref-typed keys (struct/enum) error with a per-slice diagnostic until a fixture needs them (the identity-vs-structural choice is deferred with them).

**Why not the faithful hash table.** Porting FNV-1a + open addressing + linear probing + tombstones + the order array + rehashing into hand-written bytecode is several hundred intricate, bug-prone instructions for **zero observable benefit** at fixture scale — and the O(1) advantage is invisible to the matrix (`map_hash_many_keys.phx`'s 100 inserts are trivial either way). The ordered-association form is observably byte-identical, reuses K.7 wholesale, and makes insertion-order preservation structural rather than a rehash-invariant to maintain.

**Scope.** Core `Map`: literal, `get` / `contains` / `length` / `set` / `remove` / `keys` / `values`, key types Int/Float/Bool/String. **Deferred:** `MapBuilder` (no matrix fixture; bench-corpus-only) and ref-typed keys, each to the slice that needs it.

#### K.10. wasm32-gc `dyn Trait`: per-trait typed-funcref vtable struct, trampolines, `call_ref`

**Decided 2026-06-14 (dyn design lock).**

**Context.** The IR pre-resolves each `dyn` method call to a slot index (`Op::DynCall(trait, slot, recv, args)`) and registers a per-`(concrete, trait)` vtable in `IrModule::dyn_vtables` as `Vec<(method_name, FuncId)>` in trait-declaration order (slot = index). Native emits a rodata function-pointer table; wasm32-linear a data-section i32-index table dispatched via `call_indirect`. Both reach the concrete methods — but a `dyn` call site only knows the *abstract* receiver, while the concrete methods are typed `self: Circle`. So a uniform-signature bridge is unavoidable on any backend. K.8 already established typed `call_ref` + the `function-references` feature + the abstract-receiver-cast pattern, which this reuses wholesale.

**Chosen shapes.** Per trait `T`, one func type per method slot (its `self` parameter the **abstract** `(ref null struct)`, exactly K.8's env typing, so the dispatched function accepts any concrete receiver), a per-trait vtable struct of non-null typed funcrefs, and a two-field `$dyn_T` struct (data ref + vtable ref). `IrType::DynRef(T)` lowers to the nullable `$dyn_T` ref — a single slot, so it flows uniformly into params / returns / locals / struct fields / list elements with no special-casing.

**Trampolines bridge abstract→concrete.** A `dyn` value's data is `(ref null struct)`, but `Circle.draw` expects `(ref $Circle)`. So per `(trait T, concrete C, slot i)` the backend synthesizes a trampoline `tramp_T_C_i(self: (ref null struct), args…) -> Rᵢ { ref.cast self to (ref $C); <push args>; call C.mᵢ }` — its `ref.func` fills the vtable. (Identical in spirit to a closure's `$site` cast in `ClosureLoadCapture`.) Concrete `C` resolves to its K.1 struct index (`ref.cast` target); a non-struct concrete (an enum `impl`) errors until a fixture needs it.

**One shared vtable instance per `(trait, concrete)`, via a global.** The vtable for a `(T, C)` pair is identical for every value, so it's a WASM global `(ref null $vtable_T)` built once — `(global $vt_T_C (ref null $vtable_T) (struct.new $vtable_T (ref.func tramp_T_C_0) …))` — and reused: a `List<dyn Shape>` of 100 elements allocates the vtable once, not 100×. (`struct.new` in a global init expression validates under `-W function-references=y,gc=y` on the pinned wasmtime — verified 2026-06-14.) The trampoline `ref.func`s join the K.8 `(elem declare func …)` segment.

**Type declaration: one rec group + `$dyn_T` index reservation.** `dyn` is the first wasm32-gc feature whose types genuinely *cycle* across the declaration order: a `dyn` method returning a `List` needs lists declared *before* the dyn func types, while a `List<dyn T>` element needs `$dyn_T` declared *before* lists — no single linear order satisfies both across the independently-compiled fixtures. This is the "rec group customer" K.8 anticipated. It is resolved by emitting all wasm32-gc types as one explicit `(rec …)` group (which legalizes every member's forward references) and reserving `$dyn_T` indices early so the referring types resolve a real index before the bodies are defined.

**Why not a funcref table + `call_indirect`** (the wasm32-linear shape). It still needs the same trampolines (`call_indirect` also requires a uniform signature), and re-introduces a function table + element segments + per-slot interned types + index bookkeeping that `call_ref` makes unnecessary. The typed-funcref vtable is GC-native, statically checked, and consistent with K.8 (same feature flag, same cast pattern). Rejected with user 2026-06-14.

**Scope (as shipped).** The dyn core (the dyn/vtable types, trampolines, vtable globals, `DynAlloc` / `DynCall` / `DynRef`) with `DynRef` wired into function params / returns / locals and list elements. `dyn` as a *struct field* was resolved separately by K.11 (which generalized reference-typed struct fields); the `dyn` ABI needed no further work, since `DynRef` already slots into a field like any other single-slot ref.

---

#### K.11. wasm32-gc reference-typed struct fields: reserve struct indices early, define bodies late

**Decided & implemented 2026-06-15 (full generalization over a dyn-field-only).**

**Context.** K.1 declared each Phoenix struct as a nominal WASM-GC `(struct …)` but supported only scalar (`Int`/`Float`/`Bool`) and, later, `String` fields; every reference-typed field (nested `StructRef`, `EnumRef`, `ListRef`, `MapRef`, `ClosureRef`, `DynRef`) errored with a per-field diagnostic. The blocker was never the field encoding — `Op::StructAlloc` / `StructGetField` / `StructSetField` lower as plain `struct.new` / `struct.get` / `struct.set` by field index and are already valtype-agnostic, and `struct.get` binds its result through the standard `single_slot` mapping — but the field *type declaration*: a struct field referencing another struct (or a list / map / enum / closure / dyn type) needs that type's section index at field-build time, and the declaration passes ran structs *before* enums / lists / maps / closures / dyn, so the index didn't exist yet. Cross-struct references are moreover cyclic (a struct field holding a list whose element is a struct).

**Chosen.** Struct fields map through the *same* single-slot mapping as every other position — a reference field is a nullable concrete ref, mutable (K.1's all-fields-mutable rule). No field-specific type logic survives. The declaration-order cycle is dissolved with the K.10 rec-group machinery: struct indices are reserved early (so every later type that references a struct resolves a real index) and struct bodies are defined late (after enums / lists / maps / closures / dyn all exist), with any field referencing a later-indexed type left as a forward reference legal inside the single rec group.

**Scope.** All six reference-field kinds land together (struct / enum / list / map / closure / dyn), unblocking `dyn`-in-a-struct-field plus nested-struct / list / map / enum / closure fields.

---

#### K.12. Phantom-parameter enum inference: expected-type pinning + a partial-generic IR verifier invariant

**Decided & implemented 2026-06-15 (root-cause fix in sema over backend-side or monomorphization repair; cross-backend output determinism declared non-negotiable).**

**Problem.** A constructor with a *phantom* type parameter — one not determined by its arguments — leaves that parameter unbound: `Ok(99)` fixes `Result`'s `T` but not `E`; `None` fixes neither of `Option`'s. In a *concrete* (non-generic) function, sema's bottom-up checker recorded e.g. `Result<Int, free-var>`, which monomorphization erases to `Result<Int, __generic>`. The native and tree-walk/IR interpreters tolerate it (their enums are structural), but wasm32-gc enums are *nominal* (K.4): when two instantiations of a template coexist (`Result<Int,String>` and `Result<Int,Int>`), a `Result<Int, __generic>` reference can't be pinned to a unique nominal type, and the build fails. So the *same program* produced output on some backends and a compile error on others — a determinism break the five-backend byte-identical contract rules out.

**Why sema, not the backend or a mono pass.** The defect is that `Ok(99)`-in-a-`Result<Int,String>`-context is *typed wrong* — the context is right there and a complete checker uses it. Fixing it at the wasm32-gc backend (or in a post-hoc monomorphization repair pass) would let the wrong type into the IR and patch it downstream, leaving sema's `expr_types` — consumed by every backend, the IR interpreter, and future tooling — inaccurate. The root-cause fix is expected-type propagation, which is also reusable type-system infrastructure (it subsumes the existing `None`/empty-literal special-casing and is the foundation a future full bidirectional checker builds on).

**Chosen: scoped expected-type pinning, not full bidirectional checking.** `check_expr` stays bottom-up (synthesizing). At each boundary where an expected type is known — `let` with annotation, explicit `return`, lambda/implicit return, function/method call arguments, struct/enum constructor arguments, collection elements, `if`/`match` branches, and *nested* constructor arguments — `pin_inferred_type_to_annotation` refines the expression's recorded type toward the concrete expected type. It only fills type-var holes (`Result<Int, free-var>` → `Result<Int, String>`); it is a no-op on already-concrete types and never changes diagnostics or what the checker accepts/rejects — so it is far lower-risk than threading an expected type through `check_expr`. It recurses structurally (collection elements *and the container itself*, `if`/`match` arms *and* the branch expression itself, enum- and struct-constructor arguments via the declared type's variant/field types) so deeply-nested phantoms (`Ok(None): Result<Option<String>, String>`) resolve. The container must be pinned alongside its elements because `check_list_literal`/`check_map_literal` record the container as `List<first_element_type>` / `Map<first_key, first_value>` with no cross-element unification — so a phantom-typed *first* element (`[None, Some(1)]: List<Option<Int>>`) leaves the container `List<Option<?>>`, and `lower_list_literal` uses that recorded type as the `ListAlloc` result type the verifier checks. The struct-constructor case is what resolves a *sole-phantom-field* generic struct (`Box(None)` where `Box<T> { v: Option<T> }`, against `Box<Int>`): the param `T` can't be recovered from the argument's own type, so it must come from the outer declared type — exactly what this propagation supplies. A subtlety fixed alongside: `check_block_type` re-ran `infer_expr_type` on a `return` expression *after* `check_return` had pinned it, clobbering the pin — it now reads the already-recorded type instead.

**The IR verifier invariant (the determinism guarantee).** `phoenix_ir::verify` rejects any concrete function whose value types contain `__generic` *as an enum type argument* (`Result<Int, __generic>`, `Option<__generic>`, `List<Option<__generic>>` — recursing through containers). This is the postcondition of complete inference and a hard, backend-agnostic error: a program either type-resolves fully (and runs identically everywhere) or is rejected everywhere — never one backend's output vs another's compile error. It also subsumes the K.4 nested-generic-variant limitation (`enum Wrapper<T> { W(Option<T>) … }`), which now surfaces here rather than deep in wasm32-gc codegen.

**Why scoped to *enum* arguments.** A *bare* `__generic`, or one in a list/map *element* (`List<__generic>`), a struct argument, or a closure parameter/return, comes from **inert** sources — a dead generic-closure copy's erased capture, an unconstrained empty literal no nominal codegen consumes — that run identically on every backend. A blanket "no nested `__generic`" invariant rejected those working programs (verified: it broke `closures_over_generic`, `list_of_options`, et al.). Enum arguments are precisely where phantom-parameter constructors create the nominal-ambiguity divergence, so the invariant targets them and recurses *through* the other containers to catch an enum nested inside.

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

The DOM tier (decision I) ships as two runners over the same fixtures: an always-on jsdom smoke (DOM-host marshalling and the retained-event-handler path at the API level) and a gated Playwright tier in real headless Chromium that catches real-engine behavior jsdom cannot.

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

**Alternatives considered:**
- *Embed the glue as a custom section in the `.wasm`.* Rejected: hosts would need a Phoenix-specific extraction step before instantiation; a plain `.js` file is directly `import`-able / `require`-able.
- *Hand-written per-project glue.* Rejected: drifts from the import section the moment an extern signature changes; the generator is the drift-proof source of truth.

#### D. `JsValue` representation: per-backend, same user-facing type

**Decided:** 2026-06-17

**Rationale:** `JsValue` is the opaque handle for a JS value Phoenix holds but never inspects. Its lowering is necessarily different per backend — linear memory has no notion of a host reference, while WASM-GC has `externref` precisely for this — but the user-facing type is identical. `Type::JsValue` is a pre-registered synthetic primitive, modeled on the Option/Result pre-registration in `resolved.rs::build_from_checker`.

- Linear: an `i32` handle into a JS-owned handle table the glue manages; Phoenix never dereferences it, only passes it back to externs.
- WASM-GC: an `externref` passed directly; the host VM owns and traces it (no handle table).

**Alternatives considered:**
- *A uniform i32-handle model on both backends.* Rejected: throws away WASM-GC's externref tracing (the callback-lifetime win in decision B/G) to make the two backends superficially identical.

#### E. Extern-call ABI: per-backend marshalled signatures

**Decided:** 2026-06-17

Per the [A0 host-FFI model](#a0-parity-model-extern-functions-are-a-uniform-host-ffi-boundary), each backend binds `Op::ExternCall` differently. The marshalled signature per binding:

- **WASM (linear):** each distinct extern becomes one custom WASM function import; scalars cross as their natural WASM types, `String` as a fat pointer, `JsValue` as an `i32` handle, a closure as its single `i32` env pointer.
- **WASM (gc):** one import per extern; scalars as above, `String` copied across a small linear-memory scratch region, `JsValue` as an `externref`, a closure as a managed ref. Strings over the scratch's 4095-byte cap trap ([known-issues.md](known-issues.md#wasm32-gc-extern-js-strings-are-capped-at-4095-bytes)).
- **Interpreters (AST + IR):** no marshalling — extern calls dispatch on `(module, name)` to the registered Rust host closure with host values directly; `JsValue` is an opaque interpreter-side handle the host stub owns.
- **Native (Cranelift):** each distinct extern lowers to a call of a C-ABI symbol `phx_extern_<module>__<name>` with the native value ABI; `JsValue` is an opaque handle owned by the linked host shim. The compiler emits a **weak** default definition that aborts naming the missing binding (the A0 "clear runtime error when unbound"), which a linked host shim's strong definition overrides. (Weak-symbol override is the ELF/Mach-O model; Windows/COFF native interop is out of scope for this phase — see [known-issues.md](known-issues.md#native-extern-js-interop-is-elfmach-o-only-no-windowscoff-weak-override).)

This is an internal compiler↔host contract, not a user-visible ABI — which is what lets the bindings differ without forking the language surface.

#### F. String ownership across the boundary: copied, never shared

**Decided:** 2026-06-17

**Rationale:** Sharing a Phoenix GC string's bytes with the JS engine (or vice versa) would couple two independent garbage collectors' lifetimes across the boundary — a correctness hazard with no upside at 2.5's scale. Strings are copied at the crossing on both backends: out via `TextDecoder` over the staged bytes (the `phx_print_str` scratch-copy pattern), in via `phx_string_alloc` (linear) / GC `$string` allocation (WASM-GC) into a Phoenix-owned string. The GC owns the Phoenix side; the JS engine owns the JS side; neither aliases the other.

#### G. Closures-as-callbacks lifetime: per-backend

**Decided:** 2026-06-17

A Phoenix closure passed to a host crosses through each backend's binding (per [A0](#a0-parity-model-extern-functions-are-a-uniform-host-ffi-boundary)). On WASM the module exports a per-signature trampoline that the glue wraps in a JS callable. Lifetime management differs per binding:

- **WASM (linear):** the glue registers the wrapped callable in a retention table and the Phoenix side roots the closure (manual rooting via a process-global pin set) so the GC can't collect a host-retained callback. Freeing is **explicit** (a drop extern / `FinalizationRegistry` tie-in) — callbacks-only async has no `Promise` to anchor lifetime. The host-never-released path is a linear-only leak filed in [`known-issues.md`](known-issues.md#a-retained-extern-js-callback-is-pinned-for-the-programs-life-on-wasm32-linear) as a *forward* deferral, not a 2.5 blocker.
- **WASM (gc):** the glue holds the closure ref via `externref`, so the host VM GC traces a host-retained callback automatically — **no manual rooting, no explicit-free leak.** Dropping the JS reference lets the host VM reclaim the closure.
- **Interpreters:** the host table receives the Phoenix `Value::Closure` directly and invokes it via the interpreter's normal call path; the interpreter's own GC/ownership keeps it alive for as long as the host table holds it.
- **Native:** the closure crosses to the host shim as its env pointer; retention mirrors the linear contract (a synchronous callback is rooted by the calling frame, and a shim that retains a callback past the call must pin it via the same pin set) — with the same host-never-released leak as linear.

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

### Phase 3.1 Package manager

Subordinate decisions for the Phase 3.1 package manager. Phase-level scope and exit criteria live in [phase-3.md §3.1](phases/phase-3.md#31-package-manager).

#### A. Registry-readiness seams (do as part of 3.1)

**Decided:** 2026-06-27
**Context:** 3.1 is git-first; a central registry is deferred to a later phase (see [phase-3.md §3.1 scope boundaries](phases/phase-3.md#31-package-manager) and [Phase 6.2](phase-6.md)). The resolver, fetcher, and lockfile are nonetheless built so a registry can be added later as an additive source rather than a rewrite. An audit of the 3.1 implementation found the **plumbing** already registry-ready but two concrete assumptions worth neutralizing *now*, while the git code is being written, so the eventual registry doesn't have to break a signature or a serialized format.

**What is already registry-ready (keep it this way):**
- **`ManifestProvider` is the source seam.** The graph walker (`deps/graph.rs`) never mentions git — it fetches through the trait and unifies on an opaque `source_id` string. A registry is "another provider" as far as this layer's interface is concerned.
- **The manifest reserves registry syntax.** A bare-string dep (`dep = "^1.2"`) is parsed and rejected with `RegistryUnsupported`, not silently mishandled (`manifest.rs`). The grammar slot exists.
- **`Dependency` is a closed enum** — a `Registry { name, req }` variant is purely additive.
- **The lockfile is schema-versioned** (`LOCKFILE_VERSION`) so adding registry entries is a clean migration with a loud error on mismatch.

**The seams to fix as part of 3.1 (these are the decision):**
1. **Make source-kind explicit on `ResolvedPackage` / `LockedPackage`; do not infer it from "has a rev."** Today git-vs-other is told apart by `pkg.rev.is_some()` (e.g. `Lockfile::from_graph` in `deps/lock.rs`). A registry package has no git rev but **must** be locked (by version + checksum), so the rev-presence heuristic would silently drop it from the lockfile. Carry an explicit source-kind discriminant instead. `LockedPackage` is currently git-shaped (required `git`, plus `tag` / `branch` / `rev_req` / `rev`); it should become an enum (or gain a source-kind tag) so a registry entry (`name` / `version` / `checksum` / registry id) is representable without overloading the git fields.
2. **Sketch a version-enumeration capability into the provider trait now.** `ManifestProvider::fetch` returns exactly **one** `FetchedPackage` per edge, because a git ref resolves to exactly one commit → one version. This is the deeper mismatch: a registry resolves the *opposite* direction — each dep states a version *requirement range*, and the resolver must choose among many *published* versions a set that satisfies all constraints jointly (in general, with backtracking). The current "fetch one concrete version → unify by caret → pick highest, no backtracking" is a genuinely different algorithm, and the two "known residual" tests in `deps/graph.rs` (shared-transitive-dep conflict; superseded-but-compatible version selection) are symptoms of having no real solver. The full backtracking solver is **out of scope for 3.1** — but the trait should be shaped now (e.g. an `available_versions(name) -> [Version]` capability alongside `fetch`) so the eventual registry + solver does not force a breaking change to the source seam. Document that git/path providers return a one-element version set (the ref *is* the version choice).

**Why fix the seams now and not at registry time:** both are cheap while the git code is in flight and expensive afterward — (1) is a serialized lockfile format, so changing it post-hoc means a format migration on every user's `phoenix.lock`; (2) is the public shape of the provider trait, so changing it post-hoc breaks every provider. Neither requires implementing any registry behavior in 3.1; they only stop the git-shaped code from hardening assumptions a registry would have to undo. The accurate framing recorded here so it isn't forgotten: **adding a registry later is "add a provider *and* add a version solver," not "add a provider"** — git got to skip the solver only because a ref already pins the version.

#### B. Dependency identity is the dependency *key*, not the package's own name

**Decided:** 2026-07-01
A dependency's identity across a project — what an `import`'s first path segment matches, and what the resolver unifies a diamond on — is the **key** on the left of `=` in `[dependencies]`, *not* the `[package].name` the dependency declares for itself. `[package].name` is metadata (diagnostics, eventual publishing).
**Why:** git-first has no registry to enforce globally-unique names, so two unrelated repos can legitimately share a `[package].name`. Keying identity on the *consumer-chosen* key lets them coexist and lets a consumer rename a dependency (Cargo's model: `foo = { package = "real-name" }`). Name-identity was rejected — it would force unrelated same-named packages into a false conflict and dictate the import name to every consumer.
**Trade-off:** two manifests must use the *same* key to share one package in a diamond; different keys for the same repo fetch two copies (correct, if not minimal).

#### C. Version reconciliation: one source per name, caret-compatible, highest wins

**Decided:** 2026-07-01
When one key is required more than once (a diamond), all requirements must share a single upstream **source** — the git URL (without the ref) or the canonical path; differing upstreams are a hard `SourceConflict`. Among same-upstream requirements that pin different refs (and thus possibly different `[package].version`s), the versions must be semver-compatible under caret (`^`) semantics and the **highest** is chosen; incompatible majors are a `VersionConflict`. This is where the `semver` crate does real work.
**Why not exact-match-only:** it would force a hand-resolved conflict for every trivially-compatible minor bump. **Why no solver:** a backtracking version-requirement solver rides the registry (decision A), because a git ref already pins one concrete version. The two documented "known residual" tests in `deps/graph.rs` (a dependency *shared* with a superseded version can still be conflict-checked, or selected higher-than-minimal) are symptoms of having no solver — sound, but not minimal-version selection.

#### D. Lockfile format: name-keyed tables, git-only, requested ref recorded

**Decided:** 2026-07-01
`phoenix.lock` is TOML with one **name-keyed** `[packages.<name>]` table per resolved package — not a Cargo-style `[[package]]` array, because Phoenix's flat namespace admits exactly one version per name, so a keyed table is the honest, directly-indexable shape. Only **git** dependencies are recorded; **path** dependencies resolve in place each build and are never locked. Each git entry records the resolved commit (`rev`) **and** the requested ref (`tag` / `branch` / `rev_req`): recording the requested ref is what lets a manifest ref bump (e.g. `tag = "v1"` → `"v2"`) surface as `--locked` drift while a clean checkout still rebuilds the pinned commit fully offline. `LockedPackage` is an untagged source-kind enum, so the git entry serializes byte-identically to the pre-seam format today and a registry entry stays representable later (decision A).

#### E. Cross-package identity: sema is package-aware; dependency ASTs stay verbatim

**Decided:** 2026-07-01 (identity representation hardened 2026-07-01 post-close — see [phase-3.md Bugs closed](phases/phase-3.md#bugs-closed-in-this-phase-post-close-review)).
A module's identity is `(package, module path)`. A dependency's modules are **package-qualified**, so a dependency's internal module name can never silently collide with the project's, or another dependency's, module of the same name. The package dimension is a **reserved, un-forgeable marker segment** (`<pkg:greet>`, in the same spirit as the `<builtin>` sentinel — angle brackets are illegal in a Phoenix identifier, so no user module path can produce one); it is invisible in the human display form (`greet` / `greet.util`) but preserved in the `module_qualify` symbol-table key, so identities stay distinct without leaking the marker into diagnostics. The dependency's **source is left verbatim** — its own `import helpers` stays bare; the resolver hands sema the resolved package-qualified target for each import (a per-module `import_targets` map), and that identity flows through `module_qualify`, so registration, the interpreter, and IR all inherit it with no per-consumer change. Visibility comes for free: the 2.6 public-only rule is per-*module*, so it already governs the package edge.
**Why the marker, not a bare path prefix:** folding the package in as an ordinary path segment (a dependency `greet`'s root becoming the bare path `greet`) does **not** make identity `(package, module path)` — it collapses back to a flat path that a same-named entry-package module (or a transitive package alias vs. an entry top-level module) collides with, silently. The distinguished marker is what makes the "never collides" guarantee actually hold. Rejected alternatives: (a) the resolver rewriting a dependency's `import` ASTs to be package-qualified — rejected because the parsed AST would then differ from the on-disk source (bad for the LSP/tooling); (b) leaving module paths literal with no qualification — rejected because a dependency's internal module would silently collide with a local one; (c) threading a separate `PackageId` field through every symbol table — same semantics as the marker but far more churn across sema/IR/interp, with no display or key benefit. The original brief forbade touching sema (parallel-track hygiene); that constraint was **deliberately lifted** to enable this. Entry-package modules keep their bare paths, so single-package behavior is unchanged.

#### F. `init` scaffolds a flat project; `add` is atomic

**Decided:** 2026-07-01
`phoenix init` writes `phoenix.toml` (a `[package]`) and a **root** `main.phx` carrying a runnable stub. The entry file's directory is both its module root and the project root — matching Phoenix's "the entry file's directory is the root" model (Go-like: source beside the manifest) rather than imposing a `src/` layout Phoenix does not otherwise enforce. `phoenix add` validates the requested source through the *same* `parse_dependency` a hand-written manifest uses (so the CLI and the manifest accept identical inputs), edits `phoenix.toml` format-preservingly (`toml_edit`, so comments/layout survive), then resolves to refresh the lockfile; on **any** resolution failure the manifest edit is rolled back, so `add` is atomic — the project is never left holding a manifest entry that doesn't resolve.


---

## Phoenix Gen

Phoenix Gen is a parallel product track; its design decisions have their own
record. See **[phoenix-gen-design-decisions.md](phoenix-gen-design-decisions.md)**.
