//! wasm32-gc backend integration tests (Phase 2.4 PR 5).
//!
//! These tests exercise `phoenix-cranelift`'s `Target::Wasm32Gc`
//! pipeline end-to-end. The structural tier always runs (it just
//! asks `wasmparser` whether the module parses with the GC proposal
//! enabled); the execution tier runs whenever `wasmtime` is on
//! `$PATH`, invoking it with `-W gc=y` to enable the GC proposal.
//!
//! `PHOENIX_REQUIRE_WASMTIME=1` turns the soft-skip on missing
//! wasmtime into a hard failure — same gating shape as the
//! wasm32-linear integration tests in [`compile_wasm_linear.rs`].

use std::process::{Command, Stdio};

use phoenix_common::SourceId;
use phoenix_cranelift::{Target, compile};
use phoenix_ir::instruction::{FuncId, Op};
use phoenix_ir::module::{IrFunction, IrModule};
use phoenix_ir::terminator::Terminator;
use phoenix_ir::types::IrType;

/// Lower a Phoenix source string through lexer → parser → sema → IR
/// to produce an `IrModule` for the codegen pipeline to operate on.
/// Same recipe the wasm32-linear tests use; if any front-end stage
/// rejects the program, this panics with the diagnostics.
fn lower_to_ir(source: &str) -> IrModule {
    let tokens = phoenix_lexer::tokenize(source, SourceId(0));
    let (program, parse_errors) = phoenix_parser::parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parser errors: {parse_errors:?}");
    let analysis = phoenix_sema::checker::check(&program);
    assert!(
        analysis.diagnostics.is_empty(),
        "sema errors: {:?}",
        analysis.diagnostics
    );
    let ir_module = phoenix_ir::lower(&program, &analysis.module);
    let verify_errors = phoenix_ir::verify::verify(&ir_module);
    assert!(
        verify_errors.is_empty(),
        "IR verification errors: {verify_errors:?}"
    );
    ir_module
}

/// `PHOENIX_REQUIRE_WASMTIME=1` turns the soft skip on missing
/// wasmtime into a hard failure. Same shape as the wasm32-linear
/// integration tests.
fn require_wasmtime() -> bool {
    std::env::var("PHOENIX_REQUIRE_WASMTIME").as_deref() == Ok("1")
}

/// Compile `source` through `Target::Wasm32Gc` and return the
/// resulting WASM bytes. Panics with the codegen diagnostic on
/// compile failure.
fn compile_to_wasm_gc(source: &str) -> Vec<u8> {
    let ir_module = lower_to_ir(source);
    compile(&ir_module, Target::Wasm32Gc)
        .unwrap_or_else(|e| panic!("wasm32-gc compile failed: {e}"))
}

/// Validate `bytes` with `wasmparser`, GC proposal enabled. Panics
/// with the parse diagnostic on rejection. Always runs (no wasmtime
/// dependency), so it guards module structure even when the execution
/// tier is skipped.
///
/// Note on what this currently proves: slices 1–2 emit **no** WASM-GC
/// types or instructions — they are structurally plain linear modules.
/// Enabling `WasmFeatures::GC` here validates against a superset, so
/// today this asserts only "is a structurally valid module," not "uses
/// the GC proposal correctly." The GC-specific coverage arrives with
/// the struct slice (decision J slice 3), which is the first to declare
/// `struct.new` types; the feature flag is enabled now so that fixture
/// needs no test-harness change when it lands.
fn validate_gc_module(bytes: &[u8], label: &str) {
    let mut features = wasmparser::WasmFeatures::default();
    features.insert(wasmparser::WasmFeatures::GC);
    let mut validator = wasmparser::Validator::new_with_features(features);
    validator
        .validate_all(bytes)
        .unwrap_or_else(|e| panic!("wasmparser rejected wasm32-gc {label}: {e}"));
}

/// Compile `source`, structurally validate it, and — when wasmtime is
/// available — assert its stdout equals `expected`. Shared by the
/// `print(Int)` digit-conversion cases.
fn assert_prints(source: &str, label: &str, expected: &[u8]) {
    let bytes = compile_to_wasm_gc(source);
    assert_wasm_prints(&bytes, label, expected);
}

