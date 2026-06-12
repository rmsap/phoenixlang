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

/// Count the locally-defined functions in `bytes` (code-section
/// entries — imports don't count). Used to assert a synthesized helper
/// is present in (and only in) the modules that need it when the
/// helper is too small for a `float_free_module_carries_no_ryu_tables`
///-style size-delta argument: two fixtures identical except for the
/// op that demands the helper must differ by exactly one function.
fn count_local_functions(bytes: &[u8]) -> usize {
    use wasmparser::{Parser, Payload};
    Parser::new(0)
        .parse_all(bytes)
        .filter(|payload| {
            matches!(
                payload.as_ref().expect("parse wasm payload"),
                Payload::CodeSectionEntry(_)
            )
        })
        .count()
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
/// These are the non-finite inputs the d2s general case never sees —
/// `phx_print_f64` short-circuits them inline before bit extraction,
/// exactly as ryu's `Buffer::format` checks `is_nan` / `is_finite`
/// before calling `format64`. (`-0.0` is finite and is covered by
/// `float_print_computed_values_run_under_wasmtime_gc`.)
///
/// Source-level Phoenix can't easily produce NaN/Infinity, so the
/// fixture computes them: `0.0 / 0.0` for NaN, `±1.0 / 0.0` for ±inf.
/// These flow through the synthesized helper at runtime, exercising
/// the IEEE-754 branches under wasmtime.
#[test]
fn print_float_special_cases_run_under_wasmtime_gc() {
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
    assert_prints(source, "print_float_specials", b"NaN\ninf\n-inf\n");
}

/// The adversarial corpus pinning the synthesized Ryu d2s port against the
/// `ryu` crate — the exact implementation native's
/// `phoenix_runtime::format_f64` wraps, so agreement here is agreement
/// with every other backend, byte for byte.
///
/// Each corpus entry is a Phoenix float literal; the expected stdout
/// is computed by parsing the same literal as a Rust `f64` (both
/// Phoenix's parser and Rust's use correctly-rounded `str::parse`, so
/// the bit patterns match) and formatting it with `ryu`. The corpus
/// deliberately stresses every branch of the port:
///
/// - **`12340000000.0` shape** (`0 <= k && kk <= 16`): integer-valued
///   floats incl. the 10^15/10^16 positional/scientific boundary and
///   2^53 (the integer-precision edge).
/// - **`12.34` shape** (`0 < kk <= 16`): the staged-digits +
///   `memory.copy` shift path, incl. classic non-terminating binaries
///   (0.1, 0.3) and a 17-significant-digit value.
/// - **`0.001234` shape** (`-5 < kk <= 0`): leading-zeros path, incl.
///   the 1e-5 boundary (last value formatted positionally).
/// - **Scientific, single digit** (`length == 1`): 1e16, 1e-6, and the
///   extremes — f64::MAX-longhand (309 digits, exponent `e308`) and
///   the smallest subnormal (5e-324, the `q <= 1` / `mm_shift`
///   bookkeeping corner).
/// - **Scientific, multi-digit**: the first-digit copy-down + `'.'`
///   insertion path (`"1.2345e20"`).
/// - **Round-even fixup**: values whose rare-path digit search
///   (`vm/vr_is_trailing_zeros` set, so the trailing-zeros loop runs)
///   ends with `last_removed_digit == 5` and an even `vr`, making the
///   demote-to-4 fixup decide the final digit — without the fixup
///   they print `…3` where ryu prints `…2`. Found by instrumented
///   search over random bit patterns; deterministic corpus entries so
///   the branch doesn't rely on the random streams to be reached.
/// - **Negatives** of several shapes (sign byte + every branch).
#[test]
fn float_print_matches_native() {
    let corpus: &[&str] = &[
        // zero / one / small integers (positional ".0" branch)
        "0.0",
        "1.0",
        "5.0",
        "100.0",
        "9007199254740992.0",      // 2^53
        "1000000000000000.0",      // 1e15 — last positional integer
        "10000000000000000.0",     // 1e16 — first scientific ("1e16")
        "100000000000000000000.0", // 1e20 — "1e20"
        // decimal-point-inside branch
        "1.5",
        "2.5",
        "12.34",
        "0.1",
        "0.2",
        "0.3",
        "123456.789",
        "3.141592653589793",
        "2.718281828459045",
        "1.7976931348623157", // 17 significant digits
        // leading-zeros branch
        "0.001234",
        "0.5",
        "0.00001",   // 1e-5 — last positional small value
        "0.000001",  // 1e-6 — first scientific ("1e-6")
        "0.0000001", // 1e-7 — "1e-7"
        "0.000030000000000000004",
        // scientific multi-digit branch ("1.2345e20")
        "123450000000000000000.0",
        // rare-path round-even fixup (last_removed_digit == 5, vr even,
        // vr_is_trailing_zeros) — these print …3 without it
        "894048597157646.2",
        "83992540645848.12",
        // negatives across branches
        "-1.0",
        "-1.5",
        "-0.001234",
        "-123456.789",
        "-10000000000000000.0",
    ];

    let mut source = String::from("function main() {\n");
    let mut expected = Vec::new();
    for lit in corpus {
        source.push_str(&format!("  print({lit})\n"));
        let val: f64 = lit.parse().expect("corpus literal parses as f64");
        expected.extend_from_slice(ryu::Buffer::new().format(val).as_bytes());
        expected.push(b'\n');
    }
    source.push_str("}\n");
    assert_prints(&source, "float_print_corpus", &expected);
}

/// The f64 extremes as longhand literals — too long to keep readable
/// inline in the corpus above, and worth their own test because they
/// stress different machinery: f64::MAX exercises the largest table
/// index / `e308` exponent emission, and the smallest subnormal
/// (5e-324) exercises the `ieee_exponent == 0` decode plus the
/// `q <= 1` trailing-zero bookkeeping. The longhand decimal expansions
/// are generated with Rust's exact fixed-point formatter; Phoenix's
/// lexer takes arbitrary-length digit strings and its parser delegates
/// to the same correctly-rounded `str::parse::<f64>`.
#[test]
fn float_print_extremes_match_native() {
    let max_longhand = format!("{:.1}", f64::MAX); // 309 digits + ".0"
    let min_subnormal = 5e-324_f64;
    let min_subnormal_longhand = format!("{min_subnormal:.1074}");

    let source = format!(
        "function main() {{\n  print({max_longhand})\n  print({min_subnormal_longhand})\n}}\n"
    );
    let mut expected = Vec::new();
    expected.extend_from_slice(ryu::Buffer::new().format(f64::MAX).as_bytes());
    expected.push(b'\n');
    expected.extend_from_slice(ryu::Buffer::new().format(min_subnormal).as_bytes());
    expected.push(b'\n');
    assert_prints(&source, "float_print_extremes", &expected);
}

/// Differential check over arbitrary IEEE-754 bit patterns: 200
/// deterministic pseudo-random u64s (SplitMix64, fixed seed — tests
/// must not be flaky) reinterpreted as f64, non-finite patterns
/// skipped, each printed by the wasm32-gc module and compared against
/// `ryu`. A hand-picked corpus pins the branches we know about; this
/// sweep catches what hand-picking can't — a transposed table entry,
/// an off-by-one in the 128-bit carry, a wrong rounding decision on
/// some unanticipated mantissa shape — since random bit patterns are
/// uniform over exponents (most land in the extreme-magnitude
/// scientific branches, where the table indices range widest).
///
/// Each value is fed to Phoenix as its exact fixed-point decimal
/// expansion (`{:.1074}` — every finite f64's exact expansion has at
/// most 1074 fractional digits, so the literal round-trips to the
/// identical bit pattern through correctly-rounded `str::parse`).
#[test]
fn float_print_random_bits_match_native() {
    // SplitMix64 — deterministic, seed pinned.
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut next = move || {
        state = state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    };

    let mut source = String::from("function main() {\n");
    let mut expected = Vec::new();
    let mut emitted = 0;
    while emitted < 200 {
        let val = f64::from_bits(next());
        if !val.is_finite() {
            continue;
        }
        source.push_str(&format!("  print({val:.1074})\n"));
        expected.extend_from_slice(ryu::Buffer::new().format(val).as_bytes());
        expected.push(b'\n');
        emitted += 1;
    }
    source.push_str("}\n");
    assert_prints(&source, "float_print_random_bits", &expected);
}

/// One value per IEEE-754 binary exponent (0 = subnormal through 2046;
/// 2047 is inf/NaN, covered by the specials test), each printed by the
/// wasm32-gc module and compared against `ryu`.
///
/// This test carries the power-of-5 table guarantee: the tables in
/// `ryu_tables.rs` are *computed* from their mathematical definitions
/// rather than copied from the `ryu` crate (which keeps the repo
/// MIT-only — see the module doc there), and `phx_ryu_d2d`'s table
/// index is a function of the binary exponent alone (q and i derive
/// from e2, which derives from the IEEE exponent). Sweeping every
/// exponent therefore exercises every reachable entry of both tables
/// — inverse indices 0..=290 and pow5 indices 1..=325 — end-to-end
/// against the oracle. A single wrong bit in any computed entry shows
/// up here as a digit mismatch. Mantissas are drawn from SplitMix64
/// (pinned seed, distinct from `float_print_random_bits`' stream) so
/// the digit-finding loops see varied shapes too.
#[test]
fn float_print_every_binary_exponent_matches_native() {
    let mut state: u64 = 0x243F6A8885A308D3; // pi digits — any fixed seed
    let mut next = move || {
        state = state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    };

    let mut source = String::from("function main() {\n");
    let mut expected = Vec::new();
    for ieee_exponent in 0u64..=2046 {
        let mut mantissa = next() & ((1u64 << 52) - 1);
        if ieee_exponent == 0 && mantissa == 0 {
            mantissa = 1; // keep the subnormal row non-zero
        }
        let val = f64::from_bits(ieee_exponent << 52 | mantissa);
        source.push_str(&format!("  print({val:.1074})\n"));
        expected.extend_from_slice(ryu::Buffer::new().format(val).as_bytes());
        expected.push(b'\n');
    }
    source.push_str("}\n");
    assert_prints(&source, "float_print_exponent_sweep", &expected);
}

/// Runtime-computed float values (no literal on the print path, so the
/// full IEEE-754 bit pattern genuinely flows through `phx_ryu_d2d`
/// under wasmtime — Phoenix has no constant-folding pass, but this
/// removes any doubt):
///
/// - `-1.0 * 0.0` → `"-0.0"` — the sign-of-zero case the pre-amendment
///   integer fast-path silently dropped (it printed `"0"`).
/// - `0.1 + 0.2` → `"0.30000000000000004"` — the canonical
///   shortest-roundtrip showcase: 17 digits, not `"0.3"`.
/// - `1.0 / 3.0` → `"0.3333333333333333"` — non-terminating binary
///   with a 16-digit shortest form.
#[test]
fn float_print_computed_values_run_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let neg_zero: Float = -1.0 * 0.0\n",
        "  let classic: Float = 0.1 + 0.2\n",
        "  let third: Float = 1.0 / 3.0\n",
        "  print(neg_zero)\n",
        "  print(classic)\n",
        "  print(third)\n",
        "}\n",
    );
    let mut expected = Vec::new();
    // -0.0 is the value Phoenix's `-1.0 * 0.0` computes at runtime.
    for val in [-0.0, 0.1 + 0.2, 1.0 / 3.0] {
        expected.extend_from_slice(ryu::Buffer::new().format(val).as_bytes());
        expected.push(b'\n');
    }
    assert_prints(source, "float_print_computed", &expected);
}

