//! Shared implementation for the `phoenix` and `phoenix-gen` binaries.
//!
//! The Phoenix CLI driver wires together the lexer, parser, semantic checker,
//! interpreter, IR pipeline, and Cranelift backend. The bulk of that work lives
//! here, in a library crate, so that the full `phoenix` binary and the
//! standalone `phoenix-gen` binary call into ONE implementation of the code
//! generation pipeline and cannot drift.
//!
//! `main.rs` (the `phoenix` binary) and `src/bin/phoenix-gen.rs` (the
//! `phoenix-gen` binary) are thin clap front-ends over the functions exported
//! here; in particular both route code generation through [`run_gen`].
#![warn(missing_docs)]

pub mod build;
pub mod config;
pub mod deps;
pub mod manifest;

use std::fs;
use std::path::Path;
use std::process;

use config::PhoenixConfig;
use phoenix_codegen::GenMode;
use phoenix_common::source::SourceMap;
use phoenix_common::span::SourceId;
use phoenix_interp::interpreter;
use phoenix_lexer::lexer::tokenize;
use phoenix_modules::{ResolveError, ResolvedSourceModule};
use phoenix_parser::parser;
use phoenix_sema::checker;

/// Runs the `gen` command: generates typed code (or an OpenAPI spec) from a
/// Phoenix schema file.
///
/// This is the single shared entry point used by both the `phoenix gen`
/// subcommand and the standalone `phoenix-gen` binary, so their behavior cannot
/// diverge. The arguments mirror the `Gen` subcommand's CLI fields exactly:
///
/// - `file`: schema file path; falls back to `gen.schema` in `phoenix.toml`.
/// - `target`: target language (`typescript`, `python`, `go`, `openapi`);
///   `None` falls back to config targets, then defaults to `typescript`.
/// - `out`: output directory; `None` falls back to config, then `./generated`.
/// - `client` / `server`: restrict generation to client- or server-only code.
/// - `watch`: re-generate on `.phx` file changes.
pub fn run_gen(
    file: Option<String>,
    target: Option<String>,
    out: Option<String>,
    client: bool,
    server: bool,
    watch: bool,
    framework: Option<String>,
) {
    // Load phoenix.toml if present
    let cwd = std::env::current_dir().unwrap_or_default();
    let config = match PhoenixConfig::find_and_load(&cwd) {
        Ok(Some(c)) => c,
        Ok(None) => PhoenixConfig::default(),
        Err(e) => {
            eprintln!("error: {}", e);
            process::exit(1);
        }
    };

    // Resolve schema file: CLI > config
    let file = file.or(config.codegen.schema.clone()).unwrap_or_else(|| {
        eprintln!(
            "error: no schema file specified\n\
             Pass a file argument: phoenix gen <FILE>\n\
             Or set gen.schema in phoenix.toml"
        );
        process::exit(1);
    });

    // CLI mode override from --client/--server flags
    let cli_mode = if client {
        Some(GenMode::ClientOnly)
    } else if server {
        Some(GenMode::ServerOnly)
    } else {
        None
    };

    // Determine targets to generate:
    // 1. CLI --target overrides everything (single target)
    // 2. Config targets map (multi-target)
    // 3. Config single target
    // 4. Default: typescript
    if let Some(cli_target) = target {
        // CLI specifies a single target — use it with CLI out/mode. Strictness
        // follows the framework's provenance (see `resolve_target_framework`): a
        // CLI `--framework` is bound to this chosen target (a typo errors), but a
        // value falling through to the top-level config default is bound only when
        // it was written for this exact target — a single-target config whose
        // `target` is the one we're generating (the same case `resolve_targets`
        // marks `framework_explicit`). Otherwise it's tolerated, so a config whose
        // `framework` was meant for a different target (e.g. `--target go` over a
        // TS config) can't abort the run.
        let out = out
            .or(config.codegen.out_dir)
            .unwrap_or_else(|| "./generated".to_string());
        let mode = cli_mode.unwrap_or_else(|| parse_mode(config.codegen.mode.as_deref()));
        let strict = cli_target_framework_is_bound(
            framework.is_some(),
            config.codegen.targets.is_none(),
            config.codegen.target.as_deref() == Some(cli_target.as_str()),
        );
        let raw_fw = framework.as_deref().or(config.codegen.framework.as_deref());
        let fw = resolve_target_framework_or_exit(&cli_target, raw_fw, strict);
        if watch {
            cmd_gen_watch(&file, &[(&cli_target, out.as_str(), mode, fw)]);
        } else {
            cmd_gen(&file, &cli_target, &out, mode, fw.as_deref());
        }
    } else if let Some(resolved) = config.codegen.resolve_targets() {
        // Config provides target(s) — run them all. A CLI `--framework` overrides
        // each target's configured framework. Framework provenance decides
        // strictness (see `resolve_target_framework`): a per-target
        // `[gen.targets.<name>] framework` is bound to its target (a typo errors);
        // a top-level `[gen] framework` is a broadcast default, tolerated where a
        // target can't use it. A global CLI `--framework` is bound only when it
        // lands on a single target — across several it's a broadcast too.
        let single = resolved.len() == 1;
        let cli_framework = framework.is_some();
        let resolve_fw = |rt: &config::ResolvedTarget| -> Option<String> {
            let raw_fw = framework.as_deref().or(rt.framework.as_deref());
            let strict = framework_is_bound(cli_framework, single, rt.framework_explicit);
            resolve_target_framework_or_exit(&rt.target, raw_fw, strict)
        };
        // A global CLI `--framework` across several targets is a broadcast, so a
        // value no target recognizes is dropped silently per target — right for a
        // value meant for one of them (e.g. `chi` in a TS+Go config), but a genuine
        // typo would then vanish without feedback. Warn once if the explicit CLI
        // value lands on no framework-aware target. (Bound values — a single
        // target, or a per-target config key — already error via `resolve_fw`.)
        let target_names: Vec<&str> = resolved.iter().map(|rt| rt.target.as_str()).collect();
        if let Some(warning) =
            broadcast_framework_warning(framework.as_deref(), single, &target_names)
        {
            eprintln!("{warning}");
        }
        if watch {
            let targets: Vec<(&str, &str, GenMode, Option<String>)> = resolved
                .iter()
                .map(|rt| {
                    let out_dir = out.as_deref().unwrap_or(&rt.out_dir);
                    let mode = cli_mode.unwrap_or_else(|| parse_mode(rt.mode.as_deref()));
                    (rt.target.as_str(), out_dir, mode, resolve_fw(rt))
                })
                .collect();
            cmd_gen_watch(&file, &targets);
        } else {
            for rt in &resolved {
                let out_dir = out.as_deref().unwrap_or(&rt.out_dir);
                let mode = cli_mode.unwrap_or_else(|| parse_mode(rt.mode.as_deref()));
                let fw = resolve_fw(rt);
                cmd_gen(&file, &rt.target, out_dir, mode, fw.as_deref());
            }
        }
    } else {
        // No config targets — fall back to the default single target (typescript),
        // which the framework is therefore bound to: validate it strictly.
        let out = out.unwrap_or_else(|| "./generated".to_string());
        let mode = cli_mode.unwrap_or(GenMode::Both);
        let fw = resolve_target_framework_or_exit("typescript", framework.as_deref(), true);
        if watch {
            cmd_gen_watch(&file, &[("typescript", out.as_str(), mode, fw)]);
        } else {
            cmd_gen(&file, "typescript", &out, mode, fw.as_deref());
        }
    }
}

