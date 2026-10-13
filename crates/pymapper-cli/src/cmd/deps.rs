//! `pymapper deps` subcommand: third-party distributions, how much of each first-party code
//! uses, and (`--score`) their scorecards.
//!
//! ```text
//!   config (include-deps forced on) ──> roots ──> walk ──> index (cache) ──> imports
//!     ──> calls ──> usage (importers, API used, LOC reached)  from first-party files in PATHS
//!     ──> --score: scorecards (cached per distribution in `scores.pmc`, computed when missing)
//!     ──> report (text | llm | json) ──> exit 0
//! ```
//!
//! Scorecards are information, never findings: the exit code is 0 whatever the grades, 2 only
//! for usage and fatal errors (as `check`). `--check` runs the dependency-hygiene rules instead
//! (PM601-PM604, see [`hygiene`]), with the findings, formats and exit codes of `check`.

use std::fmt::Write as _;
use std::io::{self, Write as _};
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

use super::check::RelPath;
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
    let explicit_limit = args.config.limit;
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
    let llm = config.output_format == OutputFormat::Llm;
    let json = match config.output_format {
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
    if llm {
        let cwd = std::env::current_dir().context("cannot determine the working directory")?;
        let limit = match explicit_limit.unwrap_or(config.limit) {
            0 => usize::MAX,
            n => n as usize,
        };
        let text = write_llm(&report, &opts, &RelPath(root, &cwd).to_string(), limit);
        timing::time("output", || out.write_all(text.as_bytes()).and_then(|()| out.flush()))
            .or_else(|e| if e.kind() == io::ErrorKind::BrokenPipe { Ok(()) } else { Err(e) })
            .context("cannot write to stdout")?;
    } else {
        timing::time("output", || report.write(&opts, &mut out)).context("cannot write to stdout")?;
    }

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

/// The llm rendering of the listing and `--score` (ISSUES O4): a header, what usage does not
/// see, then one line per distribution, most used first, at most `limit`:
/// `requests 2.32.3: grade B (78.2) · importers 3 · names used 14 (public api 152) · loc
/// reached 1203/5301 (23%)`. "names used" counts every class and function first-party code
/// imports or calls, private ones included, so it is not a share of the public API.
fn write_llm(report: &DepsReport<'_>, opts: &ReportOptions, root: &str, limit: usize) -> String {
    let mut out = String::new();
    let rows: Vec<_> = {
        let mut v: Vec<_> = report.rows().iter().filter(|r| opts.all || r.usage.is_used()).collect();
        v.sort_by(|a, b| {
            (b.usage.importers, b.usage.loc_reached, b.usage.call_sites)
                .cmp(&(a.usage.importers, a.usage.loc_reached, a.usage.call_sites))
                .then_with(|| a.key.cmp(b.key))
        });
        v
    };
    let total = report.rows().len();
    let _ = writeln!(out, "# pymapper deps{}", if opts.score { " --score" } else { "" });
    let _ = writeln!(
        out,
        "root: {root} · {} of {total} distributions used by first-party code ({} first-party files) · listed: {}{}",
        report.used(),
        report.first_party_files,
        rows.len(),
        if opts.all { " (--all)" } else { "" }
    );
    out.push_str("Usage counts static imports and resolved calls from first-party code; dynamic imports, CLI tools, plugins and unresolved calls are not seen. Scorecards are information, never findings (exit 0).\n");
    if opts.score {
        out.push_str("Grade A-F from the 0-100 score (complexity, hotspots, maintainability, coupling; formula in docs/deps.md).\n");
    }
    if !rows.is_empty() {
        out.push('\n');
    }
    let s = |n: u32| if n == 1 { "" } else { "s" };
    for r in rows.iter().take(limit) {
        let u = &r.usage;
        let name = String::from_utf8_lossy(r.name);
        let version = if r.version.is_empty() { "-".into() } else { String::from_utf8_lossy(r.version) };
        let _ = write!(out, "{name} {version}:");
        if opts.score {
            match r.score.as_ref().and_then(|sc| sc.grade().map(|g| (g, sc.score))) {
                Some((g, score)) => {
                    let _ = write!(out, " grade {} ({:.1}) ·", g.as_str(), score);
                }
                None if r.loaded => out.push_str(" grade - (no functions) ·"),
                None => out.push_str(" grade - (code not loaded) ·"),
            }
        }
        let _ = write!(out, " importers {}", u.importers);
        if u.importers == 0 && u.call_sites > 0 {
            out.push_str(" (reached only through calls)");
        }
        if r.loaded {
            let _ = write!(out, " · names used {}", u.api_used);
            if let Some(sc) = r.score.as_ref().filter(|_| opts.score) {
                let _ = write!(out, " (public api {})", sc.public_api);
            }
            let pct = if u.loc == 0 { 0 } else { (u64::from(u.loc_reached) * 100 + u64::from(u.loc) / 2) / u64::from(u.loc) };
            let _ = write!(out, " · loc reached {}/{} ({pct}%)", u.loc_reached, u.loc);
        } else {
            let _ = write!(out, " · {} import row{} · code not loaded", u.import_rows, s(u.import_rows));
        }
        out.push('\n');
    }
    if rows.len() > limit {
        let _ = writeln!(out, "\n{} more distributions not displayed. Use --limit 0 to show all.", rows.len() - limit);
    }
    if !opts.all && rows.len() < total {
        let n = total - rows.len();
        let _ = writeln!(out, "\n{n} unused distribution{} not listed (--all lists every one).", if n == 1 { "" } else { "s" });
    }
    out.push_str(if opts.score {
        "\nNext: `pymapper deps --check --llm` for undeclared and unused dependencies · `pymapper imports <module>` for where one is used\n"
    } else {
        "\nNext: `pymapper deps --score --llm` for scorecards · `pymapper deps --check --llm` for undeclared and unused dependencies\n"
    });
    out
}
