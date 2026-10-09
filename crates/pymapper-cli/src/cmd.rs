//! Subcommands. Each lives in its own file (arguments + `run`) so steps can work on them in parallel.

pub mod callees;
pub mod callers;
pub mod check;
pub mod config;
pub mod cycles;
pub mod deps;
pub mod impact;
pub mod path;
pub mod summary;

use std::io::Write;
use std::process::ExitCode;

/// Placeholder for subcommands that are not implemented yet: reports on stderr and exits 2.
pub fn not_implemented(name: &str) -> anyhow::Result<ExitCode> {
    // Ignore write failures: stderr may be closed and there is nowhere else to report.
    let _ = writeln!(std::io::stderr().lock(), "pymapper {name}: not implemented");
    Ok(ExitCode::from(crate::EXIT_ERROR))
}
