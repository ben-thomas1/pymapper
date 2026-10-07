//! Dev tooling for pymapper. Run with `cargo xtask <command>`. Never shipped.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "pymapper dev tooling")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Measure time and peak RSS on the benchmark corpora.
    Bench,
    /// Compare extracted facts/metrics against ruff's parser.
    DiffRuff,
    /// Fuzz the lexer/parser.
    Fuzz,
}

fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    let name = match cli.command {
        Command::Bench => "bench",
        Command::DiffRuff => "diff-ruff",
        Command::Fuzz => "fuzz",
    };
    eprintln!("xtask {name}: not implemented");
    Ok(ExitCode::from(2))
}
