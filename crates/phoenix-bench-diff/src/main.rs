//! CLI entry point for `phoenix-bench-diff`. Argument parsing only;
//! the bulk of the logic lives in the `phoenix_bench_diff` library
//! crate (see `commands`, `baseline`, `pause`, `criterion_walk`,
//! `routing`).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use phoenix_bench_diff::commands::{DEFAULT_REGRESSION_THRESHOLD, cmd_diff, cmd_update};
use phoenix_bench_diff::pause::DEFAULT_PAUSE_SIDECAR;

#[derive(Parser)]
#[command(
    name = "phoenix-bench-diff",
    about = "Detect bench regressions for Phoenix"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compare a bench run against the committed baselines.
    Diff {
        /// Directory containing committed baseline `.md` files.
        #[arg(long, default_value = "docs/perf-baselines")]
        baseline: PathBuf,
        /// Directory containing criterion's per-bench output
        /// (`target/criterion/` by default).
        #[arg(long, default_value = "target/criterion")]
        criterion: PathBuf,
        /// Path to the pause bench's JSON sidecar.
        #[arg(long, default_value = DEFAULT_PAUSE_SIDECAR)]
        pause_sidecar: PathBuf,
        /// Regression threshold as a fraction (0.20 = 20% slower).
        #[arg(long, default_value_t = DEFAULT_REGRESSION_THRESHOLD)]
        threshold: f64,
    },
    /// Overwrite the committed baselines from the latest bench run.
    Update {
        /// Directory containing committed baseline `.md` files.
        #[arg(long, default_value = "docs/perf-baselines")]
        baseline: PathBuf,
        /// Directory containing criterion's per-bench output.
        #[arg(long, default_value = "target/criterion")]
        criterion: PathBuf,
        /// Path to the pause bench's JSON sidecar.
        #[arg(long, default_value = DEFAULT_PAUSE_SIDECAR)]
        pause_sidecar: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Diff {
            baseline,
            criterion,
            pause_sidecar,
            threshold,
        } => cmd_diff(&baseline, &criterion, &pause_sidecar, threshold),
        Command::Update {
            baseline,
            criterion,
            pause_sidecar,
        } => cmd_update(&baseline, &criterion, &pause_sidecar),
    }
}
