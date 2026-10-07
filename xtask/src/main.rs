//! Dev tooling for pymapper. Run with `cargo xtask <command>`. Never shipped.
//!
//! Exit codes: 0 ok, 1 findings (e.g. benchmark regressions), 2 usage or fatal error.

mod bench;
mod build;
mod fuzz;

use std::io::Write;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Exit code for a usage or fatal error (clap also uses 2 for usage errors).
const EXIT_ERROR: u8 = 2;

#[derive(Parser)]
#[command(name = "xtask", about = "pymapper dev tooling")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Measure time and peak RSS on the benchmark corpora (docs/benchmarks.md).
    Bench(bench::Args),
    /// Internal: one measured run in a fresh process; prints a JSON record on stdout.
    #[command(hide = true)]
    BenchRun(bench::RunArgs),
    /// Compare extracted facts/metrics against ruff's parser.
    DiffRuff,
    /// Mutation-fuzz the lexer/parser (docs/fuzz.md).
    Fuzz(fuzz::Args),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Bench(args) => bench::run(args),
        Command::BenchRun(args) => bench::run_child(args),
        Command::DiffRuff => not_implemented("diff-ruff"),
        Command::Fuzz(args) => fuzz::run(args),
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            // Ignore write failures: stderr may be closed and there is nowhere else to report.
            let _ = writeln!(std::io::stderr().lock(), "xtask: error: {err:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn not_implemented(name: &str) -> anyhow::Result<ExitCode> {
    eprintln!("xtask {name}: not implemented");
    Ok(ExitCode::from(EXIT_ERROR))
}
