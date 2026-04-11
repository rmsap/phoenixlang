# Known Issues & Design Decisions

Issues identified during audits that require design decisions or architectural changes beyond simple bug fixes. Items marked "Scheduled" have been addressed.

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