/// Parses a mode string from config into a [`GenMode`].
fn parse_mode(mode: Option<&str>) -> GenMode {
    match mode {
        Some("client") => GenMode::ClientOnly,
        Some("server") => GenMode::ServerOnly,
        Some("both") | None => GenMode::Both,
        Some(other) => {
            eprintln!(
                "error: invalid gen mode '{}' (expected: client, server, both)",
                other
            );
            process::exit(1);
        }
    }
}

/// Reads a source file from disk and registers it in a fresh [`SourceMap`].
///
/// Returns the source map, the file's [`SourceId`], and the raw contents.
/// Exits the process with an error message if the file cannot be read.
///
/// Used by `cmd_lex` and `cmd_parse`, which operate on a single file
/// without invoking the module resolver. The full-pipeline commands
/// (`check`, `run`, `build`, `ir`, `run-ir`, `gen`) go through
/// [`parse_resolve_check`] instead, which uses [`phoenix_modules::resolve`]
/// for multi-module discovery.
fn read_source(path: &str) -> (SourceMap, SourceId, String) {
    let contents = fs::read_to_string(path).unwrap_or_else(|err| {
        eprintln!("error: could not read file '{}': {}", path, err);
        process::exit(1);
    });
    let mut source_map = SourceMap::new();
    let source_id = source_map.add(path, &contents);
    (source_map, source_id, contents)
}

/// Reports a [`ResolveError`] to stderr in the same shape as parser /
/// sema diagnostics, then returns the count of diagnostics emitted.
///
/// `MalformedSourceFiles` carries a parser-diagnostic vector per failing
/// file; those are routed through `report_diagnostics` so they render with
/// full source context. Other variants currently render via `Display`
/// (rich span-resolved diagnostics for them are a Phase 2.6 follow-up;
/// see the §2.6 exit criterion "module-system diagnostics exercise the
/// rich diagnostic shape").
fn report_resolve_error(err: &ResolveError, source_map: &SourceMap) {
    match err {
        ResolveError::MalformedSourceFiles { files } => {
            for (_path, diags) in files {
                report_diagnostics(diags, source_map);
            }
        }
        other => {
            eprintln!("error: {}", other);
        }
    }
}

/// Prints a slice of diagnostics to stderr with `file:line:col` prefixes.
///
/// Each diagnostic is rendered through
/// [`Diagnostic::display_with`](phoenix_common::diagnostics::Diagnostic::display_with),
/// which resolves every span — primary and notes — against its own
/// [`SourceId`] inside the [`SourceMap`]. The function does not take
/// a `source_id` parameter because that would force every diagnostic
/// to claim a single file, which is wrong for multi-file diagnostics
/// (e.g. "symbol X is private; defined here: [other_file:line:col]").
fn report_diagnostics(
    diagnostics: &[phoenix_common::diagnostics::Diagnostic],
    source_map: &SourceMap,
) {
    for diag in diagnostics {
        eprintln!("{}", diag.display_with(source_map));
    }
}

/// Tokenizes a source file and prints the token stream.
pub fn cmd_lex(path: &str) {
    let (_source_map, source_id, contents) = read_source(path);
    let tokens = tokenize(&contents, source_id);

    for token in &tokens {
        println!(
            "{:?}\t{:?}\t[{}..{}]",
            token.kind, token.text, token.span.start, token.span.end
        );
    }
}

/// Parses a source file and prints the AST as JSON.
pub fn cmd_parse(path: &str) {
    let (source_map, source_id, contents) = read_source(path);
    let tokens = tokenize(&contents, source_id);
    let (program, diagnostics) = parser::parse(&tokens);

    if !diagnostics.is_empty() {
        report_diagnostics(&diagnostics, &source_map);
        process::exit(1);
    }

    let json = serde_json::to_string_pretty(&program).unwrap_or_else(|err| {
        eprintln!("error: failed to serialize AST: {}", err);
        process::exit(1);
    });
    println!("{}", json);
}

/// Resolves the import graph rooted at `path`, parses every reachable
/// module, type-checks the project, and exits the process on errors.
///
/// Returns the *entry module's* program AST and the project-wide semantic
/// analysis. For single-file inputs (no `import` declarations), the
/// returned program, analysis, and diagnostic file labels are identical
/// to what the previous single-file `parse_and_check` produced — the
/// resolver finds exactly one module (the entry), uses the caller-supplied
/// path verbatim as its `SourceMap` display name, and `check_modules` runs
/// the same registration / checking passes.
///
/// IR lowering, the interpreter, and code generators currently consume
/// the entry program directly. Task #6 (IR module-keying) extends those
/// to walk the full `Vec<ResolvedSourceModule>`; this entry point's
/// signature stays stable across that change.
pub(crate) fn parse_and_check(
    path: &str,
    locked: bool,
) -> (phoenix_parser::ast::Program, phoenix_sema::Analysis) {
    let (modules, analysis, _source_map) = parse_resolve_check(path, locked);
    // `phoenix_modules::resolve` always returns at least the entry module
    // and places it first; both invariants are load-bearing here.
    debug_assert!(
        !modules.is_empty() && modules[0].is_entry,
        "parse_resolve_check returned modules without the entry first"
    );
    let entry_program = modules.into_iter().next().expect("entry module").program;
    (entry_program, analysis)
}

/// Multi-module parse + resolve + type-check entry point.
///
/// Returns the full resolver output (in deterministic topological order,
/// entry first), the project-wide semantic analysis, and the shared
/// [`SourceMap`] so cross-module diagnostics resolve their own
/// `SourceId`s. Exits the process on parse, resolve, or type errors.
///
/// Callers that need the entry [`Program`] standalone use
/// [`parse_and_check`], which extracts it from `modules[0]` without an
/// extra clone.
pub(crate) fn parse_resolve_check(
    path: &str,
    locked: bool,
) -> (Vec<ResolvedSourceModule>, phoenix_sema::Analysis, SourceMap) {
    let mut source_map = SourceMap::new();
    let modules = resolve_modules_with_deps(path, locked, &mut source_map);

    let analysis = checker::check_modules(&modules);
    if !analysis.diagnostics.is_empty() {
        report_diagnostics(&analysis.diagnostics, &source_map);
        process::exit(1);
    }

    (modules, analysis, source_map)
}

/// Resolve the module graph for `path`, first fetching + wiring any declared
/// package dependencies so cross-package `import`s resolve. On any error
/// (manifest, dependency resolution, or module resolution) the diagnostic is
/// printed and the process exits.
fn resolve_modules_with_deps(
    path: &str,
    locked: bool,
    source_map: &mut SourceMap,
) -> Vec<ResolvedSourceModule> {
    resolve_modules_reporting(path, locked, source_map).unwrap_or_else(|_| process::exit(1))
}

