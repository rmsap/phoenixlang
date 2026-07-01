//! The Phoenix CLI driver.
//!
//! This binary is a thin clap front-end over the [`phoenix_driver`] library
//! crate, which holds the shared implementation (notably the code-generation
//! pipeline shared with the standalone `phoenix-gen` binary).
//!
//! # Subcommands
//!
//! | Command | Description |
//! |---------|-------------|
//! | `lex`    | Tokenize a source file and print the token stream |
//! | `parse`  | Parse a source file and dump the AST as JSON |
//! | `check`  | Type-check a source file and report errors |
//! | `run`    | Execute a Phoenix program via the tree-walk interpreter |
//! | `build`  | Compile a Phoenix program to a native executable via Cranelift |
//! | `gen`    | Generate typed code or OpenAPI specs from a Phoenix schema file (supports `--watch`) |
//! | `ir`     | Dump the SSA-style IR for a Phoenix source file |
//! | `run-ir` | Run a Phoenix program via the IR interpreter |
#![warn(missing_docs)]

use clap::{Parser, Subcommand};

use phoenix_driver::{build, pkg_cli};
use phoenix_driver::{cmd_check, cmd_ir, cmd_lex, cmd_parse, cmd_run, cmd_run_ir, run_gen};

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
    /// Scaffold a new project (phoenix.toml + main.phx) in the current directory
    Init {
        /// Package name (default: the current directory's name)
        #[arg(long)]
        name: Option<String>,
    },
    /// Add a dependency to phoenix.toml and refresh phoenix.lock
    Add {
        /// Dependency name (the import prefix; the key under [dependencies])
        name: String,
        /// Git source URL
        #[arg(long)]
        git: Option<String>,
        /// Git tag to pin (use with --git)
        #[arg(long)]
        tag: Option<String>,
        /// Git commit to pin (use with --git)
        #[arg(long)]
        rev: Option<String>,
        /// Git branch to track (use with --git)
        #[arg(long)]
        branch: Option<String>,
        /// Local path source (relative to phoenix.toml)
        #[arg(long)]
        path: Option<String>,
    },
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
        /// Require phoenix.lock to be up to date; error on drift instead of updating it
        #[arg(long)]
        locked: bool,
    },
    /// Run a Phoenix program
    Run {
        /// Path to the source file
        file: String,
        /// Require phoenix.lock to be up to date; error on drift instead of updating it
        #[arg(long)]
        locked: bool,
    },
    /// Dump the SSA-style intermediate representation
    Ir {
        /// Path to the source file
        file: String,
    },
    /// Run a Phoenix program via the IR interpreter
    RunIr {
        /// Path to the source file
        file: String,
    },
    /// Compile a Phoenix program to a native executable
    Build {
        /// Path to the source file
        file: String,
        /// Output executable path (default: input filename without extension)
        #[arg(long, short)]
        output: Option<String>,
        /// Compilation target: `native` (default), `wasm32-linear`, or
        /// `wasm32-gc`. The WASM variants land incrementally during
        /// Phase 2.4 — see `docs/design-decisions.md` §Phase 2.4
        /// WebAssembly compilation for the per-target PR sequence.
        #[arg(long)]
        target: Option<String>,
        /// Require phoenix.lock to be up to date; error on drift instead of updating it
        #[arg(long)]
        locked: bool,
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
        /// Server framework (per target): TypeScript express|fastify, Go net/http|chi
        #[arg(long)]
        framework: Option<String>,
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
        Commands::Init { name } => pkg_cli::cmd_init(name.as_deref()),
        Commands::Add {
            name,
            git,
            tag,
            rev,
            branch,
            path,
        } => pkg_cli::cmd_add(&name, git, tag, rev, branch, path),
        Commands::Lex { file } => cmd_lex(&file),
        Commands::Parse { file } => cmd_parse(&file),
        Commands::Check { file, locked } => cmd_check(&file, locked),
        Commands::Ir { file } => cmd_ir(&file),
        Commands::Run { file, locked } => cmd_run(&file, locked),
        Commands::RunIr { file } => cmd_run_ir(&file),
        Commands::Build {
            file,
            output,
            target,
            locked,
        } => build::cmd_build(&file, output.as_deref(), target.as_deref(), locked),
        Commands::Gen {
            file,
            target,
            out,
            watch,
            client,
            server,
            framework,
        } => run_gen(file, target, out, client, server, watch, framework),
    }
}
