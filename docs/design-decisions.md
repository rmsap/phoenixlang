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
| Drop slot / custom destructor in vtable | Phase 2.3 | Ties to [GC strategy](#gc-strategy). Pure tracing GC obviates the need; `defer` / `using` semantics may change this. |
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