/// Shared core of dependency-aware module resolution: discover + fetch declared
/// dependencies, then resolve the import graph with them wired in. All
/// user-facing stderr — both errors and the "Updated phoenix.lock" notice — is
/// emitted here; the `Err` payload is an opaque category tag the caller maps to
/// its own policy — [`resolve_modules_with_deps`] exits the process, while the
/// watch path ([`try_parse_and_check`]) keeps the loop alive. `locked` forbids
/// updating `phoenix.lock`.
fn resolve_modules_reporting(
    path: &str,
    locked: bool,
    source_map: &mut SourceMap,
) -> Result<Vec<ResolvedSourceModule>, String> {
    let entry = Path::new(path);
    let (packages, lock_changed) =
        deps::project::build_package_resolution(entry, locked).map_err(|msg| {
            eprintln!("error: {msg}");
            "dependency resolution errors".to_string()
        })?;
    if lock_changed {
        eprintln!("Updated phoenix.lock");
    }
    phoenix_modules::resolve_with_packages(entry, source_map, &packages).map_err(|err| {
        report_resolve_error(&err, source_map);
        "resolve / parse errors".to_string()
    })
}

/// Type-checks a source file and reports diagnostics. Resolves and fetches any
/// declared dependencies first; `locked` forbids updating `phoenix.lock`.
pub fn cmd_check(path: &str, locked: bool) {
    parse_and_check(path, locked);
    println!("No errors found.");
}

/// Lower a Phoenix source file to IR and print the textual representation.
/// Declared dependencies (if any) still resolve, but — like `gen` and unlike
/// `check`/`run`/`build` — this is a non-`--locked`-aware path: the lockfile may
/// be updated but is never gated.
pub fn cmd_ir(path: &str) {
    let (modules, check_result, _sm) = parse_resolve_check(path, false);
    let ir_module = phoenix_ir::lower_modules(&modules, &check_result.module);

    // Run the IR verifier to catch structural errors.
    let errors = phoenix_ir::verify::verify(&ir_module);
    if !errors.is_empty() {
        for err in &errors {
            eprintln!("IR verification error in {}: {}", err.function, err.message);
        }
        process::exit(1);
    }

    print!("{}", ir_module);
}

/// Runs a Phoenix program via the tree-walk interpreter. Resolves and fetches
/// any declared dependencies first; `locked` forbids updating `phoenix.lock`.
pub fn cmd_run(path: &str, locked: bool) {
    // The AST interpreter goes through `interpreter::run_modules` so
    // multi-file programs work end-to-end (sema's `module_scopes`
    // translate cross-module name references). For single-file
    // inputs this reduces to the same behavior as the previous
    // single-program path — entry module qualifies to bare, every
    // name resolves identically.
    let (modules, mut analysis, _sm) = parse_resolve_check(path, locked);
    if let Err(err) = interpreter::run_modules(&modules, &mut analysis) {
        eprintln!("runtime error: {}", err);
        process::exit(1);
    }
}

/// Run a Phoenix program via the IR interpreter. Declared dependencies (if any)
/// still resolve, but — like `gen`/`ir` and unlike `check`/`run`/`build` — this
/// is a non-`--locked`-aware path: the lockfile may be updated but is never
/// gated.
pub fn cmd_run_ir(path: &str) {
    let (modules, check_result, _sm) = parse_resolve_check(path, false);
    let ir_module = phoenix_ir::lower_modules(&modules, &check_result.module);

    let errors = phoenix_ir::verify::verify(&ir_module);
    if !errors.is_empty() {
        for err in &errors {
            eprintln!("IR verification error in {}: {}", err.function, err.message);
        }
        process::exit(1);
    }

    if let Err(err) = phoenix_ir_interp::run(&ir_module) {
        eprintln!("runtime error: {}", err);
        process::exit(1);
    }
}

// ── Code generation ────────────────────────────────────────────────

/// Parses, type-checks, and generates code from a Phoenix schema file.
///
/// Supports `typescript`, `python`, `go`, and `openapi` targets. The `mode`
/// parameter controls whether to emit client-only, server-only, or all files.
///
/// Parse / resolve / type errors are fatal (diagnostics already printed by
/// [`parse_and_check`], which exits); generation/IO errors print `error: …` and
/// exit. The actual file emission is shared with the watch path via
/// [`emit_target`] so the two cannot drift.
fn cmd_gen(path: &str, target: &str, out_dir: &str, mode: GenMode, framework: Option<&str>) {
    // Code generation is schema-first and not `--locked`-aware; declared
    // dependencies (if any) still resolve, but the lockfile is never gated.
    let (program, check_result) = parse_and_check(path, false);

    if target == "openapi" && mode != GenMode::Both {
        eprintln!("warning: --client/--server flags have no effect on OpenAPI target");
    }

    match emit_target(&program, &check_result, target, out_dir, mode, framework) {
        Ok(generated) => println!("Generated {}", generated.join(", ")),
        Err(e) => {
            eprintln!("error: {}", e);
            process::exit(1);
        }
    }
}

/// Parses the TypeScript server-framework selector (CLI `--framework` or config
/// `framework`) for the TypeScript target. Returns an error message for an
/// unknown value so a typo can't silently fall back to Express; `None` is the
/// default (Express). The error propagates out of [`emit_target`] as an `Err`,
/// so the caller reports it; keeping the mapping pure makes it unit-testable.
fn parse_framework(framework: Option<&str>) -> Result<phoenix_codegen::TsServerFramework, String> {
    match framework {
        None | Some("express") => Ok(phoenix_codegen::TsServerFramework::Express),
        Some("fastify") => Ok(phoenix_codegen::TsServerFramework::Fastify),
        Some(other) => Err(format!(
            "unknown ts framework '{}' (supported: express, fastify)",
            other
        )),
    }
}

/// Parses the Go server-framework selector (CLI `--framework` or config
/// `framework`) for the Go target. Returns an error message for an unknown value
/// so a typo can't silently fall back to net/http; `None` is the default
/// (net/http). Both `"net/http"` and `"nethttp"` select net/http. The error
/// propagates out of [`emit_target`] as an `Err`, so the caller reports it;
/// keeping the mapping pure makes it unit-testable.
fn parse_go_framework(
    framework: Option<&str>,
) -> Result<phoenix_codegen::GoServerFramework, String> {
    match framework {
        None | Some("net/http") | Some("nethttp") => {
            Ok(phoenix_codegen::GoServerFramework::NetHttp)
        }
        Some("chi") => Ok(phoenix_codegen::GoServerFramework::Chi),
        Some(other) => Err(format!(
            "unknown go framework '{}' (supported: net/http, chi)",
            other
        )),
    }
}

/// Decides whether a `framework` value reaching a config-resolved target is
/// *bound* to it (strict — an unknown value is a typo that must error) or merely
/// *broadcast* across heterogeneous targets (tolerant — an unrecognized value is
/// dropped to that target's default). Pulled out of `run_gen`'s per-target
/// closure so the rule is unit-testable:
/// - When there's only one resolved target (`single`), any value — CLI
///   `--framework` or inherited top-level `[gen] framework` — is unambiguously
///   aimed at that one target, so it's bound regardless of provenance. (This is
///   why a single-entry `[gen.targets.<name>]` map inheriting a top-level default
///   is treated the same as a single-target `[gen]` config.)
/// - Across several targets, a CLI `--framework` (`cli_framework`) is a broadcast.
/// - Otherwise the value comes from config, where provenance is precomputed:
///   `framework_explicit` is set for a per-target `[gen.targets.<name>] framework`
///   (bound to that target) and cleared for a top-level `[gen] framework`
///   broadcast across a multi-target config.
fn framework_is_bound(cli_framework: bool, single: bool, framework_explicit: bool) -> bool {
    if single {
        true
    } else if cli_framework {
        false
    } else {
        framework_explicit
    }
}

