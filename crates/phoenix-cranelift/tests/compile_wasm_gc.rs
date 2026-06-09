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

/// Count the WASM-GC `struct.new` / `struct.get` / `struct.set`
/// operators emitted across every function body in `bytes`, returned
/// as `(new, get, set)`.
///
/// The struct tests pair this with [`assert_wasm_prints`] so that the
/// presence of the three struct ops is asserted *structurally* —
/// independent of whether the wasmtime execution tier runs. Structural
/// validation alone only proves the module parses; in a wasmtime-less
/// environment it would not catch a regression that dropped the
/// `struct.set` lowering, since the field-mutation behavior is only
/// observable at runtime. Counting the operators closes that gap.
fn count_struct_ops(bytes: &[u8]) -> (usize, usize, usize) {
    use wasmparser::{Operator, Parser, Payload};
    let (mut new, mut get, mut set) = (0, 0, 0);
    for payload in Parser::new(0).parse_all(bytes) {
        if let Payload::CodeSectionEntry(body) = payload.expect("parse wasm payload") {
            let reader = body.get_operators_reader().expect("operators reader");
            for op in reader {
                match op.expect("decode operator") {
                    Operator::StructNew { .. } => new += 1,
                    Operator::StructGet { .. } => get += 1,
                    Operator::StructSet { .. } => set += 1,
                    _ => {}
                }
            }
        }
    }
    (new, get, set)
}

/// Count the WASM-GC `(struct …)` type declarations in the module's
/// type section. Used to assert §Phase 2.4 decision K.1's nominal
/// mapping — *one distinct WASM struct type per Phoenix struct* — so a
/// regression that deduped two same-shape structs into a single WASM
/// type (structural sharing, which K.1 explicitly rejects) is caught.
fn count_struct_type_decls(bytes: &[u8]) -> usize {
    use wasmparser::{CompositeInnerType, Parser, Payload};
    let mut structs = 0;
    for payload in Parser::new(0).parse_all(bytes) {
        if let Payload::TypeSection(reader) = payload.expect("parse wasm payload") {
            for rec_group in reader {
                for sub_ty in rec_group.expect("rec group").types() {
                    if matches!(sub_ty.composite_type.inner, CompositeInnerType::Struct(_)) {
                        structs += 1;
                    }
                }
            }
        }
    }
    structs
}

/// Count the §Phase 2.4 K.4 enum type declarations by their subtype
/// role, returning `(parents, variants)`. Lets the enum gates assert
/// the parent + per-variant hierarchy was emitted *structurally* —
/// independent of whether the wasmtime execution tier runs. The
/// `TypeInterner` encodes the three struct flavors distinctly:
/// - **enum parent** — `(sub (struct …))`, non-final, no supertype;
/// - **enum variant** — final struct *with* a supertype (its parent);
/// - **regular struct / `$string`** — final struct, no supertype.
///
/// So a struct with a supertype is a variant, a non-final struct
/// without one is a parent, and everything else (plain structs,
/// `$string`) falls into neither bucket. A regression that dropped the
/// variant subtypes or flattened the hierarchy changes these counts
/// even on a machine with no wasmtime.
fn count_enum_type_decls(bytes: &[u8]) -> (usize, usize) {
    use wasmparser::{CompositeInnerType, Parser, Payload};
    let (mut parents, mut variants) = (0, 0);
    for payload in Parser::new(0).parse_all(bytes) {
        if let Payload::TypeSection(reader) = payload.expect("parse wasm payload") {
            for rec_group in reader {
                for sub_ty in rec_group.expect("rec group").types() {
                    if !matches!(sub_ty.composite_type.inner, CompositeInnerType::Struct(_)) {
                        continue;
                    }
                    if sub_ty.supertype_idx.is_some() {
                        variants += 1;
                    } else if !sub_ty.is_final {
                        parents += 1;
                    }
                }
            }
        }
    }
    (parents, variants)
}

/// Count `ref.cast` (non-null) operators across every function body.
/// On wasm32-gc only `Op::EnumGetField` emits a `ref.cast` (it narrows
/// the parent-typed receiver to the concrete variant before the field
/// load; struct field reads use the binding's concrete type directly,
/// no cast). So a non-zero count proves the `EnumGetField` lowering is
/// present even when wasmtime isn't available to observe the field
/// value — closing the same gap `count_struct_ops` closes for structs.
fn count_ref_cast_ops(bytes: &[u8]) -> usize {
    use wasmparser::{Operator, Parser, Payload};
    let mut casts = 0;
    for payload in Parser::new(0).parse_all(bytes) {
        if let Payload::CodeSectionEntry(body) = payload.expect("parse wasm payload") {
            let reader = body.get_operators_reader().expect("operators reader");
            for op in reader {
                if matches!(
                    op.expect("decode operator"),
                    Operator::RefCastNonNull { .. }
                ) {
                    casts += 1;
                }
            }
        }
    }
    casts
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

/// NaN and ±inf special cases. Each prints byte-for-byte with native's
/// post-amendment `format_f64` (ryu) (see §Phase 2.4 K.6 2026-06-09):
/// - `f64::NAN` → `"NaN"`
/// - `f64::INFINITY` → `"inf"`
/// - `f64::NEG_INFINITY` → `"-inf"`
///
/// `-0.0` is **not** asserted here. Under ryu it prints as `"-0.0"`,
/// which the Phase-2 d2s general case will emit; the pre-amendment
/// Phase-1 `-0.0 → "-0"` literal is gone with the integer fast-path
/// that produced it.
///
/// Source-level Phoenix can't easily produce NaN/Infinity, so the
/// fixture computes them: `0.0 / 0.0` for NaN, `±1.0 / 0.0` for ±inf.
/// These flow through the synthesized helper at runtime, exercising
/// the IEEE-754 branches under wasmtime.
#[test]
fn print_float_phase1_special_cases_run_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let nan: Float = 0.0 / 0.0\n",
        "  let pinf: Float = 1.0 / 0.0\n",
        "  let ninf: Float = -1.0 / 0.0\n",
        "  print(nan)\n",
        "  print(pinf)\n",
        "  print(ninf)\n",
        "}\n",
    );
    assert_prints(source, "print_float_phase1_specials", b"NaN\ninf\n-inf\n");
}