/// Float `%` via the synthesized
/// `phx_fmod` helper (musl `fmod` port), differentially pinned against
/// Rust's `f64 % f64` — the semantics every other backend inherits
/// from the runtime / interpreters. The pairs stress the algorithm's
/// distinct paths: sign combinations (truncated remainder keeps the
/// *dividend's* sign), the `|x| < |y|` return-x early-out, the
/// `|x| == |y|` signed-zero early-out, an exact-division interior
/// zero, magnitude gaps large enough to run the alignment loop
/// hundreds of iterations, and classic non-terminating binaries
/// (1.0 % 0.1). Operands are parenthesized in the emitted source (here
/// and in every `float_fmod_*` test) so the expectation pins
/// `fmod(a, b)` itself, not Phoenix's unary-minus/`%` precedence —
/// without parens, `-5.5 % 2.0` would only match Rust by the accident
/// that fmod is odd in its dividend, making `-(a % b) == (-a) % b`.
#[test]
fn float_fmod_matches_native() {
    let pairs: &[(&str, &str)] = &[
        ("5.5", "2.0"),
        ("-5.5", "2.0"),
        ("5.5", "-2.0"),
        ("-5.5", "-2.0"),
        ("1.0", "0.1"),
        ("123456.789", "0.001"),
        ("3.0", "3.0"),
        ("-3.0", "3.0"),
        ("2.0", "5.5"),
        ("-2.0", "5.5"),
        ("6.0", "1.5"),
        ("0.3", "0.1"),
        ("100000000000000000000.0", "3.7"),
        ("0.00001", "10000000000000000.0"),
        ("7.25", "0.25"),
    ];

    let mut source = String::from("function main() {\n");
    let mut expected = Vec::new();
    for (a, b) in pairs {
        source.push_str(&format!("  print(({a}) % ({b}))\n"));
        let av: f64 = a.parse().expect("dividend literal parses");
        let bv: f64 = b.parse().expect("divisor literal parses");
        expected.extend_from_slice(ryu::Buffer::new().format(av % bv).as_bytes());
        expected.push(b'\n');
    }
    source.push_str("}\n");
    assert_prints(&source, "float_fmod_corpus", &expected);
}

/// Deterministic subnormal coverage for Float `%`. The random sweep
/// can't reach these (see its doc), so each subnormal path in
/// `phx_fmod` gets a hand-picked pair, fed as exact longhand decimal
/// literals the same way the sweep feeds its operands:
/// - both operands subnormal → both mantissa-normalize loops run
///   (x's leading mantissa bit sits at 43, so its loop iterates;
///   y = 7 × 2⁻¹⁰⁷⁴ makes its loop iterate ~49 times);
/// - a negative largest-subnormal dividend → the x-normalize loop's
///   zero-iteration edge (leading bit already at the top after the
///   `<< 12`), plus the dividend's sign reapplied to a scaled-down
///   subnormal result;
/// - normal % subnormal → the alignment loop's longest practical run
///   (~1074 iterations);
/// - subnormal % normal → the `|x| < |y|` return-x early-out handing
///   back the minimum subnormal untouched;
/// - normal operands with a subnormal remainder
///   (1.5·MIN_POSITIVE % MIN_POSITIVE = 2⁻¹⁰²³) → the `ex <= 0`
///   scale-down re-encoding without either normalize loop.
#[test]
fn float_fmod_subnormals_match_native() {
    let pairs: &[(f64, f64)] = &[
        (
            f64::from_bits(0x0000_0FFF_FFFF_FFFF),
            f64::from_bits(0x0000_0000_0000_0007),
        ),
        (
            f64::from_bits(0x800F_FFFF_FFFF_FFFF),
            f64::from_bits(0x0000_0000_0000_0007),
        ),
        (3.5, f64::from_bits(0x0000_0000_0000_0003)),
        (f64::from_bits(0x0000_0000_0000_0001), 1.0),
        (f64::MIN_POSITIVE * 1.5, f64::MIN_POSITIVE),
    ];

    let mut source = String::from("function main() {\n");
    let mut expected = Vec::new();
    for (a, b) in pairs {
        source.push_str(&format!("  print(({a:.1074}) % ({b:.1074}))\n"));
        expected.extend_from_slice(ryu::Buffer::new().format(a % b).as_bytes());
        expected.push(b'\n');
    }
    source.push_str("}\n");
    assert_prints(&source, "float_fmod_subnormals", &expected);
}

