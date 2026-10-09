//! `pymapper check` subcommand: load the project, run the enabled analyses, report findings.
//!
//! ```text
//!   config ──> roots ──> walk ───> index (cache) ──> imports ──┬─> hotspot      (PM1xx)
//!                                                             ├─> cycles       (PM301)
//!                                                             ├─> layers       (PM302-304, contracts)
//!                                                             ├─> coupling     (PM201-204; call graph for PM204)
//!                                                             ├─> dead code    (PM4xx; call resolution)
//!                                                             └─> duplication  (PM501, gated)
//!   findings ──> filter (selection, per-file-ignores, suppressions, confidence)
//!            ──> report (sorted) ──> baseline ──> output ──> exit 0 clean / 1 findings / 2 error
//! ```
//!
//! Positional paths never change what is analysed (the project root and module names come
//! from the whole project, see `config::load`): findings outside them are dropped before
//! filtering, so subset runs give exactly the whole-project findings under those paths.
//!
//! Analyses run only when one of their rules is enabled (`RuleEngine::should_run`), and call
//! resolution only when an enabled rule needs it (dead code, god classes).

pub(crate) mod plan;

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::Context;
use pymapper_core::analysis::deadcode::coverage::Coverage;
use pymapper_core::analysis::deadcode::{self, DeadCode, DeadCodeOptions, DeadCodeStats, unit_flags};
use pymapper_core::analysis::dupes::{self, DupesParams, Fingerprints};
use pymapper_core::analysis::{coupling, cycles, hotspot, layers};
use pymapper_core::config::{Config, EntrypointMatcher, OutputFormat, RuleSelector};
use pymapper_core::graph::{CallFilter, call_graph};
use pymapper_core::index::Index;
use pymapper_core::output::{self, Baseline, IndexContext, Options, Outcome, Report, Summary};
use pymapper_core::resolve::resolve_calls;
use pymapper_core::roots::{Origin, ProjectUnits};
use pymapper_core::rules::codes::GOD_CLASS;
use pymapper_core::rules::{BadCodeKind, FileId, Findings, Pass, RuleEngine};
use pymapper_core::timing;
use pymapper_core::walk::{Gitignore, Match};

use self::plan::{CodeList, Footnote, RulePlan, check_selectors};
use super::config::ConfigArgs;
use crate::pipeline::{self, Project, warn};

#[derive(clap::Args)]
pub struct Args {
    /// Report only findings in these files and directories (default: everything). The whole
    /// project is analysed either way; config and project-root discovery start at the first.
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

/// Runs `check`. Configuration, baseline and output errors, paths that do not exist, unknown
/// rule selectors and selections of which no rule can run are fatal (exit 2); unreadable files
/// and directories are warnings.
pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    let t0 = Instant::now();
    if args.stats {
        timing::enable(true);
    }
    let cwd = std::env::current_dir().context("cannot determine the working directory")?;
    let explicit = !args.paths.is_empty();
    let mut loaded = timing::time("config", || args.config.load(args.paths))?;
    let config_file = loaded.project.config_file.as_ref().map(|(p, _)| p.as_path());
    pipeline::require_paths(&loaded.scope, true, None)?;
    pipeline::require_paths(&loaded.config.paths, explicit, config_file)?;
    if let Some(b) = args.baseline {
        loaded.config.baseline = Some(cwd.join(b));
    }
    let config = &loaded.config;
    let root = &loaded.project.root;
    let scope = &loaded.scope;
    for p in scope.iter().filter(|p| !p.starts_with(root)) {
        warn(format_args!(
            "{} is outside the project root {} and is not checked (run pymapper on it separately)",
            p.display(),
            root.display()
        ));
    }
    // A path covering the whole project (`check .` at the root) filters nothing.
    let scope: &[PathBuf] = if scope.iter().any(|s| root.starts_with(s)) { &[] } else { scope };
    pipeline::configure_threads(config.threads)?;
    check_selectors(config)?;
    let engine = RuleEngine::new(config);
    let plan = RulePlan::new(config, &engine);
    plan.check()?;
    plan.warn_skipped();
    let per_file = compile_per_file_ignores(config)?;
    // Read the baseline before the expensive work, so a bad file fails fast. Saving a new
    // baseline ignores the old one.
    let baseline = match (&args.save_baseline, &config.baseline) {
        (None, Some(path)) => Some(read_baseline(path)?),
        _ => None,
    };
    // Likewise a missing coverage report (read after the index is built).
    let coverage_path = config.analysis.coverage.as_deref().filter(|_| engine.should_run(Pass::DeadCode));
    if let Some(path) = coverage_path {
        std::fs::File::open(path)
            .with_context(|| format!("cannot read coverage report {} (create it with `coverage json`)", path.display()))?;
    }

