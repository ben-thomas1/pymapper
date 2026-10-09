//! `pymapper callees` subcommand: what a symbol calls (and, with `--depth`, what those call).
//! The arguments and the implementation are `callers`'s, walking forward edges.

use std::process::ExitCode;

use super::query::hops::Side;

pub use super::callers::Args;

pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    super::callers::run_walk(args, Side::Callees)
}