/// Strictness rule for the `--target` (CLI single-target) path, the analogue of
/// [`framework_is_bound`] for the branch that never calls `resolve_targets` (so it
/// has no precomputed `framework_explicit` to read). Pulled out of `run_gen` so
/// the rule is unit-testable:
/// - A CLI `--framework` (`cli_framework`) is aimed squarely at the chosen target,
///   so it's always bound (a typo errors).
/// - Otherwise the value comes from the top-level `[gen] framework`. It's bound
///   only when the config is single-target (`config_is_single_target`, i.e. no
///   `[gen.targets]` map) *and* its `target` is the one being generated
///   (`config_target_matches`) — exactly the case `resolve_targets` marks
///   `framework_explicit`. A multi-target config's top-level value is a broadcast,
///   and a single-target config whose `framework` was written for a different
///   target than the CLI `--target` override wasn't aimed here; both stay tolerant.
fn cli_target_framework_is_bound(
    cli_framework: bool,
    config_is_single_target: bool,
    config_target_matches: bool,
) -> bool {
    cli_framework || (config_is_single_target && config_target_matches)
}

/// Whether `target` has a server-framework vocabulary at all — only `typescript`
/// (express/fastify) and `go` (net/http/chi) do. The python/openapi targets
/// ignore any `framework` value, so a CLI `--framework` landing only on them has
/// no effect; used to decide whether a broadcast CLI value went unrecognized
/// everywhere (worth a warning). Mirrors the framework-aware arms of
/// [`resolve_target_framework`].
fn target_has_framework(target: &str) -> bool {
    matches!(target, "typescript" | "go")
}

/// The warning (if any) `run_gen` emits for a broadcast CLI `--framework` over a
/// multi-target config. A broadcast value no target recognizes is dropped
/// silently per target (right for a value meant for one of them, e.g. `chi` in a
/// TS+Go config), so a genuine typo would otherwise vanish without feedback.
/// Returns `Some(message)` iff the value is explicit, broadcast (`!single`), and
/// recognized by no framework-aware target — otherwise `None`. Pulled out of
/// `run_gen` so the warning text and condition are unit-testable (the call site
/// only prints the returned message).
fn broadcast_framework_warning(
    framework: Option<&str>,
    single: bool,
    targets: &[&str],
) -> Option<String> {
    let value = framework.filter(|_| !single)?;
    let recognized = targets
        .iter()
        .any(|t| target_has_framework(t) && resolve_target_framework(t, Some(value), true).is_ok());
    if recognized {
        None
    } else {
        Some(format!(
            "warning: --framework '{value}' is not recognized by any generated target; \
             each target uses its own default"
        ))
    }
}

/// Validates the `framework` selector for one target, applying provenance-aware
/// strictness, and returns the value to thread downstream (unchanged when the
/// target accepts it, `None` when a broadcast value is tolerantly dropped).
///
/// `framework` carries one string but feeds two vocabularies (TypeScript
/// `express`/`fastify`, Go `net/http`/`chi`), and it can be *bound* to a single
/// target or *broadcast* across several. `strict` distinguishes the two:
/// - **strict** (`true`) — the value is aimed at exactly this target (a per-target
///   `[gen.targets.<name>] framework`, or a single chosen target via `--target` /
///   a single-target config / the default fallback). An unknown value is a typo
///   and surfaces as `Err`, so it can't silently degrade to the default.
/// - **broadcast** (`false`) — the value is a shared default spread across
///   heterogeneous targets (top-level `[gen] framework`, or a global `--framework`
///   over a multi-target config). A value valid for one target is meaningless for
///   another, so an unrecognized value is dropped to `None` (this target's
///   default) rather than aborting the whole run.
///
/// The python/openapi targets have no framework vocabulary, so they ignore the
/// value entirely (it threads through as-is and `emit_target` never consults it).
fn resolve_target_framework(
    target: &str,
    framework: Option<&str>,
    strict: bool,
) -> Result<Option<String>, String> {
    let accepted = match target {
        "typescript" => parse_framework(framework).map(|_| ()),
        "go" => parse_go_framework(framework).map(|_| ()),
        // No framework vocabulary — accept anything; the value is ignored.
        _ => Ok(()),
    };
    match accepted {
        Ok(()) => Ok(framework.map(str::to_owned)),
        // Broadcast value this target doesn't recognize → use its own default.
        Err(_) if !strict => Ok(None),
        // Bound value the target doesn't recognize → a typo; fail loud.
        Err(e) => Err(e),
    }
}

/// [`resolve_target_framework`] wrapper that prints `error: …` and exits on a
/// strict (bound, unknown-for-target) value — the behavior every `run_gen` call
/// site wants. Mirrors how an unsupported target is surfaced.
fn resolve_target_framework_or_exit(
    target: &str,
    framework: Option<&str>,
    strict: bool,
) -> Option<String> {
    resolve_target_framework(target, framework, strict).unwrap_or_else(|err| {
        eprintln!("error: {}", err);
        process::exit(1);
    })
}