    let mut project = pipeline::load(config, root, DupesParams::from_engine(&engine));
    // Matching consumes the fingerprints.
    let fps = project.fingerprints.take();
    let matcher = config.entrypoint_matcher();
    let coverage = match coverage_path {
        Some(path) => Some(timing::time("coverage", || load_coverage(&project.index, path, root))?),
        None => None,
    };
    let (mut findings, dead, modes) = analyse(config, &engine, &project, &matcher, coverage.as_ref(), scope, fps, args.stats);
    if dead.is_some() {
        for (key, pattern) in matcher.unmatched() {
            warn(format_args!(
                "entrypoint pattern `{pattern}` (`entrypoints.{key}`) matches nothing in the project{}",
                if key == "names" { "; name patterns match a bare name (`main`), a qualified id (`pkg.mod:func`) or a module (`pkg.mod`)" } else { "" }
            ));
        }
    }
    let ix = &project.index;
    // The whole project was analysed; report only what lies under the positional paths.
    let in_scope = file_scope(ix, scope);
    let (files_checked, files_total) = first_party_files(ix, in_scope.as_deref());
    if let Some(sc) = &in_scope {
        findings.retain(|f, _| f.loc.file.0 < ix.file_count() && sc[f.loc.file.0 as usize]);
    }
    let suppressions = timing::time("suppressions", || {
        pipeline::suppressions(ix, |f, line, code, kind| {
            if ix.file_origin(f) == Origin::FirstParty && in_scope.as_ref().is_none_or(|sc| sc[f as usize]) {
                warn_bad_suppression(ix, root, f, line, code, kind);
            }
        })
    });
    let filter = timing::time("filter", || {
        engine.filter(&mut findings, &suppressions, false, |f, i| per_file_matches(ix, root, &per_file[i], f))
    });