/// Float `%` IEEE-754 special cases, computed at runtime so the bit
/// patterns genuinely reach `phx_fmod` under wasmtime (Phoenix can't
/// lex inf/NaN literals anyway). Each expectation is Rust `%`'s
/// behavior, which `phx_fmod` mirrors via musl's NaN funnel and
/// early-outs:
/// - `x % inf = x` (and preserves a negative dividend); `x % -inf`
///   likewise — the `|x| < |y|` early-out compares with y's sign bit
///   shifted out
/// - `inf % y` / `x % 0.0` / `x % NaN` → NaN
/// - `NaN % y` / `inf % inf` / `0.0 % 0.0` → NaN, each reaching the
///   funnel by a different arm than the trio above: a NaN dividend
///   hits `ex == 0x7ff` with a non-inf mantissa, `inf % inf` trips
///   both the dividend and divisor checks at once, and `0.0 % 0.0`
///   hits the zero-divisor arm with a zero dividend
/// - `0.0 % y = 0.0`; `-0.0 % y = -0.0` (sign of dividend survives)
#[test]
fn float_fmod_special_cases_run_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let inf: Float = 1.0 / 0.0\n",
        "  let neg_inf: Float = -1.0 / 0.0\n",
        "  let nan: Float = 0.0 / 0.0\n",
        "  let neg_zero: Float = -1.0 * 0.0\n",
        "  print(5.0 % inf)\n",
        "  print((-5.0) % inf)\n",
        "  print(5.0 % neg_inf)\n",
        "  print(inf % 5.0)\n",
        "  print(5.0 % 0.0)\n",
        "  print(5.0 % nan)\n",
        "  print(nan % 5.0)\n",
        "  print(inf % inf)\n",
        "  print(0.0 % 0.0)\n",
        "  print(0.0 % 5.0)\n",
        "  print(neg_zero % 5.0)\n",
        "}\n",
    );
    let inf = f64::INFINITY;
    let neg_inf = f64::NEG_INFINITY;
    let nan = f64::NAN;
    let neg_zero = -0.0_f64;
    let mut expected = Vec::new();
    for val in [
        5.0 % inf,
        -5.0 % inf,
        5.0 % neg_inf,
        inf % 5.0,
        5.0 % 0.0,
        5.0 % nan,
        nan % 5.0,
        inf % inf,
        0.0 % 0.0,
        0.0 % 5.0,
        neg_zero % 5.0,
    ] {
        if val.is_nan() {
            expected.extend_from_slice(b"NaN");
        } else {
            // No case yields ±inf (`x % inf` returns x; the rest are
            // NaN), and `ryu` debug-asserts finiteness — a future case
            // that breaks this panics here rather than passing loosely.
            expected.extend_from_slice(ryu::Buffer::new().format(val).as_bytes());
        }
        expected.push(b'\n');
    }
    assert_prints(source, "float_fmod_specials", &expected);
}

/// Differential sweep of Float `%` over arbitrary finite operand
/// pairs — same SplitMix64 construction as
/// [`float_print_random_bits_match_native`]: random bit patterns are
/// uniform over exponents, so the alignment loop runs with magnitude
/// gaps and mantissa shapes a hand-picked corpus wouldn't include.
/// What it does *not* reach: a uniform 11-bit exponent field is
/// subnormal with probability 1/2048 per operand, and the loop
/// asserts that no operand or result is subnormal (or NaN, from a ±0
/// divisor) — those paths are pinned deterministically by
/// [`float_fmod_subnormals_match_native`] and the specials test, and
/// the assertion keeps that division of labor true if the seed or
/// pair count ever changes. 100 pairs, fed as exact longhand decimal
/// literals.
#[test]
fn float_fmod_random_bits_match_native() {
    // SplitMix64 — deterministic, seed pinned (distinct from the
    // print sweep's seed so the two tests cover different values).
    let mut state: u64 = 0x243F6A8885A308D3;
    let mut next = move || {
        state = state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    };
    let mut next_finite = move || loop {
        let val = f64::from_bits(next());
        if val.is_finite() {
            return val;
        }
    };

    let mut source = String::from("function main() {\n");
    let mut expected = Vec::new();
    for _ in 0..100 {
        let a = next_finite();
        let b = next_finite();
        // The doc's claim that this sweep stays out of the subnormal
        // and NaN paths (covered by the subnormals/specials tests) is
        // seed-dependent — enforce it so a seed or count change can't
        // silently shift coverage between the tests.
        for val in [a, b, a % b] {
            assert!(
                val == 0.0 || val.is_normal(),
                "sweep drew a subnormal/NaN ({val:e} from {a:e} % {b:e}); \
                 the seed change moved coverage pinned by the subnormals/\
                 specials tests — restore the no-subnormal property or \
                 update both docs",
            );
        }
        source.push_str(&format!("  print(({a:.1074}) % ({b:.1074}))\n"));
        expected.extend_from_slice(ryu::Buffer::new().format(a % b).as_bytes());
        expected.push(b'\n');
    }
    source.push_str("}\n");
    assert_prints(&source, "float_fmod_random_bits", &expected);
}

/// Pins `phx_fmod`'s pay-per-use claim (§Phase 2.4 K.5: "synthesized
/// only when an `Op::FMod` site exists") structurally, the way
/// [`float_free_module_carries_no_ryu_tables`] pins the K.6 claim —
/// but by function count rather than byte size, since the helper is a
/// few hundred bytes, far too small for a size-delta argument. The two
/// fixtures are identical except `-` vs `%` on the same Float
/// operands, so the only function-count difference an extra entry
/// could come from is the helper itself.
#[test]
fn fmod_free_module_carries_no_fmod_helper() {
    let without_fmod = compile_to_wasm_gc("function main() {\n  print((5.5 - 2.0) > 1.0)\n}\n");
    validate_gc_module(&without_fmod, "fmod_free_without");
    let with_fmod = compile_to_wasm_gc("function main() {\n  print((5.5 % 2.0) > 1.0)\n}\n");
    validate_gc_module(&with_fmod, "fmod_free_with");
    assert_eq!(
        count_local_functions(&with_fmod),
        count_local_functions(&without_fmod) + 1,
        "a `%` module must carry exactly one more function (`phx_fmod`) than the \
         otherwise-identical `-` module — either the helper is missing where needed \
         or it is being synthesized into modules with no `Op::FMod` site",
    );
}

