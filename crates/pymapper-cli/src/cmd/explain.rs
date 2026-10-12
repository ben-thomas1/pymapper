//! `pymapper explain` subcommand (design: ISSUES O3): everything pymapper knows about one
//! finding (by id, `PM401-3fa2`) or every finding on one symbol, so an agent can verify and
//! act on it without rebuilding the context by hand. Stateless: re-runs the analysis (warm
//! cache) with the same configuration as `check` and looks the finding up by its stable id
//! (`pymapper_core::output::FindingIds::find`).
//!
//! PLACEHOLDER: the arguments are final; the implementation is being built.

use std::path::PathBuf;
use std::process::ExitCode;

use super::config::ConfigArgs;

#[derive(clap::Args)]
pub struct Args {
    /// A finding id from `check` (PM401-3fa2), or a symbol: a qualified id
    /// (pkg.mod:Class.method), a dotted name or its last segments.
    target: String,
    /// Project to analyse (default: the working directory); config and project-root discovery
    /// start here.
    path: Option<PathBuf>,
    #[command(flatten)]
    config: ConfigArgs,
}

pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    let _ = (&args.target, &args.path, &args.config);
    anyhow::bail!("`pymapper explain` is not implemented yet")
}
