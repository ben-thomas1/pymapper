//! `pymapper check` subcommand: load the project, run the enabled analyses, report findings.
//!
//! ```text
//!   config ──> roots ──> files ──> index (cache) ──> imports ──┬─> hotspot      (PM1xx)
//!                                                             ├─> cycles       (PM301)
//!                                                             ├─> layers       (PM302-304, contracts)
//!                                                             ├─> coupling     (PM201-204; call graph for PM204)
//!                                                             ├─> dead code    (PM4xx; call resolution)
//!                                                             └─> duplication  (PM501, gated)
//!   findings ──> filter (selection, per-file-ignores, suppressions, confidence)
//!            ──> report (sorted) ──> baseline ──> output ──> exit 0 clean / 1 findings / 2 error
//! ```
//!
//! Analyses run only when one of their rules is enabled (`RuleEngine::should_run`), and call
//! resolution only when an enabled rule needs it (dead code, god classes).

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::Context;
use pymapper_core::analysis::deadcode::{self, DeadCodeOptions};
use pymapper_core::analysis::dupes::{self, DupesParams, Fingerprints};
use pymapper_core::analysis::{coupling, cycles, hotspot, layers};
use pymapper_core::config::{Config, RuleSelector};
use pymapper_core::graph::{CallFilter, call_graph};
use pymapper_core::index::Index;
use pymapper_core::output::{self, Baseline, IndexContext, Options, Report, Summary};
use pymapper_core::resolve::resolve_calls;
use pymapper_core::roots::Origin;
use pymapper_core::rules::codes::GOD_CLASS;
use pymapper_core::rules::{FileId, Findings, Pass, RuleEngine};
use pymapper_core::timing;
use pymapper_core::walk::{Gitignore, Match};

use super::config::ConfigArgs;
use crate::pipeline::{self, Project, warn};

#[derive(clap::Args)]
pub struct Args {
    /// Files and directories to check (default: the configured `paths`, else the project
    /// root). Config discovery starts at the first.
    paths: Vec<PathBuf>,
    #[command(flatten)]
    config: ConfigArgs,
    /// Report only findings not recorded in this baseline file (overrides `baseline`).
    #[arg(long, value_name = "FILE", help_heading = "Baseline")]
    baseline: Option<PathBuf>,
    /// Record every reported finding in this baseline file and exit 0 (no report is written).
    #[arg(long, value_name = "FILE", help_heading = "Baseline")]
    save_baseline: Option<PathBuf>,
    /// Print phase timings, counters and peak RSS to stderr.
    #[arg(long)]
    stats: bool,
}