/// PR 6 slice 7 (§Phase 2.4 K.7): the core `List<T>` surface —
/// literal (`Op::ListAlloc` → `array.new_fixed` + `struct.new`),
/// `get`, `length`, and for-in iteration (which the frontend lowers to
/// `List.length` + `List.get`, so it rides the same machinery).
#[test]
fn list_literal_get_length_run_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let xs: List<Int> = [10, 20, 30]\n",
        "  print(xs.length())\n",
        "  print(xs.get(0))\n",
        "  print(xs.get(2))\n",
        "  for x in xs {\n",
        "    print(x)\n",
        "  }\n",
        "}\n",
    );
    assert_prints(source, "list_core_wasm_gc", b"3\n10\n30\n10\n20\n30\n");
}

/// `List.push` is immutable (K.7 / native parity): the result is a
/// fresh list of `len + 1` and the receiver is untouched. `take` /
/// `drop` clamp over-length arguments to the list length ("take/skip
/// at most n"); negative arguments are a runtime error and get their
/// own trap test below.
#[test]
fn list_push_take_drop_run_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let xs: List<Int> = [1, 2, 3]\n",
        "  let ys = xs.push(4)\n",
        "  print(ys.length())\n", // 4
        "  print(xs.length())\n", // 3 — receiver untouched
        "  print(ys.get(3))\n",   // 4
        "  let front = ys.take(2)\n",
        "  print(front.length())\n", // 2
        "  print(front.get(1))\n",   // 2
        "  let back = ys.drop(2)\n",
        "  print(back.length())\n",        // 2
        "  print(back.get(0))\n",          // 3
        "  print(ys.take(99).length())\n", // 4 — clamped to len
        "  print(ys.drop(99).length())\n", // 0 — clamped to len
        "}\n",
    );
    assert_prints(
        source,
        "list_push_take_drop_wasm_gc",
        b"4\n3\n4\n2\n2\n2\n3\n4\n0\n",
    );
}

/// Negative `take` / `drop` arguments trap (2026-06-10 unification,
/// K.7 "Semantics mapping"): every backend errors — the interpreters
/// abort with `take()/drop() argument must be non-negative`, native
/// aborts in `phx_list_take` / `phx_list_drop` (pinned by
/// `take_negative_aborts` / `drop_negative_aborts` in
/// phoenix-runtime), and wasm32-gc traps. The pre-unification native
/// clamp-to-0 was a silent cross-backend divergence this slice
/// surfaced and closed.
#[test]
fn list_take_drop_negative_traps_under_wasmtime_gc() {
    assert_traps(
        concat!(
            "function main() {\n",
            "  let xs: List<Int> = [1, 2, 3]\n",
            "  let neg: Int = 0 - 2\n",
            "  print(xs.take(neg).length())\n",
            "}\n",
        ),
        "list_take_negative_wasm_gc",
    );
    assert_traps(
        concat!(
            "function main() {\n",
            "  let xs: List<Int> = [1, 2, 3]\n",
            "  let neg: Int = 0 - 2\n",
            "  print(xs.drop(neg).length())\n",
            "}\n",
        ),
        "list_drop_negative_wasm_gc",
    );
}

/// `List.contains` per-element-type equality (K.7 "Semantics
/// mapping", matching native's `elements_equal`):
/// - Int: value compare.
/// - Float: IEEE `f64.eq` — NaN is never equal to anything, including
///   itself, so a list containing NaN reports `false` for NaN.
/// - String: byte equality via `phx_str_eq` — a probe string from a
///   *different allocation* than the stored element still matches
///   (this is the line that would catch an accidental `ref.eq`).
#[test]
fn list_contains_runs_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let xs: List<Int> = [1, 2, 3]\n",
        "  print(xs.contains(2))\n", // true
        "  print(xs.contains(9))\n", // false
        "  let fs: List<Float> = [0.5, 2.5]\n",
        "  print(fs.contains(2.5))\n",  // true
        "  print(fs.contains(0.25))\n", // false
        "  let nan: Float = 0.0 / 0.0\n",
        "  let withnan: List<Float> = [nan, 1.0]\n",
        "  print(withnan.contains(nan))\n", // false — NaN != NaN
        "  let ss: List<String> = [\"alpha\", \"beta\"]\n",
        "  print(ss.contains(\"beta\"))\n", // true — byte equality
        "  print(ss.contains(\"gamma\"))\n", // false
        "}\n",
    );
    assert_prints(
        source,
        "list_contains_wasm_gc",
        b"true\nfalse\ntrue\nfalse\nfalse\ntrue\nfalse\n",
    );
}

/// Reference-typed list elements: `List<String>` (elements are
/// `(ref null $string)` — single refs, not the linear backend's fat
/// pointers) and `List<Point>` (struct refs, with field reads through
/// `get`'s result).
#[test]
fn list_of_string_and_struct_elements_run_under_wasmtime_gc() {
    let source = concat!(
        "struct Point {\n  x: Int\n  y: Int\n}\n",
        "function main() {\n",
        "  let names: List<String> = [\"alpha\", \"beta\"]\n",
        "  print(names.get(1))\n",   // beta
        "  print(names.length())\n", // 2
        "  let ps: List<Point> = [Point(1, 2), Point(3, 4)]\n",
        "  print(ps.get(1).x)\n", // 3
        "  for p in ps {\n",
        "    print(p.y)\n", // 2, 4
        "  }\n",
        "}\n",
    );
    assert_prints(source, "list_ref_elements_wasm_gc", b"beta\n2\n3\n2\n4\n");
}

/// Nested `List<List<Int>>` — the inner instantiation's `$list_T` is
/// the element ValType of the outer's `$arr_T`, so this pins the K.7
/// inner-before-outer declaration ordering end-to-end.
#[test]
fn nested_list_runs_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let xss: List<List<Int>> = [[1, 2], [3, 4, 5]]\n",
        "  print(xss.length())\n",        // 2
        "  print(xss.get(0).length())\n", // 2
        "  print(xss.get(1).get(2))\n",   // 5
        "  for xs in xss {\n",
        "    print(xs.length())\n", // 2, 3
        "  }\n",
        "}\n",
    );
    assert_prints(source, "nested_list_wasm_gc", b"2\n2\n5\n2\n3\n");
}

/// Out-of-bounds `List.get` traps — both the past-the-end index and a
/// negative index (which the single unsigned compare catches by
/// wrapping to a huge u64). Native prints `runtime error: list index …
/// out of bounds` and exits 1; wasm32-gc follows the established trap
/// precedent (K.7 "Semantics mapping").
#[test]
fn list_get_out_of_bounds_traps_under_wasmtime_gc() {
    assert_traps(
        "function main() {\n  let xs: List<Int> = [1, 2, 3]\n  print(xs.get(3))\n}\n",
        "list_get_oob_high_wasm_gc",
    );
    assert_traps(
        concat!(
            "function main() {\n",
            "  let xs: List<Int> = [1, 2, 3]\n",
            "  let neg: Int = 0 - 1\n",
            "  print(xs.get(neg))\n",
            "}\n",
        ),
        "list_get_oob_negative_wasm_gc",
    );
}

/// `ListBuilder<Int>` end-to-end (K.7): alloc → 12 pushes (the buffer
/// starts at capacity 8, so this crosses one 2× growth) → zero-copy
/// freeze → read the frozen list. The frozen list's `$len` (12) is
/// smaller than its shared buffer (16 slots) — `length` and iteration
/// must read `$len`, not the array size; the expected output pins
/// that.
#[test]
fn list_builder_push_freeze_runs_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let b: ListBuilder<Int> = List.builder()\n",
        "  let mut i: Int = 0\n",
        "  while (i < 12) {\n",
        "    b.push(i * i)\n",
        "    i = i + 1\n",
        "  }\n",
        "  let frozen = b.freeze()\n",
        "  print(frozen.length())\n",
        "  for x in frozen {\n",
        "    print(x)\n",
        "  }\n",
        "}\n",
    );
    let mut expected = b"12\n".to_vec();
    for i in 0..12i64 {
        expected.extend_from_slice(format!("{}\n", i * i).as_bytes());
    }
    assert_prints(source, "list_builder_wasm_gc", &expected);
}