/// Emits the generated files for one target into `out_dir`, returning the paths
/// written (in emission order) or an error message.
///
/// This is the single source of truth for the per-target filename / mode matrix,
/// shared by the one-shot [`cmd_gen`] path and the watch-mode [`generate_once`]
/// path so a new target or filename cannot be added to one and forgotten in the
/// other. It performs no parsing — callers supply an already-checked program —
/// and never exits the process, leaving error policy to the caller.
fn emit_target(
    program: &phoenix_parser::ast::Program,
    check_result: &phoenix_sema::Analysis,
    target: &str,
    out_dir: &str,
    mode: GenMode,
    framework: Option<&str>,
) -> Result<Vec<String>, String> {
    // `extern js` is an executable-language interop feature (host-FFI for
    // `phoenix run` / `build`), not a Phoenix Gen schema feature. The Gen
    // backends match `Declaration` with a trailing `_ => {}`, so an `extern js`
    // block would otherwise be dropped silently. Reject it here — the single
    // entry both `phoenix gen` and `phoenix-gen` route through — before any
    // output directory or file is created. See design-decisions §Phase 2.5 (J).
    //
    // This scans only the entry program's declarations, not imported modules:
    // an `extern js` block in an imported module is not caught by *this* check.
    // That is defensive-only — any `JsValue`-typed surface such a block exposes
    // is still caught by `schema_mentions_jsvalue` below, which scans the
    // *resolved* module (imports included).
    if program
        .declarations
        .iter()
        .any(|d| matches!(d, phoenix_parser::ast::Declaration::ExternJs(_)))
    {
        return Err(
            "`extern js` blocks are not supported in Phoenix Gen schemas — \
             they are an executable-language interop feature, not a schema feature. \
             Remove the `extern js` block, or compile the program with `phoenix build` / \
             `phoenix run` instead."
                .to_string(),
        );
    }

    // The same reasoning extends to the `JsValue` *type* wherever it appears (a
    // struct field, an endpoint parameter, a type alias, …), not just `extern js`
    // blocks: `JsValue` is an executable-language host-FFI handle with no wire
    // representation. Rejecting it here keeps all four targets consistent —
    // otherwise each backend's type-mapper would silently emit a different
    // fallback (`unknown` / `interface{}` / `object`).
    if phoenix_codegen::schema_mentions_jsvalue(check_result) {
        return Err(
            "`JsValue` is not supported in Phoenix Gen schemas — it is an \
             executable-language host-FFI handle (Phase 2.5) with no wire representation. \
             Remove the `JsValue`-typed field/parameter, or compile the program with \
             `phoenix build` / `phoenix run` instead."
                .to_string(),
        );
    }

    fs::create_dir_all(out_dir)
        .map_err(|err| format!("could not create output directory '{}': {}", out_dir, err))?;

    let mut generated = Vec::new();
    match target {
        "typescript" => {
            let fw = parse_framework(framework)?;
            let files = phoenix_codegen::generate_typescript_with(program, check_result, fw);
            generated.push(write_file(out_dir, "types.ts", &files.types)?);
            if mode != GenMode::ServerOnly {
                generated.push(write_file(out_dir, "client.ts", &files.client)?);
            }
            if mode != GenMode::ClientOnly {
                generated.push(write_file(out_dir, "handlers.ts", &files.handlers)?);
                generated.push(write_file(out_dir, "server.ts", &files.server)?);
            }
        }
        "python" => {
            let files = phoenix_codegen::generate_python(program, check_result);
            generated.push(write_file(out_dir, "__init__.py", &files.init)?);
            generated.push(write_file(out_dir, "models.py", &files.models)?);
            if mode != GenMode::ServerOnly {
                generated.push(write_file(out_dir, "client.py", &files.client)?);
            }
            if mode != GenMode::ClientOnly {
                generated.push(write_file(out_dir, "handlers.py", &files.handlers)?);
                generated.push(write_file(out_dir, "server.py", &files.server)?);
            }
        }
        "go" => {
            let fw = parse_go_framework(framework)?;
            let files = phoenix_codegen::generate_go_with(program, check_result, fw);
            generated.push(write_file(out_dir, "types.go", &files.types)?);
            if mode != GenMode::ServerOnly {
                generated.push(write_file(out_dir, "client.go", &files.client)?);
            }
            if mode != GenMode::ClientOnly {
                generated.push(write_file(out_dir, "handlers.go", &files.handlers)?);
                generated.push(write_file(out_dir, "server.go", &files.server)?);
            }
        }
        "openapi" => {
            let spec = phoenix_codegen::generate_openapi(program, check_result);
            generated.push(write_file(out_dir, "openapi.json", &spec)?);
        }
        _ => {
            return Err(format!(
                "unsupported target '{}' (supported: typescript, python, go, openapi)",
                target
            ));
        }
    }
    Ok(generated)
}

// ── Watch mode ──────────────────────────────────────────────────────

/// Resolves and type-checks a source file, returning the results or an error.
///
/// Unlike [`parse_and_check`], this does not call `process::exit` on failure.
/// Diagnostics are printed to stderr and an `Err` is returned so the caller
/// can decide how to proceed (e.g., continue in watch mode).
///
/// The `Err` payload is an opaque category tag (`"resolve / parse errors"`
/// or `"type errors"`) — the user-facing detail has already been written
/// to stderr by `report_resolve_error` / `report_diagnostics`. Callers
/// should not display the string verbatim.
fn try_parse_and_check(
    path: &str,
) -> Result<(phoenix_parser::ast::Program, phoenix_sema::Analysis), String> {
    let mut source_map = SourceMap::new();
    // Watch mode never gates the lockfile (`locked = false`); a dependency- or
    // manifest-resolution failure is reported and surfaced as an `Err` rather
    // than exiting the watch loop.
    //
    // NOTE: this re-resolves dependencies on every rebuild. For path-only deps
    // that is cheap; for git deps it re-runs `resolve_project` against the cache
    // per file change. A future refinement could resolve once per watch session
    // and re-resolve only when the manifest itself changes.
    let modules = resolve_modules_reporting(path, false, &mut source_map)?;

    let analysis = checker::check_modules(&modules);
    if !analysis.diagnostics.is_empty() {
        report_diagnostics(&analysis.diagnostics, &source_map);
        return Err("type errors".to_string());
    }

    let entry_program = modules[0].program.clone();
    Ok((entry_program, analysis))
}

/// Runs code generation once, returning `Ok(())` on success or `Err(message)`
/// on failure. Unlike [`cmd_gen`], this does not call `process::exit` — errors
/// are reported to stderr and the caller decides whether to continue (e.g., in
/// watch mode). The file emission itself is shared via [`emit_target`].
fn generate_once(
    path: &str,
    target: &str,
    out_dir: &str,
    mode: GenMode,
    framework: Option<&str>,
) -> Result<(), String> {
    let (program, result) = try_parse_and_check(path)?;
    emit_target(&program, &result, target, out_dir, mode, framework)?;
    Ok(())
}

/// Writes a single file to the output directory, returning its path.
fn write_file(out_dir: &str, name: &str, content: &str) -> Result<String, String> {
    let path = format!("{}/{}", out_dir, name);
    fs::write(&path, content).map_err(|err| format!("could not write '{}': {}", path, err))?;
    Ok(path)
}

