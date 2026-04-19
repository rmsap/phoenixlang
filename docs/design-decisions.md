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

For a web-focused language, explicit resource cleanup with `defer conn.close()` (Go-style) is often more readable than implicit drop semantics, especially in async contexts.

**Recommendation:** Revisit after Phase 4.3 (Async Runtime).

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
**Rationale:** Phoenix is positioned as a compiled language with native performance, and the stdlib generics are already hand-monomorphized in the Cranelift backend. Any other strategy creates a two-tier world where stdlib is fast and user generics are slow. Monomorphization also stacks cleanly with a future vtable ABI for `dyn Trait`. The compile-time fan-out cost is real but mitigable (shared specialization where layouts match, incremental caching of instantiations) and only bites meaningfully once the stdlib grows in Phase 4.

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

### Centralized layout for reference types

Fat-pointer layouts (e.g., `String` as `(ptr, len)`) and their slot counts are scattered across `phoenix-cranelift/src/types.rs`, `translate/helpers.rs`, and per-method files (`store_fat_value`, `slots_for_type`, etc.). Every new heap-backed type (`Bytes`, `BigInt`, `Date`, future `Vec` / `Buffer`, etc.) requires touching each of them, and the invariants live implicitly in whoever last edited the backend. This is a compiler-internals question, not a user-facing one.

**Decision:** Introduce a single `Layout` trait or registry that owns slot count, alignment, load/store codegen, and calling-convention handling per reference type. Each reference type becomes one entry in the abstraction; the scattered branches in the Cranelift backend become calls into it.
**Decided:** 2026-04-19
**Target phase:** Phase 2.2 (Cranelift native compilation, in flight)
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

### Interpreter-parser coupling via `Value::Closure`

`Value::Closure` in `phoenix-interp` stores `phoenix_parser::ast::Block` directly — the literal parser AST node representing the closure's body. The interpreter walks that AST at call time. After Phase 2.1 introduced the IR and Phase 2.2 built the Cranelift backend on top of it, the AST-storing closure representation became a vestigial coupling: the interpreter still holds raw AST nodes while the rest of the pipeline has moved to IR. Any parser AST change risks silently breaking interpreter closures.

**Decision:** Switch `Value::Closure` to store IR blocks instead of AST blocks. The interpreter walks IR (the same artifact the Cranelift backend consumes) rather than the parser AST. AST becomes a parser-only concern; everything downstream — interpreter, Cranelift, future WASM backend — consumes IR.
**Decided:** 2026-04-19
**Target phase:** Phase 2.6 (module system). Pairs naturally with the diagnostic-builder refactor also landing in 2.6; both are codebase-hygiene work that's cheap once, painful later. The IR stabilizes during Phase 2.2 as monomorphization and dyn-trait codegen land, so starting this refactor in 2.6 means it targets a settled IR rather than tracking a moving one.
**Rationale:** This is implementation hygiene, not a language-semantics decision. Keeping the interpreter coupled to the parser AST makes every future parser change a multi-crate edit; decoupling now pays for itself as soon as the parser evolves (module-system path qualification in 2.6, formatter round-tripping in 3.3, any future syntactic extension). Phase 2.6 is the first slot where the IR is stable and the refactor doesn't fight in-flight backend work.

**Alternatives considered:**
- **Status quo — keep AST-backed closures.** Rejected: the coupling persists indefinitely and every parser change requires manual interpreter rewiring.
- **Late Phase 2.2** — do it while Cranelift work is still landing. Rejected: IR is still evolving during 2.2 with monomorphization and dyn-trait machinery; better to target a stable IR in 2.6.
- **Explicit standalone phase.** Rejected: not large enough to justify a dedicated phase slot; pairs cleanly with other 2.6 work.
