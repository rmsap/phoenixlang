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

use std::fs;
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
        // CLI specifies a single target — use it with CLI out/mode
        let out = out
            .or(config.codegen.out_dir)
            .unwrap_or_else(|| "./generated".to_string());
        let mode = cli_mode.unwrap_or_else(|| parse_mode(config.codegen.mode.as_deref()));
        if watch {
            cmd_gen_watch(&file, &[(&cli_target, out.as_str(), mode)]);
        } else {
            cmd_gen(&file, &cli_target, &out, mode);
        }
    } else if let Some(resolved) = config.codegen.resolve_targets() {
        // Config provides target(s) — run them all
        if watch {
            let targets: Vec<(&str, &str, GenMode)> = resolved
                .iter()
                .map(|rt| {
                    let out_dir = out.as_deref().unwrap_or(&rt.out_dir);
                    let mode = cli_mode.unwrap_or_else(|| parse_mode(rt.mode.as_deref()));
                    (rt.target.as_str(), out_dir, mode)
                })
                .collect();
            cmd_gen_watch(&file, &targets);
        } else {
            for rt in &resolved {
                let out_dir = out.as_deref().unwrap_or(&rt.out_dir);
                let mode = cli_mode.unwrap_or_else(|| parse_mode(rt.mode.as_deref()));
                cmd_gen(&file, &rt.target, out_dir, mode);
            }
        }
    } else {
        // No config targets — fall back to defaults
        let out = out.unwrap_or_else(|| "./generated".to_string());
        let mode = cli_mode.unwrap_or(GenMode::Both);
        if watch {
            cmd_gen_watch(&file, &[("typescript", out.as_str(), mode)]);
        } else {
            cmd_gen(&file, "typescript", &out, mode);
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
) -> (phoenix_parser::ast::Program, phoenix_sema::Analysis) {
    let (modules, analysis, _source_map) = parse_resolve_check(path);
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
) -> (Vec<ResolvedSourceModule>, phoenix_sema::Analysis, SourceMap) {
    let mut source_map = SourceMap::new();
    let modules = match phoenix_modules::resolve(std::path::Path::new(path), &mut source_map) {
        Ok(modules) => modules,
        Err(err) => {
            report_resolve_error(&err, &source_map);
            process::exit(1);
        }
    };

    let analysis = checker::check_modules(&modules);
    if !analysis.diagnostics.is_empty() {
        report_diagnostics(&analysis.diagnostics, &source_map);
        process::exit(1);
    }

    (modules, analysis, source_map)
}

/// Type-checks a source file and reports diagnostics.
pub fn cmd_check(path: &str) {
    parse_and_check(path);
    println!("No errors found.");
}

/// Lower a Phoenix source file to IR and print the textual representation.
pub fn cmd_ir(path: &str) {
    let (modules, check_result, _sm) = parse_resolve_check(path);
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

/// Runs a Phoenix program via the tree-walk interpreter.
pub fn cmd_run(path: &str) {
    // The AST interpreter goes through `interpreter::run_modules` so
    // multi-file programs work end-to-end (sema's `module_scopes`
    // translate cross-module name references). For single-file
    // inputs this reduces to the same behavior as the previous
    // single-program path — entry module qualifies to bare, every
    // name resolves identically.
    let (modules, mut analysis, _sm) = parse_resolve_check(path);
    if let Err(err) = interpreter::run_modules(&modules, &mut analysis) {
        eprintln!("runtime error: {}", err);
        process::exit(1);
    }
}

/// Run a Phoenix program via the IR interpreter.
pub fn cmd_run_ir(path: &str) {
    let (modules, check_result, _sm) = parse_resolve_check(path);
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
fn cmd_gen(path: &str, target: &str, out_dir: &str, mode: GenMode) {
    let (program, check_result) = parse_and_check(path);

    if target == "openapi" && mode != GenMode::Both {
        eprintln!("warning: --client/--server flags have no effect on OpenAPI target");
    }

    match emit_target(&program, &check_result, target, out_dir, mode) {
        Ok(generated) => println!("Generated {}", generated.join(", ")),
        Err(e) => {
            eprintln!("error: {}", e);
            process::exit(1);
        }
    }
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
) -> Result<Vec<String>, String> {
    fs::create_dir_all(out_dir)
        .map_err(|err| format!("could not create output directory '{}': {}", out_dir, err))?;

    let mut generated = Vec::new();
    match target {
        "typescript" => {
            let files = phoenix_codegen::generate_typescript(program, check_result);
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
            let files = phoenix_codegen::generate_go(program, check_result);
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
    let modules = match phoenix_modules::resolve(std::path::Path::new(path), &mut source_map) {
        Ok(modules) => modules,
        Err(err) => {
            report_resolve_error(&err, &source_map);
            return Err("resolve / parse errors".to_string());
        }
    };

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
fn generate_once(path: &str, target: &str, out_dir: &str, mode: GenMode) -> Result<(), String> {
    let (program, result) = try_parse_and_check(path)?;
    emit_target(&program, &result, target, out_dir, mode)?;
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
fn cmd_gen_watch(path: &str, targets: &[(&str, &str, GenMode)]) {
    use notify::{Event, EventKind, RecursiveMode, Watcher};
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    // Validate all targets before starting watch
    for (target, _, mode) in targets {
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
    let target_list: Vec<&str> = targets.iter().map(|(t, _, _)| *t).collect();
    eprintln!(
        "[phoenix gen] targets={}, watching {}",
        target_list.join(", "),
        watch_dir.display()
    );
    let mut had_error = false;
    for (target, out_dir, mode) in targets {
        match generate_once(path, target, out_dir, *mode) {
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
            for (target, out_dir, mode) in targets {
                match generate_once(path, target, out_dir, *mode) {
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
struct User { Int id  String name }
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

    // The watch path goes through `generate_once`, which shares `emit_target`
    // with the one-shot `cmd_gen` path. These tests pin that shared matrix so a
    // filename / mode regression in either entry point is caught.

    #[test]
    fn generate_once_go_both_writes_all_files() {
        let (schema_dir, schema) = schema_in_tmp();
        let out = schema_dir.path().join("out");
        generate_once(&schema, "go", &out.to_string_lossy(), GenMode::Both).expect("generate");
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
        generate_once(&schema, "go", &out.to_string_lossy(), GenMode::ClientOnly)
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
        generate_once(&schema, "go", &out.to_string_lossy(), GenMode::ServerOnly)
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
        generate_once(&schema, "openapi", &out.to_string_lossy(), GenMode::Both).expect("generate");
        assert_eq!(files_in(&out), BTreeSet::from(["openapi.json".to_string()]));
    }

    #[test]
    fn generate_once_unsupported_target_errors() {
        let (schema_dir, schema) = schema_in_tmp();
        let out = schema_dir.path().join("out");
        let err = generate_once(&schema, "cobol", &out.to_string_lossy(), GenMode::Both)
            .expect_err("unsupported target must error");
        assert!(err.contains("unsupported target 'cobol'"), "got: {err}");
    }
}
