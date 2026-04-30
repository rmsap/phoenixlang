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

### O(n) map key lookup

`Map<K, V>` key lookup, insertion, removal, and contains operations use a
linear scan over a flat array.  Building an n-entry map is O(n²).

**Planned fix:** Hash-based implementation.
**Tracked in:** `phoenix-runtime/src/map_methods.rs` module header.
**Target phase:** Phase 2.3 (Runtime and Memory Management).

### Closure functions inside generic templates are not cloned per specialization

When a generic function `f<T>(...)` defines a closure whose body or captures reference `T`, the closure function is pushed to `IrModule.functions` as a single concrete entry at lowering time. Monomorphization specializes `f<T>` per concrete type substitution but does **not** clone the closure function — the same `FuncId` is shared by every `f<T1>` / `f<T2>` / ... specialization. Pass D (`erase_type_vars_in_non_templates`) then erases the closure body's residual TypeVars to the `__generic` placeholder.

**Symptoms.**

- `phoenix run` (tree-walk) and `phoenix run-ir` (IR-interp) produce correct output. They dispatch on the runtime value shape, not on a static layout, so the erased placeholder is harmless.
- `phoenix build` (Cranelift) is unsafe whenever the closure body's slot layout depends on `T`. `TypeLayout::of(__generic)` falls back to a 1-slot layout, so the codegen happens to work for 1-slot type instantiations (Int / Bool) and silently miscompiles for wider ones (String / fat-pointer types) — observed as wild allocations or out-of-bounds loads on the wider instantiation.
- Where the closure body *directly* references `T` in a position Cranelift cannot finesse — e.g. `Op::ClosureLoadCapture` whose result type is `T` and is consumed by an op needing a known layout — Cranelift emits an explicit ICE: `TypeLayout::of on IrType::TypeVar(T) — monomorphization should have eliminated all type variables before codegen`.

**Workaround.** Avoid generic functions that return or contain closures referencing `T` in their captures or body. For now, write monomorphic closures or manually specialize the generic at each concrete type.

**Planned fix.** Extend monomorphization to clone closure functions per substitution of their enclosing generic, mirroring how struct-mono clones methods on generic structs. The substitution machinery itself already reaches `IrFunction.capture_types` and the `Op::ClosureLoadCapture` result-type slot via `for_each_type_mut` — pinned by tests in `crates/phoenix-ir/src/monomorphize/tests.rs::for_each_type_mut_substitutes_capture_types_*`. The remaining work is in the cloning machinery (`crates/phoenix-ir/src/monomorphize/function_mono.rs::clone_and_substitute_bodies`): when a generic template body contains an `Op::ClosureAlloc(closure_fid, ...)`, the specialization needs its own clone of `closure_fid` registered, with TypeVars substituted, and the `Op::ClosureAlloc` rewritten to point at the clone.

**Tracked in:**
- `tests/fixtures/closures_over_generic.phx` — passing fixture covering single-instantiation cases that work today.
- `tests/fixtures/closures_over_generic_cross_width.phx` — `#[ignore]`d regression marker for the cross-width case.
- `crates/phoenix-driver/tests/three_backend_matrix.rs::matrix_closures_over_generic_cross_width`.

**Target phase:** Phase 2.6 if a module-system fixture trips the gap; otherwise Phase 3 (defer until generic-closure-over-T patterns appear in real code).

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

### Interpolated-expression spans are zero-based, colliding across functions

Field accesses (and other subexpressions) inside string interpolations are parsed by a sub-parser fed only the substring between `{` and `}`, with no offset relative to the enclosing source file. Spans on the resulting AST nodes therefore start at offset 0 and collide whenever two different functions interpolate field accesses with the same length — sema's per-`Span` type map (`source_type_at`) overwrites the earlier entry, and IR lowering then sees the wrong receiver type. The symptom is an `unreachable!()` in `lower_field_access` of the form `field access on unknown struct layout: field 'X' on type Named("Y")`, where X is one impl's field and Y is another impl's struct.

Concrete repro: two impls of the same trait method, each interpolating one of its own fields, e.g.
```
impl Drawable for Circle { function draw(self) -> String { "c={self.radius}" } }
impl Drawable for Square { function draw(self) -> String { "s={self.side}"   } }
```
fails under `phoenix run-ir` and `phoenix build`; the AST-walking interpreter (`phoenix run`) is unaffected because it does not consult sema's span map.

**File:** `phoenix-parser/src/expr.rs::parse_interpolation_segments` (sub-parser invocation at the `tokenize(&expr_src, source_id)` line); consumed in `phoenix-ir/src/lower_expr.rs::lower_field_access` and any other lowering that calls `source_type_at(&span)`.
**Planned fix:** Same root cause as the `call_type_args` entry above. Either adjust the sub-parser to translate spans by the interpolation's start offset, or move sema/lowering off `Span`-keyed maps onto stable per-AST-node ids. The latter retires this whole class of bug.
**Target phase:** Phase 3 (alongside the broader span-vs-id refactor).
**Discovered:** 2026-04-27 while building the Phase 2.2 three-backend roundtrip matrix. The fixture pattern is excluded from the matrix until the fix lands.

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

### Default arguments on trait-method calls (object-safe dispatch + trait-bounded generics)

Default-argument values work for inherent-impl methods as of 2026-04-24 (see [phase-2.md](phases/phase-2.md)), but *trait-method* defaults declared inside a `trait` block are not yet handled.  Two dispatch sites fall through sema's `check_method_args` with empty default maps today:

- Trait-object dispatch (`x: dyn Drawable; x.draw()`): `check_expr_call.rs` around the `Type::Dyn` branch passes `&HashMap::new()` for defaults because `TraitInfo::methods` does not carry `default_param_exprs`.
- Trait-bounded dispatch (`x.m()` where `x: T, T: Trait`): `resolve_trait_bound_method` similarly passes no defaults.