    let ctx = IndexContext::new(ix, root, &config.contracts);
    let mut report = timing::time("report", || Report::new(&findings, &ctx));
    if files_checked == 0 {
        let under = if scope.is_empty() { &config.paths } else { scope };
        warn(format_args!(
            "no first-party Python files found under {} (check `paths`, `src`, `exclude` and .gitignore files)",
            DisplayPaths(under)
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
        let mut out = io::stdout().lock();
        let outcome = timing::time("output", || report.write(&opts, &summary, &mut out)).context("cannot write to stdout")?;
        if outcome == Outcome::Written && matches!(config.output_format, OutputFormat::Concise | OutputFormat::Github) {
            let note = Footnote { scope, root, files_checked, files_total, plan: &plan };
            match write!(out, "{note}").and_then(|()| out.flush()) {
                Err(e) if e.kind() != io::ErrorKind::BrokenPipe => return Err(e).context("cannot write to stdout"),
                _ => {}
            }
        }
        report.exit_status(opts.min_confidence)
    };

    if args.stats {
        let s = ix.stats();
        let mut counters = vec![
            ("files", s.files),
            ("first_party_files", u64::from(files_total)),
            ("files_checked", u64::from(files_checked)),
            ("loc", s.lines),
            ("bytes", s.bytes),
            ("nodes", u64::from(ix.node_count())),
            ("rules_run", u64::from(plan.ran)),
            ("rules_skipped", plan.skipped.len() as u64),
            ("findings", report.len() as u64),
            ("suppressed", u64::from(filter.suppressed)),
            ("below_confidence", u64::from(filter.below_confidence)),
            ("baselined", u64::from(report.baselined())),
            ("walk_errors", project.walk_errors as u64),
            ("read_errors", ix.read_errors().len() as u64),
            ("threads", rayon::current_num_threads() as u64),
        ];
        if let Some(d) = &dead {
            counters.extend([
                ("deadcode.units", u64::from(d.units)),
                ("deadcode.library", u64::from(d.library_units)),
                ("deadcode.application", u64::from(d.units - d.library_units)),
            ]);
        }
        if let Some(cs) = &project.cache {
            counters.extend([
                ("cache_hits", cs.hits + cs.verified),
                ("cache_misses", cs.misses),
                ("cache_written_kb", cs.written_bytes / 1000),
            ]);
        }
        if let Some(c) = &coverage {
            let s = c.stats();
            counters.extend([
                ("coverage_files", u64::from(s.report_files)),
                ("coverage_matched", u64::from(s.matched)),
                ("coverage_executed", u64::from(s.executed)),
                ("coverage_not_executed", u64::from(s.not_executed)),
            ]);
        }
        if let Some(d) = &dead
            && coverage.is_some()
        {
            counters.extend([("deadcode.executed", d.executed), ("deadcode.lowered", d.not_executed)]);
        }
        if let Some(ce) = &project.config_evidence {
            counters.extend([("config_files", u64::from(ce.stats.files)), ("config_files_cached", u64::from(ce.stats.cached))]);
        }
        crate::stats::report(t0.elapsed(), &counters);
        let mut err = io::stderr().lock();
        for group in plan.groups() {
            let _ = writeln!(err, "  skipped {}: {}", CodeList(&group.codes), group.reason.why());
        }
        // One line per project unit: its dead-code mode and the deciding evidence.
        for line in &modes {
            let _ = writeln!(err, "  deadcode.mode {line}");
        }
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

/// Per file: whether it lies under one of the positional paths (`None`: no paths, everything
/// is in scope). Component-wise prefix tests on first-party files only; O(files * paths).
pub(crate) fn file_scope(ix: &Index, scope: &[PathBuf]) -> Option<Vec<bool>> {
    if scope.is_empty() {
        return None;
    }
    let mut out = vec![false; ix.file_count() as usize];
    for (f, slot) in out.iter_mut().enumerate() {
        let f = f as u32;
        if ix.file_origin(f) == Origin::FirstParty {
            let path = ix.file_path(f);
            *slot = scope.iter().any(|s| path.starts_with(s));
        }
    }
    Some(out)
}

/// First-party files loaded: `(in scope, total)`.
pub(crate) fn first_party_files(ix: &Index, in_scope: Option<&[bool]>) -> (u32, u32) {
    let mut checked = 0;
    let mut total = 0;
    for f in 0..ix.file_count() {
        if ix.file_origin(f) == Origin::FirstParty {
            total += 1;
            if in_scope.is_none_or(|sc| sc[f as usize]) {
                checked += 1;
            }
        }
    }
    (checked, total)
}

/// `path` relative to the project root when below it, else as is.
pub(crate) fn display_rel<'p>(path: &'p Path, root: &Path) -> std::path::Display<'p> {
    match path.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display(),
        _ => path.display(),
    }
}

/// Warns about one malformed or unknown code of an inline suppression (`code` empty: an empty
/// list). Never fatal: the directive just suppresses less than intended.
fn warn_bad_suppression(ix: &Index, root: &Path, f: u32, line: u32, code: &[u8], kind: BadCodeKind) {
    warn_bad_code(display_rel(ix.file_path(f), root), line, code, kind);
}

/// [`warn_bad_suppression`] for a display path.
pub(crate) fn warn_bad_code(path: impl std::fmt::Display, line: u32, code: &[u8], kind: BadCodeKind) {
    let code = String::from_utf8_lossy(code);
    match kind {
        BadCodeKind::Malformed if code.is_empty() => warn(format_args!(
            "{path}:{line}: empty `# pymapper: ignore[]` suppresses nothing (list codes, or use a bare `# pymapper: ignore`)"
        )),
        BadCodeKind::Malformed => match code.parse::<RuleSelector>() {
            Err(e) if e.suggestion.is_some() => warn(format_args!(
                "{path}:{line}: invalid suppression code `{code}` suppresses nothing; did you mean `{}`?",
                e.suggestion.unwrap_or_default()
            )),
            _ => warn(format_args!(
                "{path}:{line}: `{code}` is not a pymapper rule code (expected `ALL` or `PM` followed by up to 3 digits) and suppresses nothing"
            )),
        },
        BadCodeKind::Unknown => warn(format_args!("{path}:{line}: suppression code `{code}` matches no known rule")),
    }
}

/// Reads the coverage report at `path` and maps it to `ix`; warns when it matches no analysed
/// file or looks stale.
fn load_coverage(ix: &Index, path: &Path, root: &Path) -> anyhow::Result<Coverage> {
    let c = Coverage::load(ix, path, root).with_context(|| format!("invalid coverage report {}", path.display()))?;
    let s = c.stats();
    if s.matched == 0 && s.report_files > 0 {
        let first = String::from_utf8_lossy(c.first_unmatched().unwrap_or_default());
        warn(format_args!(
            "coverage report {} matches no analysed file (first path: {first}); it is ignored",
            path.display()
        ));
    }
    if s.stale_files > 0 {
        warn(format_args!(
            "coverage report {}: {} file(s) have line numbers past their end; the report may be stale (rerun the tests and `coverage json`)",
            path.display(),
            s.stale_files
        ));
    }
    Ok(c)
}

/// Runs every analysis with an enabled rule and returns their findings (unfiltered), the
/// dead-code counters (when that pass ran) and, with `describe_modes`, one line per project
/// unit with files describing its dead-code mode and why. `coverage`: dead-code evidence from
/// a coverage report.
#[allow(clippy::too_many_arguments)]
fn analyse(
    config: &Config,
    engine: &RuleEngine,
    p: &Project,
    matcher: &EntrypointMatcher,
    coverage: Option<&Coverage>,
    scope: &[PathBuf],
    fps: Option<Fingerprints>,
    describe_modes: bool,
) -> (Findings, Option<DeadCodeStats>, Vec<String>) {
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
    let mut dead = None;
    let mut modes = Vec::new();
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
        let scripts: Vec<&[u8]> = p.units.all_scripts().iter().map(|s| s.as_bytes()).collect();
        let opts = DeadCodeOptions::from_config(config, matcher, Some(&p.units), &scripts)
            .with_config_evidence(p.config_evidence.as_ref())
            .with_coverage(coverage);
        let dc = timing::time("deadcode", || deadcode::analyse(ix, imp, calls, &opts, engine, &mut out));
        warn_no_entry_points(&dc, &p.units, scope);
        if describe_modes {
            let m = dc.modes();
            for u in (0..m.units()).filter(|&u| dc.unit_flags(u) & unit_flags::FILES != 0) {
                let dir = p.units.dir(u).strip_prefix(p.units.dir(0)).unwrap_or(p.units.dir(u));
                let mut line = if dir.as_os_str().is_empty() { String::from(".: ") } else { format!("{}: ", dir.display()) };
                m.describe(ix, &p.units, p.units.dir(0), u, &mut line);
                modes.push(line);
            }
        }
        dead = Some(*dc.stats());
    }
    drop(calls);
    if let (Some(params), Some(fps)) = (DupesParams::from_engine(engine), fps) {
        timing::time("dupes", || dupes::analyse(ix, fps, &params, &mut out));
    }
    (out, dead, modes)
}

