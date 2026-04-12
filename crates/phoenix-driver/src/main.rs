//! The Phoenix CLI driver.
//!
//! This binary wires together the lexer, parser, semantic checker, and
//! tree-walk interpreter to provide a unified command-line interface for
//! working with Phoenix source files.
//!
//! # Subcommands
//!
//! | Command | Description |
//! |---------|-------------|
//! | `lex`   | Tokenize a source file and print the token stream |
//! | `parse` | Parse a source file and dump the AST as JSON |
//! | `check` | Type-check a source file and report errors |
//! | `run`   | Execute a Phoenix program via the tree-walk interpreter |
//! | `gen`   | Generate typed code or OpenAPI specs from a Phoenix schema file (supports `--watch`) |
#![warn(missing_docs)]

mod config;

use clap::{Parser, Subcommand};
use std::fs;
use std::process;

use config::PhoenixConfig;
use phoenix_codegen::GenMode;
use phoenix_common::source::SourceMap;
use phoenix_common::span::SourceId;
use phoenix_interp::interpreter;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

#[derive(Parser)]
#[command(name = "phoenix")]
#[command(about = "The Phoenix programming language")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Tokenize a source file and print the token stream
    Lex {
        /// Path to the source file
        file: String,
    },
    /// Parse a source file and print the AST as JSON
    Parse {
        /// Path to the source file
        file: String,
    },
    /// Type-check a source file
    Check {
        /// Path to the source file
        file: String,
    },
    /// Run a Phoenix program
    Run {
        /// Path to the source file
        file: String,
    },
    /// Generate typed code from a Phoenix schema file
    Gen {
        /// Path to the .phx schema file (or set gen.schema in phoenix.toml)
        #[arg(value_name = "FILE")]
        file: Option<String>,
        /// Target language (typescript, python, go, openapi)
        #[arg(long)]
        target: Option<String>,
        /// Output directory for generated files
        #[arg(long, short)]
        out: Option<String>,
        /// Watch for .phx file changes and re-generate automatically
        #[arg(long)]
        watch: bool,
        /// Generate only client code (types + client SDK)
        #[arg(long)]
        client: bool,
        /// Generate only server code (types + handlers + router)
        #[arg(long)]
        server: bool,
    },
}

fn main() {
    // Spawn the real work on a thread with a 16 MiB stack to handle deep
    // recursion in the parser and interpreter.  The default 8 MiB stack is
    // not enough for complex programs when RUST_MIN_STACK is not set
    // (e.g. binaries installed via the install script).
    let builder = std::thread::Builder::new().stack_size(16 * 1024 * 1024);
    let handler = builder.spawn(run).expect("failed to spawn main thread");
    if let Err(e) = handler.join() {
        std::panic::resume_unwind(e);
    }
}