**Planned fix:** Add `default_param_exprs: HashMap<usize, Expr>` to `TraitMethodInfo`, populate in `register_trait_decl`, and thread through both dispatch sites in `check_method_args`.  The IR-side synthesis is trickier for trait-bounded calls — they lower to `Op::UnresolvedTraitMethod` at first and only become a concrete `Op::Call` at monomorphization time, so defaults must be materialized *after* substitution.  The clean approach is to defer default lowering to `placeholder_resolution::resolve_trait_bound_method_calls` (same site that rewrites the placeholder), consulting the trait's `default_param_exprs` keyed by method name.

**Tripwire:** a test shape like `trait Bumpable { function bump(by: Int = 1) -> Int } ... x.bump()` on a trait-bound `x: T` fails at sema with "method takes 1 argument, got 0".  **Workaround:** declare the default at the `impl` site rather than the trait site.  **Target phase:** Phase 3 — pairs naturally with the bidirectional-inference rework since trait-method default lowering shares the placeholder-resolution machinery.

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

### `dyn Trait` and `StringRef` share a 2-slot layout with no discriminator

`TypeLayout::of` in `phoenix-cranelift/src/translate/layout/type_layout.rs` maps both `StringRef` and `DynRef` to a 2-slot `[POINTER_TYPE, POINTER_TYPE]` layout.  The Phoenix runtime's list/map helpers (`phx_list_contains`, `phx_map_*`) use `elem_size == 16` as a heuristic for "`StringRef` — compare by content"; a `List<dyn Trait>` element is indistinguishable at that boundary.

Today this is harmless because `List<dyn Trait>` does not compile (see [`List<dyn Trait>` literal initialization in compiled mode](#listdyn-trait-literal-initialization-in-compiled-mode)).  The moment the bidirectional-inference fix unblocks `List<dyn Trait>`, the runtime must gain a proper discriminator — options include a per-list element-kind tag, a `dyn`-aware runtime helper set, or embedding a 1-byte type tag in the fat-pointer layout itself (ABI change).

**File:** `phoenix-cranelift/src/translate/layout/type_layout.rs` (layout table + cross-crate-invariant comment); `phoenix-runtime/src/list_methods.rs` (consumer of the heuristic).
**Planned fix:** decide and implement a dyn-vs-string discriminator before `List<dyn Trait>` lands.  A 1-byte type tag in the first pointer slot was considered during the 2026-04-20 audit and deferred (ABI-scope change; would re-litigate the [centralized Layout trait](design-decisions.md#centralized-layout-for-reference-types) decision).  A discriminator-at-the-list-level fix is lower-impact and currently preferred.
**Target phase:** Phase 3 — lands with the bidirectional-inference fix for `List<dyn Trait>`.

### Sema `Type::Named/Generic/Dyn` payload allocates on every construction

Phase 2.6 made every `Type::Named` / `Type::Generic` / `Type::Dyn` payload carry the canonical *qualified* key (`lib::User`) so cross-module symbol-table lookups hit on a single global probe. The qualification path goes through `Checker::qualify_in_current` (`phoenix-sema/src/module_scope.rs`), which always returns an owned `String` — even in the common bare-equals-canonical case (entry-module decls and builtins, where the scope maps `name → name`). Sema runs construct many `Type::*` values per check pass, so the construction cost is a small but unmeasured per-call allocation that scales with type-annotation density.

**Planned fix:** switch the payload type to `Cow<'static, str>` (cheap for builtins) or intern identifiers in a per-`Checker` arena keyed by the canonical string. Either change keeps the qualified-equals-bare fast path zero-alloc. The `canonicalize_type_name` helper already returns `&str` and is the natural sibling for the borrow case; the work is to update every `Type::*` consumer in sema/IR/codegen (a wide diff, but mechanical).

**File:** `phoenix-sema/src/module_scope.rs` (`qualify_in_current`); `phoenix-sema/src/types.rs` (the `Type` enum); every consumer that pattern-matches on `Type::Named(name)` etc.
**Recommendation:** measure before acting — the regression may not be load-bearing on real programs.
**Target phase:** Phase 3 or later, demand-triggered by a sema-perf complaint.

**Sibling site to fix in the same pass:** `phoenix-ir/src/lower.rs::LoweringContext::qualify` returns `Cow::Owned(qualified.to_string())` for the cross-module case — the borrowed `&str` from `resolve_visible` is bound to `&self` (via `self.check`), so a longer-lived `Cow<'a, str>` API would let the cross-module case stay zero-alloc too. That's a wider refactor (callers' lifetimes would have to thread `'a`), but worth doing alongside the sema-side change so both layers' allocation profiles improve in lockstep.

### `drain_remaining_into` callback duplication in `resolved.rs`

`build_enums` / `build_structs` / `build_traits` (`phoenix-sema/src/resolved.rs`) each call `drain_remaining_into` with a four-line closure that allocates the next `*Id`, inserts it into the matching `*_by_name` map, and pushes the info into the matching `*s` vec. The three closures are structurally identical — only the id-allocator (`next_enum_id` / `next_struct_id` / `next_trait_id`), the name map, and the vec differ.

**Recommendation:** if a fourth user-table type lands (interfaces, type classes, …) factor `drain_remaining_into` to take an `(id_allocator, name_map, vec)` tuple so all four call sites collapse to one call. Three sites is below the abstraction threshold; document so the next maintainer sees the precedent.
**File:** `phoenix-sema/src/resolved.rs` (`drain_remaining_into` + its three callers in `build_enums` / `build_structs` / `build_traits`).
**Target phase:** when the fourth user-table type lands.