/// Every finite float — non-integer, integer-valued, ±0.0 — traps at
/// runtime via `unreachable` until the Ryu d2s general case lands
/// (Phase 2). The pre-amendment Phase 1 special-cased `±0.0` and
/// integer-valued in-i64-range floats via the integer fast-path; both
/// are removed under K.6 2026-06-09 (the fast-path emitted `"5"` where
/// ryu emits `"5.0"`; `-0.0` emitted `"-0"` where ryu emits `"-0.0"`).
///
/// Three sub-cases pin all three trap paths:
/// - non-integer (`1.5`) — original Phase-1 trap, unchanged
/// - integer-valued in i64 range (`5.0`) — was fast-path, now traps
/// - integer-valued *out of* i64 range (`1e20`-longhand) — also traps
///
/// `-0.0` would trap here too but is covered by its own test below
/// (the `-1.0 * 0.0` runtime computation makes its sign bit traceable
/// through the helper, which a constant `-0.0` literal would not — the
/// lexer/parser may fold `-0.0` to `0.0` at literal time).
#[test]
fn print_float_phase1_finite_traps_until_phase2() {
    assert_traps(
        "function main() {\n  print(1.5)\n}\n",
        "print_float_phase1_traps_non_integer",
    );
    assert_traps(
        "function main() {\n  print(5.0)\n}\n",
        "print_float_phase1_traps_integer_in_range",
    );
    assert_traps(
        "function main() {\n  print(100000000000000000000.0)\n}\n",
        "print_float_phase1_traps_out_of_range",
    );
}

/// `-0.0` (computed via `-1.0 * 0.0` so its sign bit reaches the
/// helper at runtime — Phoenix has no constant folding pass, so the
/// product is computed under wasmtime) traps in Phase 1. Under ryu
/// (K.6 2026-06-09) it will print `"-0.0"` once Phase 2 lands; the
/// pre-amendment Phase 1 emitted `"0"` via the i64 fast-path that
/// silently dropped the sign. Pinned separately from the
/// integer-fast-path trap to exercise the runtime-computed sign
/// genuinely reaching `phx_print_f64`.
#[test]
fn print_float_phase1_negative_zero_traps_until_phase2() {
    assert_traps(
        "function main() {\n  let neg_zero: Float = -1.0 * 0.0\n  print(neg_zero)\n}\n",
        "print_float_phase1_traps_negative_zero",
    );
}

/// PR 6 slice 4: Float scalar ops on wasm32-gc. Exercises every op
/// the slice adds, with all results funneled through `print(Bool)`
/// so the execution tier sees them (since `print(Float)` is still
/// carved out — that's the next slice). Per §Phase 2.4 decision K.5.
///
/// - **`Op::ConstF64`** — float literals materialize via `f64.const`.
/// - **F-arithmetic** (`FAdd` / `FSub` / `FMul` / `FDiv` / `FNeg`) —
///   `+ - * /` and unary `-`. WASM `f64.<op>` matches IEEE-754
///   semantics directly. The unary negation uses `f64.neg` (sign-bit
///   flip), not the `0 - x` trick the i64 path uses.
/// - **F-comparison** (`FEq` / `FNe` / `FLt` / `FGt` / `FLe` / `FGe`) —
///   WASM `f64.<cmp>` returns i32 0/1, exactly Phoenix's Bool rep.
///
/// Eleven `print(Bool)` assertions. The four arithmetic results
/// (`FAdd`/`FSub`/`FMul`/`FDiv`) and the `FNeg` result are each pinned
/// to their exact value with `==` (all five are exactly representable
/// in f64), so a wrong op output fails the test rather than slipping
/// past a loose inequality. The remaining assertions exercise every
/// comparison op at least once (`FNe`/`FLt`/`FGt`/`FLe`/`FGe`).
///
/// Expected stdout: `true\n` repeated 11 times (one per Bool assertion).
#[test]
fn float_scalar_ops_run_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let x: Float = 3.5\n",
        "  let y: Float = 2.0\n",
        "  let s: Float = x + y\n", // FAdd → 5.5
        "  let d: Float = x - y\n", // FSub → 1.5
        "  let p: Float = x * y\n", // FMul → 7.0
        "  let q: Float = x / y\n", // FDiv → 1.75
        "  let n: Float = -x\n",    // FNeg → -3.5
        "  print(s == 5.5)\n",      // FEq pins FAdd  → 5.5
        "  print(d == 1.5)\n",      // FEq pins FSub  → 1.5
        "  print(p == 7.0)\n",      // FEq pins FMul  → 7.0
        "  print(q == 1.75)\n",     // FEq pins FDiv  → 1.75
        "  print(n + x == 0.0)\n",  // FEq pins FNeg  → -3.5 (so -3.5 + 3.5 == 0.0)
        "  print(d != p)\n",        // FNe  → true (1.5 != 7.0)
        "  print(d < x)\n",         // FLt  → true (1.5 < 3.5)
        "  print(p > x)\n",         // FGt  → true (7.0 > 3.5)
        "  print(q <= 2.0)\n",      // FLe  → true (1.75 <= 2.0)
        "  print(p >= 7.0)\n",      // FGe  → true (7.0 >= 7.0)
        "  print(n < 0.0)\n",       // FLt on FNeg result → true (-3.5 < 0.0)
        "}\n",
    );
    assert_prints(
        source,
        "float_scalar_ops_wasm_gc",
        b"true\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\n",
    );
}

/// PR 6 slice 4: IEEE-754 edge cases the scalar-op comments promise but
/// the finite-literal test above can't reach — infinities and NaN. Per
/// §Phase 2.4 decision K.5 (`f64.div` does not trap on divide-by-zero).
///
/// Operands come from `let` bindings divided at runtime (`x / z`,
/// `z / z`) rather than constant literals, so the values are produced by
/// the emitted `f64.div` rather than folded away — this exercises the
/// real WASM semantics, not the frontend's constant evaluator.
///
/// - `x / z` with `z == 0.0` → `+inf` (FDiv, no trap).
/// - `z / z` → `NaN` (FDiv, no trap).
/// - `f64.neg(+inf)` → `-inf` (sign-bit flip, FNeg).
/// - NaN ordering: every *ordered* comparison (`<`, `>`) returns 0 when
///   an operand is NaN, `f64.eq(NaN, NaN)` returns 0, and
///   `f64.ne(NaN, _)` returns 1 — matching native Rust f64.
///
/// Six `print(Bool)` assertions, all `true` (the NaN-ordered checks are
/// negated with `!` so a correct `false` result prints `true`).
///
/// Expected stdout: `true\n` repeated 6 times.
#[test]
fn float_nan_and_infinity_run_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let x: Float = 3.5\n",
        "  let z: Float = 0.0\n",
        "  let pinf: Float = x / z\n", // FDiv → +inf
        "  let nan: Float = z / z\n",  // FDiv → NaN
        "  print(pinf > x)\n",         // +inf > 3.5 → true
        "  print(-pinf < x)\n",        // FNeg(+inf) = -inf < 3.5 → true
        "  print(nan != nan)\n",       // FNe with NaN → true
        "  print(!(nan == nan))\n",    // FEq with NaN → false, negated → true
        "  print(!(nan < x))\n",       // ordered FLt with NaN → false, negated → true
        "  print(!(nan > x))\n",       // ordered FGt with NaN → false, negated → true
        "}\n",
    );
    assert_prints(
        source,
        "float_nan_and_infinity_wasm_gc",
        b"true\ntrue\ntrue\ntrue\ntrue\ntrue\n",
    );
}

