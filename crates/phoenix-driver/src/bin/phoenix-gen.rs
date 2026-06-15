//! Standalone `phoenix-gen` binary.
//!
//! Behaves exactly as if `phoenix gen` were the only subcommand, for users who
//! only care about code generation and never touch the Phoenix language
//! directly: `phoenix-gen schema.phx --target typescript --out ./generated`.
//!
//! It shares ONE implementation ([`phoenix_driver::run_gen`]) with the
//! `phoenix gen` subcommand, so the two cannot drift.
#![warn(missing_docs)]

use clap::Parser;

use phoenix_driver::run_gen;

/// Generate typed code from a Phoenix schema file.
///
/// The arguments mirror the `phoenix gen` subcommand exactly.
#[derive(Parser)]
#[command(name = "phoenix-gen")]
#[command(about = "Generate typed code from a Phoenix schema file")]
#[command(version)]
struct Cli {
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
    /// TypeScript server framework: `express` (default) or `fastify`
    #[arg(long)]
    ts_framework: Option<String>,
}

fn main() {
    // Match the `phoenix` binary's large-stack behavior so deep schemas parse
    // identically under both entry points.
    let builder = std::thread::Builder::new().stack_size(16 * 1024 * 1024);
    let handler = builder.spawn(run).expect("failed to spawn main thread");
    if let Err(e) = handler.join() {
        std::panic::resume_unwind(e);
    }
}

fn run() {
    let cli = Cli::parse();
    run_gen(
        cli.file,
        cli.target,
        cli.out,
        cli.client,
        cli.server,
        cli.watch,
        cli.ts_framework,
    );
}