/// Structurally validate `bytes` and — when wasmtime is available —
/// assert its stdout equals `expected`. Shared by the source-driven
/// [`assert_prints`] and the hand-built-IR negative-path test, which has
/// no Phoenix source to lower (see [`print_int_module`]).
fn assert_wasm_prints(bytes: &[u8], label: &str, expected: &[u8]) {
    validate_gc_module(bytes, label);
    if let Some(stdout) = run_under_wasmtime_gc(bytes, label) {
        assert_eq!(
            stdout,
            expected,
            "{label} stdout mismatch:\n  got: {:?}\n  want: {:?}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(expected),
        );
    }
}

/// Run `wasmtime -W gc=y <wasm_path>` and return its raw `Output`.
/// Returns `None` with a stderr warning when `wasmtime` isn't on
/// `$PATH`; panics if `PHOENIX_REQUIRE_WASMTIME=1`. Callers decide what
/// exit status to expect — [`run_under_wasmtime_gc`] requires success,
/// while [`assert_traps`] requires a non-zero (trap) exit.
fn wasmtime_output(bytes: &[u8], label: &str) -> Option<std::process::Output> {
    // Quick liveness probe so the helpful error fires here rather
    // than as a confusing "could not spawn" further down.
    if Command::new("wasmtime").arg("--version").output().is_err() {
        if require_wasmtime() {
            panic!("PHOENIX_REQUIRE_WASMTIME=1 set but `wasmtime` is not on PATH");
        }
        eprintln!("warning: skipping wasmtime execution for {label} — `wasmtime` not on PATH");
        return None;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("{label}.wasm"));
    std::fs::write(&path, bytes).expect("write wasm");
    let out = Command::new("wasmtime")
        .args(["-W", "gc=y"])
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("invoke wasmtime");
    Some(out)
}