/// Every list op on a *frozen* list whose shared buffer carries
/// capacity slack (`$len` 12, buffer 16 — the only state where
/// `$len != array.len`). The slack slots hold `array.new_default`'s
/// zero, so the squares pushed start at 1 and the sharpest line is
/// `contains(0) == false`: a scan keyed on `array.len` instead of
/// `$len` would walk into the slack and find the default. Likewise
/// `take(99)` must clamp to `$len` (12), not the 16-slot buffer, and
/// `push` must copy exactly `$len` elements into its fresh array.
#[test]
fn frozen_list_with_capacity_slack_ops_run_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let b: ListBuilder<Int> = List.builder()\n",
        "  let mut i: Int = 1\n",
        "  while (i <= 12) {\n",
        "    b.push(i * i)\n", // 1, 4, …, 144 — never 0
        "    i = i + 1\n",
        "  }\n",
        "  let frozen = b.freeze()\n",
        "  print(frozen.length())\n",      // 12 — $len, not array.len
        "  print(frozen.contains(0))\n",   // false — must not scan slack
        "  print(frozen.contains(144))\n", // true
        "  let pushed = frozen.push(169)\n",
        "  print(pushed.length())\n",          // 13
        "  print(pushed.get(12))\n",           // 169
        "  print(frozen.length())\n",          // 12 — receiver untouched
        "  print(frozen.take(99).length())\n", // 12 — clamps to $len, not 16
        "  let back = frozen.drop(10)\n",
        "  print(back.length())\n", // 2
        "  print(back.get(0))\n",   // 121
        "  print(back.get(1))\n",   // 144
        "}\n",
    );
    assert_prints(
        source,
        "frozen_list_slack_ops_wasm_gc",
        b"12\nfalse\ntrue\n13\n169\n12\n12\n2\n121\n144\n",
    );
}

/// `get` on a frozen list bounds-checks against `$len`, not the
/// buffer: index 12 is inside the 16-slot shared buffer but past the
/// frozen `$len` of 12, and must trap rather than read a slack slot.
#[test]
fn frozen_list_get_in_slack_traps_under_wasmtime_gc() {
    assert_traps(
        concat!(
            "function main() {\n",
            "  let b: ListBuilder<Int> = List.builder()\n",
            "  let mut i: Int = 1\n",
            "  while (i <= 12) {\n",
            "    b.push(i * i)\n",
            "    i = i + 1\n",
            "  }\n",
            "  let frozen = b.freeze()\n",
            "  print(frozen.get(12))\n",
            "}\n",
        ),
        "frozen_list_slack_get_oob_wasm_gc",
    );
}

/// Use-after-freeze traps (native aborts with `builder was already
/// frozen`): the frozen flag is what makes the K.7 zero-copy freeze
/// sound — the shared buffer must never be written after the list
/// takes it.
#[test]
fn list_builder_push_after_freeze_traps_under_wasmtime_gc() {
    assert_traps(
        concat!(
            "function main() {\n",
            "  let b: ListBuilder<Int> = List.builder()\n",
            "  b.push(1)\n",
            "  let f = b.freeze()\n",
            "  b.push(2)\n",
            "  print(f.length())\n",
            "}\n",
        ),
        "list_builder_push_after_freeze_wasm_gc",
    );
}

/// Double-`freeze()` traps. Distinct from the push-after-freeze test
/// above: `translate_list_builder_freeze` carries its own `$frozen`
/// check (separate emission from push's), so this pins the second
/// guard independently. Native aborts with `builder was already
/// frozen` on the same call.
#[test]
fn list_builder_double_freeze_traps_under_wasmtime_gc() {
    assert_traps(
        concat!(
            "function main() {\n",
            "  let b: ListBuilder<Int> = List.builder()\n",
            "  b.push(1)\n",
            "  let f = b.freeze()\n",
            "  print(b.freeze().length())\n",
            "  print(f.length())\n",
            "}\n",
        ),
        "list_builder_double_freeze_wasm_gc",
    );
}

/// `List.contains` over struct elements uses `ref.eq` *identity* —
/// the exact analogue of native's bytewise compare of the stored
/// 8-byte pointer (`elements_equal` with `is_string = false`): the
/// same instance is found, a structurally-equal fresh instance is
/// not. Pins the `Cmp::RefIdentity` arm, which the Int/Float/String
/// contains test cannot reach.
#[test]
fn list_contains_struct_elements_compare_by_identity_under_wasmtime_gc() {
    let source = concat!(
        "struct Point {\n  x: Int\n  y: Int\n}\n",
        "function main() {\n",
        "  let p: Point = Point(1, 2)\n",
        "  let ps: List<Point> = [p, Point(3, 4)]\n",
        "  print(ps.contains(p))\n",           // true — same instance
        "  print(ps.contains(Point(1, 2)))\n", // false — equal shape, distinct allocation
        "}\n",
    );
    assert_prints(
        source,
        "list_contains_ref_identity_wasm_gc",
        b"true\nfalse\n",
    );
}

/// Boundary sizes: an empty list literal (`array.new_fixed` with size
/// 0), pushing onto it, and `take(0)` / `drop(0)` (zero-length
/// `array.copy` at both offset extremes).
#[test]
fn empty_list_and_zero_slices_run_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let xs: List<Int> = []\n",
        "  print(xs.length())\n", // 0
        "  let ys = xs.push(7)\n",
        "  print(ys.length())\n", // 1
        "  print(ys.get(0))\n",   // 7
        "  let zs: List<Int> = [1, 2, 3]\n",
        "  print(zs.take(0).length())\n", // 0
        "  print(zs.drop(0).length())\n", // 3
        "  print(zs.drop(0).get(2))\n",   // 3
        "}\n",
    );
    assert_prints(
        source,
        "empty_list_zero_slices_wasm_gc",
        b"0\n1\n7\n0\n3\n3\n",
    );
}

/// An `Op::ListAlloc` whose element type is still the `__generic`
/// placeholder (an unconstrained empty literal surviving to codegen)
/// gets the annotate-your-list diagnostic — not the
/// internal-compiler-bug message, since the K.7 collection pass
/// *deliberately* skips placeholders. Sema front-runs the `let xs =
/// []` surface form ("cannot infer type … add a type annotation"), so
/// this is built straight from IR, mirroring
/// [`struct_field_index_out_of_range_is_rejected`].
#[test]
fn list_alloc_with_placeholder_element_gets_annotate_diagnostic() {
    let mut func = IrFunction::new(
        FuncId(0),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let entry = func.create_block();
    func.emit_value(
        entry,
        Op::ListAlloc(Vec::new()),
        IrType::ListRef(Box::new(IrType::StructRef(
            phoenix_ir::types::GENERIC_PLACEHOLDER.to_string(),
            Vec::new(),
        ))),
        None,
    );
    func.set_terminator(entry, Terminator::Return(None));

    let mut module = IrModule::new();
    module.push_concrete(func);

    let err = compile(&module, Target::Wasm32Gc)
        .expect_err("a placeholder-element ListAlloc must be rejected with a user-facing hint");
    let msg = err.to_string();
    assert!(
        msg.contains("never constrained") && msg.contains("Annotate the list"),
        "expected the annotate-your-list diagnostic, got: {msg}"
    );
}

/// `List<Bool>.contains` — the `Cmp::I32` equality arm, which the
/// Int/Float/String contains test cannot reach (Bool is the only
/// i32-slot element type). Also reads a Bool element back through
/// `get` to pin the i32 element ValType end-to-end.
#[test]
fn list_of_bool_contains_runs_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let bs: List<Bool> = [true, true]\n",
        "  print(bs.contains(true))\n",  // true
        "  print(bs.contains(false))\n", // false
        "  print(bs.get(1))\n",          // true
        "  let fs: List<Bool> = [false]\n",
        "  print(fs.contains(true))\n",  // false
        "  print(fs.contains(false))\n", // true
        "}\n",
    );
    assert_prints(
        source,
        "list_bool_contains_wasm_gc",
        b"true\nfalse\ntrue\nfalse\ntrue\n",
    );
}

