//! `pymapper deps` subcommand.

use std::process::ExitCode;

/// Not implemented yet: accepts any operands and the shared configuration flags so that
/// scripts get the "not implemented" message (exit 2) rather than a usage error.
#[derive(clap::Args)]
pub struct Args {
    /// Operands (symbols, paths).
    #[arg(hide = true)]
    _operands: Vec<String>,
    #[command(flatten)]
    _config: super::config::ConfigArgs,
}

pub fn run(_args: &Args) -> anyhow::Result<ExitCode> {
    super::not_implemented("deps")
}