fn run() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Lex { file } => cmd_lex(&file),
        Commands::Parse { file } => cmd_parse(&file),
        Commands::Check { file } => cmd_check(&file),
        Commands::Run { file } => cmd_run(&file),
        Commands::Gen {
            file,
            target,
            out,
            watch,
            client,
            server,
        } => {
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
fn read_source(path: &str) -> (SourceMap, SourceId, String) {
    let contents = fs::read_to_string(path).unwrap_or_else(|err| {
        eprintln!("error: could not read file '{}': {}", path, err);
        process::exit(1);
    });
    let mut source_map = SourceMap::new();
    let source_id = source_map.add(path, &contents);
    (source_map, source_id, contents)
}

/// Prints a slice of diagnostics to stderr with `file:line:col` prefixes.
fn report_diagnostics(
    diagnostics: &[phoenix_common::diagnostics::Diagnostic],
    source_map: &SourceMap,
    source_id: SourceId,
) {
    for diag in diagnostics {
        let loc = source_map.line_col(source_id, diag.span.start);
        eprintln!(
            "{}:{}:{}: {}",
            source_map.name(source_id),
            loc.line,
            loc.col,
            diag,
        );
    }
}

fn cmd_lex(path: &str) {
    let (_source_map, source_id, contents) = read_source(path);
    let tokens = tokenize(&contents, source_id);

    for token in &tokens {
        println!(
            "{:?}\t{:?}\t[{}..{}]",
            token.kind, token.text, token.span.start, token.span.end
        );
    }
}

fn cmd_parse(path: &str) {
    let (source_map, source_id, contents) = read_source(path);
    let tokens = tokenize(&contents, source_id);
    let (program, diagnostics) = parser::parse(&tokens);

    if !diagnostics.is_empty() {
        report_diagnostics(&diagnostics, &source_map, source_id);
        process::exit(1);
    }

    let json = serde_json::to_string_pretty(&program).unwrap_or_else(|err| {
        eprintln!("error: failed to serialize AST: {}", err);
        process::exit(1);
    });
    println!("{}", json);
}

/// Parses and type-checks a source file, exiting on errors.
/// Returns the program AST and the semantic check result.
fn parse_and_check(path: &str) -> (phoenix_parser::ast::Program, phoenix_sema::CheckResult) {
    let (source_map, source_id, contents) = read_source(path);
    let tokens = tokenize(&contents, source_id);
    let (program, parse_errors) = parser::parse(&tokens);

    if !parse_errors.is_empty() {
        report_diagnostics(&parse_errors, &source_map, source_id);
        process::exit(1);
    }

    let result = checker::check(&program);
    if !result.diagnostics.is_empty() {
        report_diagnostics(&result.diagnostics, &source_map, source_id);
        process::exit(1);
    }

    (program, result)
}

fn cmd_check(path: &str) {
    parse_and_check(path);
    println!("No errors found.");
}

fn cmd_run(path: &str) {
    let (program, check_result) = parse_and_check(path);
    if let Err(err) = interpreter::run(&program, check_result.lambda_captures) {
        eprintln!("runtime error: {}", err);
        process::exit(1);
    }
}

// ── Code generation ────────────────────────────────────────────────

/// Parses, type-checks, and generates code from a Phoenix schema file.
///
/// Supports `typescript`, `python`, `go`, and `openapi` targets. The `mode`
/// parameter controls whether to emit client-only, server-only, or all files.
fn cmd_gen(path: &str, target: &str, out_dir: &str, mode: GenMode) {
    let (program, check_result) = parse_and_check(path);

    match target {
        "typescript" => cmd_gen_typescript(&program, &check_result, out_dir, mode),
        "python" => cmd_gen_python(&program, &check_result, out_dir, mode),
        "go" => cmd_gen_go(&program, &check_result, out_dir, mode),
        "openapi" => {
            if mode != GenMode::Both {
                eprintln!("warning: --client/--server flags have no effect on OpenAPI target");
            }
            cmd_gen_openapi(&program, &check_result, out_dir);
        }
        _ => {
            eprintln!(
                "error: unsupported target '{}' (supported: typescript, python, go, openapi)",
                target
            );
            process::exit(1);
        }
    }
}

/// Creates the output directory and returns a file-writing closure.
fn prepare_out_dir(out_dir: &str) -> impl Fn(&str, &str) -> String + '_ {
    fs::create_dir_all(out_dir).unwrap_or_else(|err| {
        eprintln!(
            "error: could not create output directory '{}': {}",
            out_dir, err
        );
        process::exit(1);
    });
    move |name: &str, content: &str| {
        let path = format!("{}/{}", out_dir, name);
        fs::write(&path, content).unwrap_or_else(|err| {
            eprintln!("error: could not write '{}': {}", path, err);
            process::exit(1);
        });
        path
    }
}

/// Generates TypeScript files (types, client, handlers, server).
fn cmd_gen_typescript(
    program: &phoenix_parser::ast::Program,
    check_result: &phoenix_sema::CheckResult,
    out_dir: &str,
    mode: GenMode,
) {
    let files = phoenix_codegen::generate_typescript(program, check_result);
    let write = prepare_out_dir(out_dir);

    let mut generated = vec![write("types.ts", &files.types)];
    if mode != GenMode::ServerOnly {
        generated.push(write("client.ts", &files.client));
    }
    if mode != GenMode::ClientOnly {
        generated.push(write("handlers.ts", &files.handlers));
        generated.push(write("server.ts", &files.server));
    }

    println!("Generated {}", generated.join(", "));
}

/// Generates Python files (models, client, handlers, server).
fn cmd_gen_python(
    program: &phoenix_parser::ast::Program,
    check_result: &phoenix_sema::CheckResult,
    out_dir: &str,
    mode: GenMode,
) {
    let files = phoenix_codegen::generate_python(program, check_result);
    let write = prepare_out_dir(out_dir);

    let mut generated = vec![write("models.py", &files.models)];
    if mode != GenMode::ServerOnly {
        generated.push(write("client.py", &files.client));
    }
    if mode != GenMode::ClientOnly {
        generated.push(write("handlers.py", &files.handlers));
        generated.push(write("server.py", &files.server));
    }

    println!("Generated {}", generated.join(", "));
}

/// Generates Go files (types, client, handlers, server).
fn cmd_gen_go(
    program: &phoenix_parser::ast::Program,
    check_result: &phoenix_sema::CheckResult,
    out_dir: &str,
    mode: GenMode,
) {
    let files = phoenix_codegen::generate_go(program, check_result);
    let write = prepare_out_dir(out_dir);

    let mut generated = vec![write("types.go", &files.types)];
    if mode != GenMode::ServerOnly {
        generated.push(write("client.go", &files.client));
    }
    if mode != GenMode::ClientOnly {
        generated.push(write("handlers.go", &files.handlers));
        generated.push(write("server.go", &files.server));
    }

    println!("Generated {}", generated.join(", "));
}