/// PR 6 slice 4: Float `%` (`Op::FMod`) is the one float-arithmetic op
/// this slice deliberately omits — WASM has no `f64.rem`, so it needs an
/// `fmod` runtime helper that lands in a later slice (§Phase 2.4 decision
/// K.5). The frontend *does* lower `Float % Float` → `Op::FMod`
/// (`lower_expr.rs`), so this pins the clean, specific rejection: the
/// backend names the missing `f64.rem` rather than falling through to the
/// generic "IR op not yet supported" catch-all. Tighten/flip this to a
/// positive execution test when the `fmod` helper lands.
#[test]
fn float_mod_is_rejected_until_the_fmod_helper_lands() {
    let ir_module = lower_to_ir(
        "function main() {\n  let a: Float = 5.5\n  let b: Float = 2.0\n  print((a % b) > 1.0)\n}\n",
    );
    let err = compile(&ir_module, Target::Wasm32Gc)
        .expect_err("Float `%` should not compile until the fmod helper lands");
    let msg = err.to_string();
    assert!(
        msg.contains("f64.rem") && msg.contains("FMod"),
        "expected a specific FMod/f64.rem diagnostic, got: {msg}"
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

/// PR 5 slice 3: structs land via `Op::StructAlloc` / `Op::StructGetField`
/// / `Op::StructSetField` lowered to WASM-GC `struct.new` / `struct.get`
/// / `struct.set`, against one nominal `(struct …)` type-section
/// declaration per Phoenix struct (per §Phase 2.4 decision K.1).
///
/// The fixture covers all three ops in one program:
///
/// - **Construction (`Op::StructAlloc`).** `Point(3, 7)` pushes both
///   field values, then `struct.new $Point` consumes them and produces
///   a `(ref $Point)` that the surrounding `Op::Store` writes into the
///   `let mut p` slot.
/// - **Field read (`Op::StructGetField`).** `p.x` / `p.y` each emit a
///   `local.get` on the slot followed by `struct.get $Point <idx>`.
///   The receiver is the nullable `(ref null $Point)` form (the
///   Alloca slot's WASM type); `struct.get` accepts nullable refs and
///   would trap on null — Phoenix's "no null structs" invariant
///   guarantees the store-before-read ordering that keeps this safe.
/// - **Field write (`Op::StructSetField`).** `p.x = 99` emits
///   `local.get $slot`, `local.get $val`, `struct.set $Point 0`.
///   Phoenix struct fields are mutable by default, so the WASM-side
///   `FieldType` is declared `mutable: true` for every field.
///
/// Also exercises the type-section ordering invariant: the struct must
/// be declared *before* `main`'s signature is interned (so any future
/// signature with a struct ref param/return encodes the right
/// `HeapType::Concrete(idx)`). The pipeline calls
/// `declare_phoenix_structs` before `declare_phoenix_functions`; a
/// reordering would emit a signature referencing an unallocated type
/// slot, which `wasmparser` would reject.
///
/// Expected stdout: `3\n7\n99\n`.
#[test]
fn struct_alloc_get_set_runs_under_wasmtime_gc() {
    // Pulled from the canonical fixture at compile time (mirroring the
    // linear backend's `include_str!` gate fixtures) so the test stays
    // locked to whatever `wasm_gc_struct.phx` says rather than a drifting
    // inline copy.
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/wasm_gc_struct.phx"
    ));
    let bytes = compile_to_wasm_gc(source);
    // Assert the struct ops are actually emitted, independent of the
    // wasmtime execution tier: structural validation alone would not
    // notice a regression that dropped `struct.set`, but the field
    // mutation it lowers (`p.x = 99` → `99`) is only observable at
    // runtime. One `struct.new` (the `Point(3, 7)` construction), one
    // `struct.set` (the `p.x = 99` write), and at least one
    // `struct.get` (the three `p.x` / `p.y` reads — exact count depends
    // on whether the IR reloads the slot per access).
    let (new, get, set) = count_struct_ops(&bytes);
    assert_eq!(new, 1, "expected exactly one struct.new, got {new}");
    assert_eq!(set, 1, "expected exactly one struct.set, got {set}");
    assert!(get >= 1, "expected at least one struct.get, got {get}");
    assert_wasm_prints(&bytes, "struct_point_wasm_gc", b"3\n7\n99\n");
}

/// PR 6 slice 1: strings on wasm32-gc. Exercises every op the slice
/// adds, in one fixture so a regression in any of them shows up here:
///
/// - **`Op::ConstString`** — `"hello"` (and the literals inside the
///   interpolation) materialize via passive data segments +
///   `array.new_data` + `struct.new $string`. The DataCount section
///   has to declare the segment count up front, or wasmparser rejects
///   the module before any test value is printed.
/// - **`print(String)`** — dispatches through `translate_print` to the
///   synthesized `phx_print_str` helper, which copies bytes from the
///   `(array i8)` into a linear-memory scratch buffer, appends `'\n'`,
///   and calls `fd_write`.
/// - **`Op::StringConcat`** — interpolation `"hello, {name}"` lowers to
///   a chain of `Op::StringConcat` calls that resolve to
///   `phx_str_concat`. Each call allocates a fresh `$bytes` of combined
///   length and does two `array.copy`s honoring source `$offset`.
/// - **`BuiltinCall("String.length")`** — `greeting.length()` lowers
///   inline as `struct.get $string $len(2)` + `i64.extend_i32_u`. No
///   helper needed.
/// - **`Op::StringEq` / `Op::StringNe`** — `greeting == "hello"` /
///   `greeting != "world"` / `"abc" != "abd"` route through
///   `phx_str_eq`'s length-check + byte loop with offset arithmetic.
///   The three cases cover, respectively: a full byte-match returning
///   equal, the length-mismatch fast path, and the same-length
///   byte-by-byte mismatch path. `Op::StringNe` adds an `i32.eqz` to
///   flip the helper's 0/1 result.
///
/// COVERAGE GAP — nonzero `$offset`: every string reachable here has
/// `$offset == 0` (literals from `Op::ConstString` start at 0, and
/// `phx_str_concat` produces results at offset 0). The `offset + i`
/// arithmetic in `phx_str_eq` / `phx_print_str` and the `$offset`-
/// honoring `array.copy` in `phx_str_concat` are therefore exercised
/// only with offset 0. The first op that produces a nonzero offset is
/// substring (decision K.3, a follow-up slice); the fixture that lands
/// it must add a substring-then-{print,eq,concat} case to cover the
/// offset path. Until then a regression that dropped the offset term
/// would not be caught here.
///
/// Expected stdout: `hello\nhello, world\n5\neq-yes\nne-yes\ndiff-yes\n`.
#[test]
fn string_ops_run_under_wasmtime_gc() {
    // Pulled from the canonical fixture at compile time (mirroring
    // `struct_alloc_get_set_runs_under_wasmtime_gc`) so the test stays
    // locked to `wasm_gc_string.phx` rather than a drifting inline copy.
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/wasm_gc_string.phx"
    ));
    assert_prints(
        source,
        "string_ops_wasm_gc",
        b"hello\nhello, world\n5\neq-yes\nne-yes\ndiff-yes\n",
    );
}

