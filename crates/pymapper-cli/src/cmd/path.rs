//! `pymapper path` subcommand.

use std::process::ExitCode;

#[derive(clap::Args)]
pub struct Args {}

pub fn run(_args: &Args) -> anyhow::Result<ExitCode> {
    super::not_implemented("path")
}
