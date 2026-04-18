# Known Issues & Design Decisions

Issues identified during audits that require design decisions or architectural changes beyond simple bug fixes. Items marked "Scheduled" have been addressed.

---

## Memory Management

### All compiled programs leak memory (no GC)

Compiled Phoenix binaries (`phoenix build`) allocate heap memory via `malloc` but never free it. There is no garbage collector, reference counting, or manual deallocation. This means every string, list, map, closure, struct, and enum variant allocated during execution is leaked.

Compiled binaries are **not suitable for long-running processes** (servers, daemons). Short-lived CLI programs are fine in practice since the OS reclaims all memory on exit.

**Planned fix:** Garbage collector (tracing GC or reference-counted) in Phase 2.3 — see [phase-2.md](phases/phase-2.md#23-runtime-library-expand).

---

## Runtime Behavior

### `List.get` panics on out-of-bounds

`List.get(index)` terminates the process (panic/exit) when the index is out of bounds, rather than returning `Option<T>`. This is inconsistent with `Map.get(key)`, which returns `Option<V>` for missing keys.

**Workaround:** Check `list.length()` before calling `get`, or use `first`/`last` which return `Option<T>`.

### `Map<Float, V>` uses byte-wise key comparison

`Map<Float, V>` compares keys using byte-wise equality, not IEEE 754 floating-point equality. This means `-0.0` and `0.0` are treated as different keys, and `NaN` equals itself (unlike IEEE 754 where `NaN != NaN`). This is deliberate — byte-wise comparison provides consistent, deterministic behavior for map lookups.

### `substring` clamps out-of-range indices silently

`substring(start, end)` silently clamps out-of-range indices instead of returning an error:
- Negative `start` is clamped to `0`
- `end` beyond the string length is clamped to the string length
- `start > end` produces an empty string

This matches the behavior of JavaScript's `String.prototype.substring()` but may surprise users expecting strict bounds checking.

---

## Architecture

### `break`/`continue` in match arms inside loops — Resolved

`break` and `continue` inside a match arm are now rejected at the semantic analysis stage with a compile error. This prevents the silent conversion to `Void` that occurred in the interpreter. Option 1 (threading `StmtResult` through expression evaluation) can be revisited in the IR phase if needed.

### Interpreter-parser coupling via `Value::Closure`

`Value::Closure` in `phoenix-interp` stores `phoenix_parser::ast::Block` directly, tightly coupling the interpreter to the parser AST. If the parser changes, closures break.

**Recommendation:** When Phase 2.1 (IR) begins, closures should store IR blocks instead of AST blocks. No action needed in the interpreter phase, but keep this in mind.

---

## Design Decisions

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

### Diagnostic builder pattern

Diagnostics are currently constructed inline everywhere via `self.error(format!(...), span)`. A builder pattern like `Diagnostic::error(span, msg).with_note(...).with_suggestion(...)` would improve consistency and make it easier to add rich diagnostics.

**Recommendation:** Implement before Phase 3.5 (Error Messages).

### `defer` for resource cleanup

For a web-focused language, explicit resource cleanup with `defer conn.close()` (Go-style) is often more readable than implicit drop semantics, especially in async contexts.

**Recommendation:** Revisit after Phase 4.3 (Async Runtime).

---

## Bugs

### SourceId hardcoded to 0 in string interpolation sub-parser — Resolved

**File:** `phoenix-parser/src/expr.rs`

The `Parser` struct now carries a `source_id` field derived from the first token's span. The interpolation sub-parser uses `self.source_id` instead of extracting it from individual tokens. Multi-file compilation will work correctly when added in Phase 2.

### Silent zero substitution on out-of-range integer/float literals

**File:** `phoenix-parser/src/expr.rs`

When an integer or float literal is out of range, the parser emits a diagnostic but substitutes `0` (or `0.0`) into the AST.

**Recommendation:** Acceptable for now. Consider adding an `ErrorLiteral` AST variant if this causes real-world confusion.

### `Option.okOr()` fails to compile when payload type cannot be inferred

The `okOr` combinator on `Option<T>` values produces a compile error when the Cranelift backend cannot infer the payload type `T`. This happens when the `Option` value comes from a function parameter or cross-function return (where generic type arguments are not propagated through the IR's `EnumRef` type). Previously this silently fell back to `IrType::I64` (1 slot), which corrupted multi-slot types like `String` (pointer + length = 2 slots). The fix surfaces a clear compile error instead of silently miscompiling.

**Workaround:** Use pattern matching instead of `okOr` when the Option comes from a function parameter.
**Root cause:** `IrType::EnumRef("Option")` does not carry generic type arguments.
**Tracked in:** Cranelift `option_methods.rs` `option_payload_type` function.

### Closure capture type ambiguity with indirect calls

When a closure is passed through a block parameter (phi node), the compiler
falls back to a heuristic scan of IR functions to find capture types.  If two
closures share the same user-param types, return type, and capture types, they
are silently conflated.  Different capture layouts are caught (compile error),
but identical-layout mismatches are invisible.

**Workaround:** Pass closures directly to methods rather than through conditional block parameters.
**Root cause:** The IR's closure representation does not carry capture metadata alongside the function pointer.
**Tracked in:** Cranelift `ir_analysis.rs` `find_closure_capture_types`.

### `Result.ok()` and `Result.err()` not supported in compiled mode

The `Result.ok()` (returns `Option<T>`) and `Result.err()` (returns `Option<E>`)
methods work in the IR interpreter but are not yet implemented in the Cranelift
backend.  Calling them produces a "not yet supported in compiled mode" error.

**Workaround:** Use pattern matching to convert a Result to an Option manually.
**Tracked in:** Cranelift `result_methods.rs` dispatch table.

### O(n) map key lookup

`Map<K, V>` key lookup, insertion, removal, and contains operations use a
linear scan over a flat array.  Building an n-entry map is O(n²).

**Planned fix:** Hash-based implementation.
**Tracked in:** `phoenix-runtime/src/map_methods.rs` module header.

### O(n²) `List.sortBy` insertion sort

`List.sortBy` uses O(n²) insertion sort in both backends.  In the interpreter,
the comparator closure requires `&mut self` on the interpreter, preventing use
of `slice::sort_by`.  In the Cranelift compiler, the comparator closure must be
called through block-based control flow, and inline insertion sort maps
naturally to nested loops.  Both backends use the same algorithm for
consistency.  Acceptable for small lists but a performance hazard for large ones.

**Planned fix:** Merge sort implementation.
**Tracked in:** `list_methods_complex.rs` `translate_list_sortby` doc comment.

---

## Code Quality

### Excessive cloning (~216 sites)

Key offenders:
- `interpreter.rs`: `self.env.snapshot()` deep-clones the entire scope stack for every closure creation
- `check_expr.rs` / `check_types.rs`: many clone calls on type information that could use references (split from the original `checker.rs`)

**Recommendation:** Address before compilation (Phase 2). Consider `Rc<str>` for token text, reference-based type checking, and `Cow`-style closure environments.

Note: `parser.rs` `advance()` no longer clones every token — it returns `&'src Token` references. `peek()`, `peek_at()`, and `expect()` also return references. This eliminates per-token cloning on the hottest parsing path.

### `checker.rs` test module extracted — Resolved

**File:** `phoenix-sema/src/checker.rs`

The ~3,000-line test module has been extracted to `checker_tests.rs`, reducing `checker.rs` from 3,997 lines to 950 lines.

### `parse_prefix` exceeds 190 lines — Resolved

**File:** `phoenix-parser/src/expr.rs`

Reviewed and found to be 75 lines. The major cases (list literals, map literals, match expressions, lambda expressions, identifiers/constructors) are already extracted into separate helper methods. The remaining inline cases (unary operators, parenthesized expressions, literals, `self` keyword) are too small to benefit from further extraction.

### Inconsistent naming in parser

Abbreviated variable names (`vstart`, `vend`, `fstart`) instead of full names. Minor readability issue.

**Recommendation:** Rename during the next parser-touching change.

---

## Testing Gaps — Scheduled

All testing gaps below have been addressed. Tests are integrated into the existing test files alongside related tests.

### Feature interaction coverage — Scheduled

- Generics + closures together — 5 tests in `functions_and_closures.rs` (generic function with closure arg, returns closure, captures generic value, struct with closure usage, list map with closure)
- Try operator (`?`) in nested closures — 3 tests in `functions_and_closures.rs` (ok path, error propagation, option variant)
- Pattern matching on nested generic types — 5 tests in `enums_and_matching.rs` (nested Option, nested Result, Option containing List)

### Missing edge case tests — Scheduled

- Multiple closures capturing the same variable — 2 tests in `functions_and_closures.rs` (inc/dec/get pattern, set/get/add pattern)
- Empty string interpolation `"{}"` — 1 test in `strings.rs` (confirmed as parse error)
- Variable shadowing across scopes — 3 tests in `basic.rs` (for loop, function params, same-scope redefinition error); pre-existing tests already covered if and while scopes
- Floating-point precision (`0.1 + 0.2`) — 3 tests in `basic.rs` (addition, subtraction, large value)
- `i64::MIN` negation — 1 test in `basic.rs` (min-1 overflow); pre-existing tests already covered negation overflow and max+1
- Deeply nested field assignment chains — 2 tests in `types_and_structs.rs` (4-level, multiple fields); pre-existing test already covered 3-level

### Missing negative tests — Scheduled

- Multiple trait bounds (`T: Foo + Bar`) — 1 test in `traits.rs`, parse error "expected '>'" confirmed
- Compound assignment operators (`+=`, `-=`, `*=`, `/=`, `%=`) — now supported; desugared to `x = x op expr` in the parser

### Snapshot tests for error messages — Scheduled

`insta` snapshot tests now active in both parser and checker:
- Parser: 4 snapshot tests in `phoenix-parser/src/parser.rs` (missing function name, missing closing brace, missing paren, unexpected token)
- Checker: 5 snapshot tests in `phoenix-sema/src/checker.rs` (type mismatch, undefined variable, immutable assignment, wrong arg count, trait not implemented)
- Snapshots stored in `crates/phoenix-parser/src/snapshots/` and `crates/phoenix-sema/src/snapshots/`

### Doc-tests — Scheduled

Key public APIs now have runnable doc-test examples (7 total, up from 1):
- `parser::parse` — 2 doc-tests (valid parse, invalid source with diagnostics)
- `checker::check` — 2 doc-tests (valid program, type error detection)
- `interpreter::run` — 1 doc-test (full pipeline execution)
- `interpreter::run_and_capture` — 1 doc-test (output capture)
- `lexer::tokenize` — 1 doc-test (pre-existing)