/// Watches for `.phx` file changes and re-generates code automatically.
///
/// Performs an initial generation, then enters a loop that watches the
/// directory containing the schema file. On each change to a `.phx` file,
/// re-runs the full pipeline for all targets. Errors are printed to stderr
/// but do not terminate the watch loop.
///
/// Events are debounced with a 100 ms delay so that multiple rapid file-system
/// events from a single save are coalesced into one regeneration pass.
fn cmd_gen_watch(path: &str, targets: &[(&str, &str, GenMode, Option<String>)]) {
    use notify::{Event, EventKind, RecursiveMode, Watcher};
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    // Validate all targets before starting watch
    for (target, _, mode, _) in targets {
        if !matches!(*target, "typescript" | "python" | "go" | "openapi") {
            eprintln!(
                "error: unsupported target '{}' (supported: typescript, python, go, openapi)",
                target
            );
            process::exit(1);
        }
        // Mirror the one-shot `cmd_gen` warning so the watch path is consistent.
        // Emitted once here at setup rather than inside the regen loop, which
        // would repeat it on every file change.
        if *target == "openapi" && *mode != GenMode::Both {
            eprintln!("warning: --client/--server flags have no effect on OpenAPI target");
        }
    }

    let watch_dir = Path::new(path)
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();

    // Initial generation
    let target_list: Vec<&str> = targets.iter().map(|(t, _, _, _)| *t).collect();
    eprintln!(
        "[phoenix gen] targets={}, watching {}",
        target_list.join(", "),
        watch_dir.display()
    );
    let mut had_error = false;
    for (target, out_dir, mode, framework) in targets {
        match generate_once(path, target, out_dir, *mode, framework.as_deref()) {
            Ok(()) => {}
            Err(e) => {
                eprintln!(
                    "[phoenix gen] initial generation failed ({}): {}",
                    target, e
                );
                had_error = true;
            }
        }
    }
    if !had_error {
        eprintln!("[phoenix gen] initial generation complete");
    }

    // Set up file watcher
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(tx).unwrap_or_else(|err| {
        eprintln!("error: could not create file watcher: {}", err);
        process::exit(1);
    });

    watcher
        .watch(&watch_dir, RecursiveMode::Recursive)
        .unwrap_or_else(|err| {
            eprintln!("error: could not watch '{}': {}", watch_dir.display(), err);
            process::exit(1);
        });

    eprintln!("[phoenix gen] watching for changes...");

    // Debounce: file watchers often fire multiple events for a single save.
    // Wait until no new events arrive for DEBOUNCE_DELAY before regenerating.
    const DEBOUNCE_DELAY: Duration = Duration::from_millis(100);
    let mut last_relevant_event: Option<Instant> = None;

    loop {
        // Use a short timeout so we can check if the debounce period elapsed
        let event = if last_relevant_event.is_some() {
            rx.recv_timeout(DEBOUNCE_DELAY).ok()
        } else {
            rx.recv().ok()
        };

        if let Some(Ok(event)) = event {
            let is_relevant = matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_));
            let is_phx = event
                .paths
                .iter()
                .any(|p| p.extension().is_some_and(|ext| ext == "phx"));

            if is_relevant && is_phx {
                last_relevant_event = Some(Instant::now());
            }
            continue;
        }

        // Timeout or channel closed — check if we should regenerate
        if let Some(last) = last_relevant_event
            && last.elapsed() >= DEBOUNCE_DELAY
        {
            last_relevant_event = None;
            eprintln!("[phoenix gen] change detected, regenerating...");
            let mut had_error = false;
            for (target, out_dir, mode, framework) in targets {
                match generate_once(path, target, out_dir, *mode, framework.as_deref()) {
                    Ok(()) => {}
                    Err(e) => {
                        eprintln!("[phoenix gen] regeneration failed ({}): {}", target, e);
                        had_error = true;
                    }
                }
            }
            if !had_error {
                eprintln!("[phoenix gen] regeneration complete");
            }
        }

        // If the channel is closed (watcher dropped), exit
        if event.is_none() && last_relevant_event.is_none() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    /// A minimal single-file schema (no imports) that exercises every emitted
    /// file: a struct/enum for types, plus an endpoint so client/handlers/server
    /// are non-trivial.
    const SCHEMA: &str = r#"