/// PR 6 slice 2: substring + lex compare + print(Bool). Exercises
/// every op the slice adds, in one fixture:
///
/// - **`print(Bool)`** — inline two-segment if/else lowering. The two
///   active data segments (`"true\n"` / `"false\n"`) materialize into
///   linear memory at module instantiation; each call site stages an
///   iovec at one of the two pre-populated offsets and calls
///   `fd_write`. No `phx_print_bool` helper.
/// - **`Op::StringLt` / `Le` / `Gt` / `Ge`** — each dispatches through
///   `phx_str_cmp` (lex byte compare with offset arithmetic) + a
///   signed-i32 cmp against 0. Covers strict, loose, prefix-vs-
///   longer-string ordering (the natural `a` is a prefix of `apple` so
///   `a < apple` case), and the equal-strings case where the strict
///   ops must be false (negative assertion — `BUG-*` sentinels that
///   must not appear in stdout).
/// - **`BuiltinCall("String.substring")`** — calls `phx_str_substring`,
///   which char-walks UTF-8 boundaries, clamps start/end, and returns
///   a view `struct.new`. Tests cover ASCII (where char index = byte
///   index), out-of-range clamping in both directions, indices past
///   `i32::MAX` (which must saturate, not wrap), and multi-byte UTF-8
///   (where the char walk actually has to skip continuation bytes).
/// - **Offset arithmetic on views** — `length`, `substring`, and lex
///   compare are re-run against substring *views* (non-zero `$offset`)
///   so the helpers' offset handling is actually exercised, not just
///   the offset-0 literal path.
///
/// Pulled from the canonical fixture at compile time (mirroring
/// `string_ops_run_under_wasmtime_gc`) so the test stays locked to
/// `wasm_gc_string_2.phx` rather than a drifting inline copy.
///
/// Expected stdout:
/// ```text
/// true
/// false
/// true
/// a-lt-b
/// b-gt-a
/// a-le-apple
/// b-ge-banana
/// prefix-lt
/// hello
/// world
///
///
///
/// hell
/// él
/// 5
/// 5
/// orl
/// w-gt-e
/// 2
/// ```
///
/// The three blank lines come from the empty-result substrings
/// (`substring(0, 0)`, `substring(100, 200)`, and the past-`i32::MAX`
/// `substring(4294967296, 4294967300)` — all clamp to an empty span
/// and print() adds a newline).
#[test]
fn string_substring_lex_cmp_and_print_bool_run_under_wasmtime_gc() {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/wasm_gc_string_2.phx"
    ));
    assert_prints(
        source,
        "string_slice2_wasm_gc",
        b"true\nfalse\ntrue\na-lt-b\nb-gt-a\na-le-apple\nb-ge-banana\nprefix-lt\nhello\nworld\n\n\n\nhell\n\xc3\xa9l\n5\n5\norl\nw-gt-e\n2\n",
    );
}

/// PR 6 slice 3: enums on wasm32-gc. Exercises every op the slice
/// adds, plus the heterogeneous-variant case (Result) and a nullary
/// variant. Per §Phase 2.4 decision K.4, each Phoenix enum gets a
/// parent type holding `$tag` and one final variant subtype per
/// variant; `EnumAlloc` is one `struct.new` against the variant,
/// `EnumDiscriminant` reads `$tag` through the parent without a
/// `ref.cast`, `EnumGetField` `ref.cast`s to the concrete variant
/// before the field load.
///
/// - **Custom 3-variant enum (`Shape`)** — covers nullary
///   (`Square`), single-field (`Circle(Int)`), and multi-field
///   (`Rect(Int, Int)`) variants. Match arms exercise field reads
///   on the multi-field variant.
/// - **`Option<Int>`** — generic enum with a single type parameter.
///   Monomorphizes to one `Option__i64` enum with `Some` and `None`
///   variants.
/// - **`Result<Int, String>`** — the heterogeneous variant case.
///   `Ok.0` is `Int`, `Err.0` is `String` — under the subtype
///   hierarchy each variant carries its field at the natural WASM
///   type (no boxing). If a regression flipped representation to
///   flat-max (decision B), this fixture would either fail at
///   `Op::EnumAlloc` (boxing not yet implemented) or silently
///   truncate the string field through a wrong slot type.
///
/// Expected stdout:
/// ```
/// 48
/// 15
/// 1
/// circle
/// rect
/// square
/// 42
/// 99
/// 7
/// oops
/// ```
#[test]
fn enum_ops_run_under_wasmtime_gc() {
    // Pulled from the canonical fixture at compile time (mirroring the
    // sibling struct/string gates above) so the test stays locked to
    // whatever `wasm_gc_enum.phx` says rather than a drifting inline copy.
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/wasm_gc_enum.phx"
    ));
    let bytes = compile_to_wasm_gc(source);
    // Structural gate (runs even without wasmtime): the three enums —
    // `Shape` (3 variants), `Option<Int>` (2), `Result<Int, String>`
    // (2) — must each emit one parent + one subtype per variant, per
    // §Phase 2.4 K.4. So 3 parents and 3 + 2 + 2 = 7 variant subtypes.
    // A regression that flattened the hierarchy or dropped a variant
    // changes these counts regardless of the execution tier.
    let (parents, variants) = count_enum_type_decls(&bytes);
    assert_eq!(parents, 3, "expected 3 enum parent types, got {parents}");
    assert_eq!(
        variants, 7,
        "expected 7 enum variant subtypes, got {variants}"
    );
    // `EnumGetField` is the only wasm32-gc op that emits `ref.cast`;
    // `area`'s field reads (`Circle(r)`, `Rect(w, h)`) guarantee at
    // least one. Asserting its presence catches a dropped field-read
    // lowering that a valid-but-wrong module would otherwise hide when
    // wasmtime is absent.
    let casts = count_ref_cast_ops(&bytes);
    assert!(
        casts >= 1,
        "expected at least one ref.cast from EnumGetField, got {casts}"
    );
    assert_wasm_prints(
        &bytes,
        "enum_ops_wasm_gc",
        b"48\n15\n1\ncircle\nrect\nsquare\n42\n99\n7\noops\n",
    );
}