/// Warns once per application-mode unit without any entry point (every definition in it looks
/// unused), or once for the project when the mode is forced and no unit has one. With
/// positional paths, only units overlapping them are mentioned.
fn warn_no_entry_points(dc: &DeadCode, units: &ProjectUnits, scope: &[PathBuf]) {
    let modes = dc.modes();
    let affected = dc.units_without_entry_points().count();
    let with_files = (0..modes.units()).filter(|&u| dc.unit_flags(u) & unit_flags::FILES != 0).count();
    let warn_unit = |dir: &Path| {
        warn(format_args!(
            "no entry points found in {} (no __main__ guard, [project.scripts], entrypoint rules or tests); \
             every definition will look unused. Consider --mode library or [entrypoints]",
            dir.display()
        ))
    };
    if modes.forced() && affected > 0 && affected == with_files {
        warn_unit(units.dir(0));
        return;
    }
    for u in dc.units_without_entry_points() {
        let dir = units.dir(u);
        if scope.is_empty() || scope.iter().any(|s| s.starts_with(dir) || dir.starts_with(s)) {
            warn_unit(dir);
        }
    }
}

/// `per-file-ignores` patterns compiled once each (gitignore syntax, relative to the project
/// root). An invalid pattern is a configuration error.
pub(crate) fn compile_per_file_ignores(config: &Config) -> anyhow::Result<Vec<Gitignore>> {
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
pub(crate) fn per_file_matches(ix: &Index, root: &Path, gi: &Gitignore, f: FileId) -> bool {
    if f.0 >= ix.file_count() {
        return false;
    }
    per_file_path_matches(ix.file_path(f.0), root, gi)
}

/// [`per_file_matches`] for a path (absolute).
pub(crate) fn per_file_path_matches(path: &Path, root: &Path, gi: &Gitignore) -> bool {
    let p = path.as_os_str().as_bytes();
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

pub(crate) fn read_baseline(path: &Path) -> anyhow::Result<Baseline> {
    let text = std::fs::read(path).with_context(|| {
        format!("cannot read baseline {} (create it with `pymapper check --save-baseline FILE`)", path.display())
    })?;
    Baseline::parse(&text).with_context(|| format!("invalid baseline {}", path.display()))
}

/// Writes the baseline of `report` to `path` atomically (temporary file + rename).
pub(crate) fn save_baseline(report: &Report<'_>, path: &Path) -> anyhow::Result<()> {
    // Per-process temp name: concurrent saves to the same baseline must not share it.
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
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