/// Generates an OpenAPI 3.1 JSON specification.
fn cmd_gen_openapi(
    program: &phoenix_parser::ast::Program,
    check_result: &phoenix_sema::CheckResult,
    out_dir: &str,
) {
    let spec = phoenix_codegen::generate_openapi(program, check_result);

    fs::create_dir_all(out_dir).unwrap_or_else(|err| {
        eprintln!(
            "error: could not create output directory '{}': {}",
            out_dir, err
        );
        process::exit(1);
    });

    let spec_path = format!("{}/openapi.json", out_dir);
    fs::write(&spec_path, &spec).unwrap_or_else(|err| {
        eprintln!("error: could not write '{}': {}", spec_path, err);
        process::exit(1);
    });

    println!("Generated {}", spec_path);
}

// ── Watch mode ──────────────────────────────────────────────────────

/// Parses and type-checks a source file, returning the results or an error.
///
/// Unlike [`parse_and_check`], this does not call `process::exit` on failure.
/// Diagnostics are printed to stderr and an `Err` is returned so the caller
/// can decide how to proceed (e.g., continue in watch mode).
fn try_parse_and_check(
    path: &str,
) -> Result<(phoenix_parser::ast::Program, phoenix_sema::CheckResult), String> {
    let contents =
        fs::read_to_string(path).map_err(|err| format!("could not read '{}': {}", path, err))?;
    let mut source_map = SourceMap::new();
    let source_id = source_map.add(path, &contents);
    let tokens = tokenize(&contents, source_id);
    let (program, parse_errors) = parser::parse(&tokens);

    if !parse_errors.is_empty() {
        report_diagnostics(&parse_errors, &source_map, source_id);
        return Err("parse errors".to_string());
    }

    let result = checker::check(&program);
    if !result.diagnostics.is_empty() {
        report_diagnostics(&result.diagnostics, &source_map, source_id);
        return Err("type errors".to_string());
    }

    Ok((program, result))
}

/// Runs code generation once, returning `Ok(())` on success or `Err(message)`
/// on failure. Unlike the `cmd_gen_*` functions, this does not call
/// `process::exit` — errors are reported to stderr and the caller decides
/// whether to continue (e.g., in watch mode).
fn generate_once(path: &str, target: &str, out_dir: &str, mode: GenMode) -> Result<(), String> {
    let (program, result) = try_parse_and_check(path)?;

    fs::create_dir_all(out_dir)
        .map_err(|err| format!("could not create '{}': {}", out_dir, err))?;

    match target {
        "typescript" => {
            let files = phoenix_codegen::generate_typescript(&program, &result);
            write_file(out_dir, "types.ts", &files.types)?;
            if mode != GenMode::ServerOnly {
                write_file(out_dir, "client.ts", &files.client)?;
            }
            if mode != GenMode::ClientOnly {
                write_file(out_dir, "handlers.ts", &files.handlers)?;
                write_file(out_dir, "server.ts", &files.server)?;
            }
        }
        "python" => {
            let files = phoenix_codegen::generate_python(&program, &result);
            write_file(out_dir, "models.py", &files.models)?;
            if mode != GenMode::ServerOnly {
                write_file(out_dir, "client.py", &files.client)?;
            }
            if mode != GenMode::ClientOnly {
                write_file(out_dir, "handlers.py", &files.handlers)?;
                write_file(out_dir, "server.py", &files.server)?;
            }
        }
        "go" => {
            let files = phoenix_codegen::generate_go(&program, &result);
            write_file(out_dir, "types.go", &files.types)?;
            if mode != GenMode::ServerOnly {
                write_file(out_dir, "client.go", &files.client)?;
            }
            if mode != GenMode::ClientOnly {
                write_file(out_dir, "handlers.go", &files.handlers)?;
                write_file(out_dir, "server.go", &files.server)?;
            }
        }
        "openapi" => {
            let spec = phoenix_codegen::generate_openapi(&program, &result);
            write_file(out_dir, "openapi.json", &spec)?;
        }
        _ => return Err(format!("unsupported target '{}'", target)),
    }
    Ok(())
}

/// Writes a single file to the output directory.
fn write_file(out_dir: &str, name: &str, content: &str) -> Result<(), String> {
    let path = format!("{}/{}", out_dir, name);
    fs::write(&path, content).map_err(|err| format!("could not write '{}': {}", path, err))
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
    for (target, _, _) in targets {
        if !matches!(*target, "typescript" | "python" | "go" | "openapi") {
            eprintln!(
                "error: unsupported target '{}' (supported: typescript, python, go, openapi)",
                target
            );
            process::exit(1);
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