/// PR 6 slice 3, reference-typed variant fields. `enum_ops` covers
/// primitive and `StringRef` payloads; this fixture drives the two
/// arms of `wasm_enum_field_type_for` it doesn't reach:
///
/// - **`Holder.Wrap(Point)`** — a `StructRef` variant field. Allocates
///   a struct, stuffs it into the variant, then `EnumGetField`
///   `ref.cast`s back and reads both struct fields (`p.x + p.y` → 10).
/// - **`IntList.Cons(Int, IntList)`** — a self-recursive `EnumRef`
///   variant field. The design doc (§Phase 2.4 K.4) claims recursive
///   enums work without special handling because all parent types are
///   declared before any variant struct; `sum` walking a 3-element
///   list (→ 6) is the gate on that claim.
///
/// The middle line (`-1`) exercises a nullary variant of a multi-variant
/// enum whose other variant carries a reference payload.
///
/// Expected stdout: `10\n-1\n6\n`.
#[test]
fn enum_reference_variant_fields_run_under_wasmtime_gc() {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/wasm_gc_enum_nested.phx"
    ));
    let bytes = compile_to_wasm_gc(source);
    // Structural gate (runs even without wasmtime): `Holder` (Wrap,
    // Empty) and `IntList` (Cons, Nil) are two enums of two variants
    // each → 2 parents, 4 variant subtypes. The `Point` struct is a
    // plain (final, no-supertype) struct and counts in neither bucket,
    // confirming the counter distinguishes enum subtypes from regular
    // structs. See §Phase 2.4 K.4.
    let (parents, variants) = count_enum_type_decls(&bytes);
    assert_eq!(parents, 2, "expected 2 enum parent types, got {parents}");
    assert_eq!(
        variants, 4,
        "expected 4 enum variant subtypes, got {variants}"
    );
    // `Wrap(p)` reads the struct payload and `Cons(head, tail)` reads
    // both recursive-enum fields, so the `EnumGetField` → `ref.cast`
    // lowering must appear.
    let casts = count_ref_cast_ops(&bytes);
    assert!(
        casts >= 1,
        "expected at least one ref.cast from EnumGetField, got {casts}"
    );
    assert_wasm_prints(&bytes, "enum_nested_wasm_gc", b"10\n-1\n6\n");
}

/// PR 6 slice 3, `Float` variant-field declaration. `Float` is the one
/// slice-3 field type (§Phase 2.4 K.4) that the run-under-wasmtime
/// fixtures can't reach end-to-end: wasm32-gc can't yet *produce* a
/// `Float` value (no `ConstF64` / float-arith lowering, and `print(Float)`
/// is deferred), so no float can flow into a variant via construction.
/// But the `F64` arm of `wasm_enum_field_type_for` runs at type-*declaration*
/// time, independent of whether any float is ever materialized. This gates
/// it: `Pulse.Tick(Float)` forces the F64 arm to encode an `f64` variant
/// field, while `main` only ever constructs the nullary `Quiet` variant —
/// so the module compiles, declares the parent + both variant subtypes, and
/// runs. A regression in the F64 arm (wrong slot type, or routing `Float`
/// to the out-of-slice catch-all) breaks the compile or the type counts.
///
/// Expected stdout: `0\n` (`classify(Quiet)` returns 0).
#[test]
fn enum_float_variant_field_declares_and_runs() {
    let source = concat!(
        "enum Pulse {\n",
        "  Tick(Float)\n",
        "  Quiet\n",
        "}\n",
        "function classify(p: Pulse) -> Int {\n",
        "  match p {\n",
        "    Tick(f) -> 1\n",
        "    Quiet -> 0\n",
        "  }\n",
        "}\n",
        "function main() {\n",
        // Only the nullary variant is constructed — no `Float` literal is
        // needed, yet `Tick(Float)` is still declared (its parent is
        // reachable through `classify`'s param type).
        "  let q: Pulse = Quiet\n",
        "  print(classify(q))\n",
        "}\n",
    );
    let bytes = compile_to_wasm_gc(source);
    // Structural gate (runs even without wasmtime): one enum, two variants.
    let (parents, variants) = count_enum_type_decls(&bytes);
    assert_eq!(parents, 1, "expected 1 enum parent type, got {parents}");
    assert_eq!(
        variants, 2,
        "expected 2 enum variant subtypes, got {variants}"
    );
    assert_wasm_prints(&bytes, "enum_float_field_wasm_gc", b"0\n");
}

/// PR 6 slice 3 error path: a variant field whose type is out of slice
/// scope (here `List<Int>`) must be rejected at codegen with the
/// per-slice diagnostic, not emitted as a structurally-invalid module.
/// The front-end accepts the program; the rejection happens in
/// `wasm_enum_field_type_for`'s catch-all arm, so this asserts on the
/// `compile` `Err` rather than going through `assert_prints`.
#[test]
fn enum_variant_field_out_of_slice_scope_errors() {
    let source = concat!(
        "enum Bag {\n",
        "  Items(List<Int>)\n",
        "  Empty\n",
        "}\n",
        "function main() {\n",
        "  let b: Bag = Items([1, 2, 3])\n",
        "  match b {\n",
        "    Items(xs) -> print(42)\n",
        "    Empty -> print(0)\n",
        "  }\n",
        "}\n",
    );
    let ir_module = lower_to_ir(source);
    let err = compile(&ir_module, Target::Wasm32Gc)
        .expect_err("a List-typed enum variant field is out of slice-3 scope");
    let msg = err.to_string();
    assert!(
        msg.contains("out of slice scope") && msg.contains("ListRef"),
        "expected an out-of-slice-scope diagnostic naming the list type, got: {msg}"
    );
}