/// Runs `check`. Configuration, baseline and output errors are fatal (exit 2); unreadable files
/// and directories are warnings.
pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    let t0 = Instant::now();
    if args.stats {
        timing::enable(true);
    }
    let cwd = std::env::current_dir().context("cannot determine the working directory")?;
    let mut loaded = timing::time("config", || args.config.load(args.paths))?;
    if let Some(b) = args.baseline {
        loaded.config.baseline = Some(cwd.join(b));
    }
    let config = &loaded.config;
    let root = &loaded.project.root;
    pipeline::configure_threads(config.threads)?;
    let engine = RuleEngine::new(config);
    warn_gated(config, &engine);
    let per_file = compile_per_file_ignores(config)?;
    // Read the baseline before the expensive work, so a bad file fails fast. Saving a new
    // baseline ignores the old one.
    let baseline = match (&args.save_baseline, &config.baseline) {
        (None, Some(path)) => Some(read_baseline(path)?),
        _ => None,
    };

    let mut project = pipeline::load(config, root, DupesParams::from_engine(&engine));
    // Matching consumes the fingerprints.
    let fps = project.fingerprints.take();
    let mut findings = analyse(config, &engine, &project, fps);
    let ix = &project.index;
    let suppressions = timing::time("suppressions", || pipeline::suppressions(ix));
    let filter = timing::time("filter", || {
        engine.filter(&mut findings, &suppressions, false, |f, i| per_file_matches(ix, root, &per_file[i], f))
    });

    let ctx = IndexContext::new(ix, root, &config.contracts);
    let mut report = timing::time("report", || Report::new(&findings, &ctx));
    let files_checked = first_party_files(&project);
    if files_checked == 0 {
        warn(format_args!(
            "no first-party Python files found under {} (check `paths`, `src`, `exclude` and .gitignore files)",
            DisplayPaths(&config.paths)
        ));
    }
    let status = if let Some(path) = &args.save_baseline {
        let path = cwd.join(path);
        save_baseline(&report, &path)?;
        let _ = writeln!(
            io::stderr().lock(),
            "pymapper: saved {} finding(s) to baseline {}",
            report.len(),
            path.display()
        );
        output::ExitStatus::Clean
    } else {
        if let Some(b) = &baseline {
            report.apply_baseline(b);
        }
        let opts = Options { format: config.output_format, min_confidence: config.min_confidence, footer: true };
        let summary = Summary { files_checked, filter };
        // A closed pipe (`pymapper check | head`) is not an error (`Outcome::Closed`); the
        // exit status still reflects the findings.
        timing::time("output", || report.write(&opts, &summary, &mut io::stdout().lock()))
            .context("cannot write to stdout")?;
        report.exit_status(opts.min_confidence)
    };

    if args.stats {
        let s = ix.stats();
        let mut counters = vec![
            ("files", s.files),
            ("first_party_files", u64::from(files_checked)),
            ("loc", s.lines),
            ("bytes", s.bytes),
            ("nodes", u64::from(ix.node_count())),
            ("findings", report.len() as u64),
            ("suppressed", u64::from(filter.suppressed)),
            ("below_confidence", u64::from(filter.below_confidence)),
            ("baselined", u64::from(report.baselined())),
            ("walk_errors", project.walk_errors as u64),
            ("read_errors", ix.read_errors().len() as u64),
            ("threads", rayon::current_num_threads() as u64),
        ];
        if let Some(cs) = &project.cache {
            counters.extend([
                ("cache_hits", cs.hits + cs.verified),
                ("cache_misses", cs.misses),
                ("cache_written_kb", cs.written_bytes / 1000),
            ]);
        }
        crate::stats::report(t0.elapsed(), &counters);
    }
    Ok(ExitCode::from(status.code()))
}

/// Paths joined by `, ` for messages.
struct DisplayPaths<'a>(&'a [PathBuf]);

impl std::fmt::Display for DisplayPaths<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, p) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}", p.display())?;
        }
        Ok(())
    }
}

/// Number of first-party files loaded.
fn first_party_files(p: &Project) -> u32 {
    let ix = &p.index;
    (0..ix.file_count()).filter(|&f| ix.file_origin(f) == Origin::FirstParty).count() as u32
}

/// Runs every analysis with an enabled rule and returns their findings (unfiltered).
fn analyse(config: &Config, engine: &RuleEngine, p: &Project, fps: Option<Fingerprints>) -> Findings {
    let (ix, imp) = (&p.index, &p.imports);
    let mut out = Findings::new();
    if engine.should_run(Pass::Hotspot) {
        timing::time("hotspot", || hotspot::analyse(ix, engine, &mut out));
    }
    if engine.should_run(Pass::Cycles) {
        let opts = cycles::CycleOptions::from_config(config);
        timing::time("cycles", || cycles::analyse(ix, imp, &opts, &mut out));
    }
    if engine.should_run(Pass::Layers) && !config.contracts.is_empty() {
        let r = timing::time("layers", || layers::analyse(ix, imp, &config.contracts, &mut out));
        for m in &r.missing {
            let contract = &config.contracts[m.contract as usize].name;
            warn(format_args!("contract `{contract}`: module `{}` matches no analysed module", m.name));
        }
    }
    let god_class = engine.should_run(Pass::Coupling) && engine.is_enabled(GOD_CLASS);
    let dead_code = engine.should_run(Pass::DeadCode);
    // Call resolution is the most expensive semantic pass: only when a rule needs it.
    let calls = (god_class || dead_code).then(|| resolve_calls(ix, imp));
    if engine.should_run(Pass::Coupling) {
        let cg = match &calls {
            Some(c) if god_class => Some(timing::time("graph.build", || call_graph(ix, c, CallFilter::default()))),
            _ => None,
        };
        timing::time("coupling", || coupling::analyse(ix, imp, cg.as_ref(), engine, &mut out));
    }
    if let (true, Some(calls)) = (dead_code, &calls) {
        let matcher = config.entrypoint_matcher();
        let scripts: Vec<&[u8]> = match &p.roots.pyproject {
            Some(info) => info.scripts.iter().map(|s| s.as_bytes()).collect(),
            None => Vec::new(),
        };
        let opts = DeadCodeOptions::from_config(config, &matcher, &scripts);
        timing::time("deadcode", || deadcode::analyse(ix, imp, calls, &opts, engine, &mut out));
    }
    drop(calls);
    if let (Some(params), Some(fps)) = (DupesParams::from_engine(engine), fps) {
        timing::time("dupes", || dupes::analyse(ix, fps, &params, &mut out));
    }
    out
}

