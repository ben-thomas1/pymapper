//! `pymapper deps` subcommand: third-party distributions, how much of each first-party code
//! uses, and (`--score`) their scorecards.
//!
//! ```text
//!   config (include-deps forced on) ──> roots ──> walk ──> index (cache) ──> imports
//!     ──> calls ──> usage (importers, API used, LOC reached)  from first-party files in PATHS
//!     ──> --score: scorecards (cached per distribution in `scores.pmc`, computed when missing)
//!     ──> report (concise | json) ──> exit 0
//! ```
//!
//! Scorecards are information, never findings: the exit code is 0 whatever the grades, 2 only
//! for usage and fatal errors (as `check`). `--check` runs the dependency-hygiene rules instead
//! (PM601-PM604, see [`hygiene`]), with the findings, formats and exit codes of `check`.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::Context;
use pymapper_core::cache::Cache;
use pymapper_core::config::OutputFormat;
use pymapper_core::deps::{self, DepsReport, GroupFiles, ReportOptions, ScoreCtx, ScoreStore, UsageOptions};
use pymapper_core::resolve::resolve_calls;
use pymapper_core::roots::Origin;
use pymapper_core::timing;

mod hygiene;

use super::config::ConfigArgs;
use crate::pipeline::{self, warn};

#[derive(clap::Args)]
pub struct Args {
    /// Count only the use made by first-party files in these files and directories (default:
    /// the whole project). The whole project is loaded either way.
    paths: Vec<PathBuf>,
    /// Add each distribution's scorecard: grade, size, hotspots, maintainability, complexity,
    /// coupling (cached per distribution version).
    #[arg(long)]
    score: bool,
    /// List every loaded distribution, not only those first-party code imports or calls.
    #[arg(long)]
    all: bool,
    /// Run the dependency-hygiene rules (PM601-PM604) instead of listing distributions:
    /// findings, formats, suppressions, baselines and exit codes as `pymapper check`.
    #[arg(long, conflicts_with_all = ["score", "all"])]
    check: bool,
    /// With --check: report only findings not recorded in this baseline file.
    #[arg(long, value_name = "FILE", requires = "check", help_heading = "Baseline")]
    baseline: Option<PathBuf>,
    /// With --check: record every reported finding in this baseline file and exit 0.
    #[arg(long, value_name = "FILE", requires = "check", help_heading = "Baseline")]
    save_baseline: Option<PathBuf>,
    #[command(flatten)]
    config: ConfigArgs,
    /// Print phase timings, counters and peak RSS to stderr.
    #[arg(long)]
    stats: bool,
}

/// Runs `deps`. Third-party code is always loaded (`--deps` semantics); configuration errors,
/// paths that do not exist and the `sarif` / `github` formats are usage errors (exit 2).
pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    let t0 = Instant::now();
    if args.stats {
        timing::enable(true);
    }
    let explicit = !args.paths.is_empty();
    let no_deps = args.config.no_deps;
    let mut loaded = timing::time("config", || args.config.load(args.paths))?;
    let config_file = loaded.project.config_file.as_ref().map(|(p, _)| p.as_path());
    pipeline::require_paths(&loaded.scope, true, None)?;
    pipeline::require_paths(&loaded.config.paths, explicit, config_file)?;
    if args.check {
        // Loaded like `check`: installed metadata maps imports whether or not third-party code
        // is loaded (`--deps`).
        let opts = hygiene::Opts { baseline: args.baseline, save_baseline: args.save_baseline, stats: args.stats };
        return hygiene::run(loaded, opts, t0);
    }
    if no_deps {
        warn(format_args!("`pymapper deps` always loads third-party code; --no-deps is ignored"));
    }
    loaded.config.include_deps = true;
    let config = &loaded.config;
    let json = match config.output_format {
        // Text renderings; the human and llm layouts for deps are not built yet.
        OutputFormat::Human | OutputFormat::Llm | OutputFormat::Concise => false,
        OutputFormat::Json => true,
        other => anyhow::bail!("`pymapper deps` writes the human, llm, concise and json formats, not `{}`", other.as_str()),
    };
    let root = &loaded.project.root;
    let scope = &loaded.scope;
    for p in scope.iter().filter(|p| !p.starts_with(root)) {
        warn(format_args!(
            "{} is outside the project root {} and is not counted (run pymapper on it separately)",
            p.display(),
            root.display()
        ));
    }
    // A path covering the whole project (`deps .` at the root) filters nothing.
    let scope: &[PathBuf] = if scope.iter().any(|s| root.starts_with(s)) { &[] } else { scope };
    pipeline::configure_threads(config.threads)?;

    let project = pipeline::load(config, root, None);
    let (ix, imp) = (&project.index, &project.imports);
    let third_party = ix.groups().iter().filter(|g| g.origin == Origin::ThirdParty).count();
    // Without a virtualenv the import roots already warned.
    if third_party == 0
        && let Some(v) = &project.roots.venv
    {
        warn(format_args!("no third-party Python code found in the virtualenv {}", v.prefix.display()));
    }
    let calls = resolve_calls(ix, imp);
    let in_scope = |f: u32| scope.is_empty() || scope.iter().any(|s| ix.file_path(f).starts_with(s));
    let usage = timing::time("deps.usage", || deps::usage(ix, imp, &calls, in_scope, &UsageOptions::default()));
    drop(calls);

    let mut store_stats = None;
    let scores = if args.score {
        let cache = Cache::from_config(config);
        let mut store = timing::time("deps.score.read", || ScoreStore::open(cache.as_ref()));
        let files = GroupFiles::new(ix);
        let cx = ScoreCtx::new(ix, imp, &files);
        let scores = timing::time("deps.score", || deps::scorecards(&cx, |g| ScoreStore::key(g, &project.roots), &mut store));
        if let Some(c) = &cache
            && let Err(e) = timing::time("deps.score.write", || store.save(c))
        {
            warn(format_args!("cannot write the scorecard cache in {}: {e}", c.dir().display()));
        }
        store_stats = Some(store.stats());
        Some(scores)
    } else {
        None
    };

    let rows = deps::rows(ix, &project.roots, &usage, scores.as_deref());
    let report = DepsReport::new(rows, usage.first_party_files, store_stats);
    let opts = ReportOptions { score: args.score, all: args.all, json };
    let mut out = io::stdout().lock();
    // A closed pipe (`pymapper deps | head`) is not an error.
    timing::time("output", || report.write(&opts, &mut out)).context("cannot write to stdout")?;

    if args.stats {
        let s = ix.stats();
        let mut counters = vec![
            ("files", s.files),
            ("first_party_files", u64::from(usage.first_party_files)),
            ("distributions", report.rows().len() as u64),
            ("distributions_used", report.used() as u64),
            ("reached_nodes", u64::from(usage.reached_nodes)),
            ("loc", s.lines),
            ("nodes", u64::from(ix.node_count())),
            ("threads", rayon::current_num_threads() as u64),
        ];
        if let Some(st) = store_stats {
            counters.extend([("scorecards_computed", u64::from(st.computed)), ("scorecards_cached", u64::from(st.cached))]);
        }
        if let Some(cs) = &project.cache {
            counters.extend([("cache_hits", cs.hits + cs.verified), ("cache_misses", cs.misses)]);
        }
        crate::stats::report(t0.elapsed(), &counters);
    }
    Ok(ExitCode::SUCCESS)
}