/// Enum-typed list elements (K.7 scope: "Int, Float, Bool, String,
/// structs, enums, and nested lists") — elements are `(ref null
/// $enum_parent)` refs into the K.4 type hierarchy, declared before
/// the list pass so the `$arr_T` element encodes the parent's index.
/// `get`'s result flows into a `match` (EnumDiscriminant + ref.cast
/// field read through a list-loaded value), and `contains` compares by
/// `ref.eq` identity like structs: the same instance is found, a
/// structurally-equal fresh instance is not.
#[test]
fn list_of_enum_elements_runs_under_wasmtime_gc() {
    let source = concat!(
        "enum Shape {\n",
        "  Circle(Int)\n",
        "  Square\n",
        "}\n",
        "function area(s: Shape) -> Int {\n",
        "  match s {\n",
        "    Circle(r) -> r * r * 3\n",
        "    Square -> 1\n",
        "  }\n",
        "}\n",
        "function main() {\n",
        "  let c: Shape = Circle(4)\n",
        "  let shapes: List<Shape> = [c, Square, Circle(2)]\n",
        "  print(shapes.length())\n",     // 3
        "  print(area(shapes.get(0)))\n", // 48
        "  print(area(shapes.get(1)))\n", // 1
        "  for s in shapes {\n",
        "    print(area(s))\n", // 48, 1, 12
        "  }\n",
        "  print(shapes.contains(c))\n", // true — same instance
        "  print(shapes.contains(Circle(4)))\n", // false — fresh allocation
        "}\n",
    );
    assert_prints(
        source,
        "list_enum_elements_wasm_gc",
        b"3\n48\n1\n48\n1\n12\ntrue\nfalse\n",
    );
}

/// `ListBuilder<String>` — a ref-element builder, which the Int
/// builder test cannot reach on two paths: `array.new_default` must
/// null-initialize the capacity slack for a *nullable-ref* element
/// type (the K.7 nullability rationale), and the 2× growth
/// `array.copy` moves refs rather than i64s. 12 pushes cross one
/// growth from the initial capacity of 8; the frozen list's `$len`
/// (12) stays below the shared 16-slot buffer, so reading every
/// element back also pins that no null slack slot is ever touched.
#[test]
fn list_builder_string_elements_run_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let b: ListBuilder<String> = List.builder()\n",
        "  let s: String = \"s\"\n",
        "  let mut i: Int = 0\n",
        "  while (i < 12) {\n",
        // Interpolation lowers to Op::StringConcat — a fresh `$string`
        // allocation per push, so the `contains` probe below is a
        // different allocation than every stored element.
        "    b.push(\"{s}!\")\n",
        "    i = i + 1\n",
        "  }\n",
        "  let frozen = b.freeze()\n",
        "  print(frozen.length())\n",
        "  for s in frozen {\n",
        "    print(s)\n",
        "  }\n",
        "  print(frozen.contains(\"s!\"))\n", // true — byte equality, fresh probe
        "}\n",
    );
    let mut expected = b"12\n".to_vec();
    for _ in 0..12 {
        expected.extend_from_slice(b"s!\n");
    }
    expected.extend_from_slice(b"true\n");
    assert_prints(source, "list_builder_string_wasm_gc", &expected);
}

/// Lists crossing *function boundaries* — every prior list test keeps
/// its lists inside `main`, so `wasm_valtypes_for`'s `ListRef` arm was
/// only exercised for locals, never for the param/return positions
/// where the signature interning order ("lists declared before any
/// signature touching `ListRef`") actually bites. Covers a `List<Int>`
/// param, a `List<Int>` return, a call result fed straight back into a
/// param (no intermediate local), and a ref-element `List<String>`
/// param.
#[test]
fn list_across_function_boundary_runs_under_wasmtime_gc() {
    let source = concat!(
        "function sum(xs: List<Int>) -> Int {\n",
        "  let mut total: Int = 0\n",
        "  for x in xs {\n",
        "    total = total + x\n",
        "  }\n",
        "  return total\n",
        "}\n",
        "function tail(xs: List<Int>) -> List<Int> {\n",
        "  return xs.drop(1)\n",
        "}\n",
        "function firstName(names: List<String>) -> String {\n",
        "  return names.get(0)\n",
        "}\n",
        "function main() {\n",
        "  let xs: List<Int> = [1, 2, 3, 4]\n",
        "  print(sum(xs))\n", // 10
        "  let t = tail(xs)\n",
        "  print(t.length())\n",   // 3
        "  print(t.get(0))\n",     // 2
        "  print(sum(tail(t)))\n", // 7 — call result straight into a param
        "  let names: List<String> = [\"ada\", \"grace\"]\n",
        "  print(firstName(names))\n", // ada
        "}\n",
    );
    assert_prints(
        source,
        "list_across_fn_boundary_wasm_gc",
        b"10\n3\n2\n7\nada\n",
    );
}

/// A module whose *only* mention of `String` is inside `List<String>`
/// — no string literal, no `print(String)`, so no instruction or
/// signature anywhere has the bare `IrType::StringRef` type the old
/// exact-match helper scan looked for (the empty literal is the one
/// way to build a `List<String>` without `ConstString` instructions).
/// Declaring `$arr_String` still needs the `$string` index, so
/// `scan_helper_needs` must flag `string_types` from the *nested*
/// type — the `type_contains_string` recursion. Under the old
/// `== IrType::StringRef` check this module failed to compile.
#[test]
fn list_of_string_with_no_bare_string_use_compiles_and_runs() {
    let source = concat!(
        "function count(xs: List<String>) -> Int {\n",
        "  return xs.length()\n",
        "}\n",
        "function main() {\n",
        "  let names: List<String> = []\n",
        "  print(count(names))\n", // 0
        "}\n",
    );
    assert_prints(source, "list_string_signature_only_wasm_gc", b"0\n");
}

