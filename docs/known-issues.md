# Known Issues

Tracked bugs, limitations, and code-quality items. For unresolved design questions and recorded decision rationales, see [design-decisions.md](design-decisions.md).

---

## Known Limitations

### Memory leaks (no GC yet)

Compiled binaries leak all heap allocations — no garbage collector is implemented. Short-lived CLI programs are fine (OS reclaims memory on exit); long-running processes (servers, daemons) are not. See [GC strategy](design-decisions.md#gc-strategy) for the open design question and planned phase.

---

## Bugs

### Silent zero substitution on out-of-range integer/float literals

**File:** `phoenix-parser/src/expr.rs`

When an integer or float literal is out of range, the parser emits a diagnostic but substitutes `0` (or `0.0`) into the AST.

**Target phase:** None — no fix planned. Acceptable as-is; revisit if it causes real-world confusion, at which point add an `ErrorLiteral` AST variant.

### `Option.okOr()` fails to compile when payload type cannot be inferred

The `okOr` combinator on `Option<T>` values produces a compile error when the Cranelift backend cannot infer the payload type `T`. This happens when the `Option` value comes from a function parameter or cross-function return (where generic type arguments are not propagated through the IR's `EnumRef` type). Previously this silently fell back to `IrType::I64` (1 slot), which corrupted multi-slot types like `String` (pointer + length = 2 slots). The fix surfaces a clear compile error instead of silently miscompiling.

**Workaround:** Use pattern matching instead of `okOr` when the Option comes from a function parameter.
**Root cause:** `IrType::EnumRef("Option")` does not carry generic type arguments.
**Tracked in:** Cranelift `option_methods.rs` `option_payload_type` function.
**Target phase:** Phase 2.2. Likely resolved as a side effect of [generic monomorphization](design-decisions.md#generic-function-monomorphization-strategy) — once `Option<String>` and `Option<Int>` are distinct specialized types, payload inference has concrete types to work with. Verify after monomorphization lands; fix directly if it doesn't absorb the issue.

### Closure capture type ambiguity with indirect calls

When a closure is passed through a block parameter (phi node), the compiler
falls back to a heuristic scan of IR functions to find capture types.  If two
closures share the same user-param types, return type, and capture types, they
are silently conflated.  Different capture layouts are caught (compile error),
but identical-layout mismatches are invisible.

Not actively miscompiling today: when the heuristic conflates closures with
identical capture layouts, the emitted load code works correctly regardless
of which concrete closure the function pointer targets at runtime. The
concern is fragility — future changes could introduce layouts where the
conflation matters.

**Workaround:** Pass closures directly to methods rather than through conditional block parameters.
**Root cause:** The IR's closure representation does not carry capture metadata alongside the function pointer.
**Tracked in:** Cranelift `ir_analysis.rs` `find_closure_capture_types`.
**Target phase:** Phase 2.6. Deferred from 2.2 — the proper fix requires changes to the IR closure representation, which is naturally reworked in the [`Value::Closure` → IR blocks refactor](design-decisions.md#interpreter-parser-coupling-via-valueclosure) scheduled for 2.6. Addressing this bug alongside that refactor is cheaper than doing either in isolation.

### O(n) map key lookup

`Map<K, V>` key lookup, insertion, removal, and contains operations use a
linear scan over a flat array.  Building an n-entry map is O(n²).

**Planned fix:** Hash-based implementation.
**Tracked in:** `phoenix-runtime/src/map_methods.rs` module header.
**Target phase:** Phase 2.3 (Runtime and Memory Management).

### O(n²) `List.sortBy` insertion sort

`List.sortBy` uses O(n²) insertion sort in both backends.  In the interpreter,
the comparator closure requires `&mut self` on the interpreter, preventing use
of `slice::sort_by`.  In the Cranelift compiler, the comparator closure must be
called through block-based control flow, and inline insertion sort maps
naturally to nested loops.  Both backends use the same algorithm for
consistency.  Acceptable for small lists but a performance hazard for large ones.

**Planned fix:** Merge sort implementation.
**Tracked in:** `list_methods_complex.rs` `translate_list_sortby` doc comment.
**Target phase:** Phase 2.3 (Runtime and Memory Management).

---

## Code Quality

### Excessive cloning (~216 sites)

Key offenders:
- `interpreter.rs`: `self.env.snapshot()` deep-clones the entire scope stack for every closure creation
- `check_expr.rs` / `check_types.rs`: many clone calls on type information that could use references (split from the original `checker.rs`)

**Recommendation:** Address before compilation (Phase 2). Consider `Rc<str>` for token text, reference-based type checking, and `Cow`-style closure environments.

Note: `parser.rs` `advance()` no longer clones every token — it returns `&'src Token` references. `peek()`, `peek_at()`, and `expect()` also return references. This eliminates per-token cloning on the hottest parsing path.

### Inconsistent naming in parser

Abbreviated variable names (`vstart`, `vend`, `fstart`) instead of full names. Minor readability issue.

**Recommendation:** Rename during the next parser-touching change.

### `CheckResult.call_type_args` is keyed by `Span`

Sema records per-call-site concrete type arguments into `HashMap<Span, Vec<Type>>` and IR lowering looks them up at the matching call expression. This relies on sema and lowering agreeing on the exact `Span` object, which holds for the current single-file, single-pass pipeline but breaks under any transformation that reparents or synthesizes AST nodes (macros, cross-file inlining).

**File:** `phoenix-sema/src/checker.rs` (`CheckResult.call_type_args`); consumed in `phoenix-ir/src/lower.rs` (`LowerContext::resolve_call_type_args`).
**Planned fix:** Assign a stable `CallId: u32` to each call expression at parse time and key the map on it. Decouples the sema→lowering handoff from span identity.
**Target phase:** Phase 3. Not urgent while compilation stays single-file/single-pass. Must land before any change that synthesizes or reparents call-expression AST nodes.

### Occurs-check suppressed pending alpha-renaming of type parameters

`Checker::unify` (`phoenix-sema/src/check_types.rs`) detects and reports *conflicting* bindings (`T := Int` and later `T := String` at a different argument position) but does not run an occurs-check against cyclic bindings such as `T := List<T>`. The check is defined on `UnifyError::OccursCheck` but not emitted: Phoenix does not alpha-rename type parameter binders, so a scope-oblivious occurs-check false-positives on every template-body shadowing (`function outer<T> { inner(x) }` where both `outer<T>` and `inner<T>` use the same name `T`).

Not actively miscompiling today: Phoenix's `substitute` is a single-pass walk, so a cyclic binding produces a weird intermediate type rather than an infinite loop, and the downstream per-argument type check catches the user-visible errors. The concern is diagnostic quality (cyclic bindings are reported as cascade errors rather than a clean "cannot bind `T` to type containing `T`").

**File:** `phoenix-sema/src/check_types.rs` — `UnifyError::OccursCheck` variant kept for future use.
**Planned fix:** Introduce alpha-renaming (fresh-name each template's type parameter binders during inference) so the occurs-check can distinguish "same name, different binder" from "genuine cycle", then re-enable the check.
**Target phase:** Phase 3 or deferred until it causes a real diagnostic complaint.

### Generic-template stubs tracked by a `bool` flag

`IrFunction.is_generic_template: bool` marks templates that remain in `module.functions` as inert stubs after monomorphization (to preserve the `FuncId`-as-vector-index invariant). Every downstream consumer must either check the flag or iterate via `IrModule::concrete_functions()` — forgetting does not fail loudly, it just exposes `IrType::TypeVar` to code that panics on it (`IrType::is_value_type`, classification helpers). The audit on 2026-04-20 caught two slips (`IrModule::Display` and `ir_analysis.rs`) that had bypassed the filter.

**File:** `phoenix-ir/src/module.rs` — `IrFunction.is_generic_template`; iteration helper `IrModule::concrete_functions`.
**Planned fix:** Replace the bool flag with a typed split — a `ConcreteFunctions` newtype iterator, or two separate `functions` / `templates` fields — so the filter is enforceable at the type system level rather than at every call site.
**Target phase:** Phase 2.6 or Phase 3. Pairs naturally with the [`Value::Closure` → IR blocks refactor](design-decisions.md#interpreter-parser-coupling-via-valueclosure) scheduled for 2.6, since both are IR-shape refactors.