/// PR 6 slice 3 known limitation: a user-defined generic enum (here
/// `Wrapper<T>`, with both a directly-generic `Bare(T)` field and a
/// nested-generic `W(Option<T>)` field) can't be resolved by the
/// position-counting substitution heuristic. It must surface as a clear
/// Known-limitation diagnostic (§Phase 2.4 K.4), not the misleading
/// out-of-scope-`TypeVar` / "struct `__generic` missing" message it
/// produced before the placeholder guard in `wasm_enum_field_type_for`.
#[test]
fn enum_nested_generic_variant_field_reports_known_limitation() {
    let source = concat!(
        "enum Wrapper<T> {\n",
        "  W(Option<T>)\n",
        "  Bare(T)\n",
        "}\n",
        "function main() {\n",
        "  let x: Wrapper<Int> = Bare(5)\n",
        "  match x {\n",
        "    W(o) -> print(1)\n",
        "    Bare(v) -> print(v)\n",
        "  }\n",
        "}\n",
    );
    let ir_module = lower_to_ir(source);
    let err = compile(&ir_module, Target::Wasm32Gc)
        .expect_err("a user-defined generic enum variant field is the K.4 known limitation");
    let msg = err.to_string();
    assert!(
        msg.contains("unresolved generic type") && msg.contains("Known limitation"),
        "expected the generic known-limitation diagnostic, got: {msg}"
    );
}

/// A program with no strings must carry no `$bytes` / `$string` types
/// and no string helpers — `scan_helper_needs` is the gate, and a
/// regression that wired the declarations unconditionally would
/// silently grow every module. The check leans on wasmparser: a module
/// declared as using `array.new_data` requires a `DataCount` section,
/// so emitting one for a string-free module would now reject in
/// validation. (Strictly the assertion is by-absence; the structural
/// validation in `assert_prints` is the proxy that catches it.)
#[test]
fn string_helpers_omitted_when_unused() {
    assert_prints(
        "function main() {\n  print(42)\n}\n",
        "no_strings_wasm_gc",
        b"42\n",
    );
}

/// Empty-string edge cases for the slice-1 string ops. The zero-length
/// string is the boundary case for every length/offset arithmetic in
/// the helpers, so each op is exercised with an empty operand:
///
/// - **`Op::ConstString("")`** — a zero-byte `array.new_data` +
///   `struct.new $string` with `$len == 0`. `print` of it copies zero
///   bytes, writes only the trailing newline (so stdout sees a bare
///   blank line), and the iovec length is `1`.
/// - **`String.length()` on `""`** — `struct.get $string $len` reads
///   `0`.
/// - **`Op::StringConcat` with empty operands** — `"{e}{h}{e}"`
///   concatenates an empty left operand, then an empty right operand,
///   so both `array.copy`s run with a zero `size` and the result is
///   exactly `"hi"`.
/// - **`Op::StringEq` on two empties** — lengths match at `0`, the
///   compare loop's `i >= len` guard fires immediately, returns equal.
/// - **`Op::StringNe` empty vs non-empty** — the length-mismatch fast
///   path (`0 != 2`) returns not-equal, which `i32.eqz` flips to true.
///
/// Expected stdout: `\n0\nhi\nempty-eq\nempty-ne\n`.
#[test]
fn empty_string_ops_run_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let e: String = \"\"\n",
        "  print(e)\n",
        "  print(e.length())\n",
        "  let h: String = \"hi\"\n",
        "  print(\"{e}{h}{e}\")\n",
        "  if e == \"\" {\n",
        "    print(\"empty-eq\")\n",
        "  }\n",
        "  if e != h {\n",
        "    print(\"empty-ne\")\n",
        "  }\n",
        "}\n",
    );
    assert_prints(
        source,
        "empty_string_ops_wasm_gc",
        b"\n0\nhi\nempty-eq\nempty-ne\n",
    );
}

/// `phx_print_str` hard-rejects strings whose `$len` exceeds its
/// fixed-size linear-memory scratch buffer (`PRINT_STR_MAX_LEN`, 4095
/// bytes) by emitting `unreachable` rather than writing past the buffer
/// and smashing the iovec staging area below it. A 5000-byte literal
/// clears that bound, so running the module must trap. Structural
/// validation alone can't prove the guard fires — `unreachable` is a
/// perfectly valid instruction — so this leans on the wasmtime
/// execution tier (degrades to validation-only when wasmtime is absent,
/// like [`assert_traps`]'s other callers).
#[test]
fn print_str_oversized_traps_under_wasmtime_gc() {
    // 5000 > PRINT_STR_MAX_LEN (4095). The constant is `pub(super)` and
    // not reachable from this integration test, so the threshold is
    // restated here; if it ever grows past 5000 this literal must too.
    let big = "x".repeat(5000);
    let source = format!("function main() {{\n  print(\"{big}\")\n}}\n");
    assert_traps(&source, "print_str_oversized_wasm_gc");
}

/// A struct field whose type isn't yet on the slice-3 surface (here a
/// nested `StructRef`) must surface a clear per-slice diagnostic — not
/// silently emit a partial declaration that later trips up
/// `wasmparser` with an "unexpected field type" deep inside the binary
/// format. The error keeps the slice from masking work that belongs to
/// follow-up slices.
#[test]
fn struct_with_nested_struct_field_is_rejected_until_a_later_slice() {
    let source = concat!(
        "struct Inner {\n",
        "  Int v\n",
        "}\n",
        "struct Outer {\n",
        "  Inner inner\n",
        "}\n",
        "function main() {\n",
        "  let o: Outer = Outer(Inner(1))\n",
        "  print(o.inner.v)\n",
        "}\n",
    );
    let ir_module = lower_to_ir(source);
    let err = compile(&ir_module, Target::Wasm32Gc)
        .expect_err("nested struct fields are outside slice 3's MVP scope");
    let msg = err.to_string();
    assert!(
        msg.contains("Outer") && msg.contains("inner"),
        "expected a per-field diagnostic naming the unsupported field, got: {msg}"
    );
}