/// A list literal beyond `array.new_fixed`'s 10 000-operand engine cap
/// is rejected at compile time with the build-it-with-`ListBuilder`
/// hint, not an opaque downstream validation error. The source is
/// generated (10 001 elements) rather than committed as a fixture.
/// Exactly 10 000 elements is still in spec, so the guard must not
/// fire early — pinned by compiling the boundary size too.
#[test]
fn list_literal_over_array_new_fixed_cap_is_rejected() {
    let make_source = |n: usize| {
        let elems = (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
        format!("function main() {{\n  let xs: List<Int> = [{elems}]\n  print(xs.length())\n}}\n")
    };
    let err = compile(&lower_to_ir(&make_source(10_001)), Target::Wasm32Gc)
        .expect_err("a 10 001-element literal can never validate on this target");
    let msg = err.to_string();
    assert!(
        msg.contains("array.new_fixed") && msg.contains("ListBuilder"),
        "expected the over-cap diagnostic with the ListBuilder hint, got: {msg}"
    );
    // Boundary: exactly 10 000 elements compiles and validates.
    let bytes = compile(&lower_to_ir(&make_source(10_000)), Target::Wasm32Gc)
        .expect("a 10 000-element literal is exactly at the engine cap and must compile");
    validate_gc_module(&bytes, "list_literal_at_cap_wasm_gc");
}

/// `toString` across the supported argument types (PR 6 toString
/// slice). Output must be byte-identical to the other backends:
/// `toString(Int)` is Rust `i64::to_string`, `toString(Bool)` is
/// `"true"`/`"false"`, `toString(String)` is the identity. Funneled
/// through `print(String)` (and concat) so the constructed `$string`
/// values are observed end-to-end, not just type-checked.
#[test]
fn tostring_int_bool_string_run_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  print(toString(0))\n",
        "  print(toString(42))\n",
        "  let neg: Int = 0 - 9876543210\n",
        "  print(toString(neg))\n",
        "  print(toString(true))\n",
        "  print(toString(false))\n",
        "  print(toString(\"already\"))\n",
        "  print(\"n=\" + toString(7))\n",
        "}\n",
    );
    assert_prints(
        source,
        "tostring_core_wasm_gc",
        b"0\n42\n-9876543210\ntrue\nfalse\nalready\nn=7\n",
    );
}

/// `toString(Float)` reuses `phx_ryu_format_f64`, so its bytes must
/// match the `ryu` crate exactly — same oracle as the `float_print_*`
/// corpus, but exercised through `$string` construction + concat
/// instead of the print fast path. NaN / ±inf are covered explicitly:
/// their literal arm of the formatter (bytes staged by `write_literal`,
/// length returned) flows through `phx_tostring_f64`'s copy loop here,
/// a path the digit values never reach.
#[test]
fn tostring_float_matches_native() {
    let values: &[&str] = &[
        "0.0",
        "1.5",
        "-1.5",
        "0.1",
        "100.0",
        "0.000001",
        "123456.789",
    ];
    let mut source = String::from("function main() {\n");
    let mut expected = Vec::new();
    for lit in values {
        source.push_str(&format!("  print(\"v=\" + toString({lit}))\n"));
        let val: f64 = lit.parse().unwrap();
        expected.extend_from_slice(b"v=");
        expected.extend_from_slice(ryu::Buffer::new().format(val).as_bytes());
        expected.push(b'\n');
    }
    // NaN / ±inf have no Phoenix literals, so the fixture computes
    // them — same workaround as `print_float_specials`.
    let specials: &[(&str, &str, f64)] = &[
        ("nan", "0.0 / 0.0", f64::NAN),
        ("pinf", "1.0 / 0.0", f64::INFINITY),
        ("ninf", "-1.0 / 0.0", f64::NEG_INFINITY),
    ];
    for (name, expr, val) in specials {
        source.push_str(&format!("  let {name}: Float = {expr}\n"));
        source.push_str(&format!("  print(\"v=\" + toString({name}))\n"));
        expected.extend_from_slice(b"v=");
        expected.extend_from_slice(ryu::Buffer::new().format(*val).as_bytes());
        expected.push(b'\n');
    }
    source.push_str("}\n");
    assert_prints(&source, "tostring_float_wasm_gc", &expected);
}

/// String interpolation lowers every non-String hole through
/// `toString` + concat — the end-to-end path the fizzbuzz / features
/// fixtures lean on. Pins a mixed-type interpolation.
#[test]
fn string_interpolation_runs_under_wasmtime_gc() {
    let source = concat!(
        "function main() {\n",
        "  let n: Int = 3\n",
        "  let f: Float = 2.5\n",
        "  let b: Bool = true\n",
        "  print(\"n={n} f={f} b={b}\")\n",
        "}\n",
    );
    assert_prints(source, "interpolation_wasm_gc", b"n=3 f=2.5 b=true\n");
}

/// String-typed struct fields (§Phase 2.4 K.1): `$string` is declared
/// ahead of the structs, so a `name: String` field encodes
/// `(ref null $string)` directly.
/// Exercises the full field lifecycle — construct with a literal,
/// read + print, reassign via `struct.set` (with a concat-built
/// value so the new ref is a fresh allocation), pass the struct
/// through a user function, and mix with primitive fields.
#[test]
fn struct_string_field_runs_under_wasmtime_gc() {
    let source = concat!(
        "struct User {\n",
        "  name: String\n",
        "  age: Int\n",
        "}\n",
        "function describe(u: User) -> String {\n",
        "  u.name + \":\" + toString(u.age)\n",
        "}\n",
        "function main() {\n",
        "  let mut u: User = User(\"ada\", 36)\n",
        "  print(u.name)\n",
        "  print(u.age)\n",
        "  u.name = u.name + \"!\"\n",
        "  print(u.name)\n",
        "  print(describe(u))\n",
        "}\n",
    );
    assert_prints(
        source,
        "struct_string_field_wasm_gc",
        b"ada\n36\nada!\nada!:36\n",
    );
}

/// Pins the `scan_helper_needs` struct-layout backstop: a struct
/// whose `String` field is the *only*
/// string in the program. No literal, concat, or `print(String)`
/// appears anywhere the function-body walk would find — `age`'s param
/// is `StructRef("User", [])`, whose `type_contains_string` only
/// inspects generic args — so `HelperNeeds::string_types` is set
/// solely by the layout scan. Without that scan,
/// `declare_phoenix_structs` would hit `wasm_field_type_for`'s
/// internal-compiler-bug error for the missing `$string` index.
/// Compile-only: the structural assertion doesn't need wasmtime.
#[test]
fn string_field_struct_without_string_ops_compiles() {
    let source = concat!(
        "struct User {\n",
        "  name: String\n",
        "  age: Int\n",
        "}\n",
        "function age(u: User) -> Int {\n",
        "  u.age\n",
        "}\n",
        "function main() {\n",
        "  print(7)\n",
        "}\n",
    );
    let bytes = compile_to_wasm_gc(source);
    validate_gc_module(&bytes, "string_field_no_string_ops");
    // `User` + `$string` — both nominal struct declarations must be
    // present (`$bytes` is an array type and doesn't count).
    assert_eq!(
        count_struct_type_decls(&bytes),
        2,
        "expected the `User` struct and `$string` to both be declared"
    );
}

/// The enum twin of the backstop test above: declaring an enum
/// instantiation declares *every* variant struct (§Phase 2.4 K.4), so
/// `S(String)`'s field needs the `$string` index even though `S` is
/// never constructed and the program touches no string otherwise.
/// `e`'s type is `EnumRef("E", [])`, whose `type_contains_string`
/// only inspects generic args, and no match arm binds the payload —
/// so `HelperNeeds::string_types` is set solely by
/// `scan_helper_needs`'s enum-instantiation scan. Without that scan,
/// `wasm_enum_field_type_for` would hit its internal-compiler-bug
/// error for the missing `$string` index. Compile-only, like the
/// struct twin.
#[test]
fn string_variant_enum_without_string_ops_compiles() {
    let source = concat!(
        "enum E {\n",
        "  N\n",
        "  S(String)\n",
        "}\n",
        "function tag(e: E) -> Int {\n",
        "  7\n",
        "}\n",
        "function main() {\n",
        "  let e: E = N\n",
        "  print(tag(e))\n",
        "}\n",
    );
    let bytes = compile_to_wasm_gc(source);
    validate_gc_module(&bytes, "string_variant_no_string_ops");
    // One parent + two variant subtypes — `S` must be declared (with
    // its `$string`-typed field) even though it is never constructed.
    let (parents, variants) = count_enum_type_decls(&bytes);
    assert_eq!(parents, 1, "expected 1 enum parent type, got {parents}");
    assert_eq!(variants, 2, "expected 2 variant subtypes, got {variants}");
}

