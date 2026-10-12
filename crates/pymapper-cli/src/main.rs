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

/// Subcommands. The queries (`callers` to `summary`) share the whole-program load of `check`
/// and its configuration flags (see `cmd::query`).
#[derive(Subcommand)]
enum Command {
    /// Run rule checks and report findings.
    Check(Box<cmd::check::Args>),
    /// List the callers of a symbol, at their call sites.
    Callers(Box<cmd::callers::Args>),
    /// List the callees of a symbol, at their call sites.
    Callees(Box<cmd::callees::Args>),
    /// Show everything that transitively calls or imports a symbol: what a change can break.
    Impact(Box<cmd::impact::Args>),
    /// Find a shortest call (or import) chain from one symbol to another.
    Path(Box<cmd::path::Args>),
    /// List the module-level imports of a symbol, in and out, optionally rolled up to packages.
    Imports(Box<cmd::imports::Args>),
    /// List third-party distributions, how much first-party code uses each, and (--score)
    /// their scorecards.
    Deps(Box<cmd::deps::Args>),
    /// Explain one finding (by id, e.g. PM401-3fa2) or every finding on a symbol: evidence,
    /// source, what else is affected, how to verify and fix it.
    Explain(Box<cmd::explain::Args>),
    /// List import cycles (or, with --calls, the recursion through a symbol).
    Cycles(Box<cmd::cycles::Args>),
    /// Summarise the codebase: size per origin, resolution rates, cycles, top hotspots.
    Summary(Box<cmd::summary::Args>),
    /// Show the effective configuration, or print the commented template.
    Config(Box<cmd::config::Args>),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Check(args) => cmd::check::run(*args),
        Command::Callers(args) => cmd::callers::run(*args),
        Command::Callees(args) => cmd::callees::run(*args),
        Command::Impact(args) => cmd::impact::run(*args),
        Command::Path(args) => cmd::path::run(*args),
        Command::Imports(args) => cmd::imports::run(*args),
        Command::Deps(args) => cmd::deps::run(*args),
        Command::Explain(args) => cmd::explain::run(*args),
        Command::Cycles(args) => cmd::cycles::run(*args),
        Command::Summary(args) => cmd::summary::run(*args),
        Command::Config(args) => cmd::config::run(*args),
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            // Ignore write failures: stderr may be closed and there is nowhere else to report.
            let _ = writeln!(std::io::stderr().lock(), "pymapper: error: {}", error_chain(&err));
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// `err` and its causes joined by `: `, skipping a cause whose text the message already holds
/// (errors that embed their source, such as `cannot read x: <io error>`).
fn error_chain(err: &anyhow::Error) -> String {
    let mut msg = err.to_string();
    for cause in err.chain().skip(1) {
        let c = cause.to_string();
        if !msg.contains(&c) {
            msg.push_str(": ");
            msg.push_str(&c);
        }
    }
    msg
}