/// Warns about rules selected by code or category that do not run because their gate is
/// closed (`--select PM501` without `analysis.dupes`). `ALL` / `PM` selections stay quiet.
fn warn_gated(config: &Config, engine: &RuleEngine) {
    let explicit = |code: u16| {
        let mut sel = config.rules.select.iter().flatten().chain(&config.rules.extend_select);
        sel.any(|s: &RuleSelector| s.specificity() >= 2 && s.matches(code))
    };
    for def in engine.gated_off() {
        if explicit(def.code) {
            let key = def.gate.key().unwrap_or("?");
            warn(format_args!(
                "PM{:03} ({}) is selected but does not run: it is expensive and needs `{key} = true`",
                def.code, def.name
            ));
        }
    }
}

/// `per-file-ignores` patterns compiled once each (gitignore syntax, relative to the project
/// root). An invalid pattern is a configuration error.
fn compile_per_file_ignores(config: &Config) -> anyhow::Result<Vec<Gitignore>> {
    let mut out = Vec::with_capacity(config.rules.per_file_ignores.len());
    for e in &config.rules.per_file_ignores {
        Gitignore::check_pattern(e.pattern.as_bytes())
            .map_err(|err| anyhow::anyhow!("invalid `rules.per-file-ignores` pattern `{}`: {err}", e.pattern))?;
        out.push(Gitignore::from_patterns(&[e.pattern.as_bytes()]));
    }
    Ok(out)
}

/// Whether a `per-file-ignores` pattern matches file `f`: its path relative to the project
/// root, or one of its parent directories (as an excluded directory excludes its contents).
/// Files outside the root are matched by their absolute path without the leading `/`.
fn per_file_matches(ix: &Index, root: &Path, gi: &Gitignore, f: FileId) -> bool {
    if f.0 >= ix.file_count() {
        return false;
    }
    let p = ix.file_path(f.0).as_os_str().as_bytes();
    let rel = match p.strip_prefix(root.as_os_str().as_bytes()) {
        Some([b'/', rest @ ..]) => rest,
        _ => p.strip_prefix(b"/").unwrap_or(p),
    };
    // Directories first, shallowest first: a match on a parent covers everything below.
    for (i, &c) in rel.iter().enumerate() {
        if c == b'/' && gi.matched(&rel[..i], true) == Match::Ignore {
            return true;
        }
    }
    gi.matched(rel, false) == Match::Ignore
}

fn read_baseline(path: &Path) -> anyhow::Result<Baseline> {
    let text = std::fs::read(path).with_context(|| {
        format!("cannot read baseline {} (create it with `pymapper check --save-baseline FILE`)", path.display())
    })?;
    Baseline::parse(&text).with_context(|| format!("invalid baseline {}", path.display()))
}

/// Writes the baseline of `report` to `path` atomically (temporary file + rename).
fn save_baseline(report: &Report<'_>, path: &Path) -> anyhow::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let res = (|| -> anyhow::Result<()> {
        let mut file = io::BufWriter::new(std::fs::File::create(&tmp)?);
        report.write_baseline(&mut file)?;
        file.into_inner().map_err(io::IntoInnerError::into_error)?.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res.with_context(|| format!("cannot write baseline {}", path.display()))
}