/// Generic struct *templates* survive in `struct_layouts` alongside
/// their monomorphized instances (`Container` with a `TypeVar("T")`
/// field next to `Container__i64`). `declare_phoenix_structs` must
/// skip the template rather than trip the field-type restriction on
/// `TypeVar` — concrete code only ever references the instances.
/// The decl count pins both directions: a regression that declares
/// the template fails compilation (loud), one that drops an instance
/// shifts the count.
#[test]
fn generic_struct_template_is_skipped_not_declared() {
    let source = concat!(
        "struct Container<T> {\n",
        "  value: T\n",
        "}\n",
        "function main() {\n",
        "  let a: Container<Int> = Container(42)\n",
        "  let b: Container<String> = Container(\"hi\")\n",
        "  print(a.value)\n",
        "  print(b.value)\n",
        "}\n",
    );
    let bytes = compile_to_wasm_gc(source);
    let decls = count_struct_type_decls(&bytes);
    assert_eq!(
        decls, 3,
        "expected `Container__i64` + `Container__string` + `$string` \
         (template skipped), got {decls}"
    );
    assert_wasm_prints(&bytes, "generic_struct_template_wasm_gc", b"42\nhi\n");
}

/// A template whose type param appears in *no* field (`struct
/// Phantom<T> { name: String }`) leaves no generic placeholder for a
/// field scan to find — it is identified as a template solely by its
/// `IrModule::struct_type_params` entry. Uninstantiated, it must
/// neither be declared (a dead WASM type) nor have its String field
/// force `$bytes`/`$string` into the module via `scan_helper_needs`'s
/// layout scan. The zero decl count pins both: a regression to
/// field-based template detection declares `Phantom` *and* drags
/// `$string` in, shifting the count to 2.
#[test]
fn phantom_param_template_is_skipped_and_forces_no_string_types() {
    let source = concat!(
        "struct Phantom<T> {\n",
        "  name: String\n",
        "}\n",
        "function main() {\n",
        "  print(7)\n",
        "}\n",
    );
    let bytes = compile_to_wasm_gc(source);
    validate_gc_module(&bytes, "phantom_param_template");
    assert_eq!(
        count_struct_type_decls(&bytes),
        0,
        "expected no struct type decls: the phantom-param template is \
         skipped and its String field must not force `$string` in"
    );
}

/// Pins the module-size claim at the heart of §Phase 2.4 K.6's
/// inline-synthesis decision: a module that never prints a Float —
/// and never `toString`s one, the second trigger for the formatter
/// chain — carries neither the synthesized `phx_ryu_format_f64`
/// machinery nor the ~9.6 KiB of power-of-5 tables. Proven
/// structurally, with no dependency on absolute module sizes (which
/// drift as codegen evolves):
///
/// - the Float-free module is *smaller than the table payload alone*,
///   so it cannot possibly contain the tables;
/// - the otherwise-identical Float-printing module is larger by at
///   least the table payload, confirming the tables land where (and
///   only where) `print(Float)` / `toString(Float)` appears.
#[test]
fn float_free_module_carries_no_ryu_tables() {
    // (291 + 325) entries × 16 bytes. Keep in sync with the
    // `*_TABLE_SIZE` constants in `src/wasm/wasm_gc/ryu_tables.rs`.
    const RYU_TABLE_BYTES: usize = (291 + 325) * 16;

    let int_only = compile_to_wasm_gc("function main() {\n  print(1)\n}\n");
    validate_gc_module(&int_only, "float_free_int_only");
    assert!(
        int_only.len() < RYU_TABLE_BYTES,
        "Float-free module is {} bytes — at least as large as the {RYU_TABLE_BYTES} bytes \
         of ryu power-of-5 tables, so it may be carrying them; the K.6 \
         pay-only-when-printing-Float size claim has regressed",
        int_only.len(),
    );

    let with_float = compile_to_wasm_gc("function main() {\n  print(1.0)\n}\n");
    validate_gc_module(&with_float, "float_free_with_float");
    assert!(
        with_float.len() >= int_only.len() + RYU_TABLE_BYTES,
        "print(Float) module ({} bytes) is not at least {RYU_TABLE_BYTES} bytes larger than \
         the Float-free module ({} bytes) — the power-of-5 data segments appear to be missing \
         or truncated",
        with_float.len(),
        int_only.len(),
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

/// `5.5 % 2.0 = 1.5 > 1.0` → `true`, with the
/// result funneled through `print(Bool)`.
#[test]
fn float_mod_runs_under_wasmtime_gc() {
    assert_prints(
        "function main() {\n  let a: Float = 5.5\n  let b: Float = 2.0\n  print((a % b) > 1.0)\n}\n",
        "float_mod_wasm_gc",
        b"true\n",
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

/// A struct field whose type isn't yet supported (here a nested
/// `StructRef`) must surface a clear per-field diagnostic — not
/// silently emit a partial declaration that later trips up
/// `wasmparser` with an "unexpected field type" deep inside the binary
/// format. The error keeps the backend from masking work that belongs
/// to follow-up slices.
#[test]
fn struct_with_nested_struct_field_is_rejected_until_a_later_slice() {
    let source = concat!(
        "struct Inner {\n",
        "  v: Int\n",
        "}\n",
        "struct Outer {\n",
        "  inner: Inner\n",
        "}\n",
        "function main() {\n",
        "  let o: Outer = Outer(Inner(1))\n",
        "  print(o.inner.v)\n",
        "}\n",
    );
    let ir_module = lower_to_ir(source);
    let err = compile(&ir_module, Target::Wasm32Gc)
        .expect_err("nested struct fields are not yet supported on wasm32-gc");
    let msg = err.to_string();
    assert!(
        msg.contains("Outer") && msg.contains("inner"),
        "expected a per-field diagnostic naming the unsupported field, got: {msg}"
    );
}

/// A `List<String>` field is the shape where `scan_helper_needs`'s
/// struct-layout backstop fires (`type_contains_string` recurses into
/// the element type) but the field itself is still unsupported: the
/// compile must fail on `wasm_field_type_for`'s per-field diagnostic,
/// not slip past it because `$string` happens to be declared.
#[test]
fn struct_with_list_string_field_is_rejected_until_a_later_slice() {
    let source = concat!(
        "struct Doc {\n",
        "  tags: List<String>\n",
        "}\n",
        "function main() {\n",
        "  print(7)\n",
        "}\n",
    );
    let ir_module = lower_to_ir(source);
    let err = compile(&ir_module, Target::Wasm32Gc)
        .expect_err("a List-typed struct field is not yet supported on wasm32-gc");
    let msg = err.to_string();
    assert!(
        msg.contains("Doc") && msg.contains("tags"),
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
        "  x: Int\n",
        "  y: Int\n",
        "}\n",
        "struct Pixel {\n",
        "  x: Int\n",
        "  y: Int\n",
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
/// Shape: `Flags { on: Bool, n: Int }`; `main` builds `Flags(true, 42)` and
/// prints `n` iff `on`, else `0`.
///
/// Expected stdout: `42\n`.
#[test]
fn struct_bool_field_runs_under_wasmtime_gc() {
    let source = concat!(
        "struct Flags {\n",
        "  on: Bool\n",
        "  n: Int\n",
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