struct User { id: Int  name: String }
endpoint getUser: GET "/api/users/{id}" {
    response User
}
"#;

    /// Writes `SCHEMA` to a fresh tempdir and returns the dir handle plus the
    /// schema path. The handle must stay alive for the dir to exist.
    fn schema_in_tmp() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("schema.phx");
        fs::write(&path, SCHEMA).expect("write schema");
        (dir, path.to_string_lossy().into_owned())
    }

    /// The set of file names present in `out_dir`.
    fn files_in(out_dir: &Path) -> BTreeSet<String> {
        fs::read_dir(out_dir)
            .expect("read out dir")
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    // `parse_framework` maps the CLI/config selector to the codegen enum. The
    // unknown-value path is an error (not a silent Express fallback); the error
    // propagates out of `emit_target` so the caller reports it, while the
    // testable mapping lives here in the `Result`-returning core.

    #[test]
    fn parse_framework_defaults_to_express() {
        assert_eq!(
            parse_framework(None),
            Ok(phoenix_codegen::TsServerFramework::Express)
        );
    }

    #[test]
    fn parse_framework_accepts_known_values() {
        assert_eq!(
            parse_framework(Some("express")),
            Ok(phoenix_codegen::TsServerFramework::Express)
        );
        assert_eq!(
            parse_framework(Some("fastify")),
            Ok(phoenix_codegen::TsServerFramework::Fastify)
        );
    }

    #[test]
    fn parse_framework_rejects_unknown_value() {
        let err = parse_framework(Some("koa")).expect_err("unknown framework must error");
        assert!(err.contains("koa"), "got: {err}");
        assert!(err.contains("express, fastify"), "got: {err}");
    }

    // `parse_go_framework` mirrors `parse_framework`: a pure mapping with an
    // error (not a silent net/http fallback) on an unknown value. Both
    // `net/http` and `nethttp` select the default so a TOML key can avoid the
    // slash.

    #[test]
    fn parse_go_framework_defaults_to_nethttp() {
        assert_eq!(
            parse_go_framework(None),
            Ok(phoenix_codegen::GoServerFramework::NetHttp)
        );
    }

    #[test]
    fn parse_go_framework_accepts_known_values() {
        assert_eq!(
            parse_go_framework(Some("net/http")),
            Ok(phoenix_codegen::GoServerFramework::NetHttp)
        );
        assert_eq!(
            parse_go_framework(Some("nethttp")),
            Ok(phoenix_codegen::GoServerFramework::NetHttp)
        );
        assert_eq!(
            parse_go_framework(Some("chi")),
            Ok(phoenix_codegen::GoServerFramework::Chi)
        );
    }

    #[test]
    fn parse_go_framework_rejects_unknown_value() {
        let err = parse_go_framework(Some("gin")).expect_err("unknown framework must error");
        assert!(err.contains("gin"), "got: {err}");
        assert!(err.contains("net/http, chi"), "got: {err}");
    }

    // `resolve_target_framework` layers provenance-aware strictness over the pure
    // parsers. The critical case is a value that's valid for one target but not
    // another (e.g. `fastify` reaching the Go target): bound to a target it's a
    // typo (error); broadcast across targets it's tolerated (dropped to that
    // target's default) so a shared `[gen] framework`/global `--framework` can't
    // abort the whole run.

    #[test]
    fn resolve_target_framework_strict_passes_known_value() {
        assert_eq!(
            resolve_target_framework("go", Some("chi"), true),
            Ok(Some("chi".to_string()))
        );
        assert_eq!(
            resolve_target_framework("typescript", Some("fastify"), true),
            Ok(Some("fastify".to_string()))
        );
    }

    #[test]
    fn resolve_target_framework_strict_rejects_unknown_value() {
        // A value aimed at this target that it doesn't recognize is a typo.
        let err = resolve_target_framework("go", Some("fastify"), true)
            .expect_err("bound, unknown-for-target value must error");
        assert!(err.contains("fastify"), "got: {err}");
        assert!(err.contains("net/http, chi"), "got: {err}");
    }

    #[test]
    fn resolve_target_framework_broadcast_drops_unrecognized_value() {
        // The reported bug: a shared `framework = "fastify"` inherited by a Go
        // target must NOT abort the run — it's meaningless for Go, so Go falls
        // back to its own default (None) instead.
        assert_eq!(
            resolve_target_framework("go", Some("fastify"), false),
            Ok(None)
        );
        // Symmetric: a Go value broadcast onto the TypeScript target.
        assert_eq!(
            resolve_target_framework("typescript", Some("chi"), false),
            Ok(None)
        );
    }

    #[test]
    fn resolve_target_framework_broadcast_keeps_recognized_value() {
        // A broadcast value the target DOES recognize is still applied.
        assert_eq!(
            resolve_target_framework("go", Some("chi"), false),
            Ok(Some("chi".to_string()))
        );
    }

    #[test]
    fn resolve_target_framework_broadcast_silently_drops_value_valid_for_no_target() {
        // A broadcast value that no target recognizes (e.g. a typo like `expres`)
        // is dropped to each target's default with NO error — the same tolerant
        // path as a value meaningful for another target. This is deliberate: a
        // broadcast carries no claim that any particular target must honor it, so
        // we can't tell a cross-target default apart from a typo here, and aborting
        // a mixed-target run on a value some other target might have wanted would
        // be worse. Strictness (and thus typo-catching) is reserved for *bound*
        // values; see `framework_is_bound`. Pinned so the silent-drop contract is
        // a conscious choice, not an accident.
        assert_eq!(
            resolve_target_framework("typescript", Some("expres"), false),
            Ok(None)
        );
        assert_eq!(
            resolve_target_framework("go", Some("expres"), false),
            Ok(None)
        );
    }

    #[test]
    fn resolve_target_framework_ignored_targets_accept_anything() {
        // python/openapi have no framework vocabulary: the value threads through
        // untouched (emit_target never consults it) and is never an error, even
        // when strict.
        assert_eq!(
            resolve_target_framework("openapi", Some("chi"), true),
            Ok(Some("chi".to_string()))
        );
        assert_eq!(
            resolve_target_framework("python", Some("fastify"), true),
            Ok(Some("fastify".to_string()))
        );
    }

    // `framework_is_bound` is the strictness rule `run_gen` applies per resolved
    // target. The regression it guards: a single-target config (no CLI override,
    // `framework_explicit == true`) must be BOUND, so a typo errors instead of
    // silently degrading to the target default.
    #[test]
    fn framework_is_bound_rule() {
        // Single-target config, no CLI: bound via the explicit flag.
        assert!(framework_is_bound(false, true, true));
        // Single resolved target inheriting a top-level `[gen] framework` (a
        // single-entry `[gen.targets.<name>]` map, so `framework_explicit` is
        // cleared): still bound, because the value is unambiguously aimed at the
        // one target. Guards the inconsistency where this used to be tolerant while
        // a single-target `[gen]` config was strict.
        assert!(framework_is_bound(false, true, false));
        // Top-level `[gen] framework` broadcast over a multi-target config: the
        // flag is cleared, so it's tolerant.
        assert!(!framework_is_bound(false, false, false));
        // Per-target `[gen.targets.<name>] framework`: explicit → bound, even
        // across a multi-target config.
        assert!(framework_is_bound(false, false, true));
        // CLI `--framework` onto a single target: bound regardless of the flag.
        assert!(framework_is_bound(true, true, false));
        // CLI `--framework` spread across several targets: a broadcast.
        assert!(!framework_is_bound(true, false, true));
    }

    // `cli_target_framework_is_bound` is the strictness rule for the `--target`
    // path (which never calls `resolve_targets`, so it has no `framework_explicit`
    // to read). The regression it guards: a single-target config's top-level
    // `[gen] framework` must be BOUND when the chosen `--target` matches the
    // config's target, so a typo there errors instead of silently degrading —
    // matching the `framework_is_bound` behavior of the config-resolved path.
    #[test]
    fn cli_target_framework_is_bound_rule() {
        // CLI `--framework` is always bound to the chosen target, regardless of
        // config shape or whether the config target matches.
        assert!(cli_target_framework_is_bound(true, false, false));
        assert!(cli_target_framework_is_bound(true, true, true));
        // No CLI framework, single-target config whose target == `--target`: the
        // inherited top-level framework is bound (a typo must error). This is the
        // case the previous `strict = framework.is_some()` got wrong.
        assert!(cli_target_framework_is_bound(false, true, true));
        // Single-target config but `--target` overrides to a DIFFERENT target: the
        // config framework was written for another target → broadcast/tolerant.
        assert!(!cli_target_framework_is_bound(false, true, false));
        // Multi-target config (top-level framework is a broadcast) → tolerant,
        // even when the chosen target happens to be one of them.
        assert!(!cli_target_framework_is_bound(false, false, true));
        // No CLI framework and no config target at all → nothing is bound here.
        assert!(!cli_target_framework_is_bound(false, false, false));
    }

    // Stand-ins for `run_gen`'s `--target` branch: compose the strictness rule
    // (`cli_target_framework_is_bound`) with the validator (`resolve_target_framework`)
    // exactly as the branch does, minus the `process::exit` that makes `run_gen`
    // itself awkward to drive from a unit test.

    #[test]
    fn cli_target_override_mismatched_config_framework_is_tolerated() {
        // `[gen] target = "typescript", framework = "fastify"` run as `--target go`
        // (no CLI `--framework`): the config framework was written for TS, so
        // reaching the overridden Go target it must be tolerated (dropped to Go's
        // default), not abort the run.
        let strict = cli_target_framework_is_bound(
            false, // no CLI --framework
            true,  // single-target config (no [gen.targets] map)
            false, // config target ("typescript") != --target ("go")
        );
        assert!(!strict);
        assert_eq!(
            resolve_target_framework("go", Some("fastify"), strict),
            Ok(None)
        );
    }

    #[test]
    fn cli_target_matching_config_framework_typo_errors() {
        // Same single-target config but run as `--target typescript` (matches): the
        // inherited framework is bound, so a typo must error rather than silently
        // fall back to Express.
        let strict = cli_target_framework_is_bound(false, true, true);
        assert!(strict);
        assert!(resolve_target_framework("typescript", Some("expres"), strict).is_err());
    }

    // `target_has_framework` decides whether a target meaningfully consumes the
    // `framework` value — the basis for warning when a broadcast CLI `--framework`
    // lands nowhere.
    #[test]
    fn target_has_framework_rule() {
        assert!(target_has_framework("typescript"));
        assert!(target_has_framework("go"));
        assert!(!target_has_framework("python"));
        assert!(!target_has_framework("openapi"));
        assert!(!target_has_framework("cobol"));
    }

    // The broadcast-warning condition `run_gen` evaluates for a global CLI
    // `--framework` over a multi-target config: warn iff no framework-aware target
    // recognizes the value (a likely typo), but stay quiet when at least one does.
    #[test]
    fn broadcast_cli_framework_recognition() {
        // Drives the real `broadcast_framework_warning` (what `run_gen` prints),
        // so both the condition AND the message are covered, not a reimplemented
        // predicate. A `Some` return is the warning `run_gen` emits; `None` is
        // silence.
        let ts_go = &["typescript", "go"][..];

        // `chi` is meant for the Go target in a TS+Go run → recognized, no warning.
        assert_eq!(broadcast_framework_warning(Some("chi"), false, ts_go), None);
        // A typo no target understands → unrecognized, warn (and name the value).
        let warning = broadcast_framework_warning(Some("fastifyy"), false, ts_go)
            .expect("unrecognized broadcast value must warn");
        assert!(warning.contains("fastifyy"), "got: {warning}");
        assert!(warning.contains("--framework"), "got: {warning}");
        // A real framework name landing only on framework-blind targets → still
        // unrecognized (python/openapi ignore it), warn.
        assert!(broadcast_framework_warning(Some("chi"), false, &["python", "openapi"]).is_some());
        // A bound value (`single == true`) is never a broadcast warning — it's
        // validated strictly elsewhere and errors instead.
        assert_eq!(
            broadcast_framework_warning(Some("fastifyy"), true, ts_go),
            None
        );
        // No CLI `--framework` at all → nothing to warn about.
        assert_eq!(broadcast_framework_warning(None, false, ts_go), None);
    }

    // End-to-end stand-in for the multi-target leak: a TS framework broadcast to
    // the Go target resolves to None (no error) and the written `server.go` is
    // the net/http default — exactly what `run_gen` does for a shared
    // `framework`, minus the `process::exit` that makes `run_gen` itself
    // awkward to drive from a unit test.
    #[test]
    fn broadcast_ts_framework_to_go_emits_default_server() {
        let fw = resolve_target_framework("go", Some("fastify"), false)
            .expect("broadcast value must not error for go");
        assert_eq!(fw, None, "broadcast TS framework must drop to Go's default");

        let (schema_dir, schema) = schema_in_tmp();
        let out = schema_dir.path().join("out");
        generate_once(
            &schema,
            "go",
            &out.to_string_lossy(),
            GenMode::ServerOnly,
            fw.as_deref(),
        )
        .expect("generate");
        let server = fs::read_to_string(out.join("server.go")).expect("read server.go");
        assert!(server.contains("http.NewServeMux()"), "got: {server}");
        assert!(!server.contains("chi.NewRouter()"), "got: {server}");
    }

    // The watch path goes through `generate_once`, which shares `emit_target`
    // with the one-shot `cmd_gen` path. These tests pin that shared matrix so a
    // filename / mode regression in either entry point is caught.

    #[test]
    fn generate_once_go_both_writes_all_files() {
        let (schema_dir, schema) = schema_in_tmp();
        let out = schema_dir.path().join("out");
        generate_once(&schema, "go", &out.to_string_lossy(), GenMode::Both, None)
            .expect("generate");
        assert_eq!(
            files_in(&out),
            BTreeSet::from([
                "types.go".to_string(),
                "client.go".to_string(),
                "handlers.go".to_string(),
                "server.go".to_string(),
            ])
        );
    }

    #[test]
    fn generate_once_go_client_only_omits_server_files() {
        let (schema_dir, schema) = schema_in_tmp();
        let out = schema_dir.path().join("out");
        generate_once(
            &schema,
            "go",
            &out.to_string_lossy(),
            GenMode::ClientOnly,
            None,
        )
        .expect("generate");
        assert_eq!(
            files_in(&out),
            BTreeSet::from(["types.go".to_string(), "client.go".to_string()])
        );
    }

    #[test]
    fn generate_once_go_server_only_omits_client_file() {
        let (schema_dir, schema) = schema_in_tmp();
        let out = schema_dir.path().join("out");
        generate_once(
            &schema,
            "go",
            &out.to_string_lossy(),
            GenMode::ServerOnly,
            None,
        )
        .expect("generate");
        assert_eq!(
            files_in(&out),
            BTreeSet::from([
                "types.go".to_string(),
                "handlers.go".to_string(),
                "server.go".to_string(),
            ])
        );
    }

    #[test]
    fn generate_once_openapi_writes_spec() {
        let (schema_dir, schema) = schema_in_tmp();
        let out = schema_dir.path().join("out");
        generate_once(
            &schema,
            "openapi",
            &out.to_string_lossy(),
            GenMode::Both,
            None,
        )
        .expect("generate");
        assert_eq!(files_in(&out), BTreeSet::from(["openapi.json".to_string()]));
    }

    // The codegen crate proves each framework's `server.ts` is correct; these
    // tests pin only the driver glue — that the selected `TsServerFramework`
    // actually reaches `generate_typescript_with` and lands in the written
    // `server.ts` (rather than being dropped on the way through `emit_target`).
    #[test]
    fn generate_once_typescript_default_emits_express_server() {
        let (schema_dir, schema) = schema_in_tmp();
        let out = schema_dir.path().join("out");
        generate_once(
            &schema,
            "typescript",
            &out.to_string_lossy(),
            GenMode::ServerOnly,
            None,
        )
        .expect("generate");
        let server = fs::read_to_string(out.join("server.ts")).expect("read server.ts");
        assert!(server.contains("from \"express\""), "got: {server}");
        assert!(!server.contains("from \"fastify\""), "got: {server}");
    }

    #[test]
    fn generate_once_typescript_fastify_emits_fastify_server() {
        let (schema_dir, schema) = schema_in_tmp();
        let out = schema_dir.path().join("out");
        generate_once(
            &schema,
            "typescript",
            &out.to_string_lossy(),
            GenMode::ServerOnly,
            Some("fastify"),
        )
        .expect("generate");
        let server = fs::read_to_string(out.join("server.ts")).expect("read server.ts");
        assert!(server.contains("from \"fastify\""), "got: {server}");
        assert!(!server.contains("from \"express\""), "got: {server}");
    }

    // Mirror of the TS framework-glue tests for Go: the codegen crate proves each
    // framework's `server.go` is correct; these pin only that the selected
    // `GoServerFramework` reaches `generate_go_with` and lands in the written
    // `server.go` (rather than being dropped passing through `emit_target`).
    #[test]
    fn generate_once_go_default_emits_nethttp_server() {
        let (schema_dir, schema) = schema_in_tmp();
        let out = schema_dir.path().join("out");
        generate_once(
            &schema,
            "go",
            &out.to_string_lossy(),
            GenMode::ServerOnly,
            None,
        )
        .expect("generate");
        let server = fs::read_to_string(out.join("server.go")).expect("read server.go");
        assert!(server.contains("http.NewServeMux()"), "got: {server}");
        assert!(!server.contains("chi.NewRouter()"), "got: {server}");
    }

    #[test]
    fn generate_once_go_chi_emits_chi_server() {
        let (schema_dir, schema) = schema_in_tmp();
        let out = schema_dir.path().join("out");
        generate_once(
            &schema,
            "go",
            &out.to_string_lossy(),
            GenMode::ServerOnly,
            Some("chi"),
        )
        .expect("generate");
        let server = fs::read_to_string(out.join("server.go")).expect("read server.go");
        assert!(server.contains("chi.NewRouter()"), "got: {server}");
        assert!(!server.contains("http.NewServeMux()"), "got: {server}");
    }

    #[test]
    fn generate_once_unsupported_target_errors() {
        let (schema_dir, schema) = schema_in_tmp();
        let out = schema_dir.path().join("out");
        let err = generate_once(
            &schema,
            "cobol",
            &out.to_string_lossy(),
            GenMode::Both,
            None,
        )
        .expect_err("unsupported target must error");
        assert!(err.contains("unsupported target 'cobol'"), "got: {err}");
    }
}
