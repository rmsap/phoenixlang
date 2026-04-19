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

**Workaround:** Pass closures directly to methods rather than through conditional block parameters.
**Root cause:** The IR's closure representation does not carry capture metadata alongside the function pointer.
**Tracked in:** Cranelift `ir_analysis.rs` `find_closure_capture_types`.
**Target phase:** Phase 2.2. Fix requires adding capture metadata to the IR closure representation. The [`Value::Closure` refactor](design-decisions.md#interpreter-parser-coupling-via-valueclosure) scheduled for Phase 2.6 touches the same representation — check whether the 2.2 fix can be staged to align with that refactor, or whether they should stay independent.

### `Result.ok()` and `Result.err()` not supported in compiled mode

The `Result.ok()` (returns `Option<T>`) and `Result.err()` (returns `Option<E>`)
methods work in the IR interpreter but are not yet implemented in the Cranelift
backend.  Calling them produces a "not yet supported in compiled mode" error.

**Workaround:** Use pattern matching to convert a Result to an Option manually.
**Tracked in:** Cranelift `result_methods.rs` dispatch table.
**Target phase:** Phase 2.2. Straightforward addition to the Cranelift dispatch table.

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