/// `Op::StructGetField` / `Op::StructSetField` with a field index past
/// the struct's declared field count must surface a clear per-op
/// diagnostic, not emit a `struct.get`/`struct.set` that only
/// `wasmparser` rejects deep in binary decoding. Built straight from IR
/// (mirroring [`print_int_module`]): no Phoenix surface produces an
/// out-of-range index, since sema resolves field *names* to in-range
/// indices, so the guard is only reachable via a hand-built (or future
/// buggy) IR.
#[test]
fn struct_field_index_out_of_range_is_rejected() {
    let mut func = IrFunction::new(
        FuncId(0),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let entry = func.create_block();
    let a = func.emit_value(entry, Op::ConstI64(1), IrType::I64, None);
    let b = func.emit_value(entry, Op::ConstI64(2), IrType::I64, None);
    let pair = func.emit_value(
        entry,
        Op::StructAlloc("Pair".to_string(), vec![a, b]),
        IrType::StructRef("Pair".to_string(), Vec::new()),
        None,
    );
    // `Pair` declares two fields; index 5 is out of range.
    func.emit_value(entry, Op::StructGetField(pair, 5), IrType::I64, None);
    func.set_terminator(entry, Terminator::Return(None));

    let mut module = IrModule::new();
    module.struct_layouts.insert(
        "Pair".to_string(),
        vec![
            ("a".to_string(), IrType::I64),
            ("b".to_string(), IrType::I64),
        ],
    );
    module.push_concrete(func);

    let err = compile(&module, Target::Wasm32Gc)
        .expect_err("an out-of-range struct field index must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("out of range") && msg.contains("Op::StructGetField"),
        "expected an out-of-range field-index diagnostic, got: {msg}"
    );
}

/// Sibling of [`struct_field_index_out_of_range_is_rejected`] for the
/// *write* path: `Op::StructSetField` shares the `check_field_index`
/// guard, but routes a distinct `"Op::StructSetField"` label into the
/// diagnostic. Pin that label so the set path can't silently regress to
/// a `struct.set` that only `wasmparser` rejects deep in decoding.
#[test]
fn struct_set_field_index_out_of_range_is_rejected() {
    let mut func = IrFunction::new(
        FuncId(0),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let entry = func.create_block();
    let a = func.emit_value(entry, Op::ConstI64(1), IrType::I64, None);
    let b = func.emit_value(entry, Op::ConstI64(2), IrType::I64, None);
    let pair = func.emit_value(
        entry,
        Op::StructAlloc("Pair".to_string(), vec![a, b]),
        IrType::StructRef("Pair".to_string(), Vec::new()),
        None,
    );
    // `Pair` declares two fields; index 5 is out of range.
    func.emit(entry, Op::StructSetField(pair, 5, a), IrType::Void, None);
    func.set_terminator(entry, Terminator::Return(None));

    let mut module = IrModule::new();
    module.struct_layouts.insert(
        "Pair".to_string(),
        vec![
            ("a".to_string(), IrType::I64),
            ("b".to_string(), IrType::I64),
        ],
    );
    module.push_concrete(func);

    let err = compile(&module, Target::Wasm32Gc)
        .expect_err("an out-of-range struct field index must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("out of range") && msg.contains("Op::StructSetField"),
        "expected an out-of-range field-index diagnostic, got: {msg}"
    );
}

/// Exercise `IrType::StructRef` in a *function signature* — the case
/// the declare-before-any-signature ordering exists for. `get_x` takes
/// a `Point` parameter and `make` returns one, so both signatures
/// encode `(ref null $Point)` and must be interned *after* the struct's
/// type-section declaration. The slice-3 fixture only uses a struct as
/// a `main`-local, so without this test the param/return resolution in
/// `wasm_valtypes_for` / `flatten_param_types` / `wasm_return_valtypes`
/// is unexercised. Built straight from IR because no slice-3 Phoenix
/// surface lowers struct-typed parameters yet.
///
/// Shape: `make()` allocates `Point(3, 7)` and returns it; `main` calls
/// `make`, passes the result to `get_x`, and prints the `x` field.
///
/// Expected stdout: `3\n`.
#[test]
fn struct_in_function_signature_runs_under_wasmtime_gc() {
    let point = || IrType::StructRef("Point".to_string(), Vec::new());

    // `make() -> Point { Point(3, 7) }`
    let mut make = IrFunction::new(
        FuncId(1),
        "make".to_string(),
        Vec::new(),
        Vec::new(),
        point(),
        None,
    );
    let mk_entry = make.create_block();
    let a = make.emit_value(mk_entry, Op::ConstI64(3), IrType::I64, None);
    let b = make.emit_value(mk_entry, Op::ConstI64(7), IrType::I64, None);
    let pt = make.emit_value(
        mk_entry,
        Op::StructAlloc("Point".to_string(), vec![a, b]),
        point(),
        None,
    );
    make.set_terminator(mk_entry, Terminator::Return(Some(pt)));

    // `get_x(p: Point) -> Int { p.x }`
    let mut get_x = IrFunction::new(
        FuncId(2),
        "get_x".to_string(),
        vec![point()],
        vec!["p".to_string()],
        IrType::I64,
        None,
    );
    let gx_entry = get_x.create_block();
    let p = get_x.add_block_param(gx_entry, point());
    let x = get_x.emit_value(gx_entry, Op::StructGetField(p, 0), IrType::I64, None);
    get_x.set_terminator(gx_entry, Terminator::Return(Some(x)));

    // `main() { print(get_x(make())) }`
    let mut main = IrFunction::new(
        FuncId(0),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let m_entry = main.create_block();
    let made = main.emit_value(
        m_entry,
        Op::Call(FuncId(1), Vec::new(), Vec::new()),
        point(),
        None,
    );
    let got = main.emit_value(
        m_entry,
        Op::Call(FuncId(2), Vec::new(), vec![made]),
        IrType::I64,
        None,
    );
    main.emit(
        m_entry,
        Op::BuiltinCall("print".to_string(), vec![got]),
        IrType::Void,
        None,
    );
    main.set_terminator(m_entry, Terminator::Return(None));

    let mut module = IrModule::new();
    module.struct_layouts.insert(
        "Point".to_string(),
        vec![
            ("x".to_string(), IrType::I64),
            ("y".to_string(), IrType::I64),
        ],
    );
    module.push_concrete(main);
    module.push_concrete(make);
    module.push_concrete(get_x);

    let bytes = compile(&module, Target::Wasm32Gc)
        .unwrap_or_else(|e| panic!("wasm32-gc compile failed: {e}"));
    assert_wasm_prints(&bytes, "struct_in_signature_wasm_gc", b"3\n");
}

/// Pin §Phase 2.4 decision K.1's *nominal* mapping: two Phoenix structs
/// with an identical field shape (`Point { Int, Int }` /
/// `Pixel { Int, Int }`) must each get their own WASM struct type, not
/// share one structurally. The behavior is otherwise invisible at this
/// slice's surface — both compile and run fine either way — so without a
/// structural count a regression that deduped same-shape structs would
/// pass every other test. Counting the type-section `(struct …)` entries
/// asserts the two-distinct-types invariant directly.
#[test]
fn same_shape_structs_get_distinct_wasm_types() {
    let source = concat!(
        "struct Point {\n",
        "  Int x\n",
        "  Int y\n",
        "}\n",
        "struct Pixel {\n",
        "  Int x\n",
        "  Int y\n",
        "}\n",
        "function main() {\n",
        "  let p: Point = Point(1, 2)\n",
        "  let q: Pixel = Pixel(3, 4)\n",
        "  print(p.x)\n",
        "  print(q.x)\n",
        "}\n",
    );
    let bytes = compile_to_wasm_gc(source);
    let decls = count_struct_type_decls(&bytes);
    assert_eq!(
        decls, 2,
        "expected two distinct WASM struct types (one per Phoenix struct, \
         per K.1's nominal mapping), got {decls}"
    );
    assert_wasm_prints(&bytes, "same_shape_structs_wasm_gc", b"1\n3\n");
}

/// `Op::StructAlloc` naming a struct absent from `IrModule::struct_layouts`
/// must surface `require_phx_struct`'s missing-declaration diagnostic —
/// the guard against a signature/alloc referencing a struct the
/// `declare_phoenix_structs` pass never saw. No Phoenix surface produces
/// this (sema resolves every constructor to a declared struct), so the
/// path is only reachable via hand-built (or future buggy) IR, mirroring
/// the out-of-range field-index tests.
#[test]
fn struct_alloc_for_undeclared_struct_is_rejected() {
    let mut func = IrFunction::new(
        FuncId(0),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let entry = func.create_block();
    // `Ghost` is deliberately never inserted into `struct_layouts` below.
    func.emit_value(
        entry,
        Op::StructAlloc("Ghost".to_string(), Vec::new()),
        IrType::StructRef("Ghost".to_string(), Vec::new()),
        None,
    );
    func.set_terminator(entry, Terminator::Return(None));

    let mut module = IrModule::new();
    module.push_concrete(func);

    let err = compile(&module, Target::Wasm32Gc)
        .expect_err("a StructAlloc naming a struct absent from struct_layouts must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("Ghost") && msg.contains("declare_phoenix_structs"),
        "expected a missing-declaration diagnostic naming the struct, got: {msg}"
    );
}

/// Exercise a `Bool` struct field end-to-end — the `Bool → i32` arm of
/// `wasm_field_type_for`, plus a `struct.get` whose result slot is `i32`
/// rather than `i64`. The fixture stores `true` into the field, reads it
/// back, and branches on it; the read is observable because the taken
/// arm prints a different `Int` than the untaken one. The slice-3
/// fixture only has `Int` fields, so without this the bool field mapping
/// (a swap to e.g. `i64` would push an i64 operand into an i32 field and
/// `wasmparser` would reject `struct.new`) is unexercised.
///
/// Shape: `Flags { Bool on, Int n }`; `main` builds `Flags(true, 42)` and
/// prints `n` iff `on`, else `0`.
///
/// Expected stdout: `42\n`.
#[test]
fn struct_bool_field_runs_under_wasmtime_gc() {
    let source = concat!(
        "struct Flags {\n",
        "  Bool on\n",
        "  Int n\n",
        "}\n",
        "function main() {\n",
        "  let f: Flags = Flags(true, 42)\n",
        "  if f.on {\n",
        "    print(f.n)\n",
        "  } else {\n",
        "    print(0)\n",
        "  }\n",
        "}\n",
    );
    let bytes = compile_to_wasm_gc(source);
    let (new, get, _set) = count_struct_ops(&bytes);
    assert_eq!(new, 1, "expected exactly one struct.new, got {new}");
    assert!(get >= 1, "expected at least one struct.get, got {get}");
    assert_wasm_prints(&bytes, "struct_bool_field_wasm_gc", b"42\n");
}

/// Exercise an `F64` struct field — the `F64 → f64` arm of
/// `wasm_field_type_for` and a `struct.new` / `struct.get` against an
/// f64 field slot. Hand-built rather than source-driven because the GC
/// backend lowers no f64-producing op (`Op::ConstF64` is outside the
/// slice-3 surface, and there's no float arithmetic), so the only way to
/// mint an f64 operand for `struct.new` is a function *parameter* of type
/// `Float`. The `build` function is never called — `main` can't produce
/// an f64 to pass it — but it is still a concrete function, so it is
/// emitted and `wasmparser`-validated. A wrong `F64 → i32` mapping would
/// push an i32-typed operand into an f64 field (or vice-versa) and
/// validation would reject the `struct.new`; this pins the mapping
/// structurally even though the execution tier can't reach the function.
#[test]
fn struct_f64_field_validates() {
    // `build(x: Float) -> Float { HasFloat(x).f }`
    let mut build = IrFunction::new(
        FuncId(1),
        "build".to_string(),
        vec![IrType::F64],
        vec!["x".to_string()],
        IrType::F64,
        None,
    );
    let b_entry = build.create_block();
    let x = build.add_block_param(b_entry, IrType::F64);
    let h = build.emit_value(
        b_entry,
        Op::StructAlloc("HasFloat".to_string(), vec![x]),
        IrType::StructRef("HasFloat".to_string(), Vec::new()),
        None,
    );
    let f = build.emit_value(b_entry, Op::StructGetField(h, 0), IrType::F64, None);
    build.set_terminator(b_entry, Terminator::Return(Some(f)));

    // `main()` is the entry point; it does nothing but anchor the module
    // (it can't call `build` — no f64 value to pass).
    let mut main = IrFunction::new(
        FuncId(0),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let m_entry = main.create_block();
    main.set_terminator(m_entry, Terminator::Return(None));

    let mut module = IrModule::new();
    module
        .struct_layouts
        .insert("HasFloat".to_string(), vec![("f".to_string(), IrType::F64)]);
    module.push_concrete(main);
    module.push_concrete(build);

    let bytes = compile(&module, Target::Wasm32Gc)
        .unwrap_or_else(|e| panic!("wasm32-gc compile failed: {e}"));
    // `wasmparser` (GC features on) rejects a `struct.new` whose operand
    // type disagrees with the declared field type, so validation is the
    // assertion that `F64` mapped to an f64 field.
    validate_gc_module(&bytes, "struct_f64_field_wasm_gc");
    let (new, get, _set) = count_struct_ops(&bytes);
    assert_eq!(new, 1, "expected exactly one struct.new, got {new}");
    assert!(get >= 1, "expected at least one struct.get, got {get}");
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