/// Run `wasmtime -W gc=y <wasm_path>` and return its stdout, asserting a
/// clean (zero) exit. Returns `None` with a stderr warning when
/// `wasmtime` isn't on `$PATH`; panics if `PHOENIX_REQUIRE_WASMTIME=1`.
fn run_under_wasmtime_gc(bytes: &[u8], label: &str) -> Option<Vec<u8>> {
    let out = wasmtime_output(bytes, label)?;
    assert!(
        out.status.success(),
        "wasmtime exited non-zero for {label}: status={:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    Some(out.stdout)
}

/// Compile `source`, structurally validate it, and — when wasmtime is
/// available — assert that running it *traps* (exits non-zero). Pins the
/// runtime trap semantics of `i64.div_s` / `i64.rem_s` (zero divisor and
/// the `i64::MIN / -1` overflow case), which structural validation alone
/// cannot prove. Like [`assert_prints`], the trap check only bites when
/// wasmtime actually runs — in a wasmtime-less environment it degrades to
/// structural validation only.
fn assert_traps(source: &str, label: &str) {
    let bytes = compile_to_wasm_gc(source);
    validate_gc_module(&bytes, label);
    if let Some(out) = wasmtime_output(&bytes, label) {
        assert!(
            !out.status.success(),
            "{label}: expected a runtime trap (non-zero exit), but wasmtime \
             exited cleanly\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

/// `hello.phx` — PR 5 slice 1's gate. The fixture binds an `Int`
/// literal to an immutable local and prints it. Exercises the full
/// minimal pipeline:
///
/// - `Op::ConstI64` lowering to `i64.const + local.set`. The `let x`
///   here is immutable, so it binds the `ConstI64` SSA value directly
///   — no `Op::Alloca` / `Store` / `Load` is emitted (that trio is the
///   *mutable* `let mut` shape, covered by
///   [`mutable_let_runs_under_wasmtime_gc`]).
/// - `Op::BuiltinCall("print", Int)` routed to the synthesized
///   `phx_print_i64` helper.
/// - `_start` → user `main` plumbing and the WASI `fd_write` import.
///   (No `proc_exit` import yet — `_start` returns normally; panic
///   routing through `proc_exit` lands in a later slice.)
/// - The codegen-synthesized `phx_print_i64` digit-conversion path:
///   write `\n`, then digits backward, then assemble the iovec entry
///   and call `fd_write`.
///
/// Expected stdout: `42\n`.
#[test]
fn hello_runs_under_wasmtime_gc() {
    // Structural validation (inside `assert_prints`) always runs. The
    // GC proposal is enabled so any GC-typed declarations parse
    // correctly (today's MVP doesn't declare any, but the validator is
    // forward-compatible).
    assert_prints(
        "function main() {\n  let x: Int = 42\n  print(x)\n}\n",
        "hello_wasm_gc",
        b"42\n",
    );
}

/// `fibonacci.phx`. Exercises:
///
/// - **Integer arithmetic and comparison.** `n <= 1` lowers to
///   `Op::ILe`; `n - 1` / `n - 2` to `Op::ISub`; the recursive sum
///   to `Op::IAdd`. Each pair routes through the shared
///   `emit_i64_binop` / `emit_i64_cmp` helpers.
/// - **Direct recursive call.** `fib(n - 1) + fib(n - 2)` emits two
///   `Op::Call(fib, _, _)` instructions that resolve through
///   `phx_user_funcs` to the same `wasm_idx` — the function's own
///   declaration must land before any body is emitted (the standard
///   declare-then-emit pipeline). A regression that lowered the
///   call before the function index was reserved would crash the
///   codegen at the call site.
/// - **Multi-block control flow.** The `if n <= 1 { return n }` /
///   fall-through pair lowers to a multi-block CFG: an entry block that
///   `Branch`es on `n <= 1`, a true-arm block that `return n`s, and a
///   fall-through continuation block that computes the recursive sum
///   and returns it. Because the true arm diverges (returns), the
///   continuation carries no block params. The loop+switch dispatcher
///   routes between them; `Terminator::Branch` sets the dispatch local
///   and `br`s back to the outer loop.
/// - **Value-returning Return.** Both `return n` and the
///   fall-through `fib(n-1) + fib(n-2)` use `Terminator::Return(Some(v))`,
///   which pushes the value local and emits an explicit `return`.
///
/// Expected stdout: `0\n1\n5\n55\n` (fib(0), fib(1), fib(5), fib(10)).
#[test]
fn fibonacci_runs_under_wasmtime_gc() {
    let source = "function fib(n: Int) -> Int {\n  \
                    if n <= 1 { return n }\n  \
                    fib(n - 1) + fib(n - 2)\n\
                  }\n\
                  function main() {\n  \
                    print(fib(0))\n  \
                    print(fib(1))\n  \
                    print(fib(5))\n  \
                    print(fib(10))\n\
                  }\n";
    assert_prints(source, "fibonacci_wasm_gc", b"0\n1\n5\n55\n");
}

/// Exercises every arithmetic / comparison / bool op the slice-2 op
/// surface added that `fibonacci` does *not* reach. `fibonacci` only
/// covers `IAdd` / `ISub` / `ILe` / `Op::Call`, so without this a
/// transposed instruction mapping (e.g. `I64LtS` ↔ `I64GtS`, or
/// `I64DivS` ↔ `I64RemS`) would compile and validate but produce wrong
/// output, and no test would catch it. Each result is funneled through
/// `print(Int)` so the execution tier verifies the actual value.
///
/// NOTE: a transposed-but-valid opcode passes structural validation, so
/// this guarantee only holds when wasmtime actually runs the module. In
/// a wasmtime-less environment `assert_prints` degrades to validation
/// only (see [`assert_wasm_prints`]); CI must set
/// `PHOENIX_REQUIRE_WASMTIME=1` (or have `wasmtime` on `$PATH`) for the
/// value-level check to be enforced.
///
/// - **Arithmetic:** `IMul` (`6 * 7`), `IDiv` (`20 / 6`, signed
///   truncation), `IMod` (`20 % 6`), `INeg` (`neg(5)` → unary `-`).
/// - **Int comparisons → Bool:** `IEq`, `INe`, `ILt`, `IGt`, `IGe`
///   (each routed through an `if` so the Bool result drives control
///   flow and surfaces as `1`/`0`).
/// - **Bool ops:** `BoolEq` / `BoolNe` (`p == q` / `p != q` on two
///   Bool locals) and `BoolNot` (`!p`).
///
/// Expected stdout: `42\n3\n2\n-5\n1\n1\n1\n1\n1\n0\n1\n1\n`.
#[test]
fn arithmetic_and_comparisons_run_under_wasmtime_gc() {
    let source = concat!(
        "function neg(x: Int) -> Int { -x }\n",
        "function eq(a: Int, b: Int) -> Int { if a == b { return 1 } return 0 }\n",
        "function ne(a: Int, b: Int) -> Int { if a != b { return 1 } return 0 }\n",
        "function lt(a: Int, b: Int) -> Int { if a < b { return 1 } return 0 }\n",
        "function gt(a: Int, b: Int) -> Int { if a > b { return 1 } return 0 }\n",
        "function ge(a: Int, b: Int) -> Int { if a >= b { return 1 } return 0 }\n",
        "function beq(a: Int, b: Int) -> Int {\n",
        "  let p = a < b\n",
        "  let q = a > b\n",
        "  if p == q { return 1 }\n",
        "  return 0\n",
        "}\n",
        "function bne(a: Int, b: Int) -> Int {\n",
        "  let p = a < b\n",
        "  let q = a > b\n",
        "  if p != q { return 1 }\n",
        "  return 0\n",
        "}\n",
        "function bnot(a: Int, b: Int) -> Int {\n",
        "  let p = a < b\n",
        "  if !p { return 1 }\n",
        "  return 0\n",
        "}\n",
        "function main() {\n",
        "  print(6 * 7)\n",      // 42  IMul
        "  print(20 / 6)\n",     // 3   IDiv
        "  print(20 % 6)\n",     // 2   IMod
        "  print(neg(5))\n",     // -5  INeg
        "  print(eq(3, 3))\n",   // 1  IEq
        "  print(ne(3, 4))\n",   // 1  INe
        "  print(lt(2, 5))\n",   // 1  ILt
        "  print(gt(5, 2))\n",   // 1  IGt
        "  print(ge(5, 5))\n",   // 1  IGe
        "  print(beq(1, 2))\n",  // p=true, q=false → p==q false → 0  BoolEq
        "  print(bne(1, 2))\n",  // p=true, q=false → p!=q true  → 1  BoolNe
        "  print(bnot(5, 2))\n", // p=(5<2)=false → !p true     → 1  BoolNot
        "}\n",
    );
    assert_prints(
        source,
        "arithmetic_and_comparisons_wasm_gc",
        b"42\n3\n2\n-5\n1\n1\n1\n1\n1\n0\n1\n1\n",
    );
}

/// Pins the *signed* semantics of `IDiv` / `IMod` with negative
/// operands — the case `arithmetic_and_comparisons` (positive-only:
/// `20 / 6`, `20 % 6`) cannot distinguish. `Op::IDiv` must lower to
/// `i64.div_s` (not `div_u`) and `Op::IMod` to `i64.rem_s` (not
/// `rem_u`); a transposition to the unsigned opcode reinterprets the
/// negative dividend as a huge positive and would surface here, but
/// only when wasmtime actually runs (signed and unsigned opcodes both
/// validate). Operands are passed through function params so the ops
/// are emitted at the call site rather than potentially observed as
/// literals.
///
/// WASM signed semantics: division truncates toward zero, and the
/// remainder takes the sign of the dividend. So:
/// - `-7 / 2 = -3`, `-7 % 2 = -1`
/// - `7 / -2 = -3`, `7 % -2 = 1`
///
/// Expected stdout: `-3\n-1\n-3\n1\n`.
#[test]
fn signed_div_mod_runs_under_wasmtime_gc() {
    let source = concat!(
        "function dv(a: Int, b: Int) -> Int { a / b }\n",
        "function md(a: Int, b: Int) -> Int { a % b }\n",
        "function main() {\n",
        "  print(dv(-7, 2))\n", // -3  signed truncation toward zero
        "  print(md(-7, 2))\n", // -1  remainder takes dividend's sign
        "  print(dv(7, -2))\n", // -3
        "  print(md(7, -2))\n", // 1
        "}\n",
    );
    assert_prints(source, "signed_div_mod_wasm_gc", b"-3\n-1\n-3\n1\n");
}

/// Locks the documented runtime trap contract for `i64.div_s` (see the
/// `Op::IDiv` lowering comment in `translate.rs`): dividing by zero
/// traps. The divisor reaches `dv` as a parameter, so the `i64.div_s`
/// instruction is genuinely emitted (not folded away), and a `0`
/// divisor at runtime aborts the instance with a non-zero exit. A
/// regression that, say, guarded the divide or swapped in a
/// non-trapping opcode would let the program exit cleanly and fail this
/// test (under wasmtime).
#[test]
fn divide_by_zero_traps_under_wasmtime_gc() {
    let source = concat!(
        "function dv(a: Int, b: Int) -> Int { a / b }\n",
        "function main() {\n",
        "  print(dv(1, 0))\n",
        "}\n",
    );
    assert_traps(source, "divide_by_zero_wasm_gc");
}

/// Pins the *second* trap case of `i64.div_s` that the `Op::IDiv`
/// comment calls out: the signed-overflow case `i64::MIN / -1` (the
/// mathematical result `2^63` is not representable in `i64`, so the WASM
/// runtime traps). `divide_by_zero` only covers the zero-divisor case;
/// without this, a regression that emitted a non-trapping/unsigned
/// opcode for `IDiv` would still satisfy the zero-divisor test yet
/// silently wrap here.
///
/// `i64::MIN` can't be written as a literal — the lexer parses the
/// magnitude `9223372036854775808` as an `i64` first, which overflows —
/// so `imin()` synthesizes it via wrapping arithmetic: `-i64::MAX - 1`.
/// Both operands reach `dv` through params, so the `i64.div_s` is
/// genuinely emitted at the call site.
#[test]
fn min_div_neg_one_traps_under_wasmtime_gc() {
    let source = concat!(
        "function dv(a: Int, b: Int) -> Int { a / b }\n",
        "function imin() -> Int { -9223372036854775807 - 1 }\n",
        "function main() {\n",
        "  print(dv(imin(), -1))\n",
        "}\n",
    );
    assert_traps(source, "min_div_neg_one_wasm_gc");
}

/// The companion to `min_div_neg_one_traps`: `i64::MIN % -1` does *not*
/// trap under `i64.rem_s` — the WASM spec defines it to yield `0` (the
/// overflowing quotient is irrelevant to the remainder). This pins the
/// asymmetry documented in the `Op::IMod` comment: `rem_s` traps only on
/// a zero divisor, unlike `div_s`. A regression that "hardened" `IMod`
/// by trapping on the `MIN % -1` overflow (mistakenly mirroring `div_s`)
/// would turn this clean `0` into a non-zero exit and fail here.
///
/// `imin()` synthesizes `i64::MIN` the same way as
/// `min_div_neg_one_traps` (see its note on the literal-overflow
/// constraint). Expected stdout: `0\n`.
#[test]
fn min_mod_neg_one_yields_zero_under_wasmtime_gc() {
    let source = concat!(
        "function md(a: Int, b: Int) -> Int { a % b }\n",
        "function imin() -> Int { -9223372036854775807 - 1 }\n",
        "function main() {\n",
        "  print(md(imin(), -1))\n",
        "}\n",
    );
    assert_prints(source, "min_mod_neg_one_wasm_gc", b"0\n");
}

/// Exercises **block-argument copies** — the one piece of the slice-2
/// dispatcher that `fibonacci` and `arithmetic_and_comparisons` leave
/// untouched (their join blocks take zero params, so the copy loop in
/// `emit_block_param_copies` never runs). A value-producing `if`/`else`
/// expression lowers to a merge block carrying an `Int` param, and each
/// arm `Jump`s to it with `args: [val]` (see `lower_if`). The dispatcher
/// must allocate a local for that param and, on each arm, copy the arm's
/// value into it before `br`-ing back to the loop. A bug in that copy
/// (wrong dest local, dropped arg, or off-by-one in the records ↔ args
/// zip) would surface as a wrong printed value here.
///
/// `classify(n)` returns `100` when `n < 0` and `200` otherwise; both
/// constants flow through the shared merge-block param. NOTE: like the
/// arithmetic test, the value-level check only bites when wasmtime runs
/// (validation alone accepts a wrong-but-valid copy).
///
/// Expected stdout: `100\n200\n`.
#[test]
fn if_expression_value_runs_under_wasmtime_gc() {
    let source = concat!(
        "function classify(n: Int) -> Int {\n",
        "  let label = if n < 0 { 100 } else { 200 }\n",
        "  label\n",
        "}\n",
        "function main() {\n",
        "  print(classify(-3))\n", // 100  (n < 0 arm → merge param = 100)
        "  print(classify(7))\n",  // 200  (else arm    → merge param = 200)
        "}\n",
    );
    assert_prints(source, "if_expression_value_wasm_gc", b"100\n200\n");
}

/// Exercises a **multi-parameter, permuting block-argument copy** with a
/// genuine source↔destination aliasing hazard — the exact case
/// `emit_block_param_copies` adopts its push-all-then-pop-in-reverse
/// shape to survive, but which no other fixture reaches.
/// `if_expression_value` copies only a *single* merge param, and every
/// other multi-block test has zero-param joins, so a regression to a
/// naive per-pair copy (`local.get src_i; local.set dst_i`) would pass
/// all of them yet silently corrupt this swap.
///
/// The hazard: a loop-header block `Branch`es back to *itself* with its
/// two `Int` params swapped (`true_args = [b, a]`). Because the back-edge
/// targets the same block, the copy's sources and destinations are the
/// *same* locals. A sequential copy would write `a`'s local before
/// reading it (`a ← b; b ← a` collapses both to `b`); the parallel-safe
/// copy reads both onto the stack before writing either, so the swap
/// lands correctly.
///
/// Hand-built (not lowered from source): no Phoenix surface syntax emits
/// a self-swapping loop header today, so the fixture mints the CFG
/// directly — same approach as [`print_int_module`].
///
/// CFG (`main`, with `blocks[i].id == BlockId(i)`):
/// - bb0 (entry): `a=10`, `b=20`, `n=0`; `Jump bb1 [a, b, n]`.
/// - bb1 (header, params `a,b,n`): while `n < 1`, `Branch` back to bb1
///   with `[b, a, n+1]` (the swap on the back-edge); else `Branch` to bb2
///   with `[a, b]`.
/// - bb2 (exit, params `x,y`): `print(x)`, `print(y)`, return.
///
/// One iteration swaps `(10,20)` → `(20,10)` and bumps `n` to `1`, so the
/// second header test fails `n < 1` and falls through to bb2. Expected
/// stdout: `20\n10\n` — NOT `20\n20\n`, which a clobbering copy would
/// produce. Like the other value-level checks, only enforced when
/// wasmtime actually runs.
#[test]
fn swapping_block_params_runs_under_wasmtime_gc() {
    let mut func = IrFunction::new(
        FuncId(0),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let bb0 = func.create_block();
    let bb1 = func.create_block();
    let bb2 = func.create_block();

    // Header (bb1) carries the swapped pair plus the iteration counter;
    // the exit (bb2) receives the final pair. Entry (bb0) takes no params
    // — it seeds the values inline.
    let a = func.add_block_param(bb1, IrType::I64);
    let b = func.add_block_param(bb1, IrType::I64);
    let n = func.add_block_param(bb1, IrType::I64);
    let x = func.add_block_param(bb2, IrType::I64);
    let y = func.add_block_param(bb2, IrType::I64);

    // bb0: seed the loop-carried values and enter the header.
    let a0 = func.emit_value(bb0, Op::ConstI64(10), IrType::I64, None);
    let b0 = func.emit_value(bb0, Op::ConstI64(20), IrType::I64, None);
    let n0 = func.emit_value(bb0, Op::ConstI64(0), IrType::I64, None);
    func.set_terminator(
        bb0,
        Terminator::Jump {
            target: bb1,
            args: vec![a0, b0, n0],
        },
    );

    // bb1: while `n < 1`, swap `(a, b)` and bump `n` on a self back-edge.
    // `true_args = [b, a, n_next]` references bb1's *own* param values, so
    // the copy's sources alias its destinations — the swap hazard.
    let one = func.emit_value(bb1, Op::ConstI64(1), IrType::I64, None);
    let cond = func.emit_value(bb1, Op::ILt(n, one), IrType::Bool, None);
    let n_next = func.emit_value(bb1, Op::IAdd(n, one), IrType::I64, None);
    func.set_terminator(
        bb1,
        Terminator::Branch {
            condition: cond,
            true_block: bb1,
            true_args: vec![b, a, n_next],
            false_block: bb2,
            false_args: vec![a, b],
        },
    );

    // bb2: print the (swapped) pair and return.
    func.emit(
        bb2,
        Op::BuiltinCall("print".to_string(), vec![x]),
        IrType::Void,
        None,
    );
    func.emit(
        bb2,
        Op::BuiltinCall("print".to_string(), vec![y]),
        IrType::Void,
        None,
    );
    func.set_terminator(bb2, Terminator::Return(None));

    let mut module = IrModule::new();
    module.push_concrete(func);

    let bytes = compile(&module, Target::Wasm32Gc).unwrap_or_else(|e| {
        panic!("wasm32-gc compile of swapping-block-params fixture failed: {e}")
    });
    assert_wasm_prints(&bytes, "swapping_block_params_wasm_gc", b"20\n10\n");
}

/// `print(0)` exercises the `phx_print_i64` zero branch (`n == 0 →
/// '0'`), which the multi-digit `hello` case never reaches.
#[test]
fn print_zero_runs_under_wasmtime_gc() {
    assert_prints(
        "function main() {\n  let x: Int = 0\n  print(x)\n}\n",
        "print_zero_wasm_gc",
        b"0\n",
    );
}

/// A long multi-digit value exercises many iterations of the
/// digit-conversion loop and the `i64`→ASCII remainder math, well past
/// `hello`'s two digits.
#[test]
fn print_large_runs_under_wasmtime_gc() {
    assert_prints(
        "function main() {\n  let x: Int = 1234567890\n  print(x)\n}\n",
        "print_large_wasm_gc",
        b"1234567890\n",
    );
}

/// A `main` that never calls `print` exercises the no-print compile
/// path, which is *structurally distinct* from every other success case:
/// `module_calls_print` returns `false`, so `declare_imports` and
/// `declare_print_helper` are both skipped. That leaves `import_func_count`
/// at `0`, which shifts every Phoenix function's WASM index down by one
/// (no `fd_write` import sits at index 0) and changes the `_start → main`
/// call target. All the other passing tests print, so they only ever
/// cover the *with*-import index arithmetic; this case locks the
/// no-import branch so a regression in `add_local_function` /
/// `declare_start` can't slip through. Empty stdout: the body is a bare
/// `Return(None)`.
#[test]
fn empty_main_with_no_print_compiles_and_runs() {
    assert_prints("function main() {\n}\n", "empty_main_wasm_gc", b"");
}

/// A *mutable* `let mut` is the only shape that emits the `Op::Alloca` /
/// `Op::Store` / `Op::Load` trio — an immutable `let` (as in `hello.phx`)
/// binds its initializer's SSA value directly and never reaches it. This
/// fixture allocates a mutable `Int` slot, stores an initial value,
/// reassigns it (a second `Op::Store`), then reads it back (`Op::Load`)
/// to print — so it drives every arm of that trio, which `hello.phx`
/// leaves unexercised. The reassignment to `42` (distinct from the
/// initial `1`) proves the store-then-load round-trips the new value
/// rather than the initializer.
///
/// Expected stdout: `42\n`.
#[test]
fn mutable_let_runs_under_wasmtime_gc() {
    assert_prints(
        "function main() {\n  let mut x: Int = 1\n  x = 42\n  print(x)\n}\n",
        "mutable_let_wasm_gc",
        b"42\n",
    );
}

/// `print(Bool)` is accepted by sema (the `print` built-in is
/// type-unconstrained) but slice 1 only synthesizes `phx_print_i64`.
/// The backend must reject a non-`Int` argument with a clear diagnostic
/// rather than emit a `Call` against a mismatched signature — which
/// would produce a structurally invalid module. Locks in the
/// `translate_print` type guard.
#[test]
fn print_bool_is_rejected_until_a_later_slice() {
    let ir_module = lower_to_ir("function main() {\n  print(true)\n}\n");
    let err = compile(&ir_module, Target::Wasm32Gc)
        .expect_err("print(Bool) should not compile under wasm32-gc slice 1");
    let msg = err.to_string();
    assert!(
        msg.contains("print") && msg.contains("Int"),
        "expected a print/Int diagnostic, got: {msg}"
    );
}

/// Multi-block control flow (here, a trivial `if true { print(1) }`)
/// now lands cleanly through the loop+switch dispatcher (PR 5 slice 2).
/// Pinning a positive test rather than the old slice-1 rejection so a
/// regression that re-introduced the single-block guard would surface
/// here. Expected output: `1` (the `if true` arm runs, printing 1).
#[test]
fn multi_block_function_runs_under_wasmtime_gc() {
    assert_prints(
        "function main() {\n  if true {\n    print(1)\n  }\n}\n",
        "multi_block_if_true",
        b"1\n",
    );
}

/// A program with no `main` reaches the backend: sema does not *require*
/// an entry point (it only constrains where `main` may be declared), so
/// the wasm32-gc module builder is the layer that must reject it. Locks in
/// the `declare_phoenix_functions` "no `main`" guard, which is reachable
/// from ordinary user source.
#[test]
fn missing_main_is_rejected() {
    let ir_module = lower_to_ir("function helper() {\n}\n");
    let err = compile(&ir_module, Target::Wasm32Gc)
        .expect_err("a program with no `main` should not compile under wasm32-gc");
    let msg = err.to_string();
    assert!(
        msg.contains("main"),
        "expected a missing-`main` diagnostic, got: {msg}"
    );
}

/// `main` with a non-`Void` return type passes sema but cannot be wired to
/// the synthesized `_start` (typed `[] -> []`, which discards no value).
/// The backend must reject it before emitting a structurally invalid
/// `_start` that leaves an operand on the stack. Locks in the
/// `declare_phoenix_functions` return-type guard — also reachable from
/// ordinary user source.
#[test]
fn main_returning_non_void_is_rejected() {
    let ir_module = lower_to_ir("function main() -> Int {\n  return 0\n}\n");
    let err = compile(&ir_module, Target::Wasm32Gc)
        .expect_err("`main` returning non-Void should not compile under wasm32-gc");
    let msg = err.to_string();
    assert!(
        msg.contains("main") && msg.contains("Void"),
        "expected a `main`/`Void` diagnostic, got: {msg}"
    );
}

/// Hand-build the IR for `function main() { print(n) }` with a literal
/// `Op::ConstI64(n)`, bypassing the front end.
///
/// Why hand-build: a source-level negative integer lowers to `Op::INeg`
/// of a positive `ConstI64`, and `INeg` (like all arithmetic) is outside
/// slice 1's op surface — so the only way to route a negative `Int` into
/// `phx_print_i64` today is to mint the `ConstI64(n)` directly. An
/// immutable `let` binds its initializer's SSA value directly (no
/// `Alloca`/`Store`/`Load`), so this two-instruction body is exactly what
/// `let x: Int = n; print(x)` would lower to if `n` could be negative.
fn print_int_module(n: i64) -> IrModule {
    let mut func = IrFunction::new(
        FuncId(0),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let entry = func.create_block();
    let value = func.emit_value(entry, Op::ConstI64(n), IrType::I64, None);
    func.emit(
        entry,
        Op::BuiltinCall("print".to_string(), vec![value]),
        IrType::Void,
        None,
    );
    func.set_terminator(entry, Terminator::Return(None));

    let mut module = IrModule::new();
    module.push_concrete(func);
    module
}

/// Pins the lowering of `Terminator::Unreachable` to a WASM `unreachable`
/// instruction (which traps at runtime). No Phoenix surface syntax emits
/// this terminator today — the front end never produces an `unreachable`
/// block exit — so it ships unexercised unless built straight from IR,
/// hand-built here like [`print_int_module`].
///
/// `main` prints a sentinel (proving the block body executes up to the
/// terminator) and then exits via `Unreachable`. A successful compile
/// plus a runtime trap together confirm the lowering: a regression that
/// dropped the terminator or emitted a fall-through would let `main` exit
/// cleanly and fail the trap assertion. Like the other trap tests, the
/// runtime check only bites when wasmtime is on `$PATH`; otherwise it
/// degrades to structural validation.
#[test]
fn unreachable_terminator_traps_under_wasmtime_gc() {
    let mut func = IrFunction::new(
        FuncId(0),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let entry = func.create_block();
    let value = func.emit_value(entry, Op::ConstI64(7), IrType::I64, None);
    func.emit(
        entry,
        Op::BuiltinCall("print".to_string(), vec![value]),
        IrType::Void,
        None,
    );
    func.set_terminator(entry, Terminator::Unreachable);

    let mut module = IrModule::new();
    module.push_concrete(func);

    let label = "unreachable_terminator_wasm_gc";
    let bytes = compile(&module, Target::Wasm32Gc)
        .unwrap_or_else(|e| panic!("wasm32-gc compile of unreachable fixture failed: {e}"));
    validate_gc_module(&bytes, label);
    if let Some(out) = wasmtime_output(&bytes, label) {
        assert!(
            !out.status.success(),
            "{label}: expected a runtime trap from `unreachable`, but wasmtime \
             exited cleanly\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

/// The negative / `is_neg` branch of `phx_print_i64`: write the digits
/// backward, then prepend `'-'`. Unreachable from slice-1 *source* (a
/// source `-7` needs `Op::INeg`, deferred to slice 2), so the body is
/// built straight from IR via [`print_int_module`]. Covers a single-digit
/// negative (the minimal sign path) and a long negative (sign path plus
/// many digit-loop iterations) so the hand-written sign emission doesn't
/// ship unexercised until arithmetic lands.
#[test]
fn print_negative_runs_under_wasmtime_gc() {
    for (n, want) in [
        (-7_i64, &b"-7\n"[..]),
        (-1234567890_i64, &b"-1234567890\n"[..]),
    ] {
        let module = print_int_module(n);
        let bytes = compile(&module, Target::Wasm32Gc)
            .unwrap_or_else(|e| panic!("wasm32-gc compile of print({n}) failed: {e}"));
        assert_wasm_prints(&bytes, &format!("print_negative_{n}_wasm_gc"), want);
    }
}
