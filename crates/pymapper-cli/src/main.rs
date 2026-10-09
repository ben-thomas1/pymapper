//! `pymapper`: find problems in Python codebases with whole-program analysis.
//!
//! Exit codes: 0 clean, 1 findings, 2 usage or fatal error.

mod cmd;
mod pipeline;
mod stats;

use std::io::Write;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Exit code for a usage or fatal error (clap also uses 2 for usage errors).
pub(crate) const EXIT_ERROR: u8 = 2;

#[derive(Parser)]
#[command(name = "pymapper", version, about = "Whole-program analysis for Python codebases")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Subcommands. The query and `deps` / `cycles` / `summary` subcommands are hidden from
/// `--help` until they are implemented; running them reports "not implemented" (exit 2).
#[derive(Subcommand)]
enum Command {
    /// Run rule checks and report findings.
    Check(Box<cmd::check::Args>),
    /// List callers of a symbol.
    #[command(hide = true)]
    Callers(cmd::callers::Args),
    /// List callees of a symbol.
    #[command(hide = true)]
    Callees(cmd::callees::Args),
    /// Show everything transitively affected by changing a symbol.
    #[command(hide = true)]
    Impact(cmd::impact::Args),
    /// Find a call/import path between two symbols.
    #[command(hide = true)]
    Path(cmd::path::Args),
    /// Analyse third-party dependencies.
    #[command(hide = true)]
    Deps(cmd::deps::Args),
    /// Report import cycles.
    #[command(hide = true)]
    Cycles(cmd::cycles::Args),
    /// Summarise the codebase.
    #[command(hide = true)]
    Summary(cmd::summary::Args),
    /// Show the effective configuration, or print the commented template.
    Config(Box<cmd::config::Args>),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Check(args) => cmd::check::run(*args),
        Command::Callers(args) => cmd::callers::run(&args),
        Command::Callees(args) => cmd::callees::run(&args),
        Command::Impact(args) => cmd::impact::run(&args),
        Command::Path(args) => cmd::path::run(&args),
        Command::Deps(args) => cmd::deps::run(&args),
        Command::Cycles(args) => cmd::cycles::run(&args),
        Command::Summary(args) => cmd::summary::run(&args),
        Command::Config(args) => cmd::config::run(*args),
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            // Ignore write failures: stderr may be closed and there is nowhere else to report.
            let _ = writeln!(std::io::stderr().lock(), "pymapper: error: {err:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}
