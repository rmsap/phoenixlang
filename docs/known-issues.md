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

### `List<dyn Trait>` literal initialization in compiled mode

See also: [design-decisions.md: *Dynamic dispatch via `dyn Trait`*](design-decisions.md#dynamic-dispatch-via-dyn-trait) — this issue is one of the deferred follow-ups in that section's table.

Both the heterogeneous case (`[Circle(1), Square(2)]` typed as `List<dyn Drawable>`) and the *homogeneous* case (`[Circle(1), Circle(2)]` typed as `List<dyn Drawable>`) fail in compiled mode. Sema accepts them (the recursive `types_compatible` rule applies the dyn coercion to each element pair), but IR lowering never materializes element-wise `Op::DynAlloc` wraps — the list is allocated with concrete single-slot elements into a container annotated for 2-slot `dyn` elements, which either fails verification or crashes Cranelift layout.

**Previously suggested workaround (does not work today).** Building the list incrementally via `push()` from `let mut shapes: List<dyn Drawable> = []` fails at type-check: sema types the empty-list literal as `List<T>` and does not propagate the let annotation's element type into the literal, producing `type mismatch: variable 'shapes' declared as 'List<dyn Drawable>' but initialized with 'List<T>'`. Unblocking the literal path and the push path requires the same bidirectional-inference work. An ignored regression test pins the current behaviour (`phoenix-cranelift/tests/compile_dyn_trait.rs::dyn_list_via_push_workaround`).

Until that lands, there is no supported way to build a `List<dyn Trait>` in compiled mode.

**File:** `phoenix-sema/src/check_expr.rs` (`check_list_literal`) for the heterogeneous case; `phoenix-ir/src/lower_expr.rs` (`lower_list_literal`) for the homogeneous case — the literal doesn't see its surrounding let's annotation to know elements should be DynAlloc-wrapped.
**Planned fix:** Bidirectional type inference — thread the expected element type from the list-literal context into both sema (to accept heterogeneous elements as `dyn`) and IR lowering (to coerce each element). Same machinery that would enable lambda parameter inference at call sites ([design-decisions.md](design-decisions.md#lambda-parameter-inference-at-call-sites)).
**Target phase:** Phase 3 — tied to the bidirectional-inference rework.

**Last remaining dyn *construction* blocker.** After the 2026-04-24 fixes closed the `<T: Trait>` → `dyn Trait` coercion and default-argument paths, this entry is the final gate on the [`dyn Trait`/`StringRef` 2-slot discriminator](#dyn-trait-and-stringref-share-a-2-slot-layout-with-no-discriminator) below — the discriminator work only becomes urgent once `List<dyn Trait>` can actually be constructed. Schedule them together. (The *match-arm* dyn coercion gap below is a separate bidirectional-inference site — same Phase 3 rework unlocks both, but the two do not block each other.)

### Default arguments are not supported on method calls

`impl Counter { function bump(self, by: Int = 1) -> Int { ... } }` declares a method with a default.  Free-function calls that omit the defaulted slot now compile (the 2026-04-24 fix synthesizes the default via `merge_call_args` in `phoenix-ir/src/lower_expr.rs`), but method calls do not: `lower_method_call` in the same file builds the arg list directly from `MethodCallExpr.args` and never routes through `merge_call_args`.  `c.bump()` therefore lowers to an `Op::Call` with too few arguments and trips the Cranelift verifier.

**File:** `phoenix-ir/src/lower_expr.rs` (`lower_method_call`).  **Tripwire:** the `#[ignore]`'d `default_on_method_parameter` test in `crates/phoenix-cranelift/tests/compile_default_args.rs`.  **Workaround:** pass every argument explicitly at method call sites.  **Planned fix:** hoist the `merge_call_args`/`coerce_call_args` pair into `lower_method_call`'s direct and builtin-call branches, treating `self` as a pre-filled slot 0.  The `MethodCallExpr` AST node does not carry named-arg syntax today, so the initial fix is positional-only — named-arg support on methods can follow.  **Target phase:** Phase 2.2 follow-up.

### Default-expression visibility across module boundaries (Phase 2.6 tripwire)

Default-argument expressions are lowered at the *caller's* call site (see [design-decisions.md: *Default-argument lowering strategy*](design-decisions.md#default-argument-lowering-strategy)).  Today every Phoenix program is single-module, so "caller's scope" and "callee's scope" coincide and visibility is a non-issue.  Phase 2.6 (module system + `public` / private) breaks this:

```phoenix
// models/user.phx
function hashPassword(plaintext: String) -> String { ... }  // module-private
public function createUser(name: String, hash: String = hashPassword("")) -> User { ... }

// main.phx
import models.user { createUser }
function main() { createUser("Alice") }  // inlines hashPassword("") into main.phx's IR
```

Three failure modes once modules land: (1) privacy leak — `main.phx`'s compiled output references the private `hashPassword` symbol directly, forcing implicit re-export; (2) contract leak — renaming / changing `hashPassword` silently breaks every caller of `createUser`, yet the module author expected it to be safely private; (3) sema doesn't detect the shape today because defaults are type-checked in the callee's module with full access.

**Planned fix:** Synthesize a private-wrapping helper.  When a public function's default references any private item, the compiler emits a hidden public wrapper (`__default_createUser_hash()`) in the callee's module that can see the private internals.  The caller's IR calls the wrapper, not the private symbol directly.  Preserves the current caller-side ABI; private symbols stay private at the binary level.  Small semantic shift — defaults referencing private state evaluate in the callee's scope rather than the caller's — accepted and to be documented at that time.  Alternatives (reject at sema; declare not-a-problem) rejected for restrictiveness / footgun reasons respectively.

**File:** `phoenix-ir/src/lower_expr.rs` (`merge_call_args`) is where the inlining happens today; Phase 2.6 sema will gain the visibility-at-default-checking pass and the wrapper-synthesis IR pass.  **Tripwire:** none today — no `#[ignore]`d test, since the shape is unreachable from single-module source.  When modules land, add a test that a public function with a private-referencing default compiles (via the wrapper) and that the private symbol is *not* exported.  **Target phase:** Phase 2.6 — lands with the module-system work itself; cannot be deferred beyond it.

### Trait bounds don't propagate through nested generic calls

`function outer<T: Drawable>(x: T) { return inner(x) }` where `inner<U: Drawable>(y: U) { ... }` is rejected by sema with "type `T` does not implement trait `Drawable`".  At the call site to `inner`, sema's trait-bound inference doesn't see that `T: Drawable` (known from the outer function's bound) satisfies `U: Drawable`.  The fix lives in bound-resolution: when inferring type args for a call whose formal has a trait bound, and the inferred concrete type is itself a type variable, consult the enclosing scope's bound environment before rejecting.

**File:** `phoenix-sema/src/check_expr_call.rs` (bound-satisfaction check at call-site type-arg inference).  **Tripwire:** the `#[ignore]`'d `nested_generic_dyn_coercion_specializes` test in `crates/phoenix-cranelift/tests/compile_dyn_trait.rs`.  **Workaround:** monomorphize the inner call by hand (`let d: dyn Drawable = x; d.draw()` at the outer level) instead of delegating to a second trait-bounded generic.  **Target phase:** Phase 3 — pairs naturally with the bidirectional-inference rework since both rely on threading expected-type / bound context through sema.

### Match-arm result coercion to `dyn Trait` return type

`function f() -> dyn Trait { match x { A -> Concrete1(...) B -> Concrete2(...) } }`
is rejected by sema with "match arm type mismatch: expected `Concrete1`
but got `Concrete2`" before lowering can attempt the dyn coercion. The
arm-result inference unifies arm types to a single concrete type rather
than propagating the function's `dyn Trait` return type as the expected
join type. Same root cause as the heterogeneous list-literal gap —
bidirectional inference into expression contexts.

**Workaround:** wrap each arm body in an explicit dyn binding
(`A -> { let d: dyn Trait = Concrete1(...); d }`) so the coercion
happens before the match union runs. **Tracked by** the `#[ignore]`'d
`dyn_match_arm_coerces_to_function_return_type` test in both
`crates/phoenix-cranelift/tests/compile_dyn_trait.rs` and the
matching round-trip file. **Target phase:** Phase 3 — pairs with the
bidirectional-inference rework that also unblocks `List<dyn Trait>`.

### Multi-bound generic parameters (`<T: Foo + Bar>`) rejected by parser

The parser today fails with "expected '>'" on the `+` in `<T: Foo + Bar>`. The design is uncontroversial (bounds are conjunctive), but the parser + sema + monomorphization plumbing needs to thread a `Vec<String>` where today it threads a single `Option<String>`.

**Target phase:** Phase 3. **Tripwire:** the `"expected '>'"` error message at any `+` inside a type-parameter bound list. No workaround except "split into two type parameters" which usually doesn't achieve what the user wanted.

### Methods on generic enums are gated off; payload-inference fallbacks kept alive as a consequence

User-defined `impl<T> MyEnum<T> { ... }` is rejected by a `debug_assert!` in `phoenix-ir/src/lower_decl.rs` (enum branch) because `register_method` emits `IrType::EnumRef(type_name, Vec::new())` for the `self` parameter — empty args, not the enum's declared type parameters. Landing this feature requires threading the enum's `info.type_params` into the `self` `EnumRef` args and teaching monomorphization to specialize methods on generic enums (already handles generic functions + methods on non-generic user types).

As a side effect, the Cranelift backend's payload-inference fallback chain in `phoenix-cranelift/src/translate/enum_type_inference.rs` (Strategies 1 / 1b / 2 / 3 / 4) stays load-bearing: today the `self` `EnumRef` on an enum method has empty args, so Strategy 0 (`try_type_from_enum_args`) returns `None` and the fallbacks pick up the slack. Once this gate lifts and args are threaded through, every `EnumRef` reaching the backend carries concrete args, Strategy 0 becomes total, and Strategies 1–4 + their tests collapse into a single pass. See the in-file FIXME (lines 46–53).

**File:** `phoenix-ir/src/lower_decl.rs` (gate + fix site, enum branch of `register_method`); `phoenix-cranelift/src/translate/enum_type_inference.rs` (dead code after the gate lifts).
**Target phase:** Phase 4 (Stdlib) by default — when user-facing generic containers with methods ship, this is the natural moment. **Earlier if demand-triggered:** the `debug_assert!` in `register_method`'s enum branch is the tripwire; whoever first writes `impl<T> MyEnum<T>` hits it and picks up the feature + the strategy collapse in one motion.

### Generic-template stubs tracked by a `bool` flag

`IrFunction.is_generic_template: bool` marks templates that remain in `module.functions` as inert stubs after monomorphization (to preserve the `FuncId`-as-vector-index invariant). Every downstream consumer must either check the flag or iterate via `IrModule::concrete_functions()` — forgetting does not fail loudly, it just exposes `IrType::TypeVar` to code that panics on it (`IrType::is_value_type`, classification helpers). The audit on 2026-04-20 caught two slips (`IrModule::Display` and `ir_analysis.rs`) that had bypassed the filter.

**File:** `phoenix-ir/src/module.rs` — `IrFunction.is_generic_template`; iteration helper `IrModule::concrete_functions`.
**Planned fix:** Replace the bool flag with a typed split — a `ConcreteFunctions` newtype iterator, or two separate `functions` / `templates` fields — so the filter is enforceable at the type system level rather than at every call site.
**Target phase:** Phase 2.6 or Phase 3. Pairs naturally with the [`Value::Closure` → IR blocks refactor](design-decisions.md#interpreter-parser-coupling-via-valueclosure) scheduled for 2.6, since both are IR-shape refactors.

### `IrFunction.value_types` is a parallel index without a type-level guarantee

`IrFunction.value_types: Vec<Option<IrType>>` is indexed by `ValueId.0` and kept in sync with `next_value_id` by `fresh_value()`, `emit()`, and `add_block_param()`.  Any pass that allocates a `ValueId` without going through those three entry points would silently desync the index — the `O(1)` type lookup via `instruction_result_type` would then return `None` (or worse, a stale type from an overwritten slot) rather than fail loudly.

Today this is partly mitigated: the verifier's `verify_value_types_index` flags length mismatches, and the centralized `IrFunction::for_each_type_mut` is the only way monomorphization walks all four parallel type annotations (param / return / block-param / per-value / instruction result).  But the invariant lives by convention, not by type — a future pass could bypass both without any compiler error.

**New consumer as of 2026-04-24:** `resolve_unresolved_dyn_allocs` in `phoenix-ir/src/monomorphize/function_mono.rs` reads `func.instruction_result_type(value)` after substitution to derive the concrete type name for a `dyn Trait` vtable registration.  A desync in `value_types` here would silently miscompile by registering against the wrong concrete type.  The mono-time hard-panic with a diagnostic is defensive coverage, but the underlying risk is the same as every other consumer — worth folding in when the typed-split refactor lands.

**File:** `phoenix-ir/src/module.rs` — `IrFunction.value_types`, `fresh_value`, `emit`, `add_block_param`.
**Planned fix:** Introduce a `ValueIdAllocator` newtype that owns both the counter and the parallel index; make `ValueId` allocation and type assignment the same operation at the type level (no API for "allocate a ValueId without assigning a type").  Length-mismatch bugs become compile errors instead of runtime verifier errors.
**Target phase:** Phase 2.6 or Phase 3.  Pairs with the `is_generic_template` typed-split refactor above — both are IR-shape rewrites.

### `dyn Trait` and `StringRef` share a 2-slot layout with no discriminator

`TypeLayout::of` in `phoenix-cranelift/src/translate/layout/type_layout.rs` maps both `StringRef` and `DynRef` to a 2-slot `[POINTER_TYPE, POINTER_TYPE]` layout.  The Phoenix runtime's list/map helpers (`phx_list_contains`, `phx_map_*`) use `elem_size == 16` as a heuristic for "`StringRef` — compare by content"; a `List<dyn Trait>` element is indistinguishable at that boundary.

Today this is harmless because `List<dyn Trait>` does not compile (see [`List<dyn Trait>` literal initialization in compiled mode](#listdyn-trait-literal-initialization-in-compiled-mode)).  The moment the bidirectional-inference fix unblocks `List<dyn Trait>`, the runtime must gain a proper discriminator — options include a per-list element-kind tag, a `dyn`-aware runtime helper set, or embedding a 1-byte type tag in the fat-pointer layout itself (ABI change).

**File:** `phoenix-cranelift/src/translate/layout/type_layout.rs` (layout table + cross-crate-invariant comment); `phoenix-runtime/src/list_methods.rs` (consumer of the heuristic).
**Planned fix:** decide and implement a dyn-vs-string discriminator before `List<dyn Trait>` lands.  A 1-byte type tag in the first pointer slot was considered during the 2026-04-20 audit and deferred (ABI-scope change; would re-litigate the [centralized Layout trait](design-decisions.md#centralized-layout-for-reference-types) decision).  A discriminator-at-the-list-level fix is lower-impact and currently preferred.
**Target phase:** Phase 3 — lands with the bidirectional-inference fix for `List<dyn Trait>`.
