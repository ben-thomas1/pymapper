//! `cargo xtask bench`: time and peak RSS of named targets over the benchmark corpora.
//!
//! Every measured run happens in a fresh child process (`xtask bench-run`) so peak RSS is per
//! run. Usage and output are documented in docs/benchmarks.md.

mod corpus;
mod extract;
mod report;
mod rusage;
mod targets;
pub(crate) mod walk;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use pymapper_core::timing;

use crate::build::{self, repo_root};

use report::{ResultsFile, Summary, Thresholds};
use targets::{RunRecord, Target};

/// Exit code when regressions were flagged.
const EXIT_REGRESSION: u8 = 1;

#[derive(clap::Args)]
pub struct Args {
    /// Target to run (repeatable).
    #[arg(short, long = "target", value_enum, default_values_t = [Target::ParseOnly])]
    targets: Vec<Target>,
    /// Corpus to run: tests, stdlib, venv, or NAME=PATH (repeatable; default: all built-ins).
    #[arg(short, long = "corpus", value_name = "NAME|NAME=PATH")]
    corpora: Vec<String>,
    /// Measured runs per corpus and target; times are best-of-N, RSS worst-of-N.
    #[arg(short = 'n', long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..))]
    runs: u32,
    /// Unmeasured warm-up runs per corpus and target (warms the OS file cache).
    #[arg(long, default_value_t = 1)]
    warmup: u32,
    /// Worker threads for the measured process (RAYON_NUM_THREADS); default: all cores.
    #[arg(short = 'j', long, value_parser = clap::value_parser!(u32).range(1..))]
    threads: Option<u32>,
    /// Where to write this run's results.
    #[arg(long, value_name = "PATH")]
    out: Option<PathBuf>,
    /// Results file to compare against.
    #[arg(long, value_name = "PATH")]
    baseline: Option<PathBuf>,
    /// Also write this run's results to the baseline file (after comparing).
    #[arg(long)]
    save_baseline: bool,
    /// Skip the baseline comparison.
    #[arg(long)]
    no_compare: bool,
    /// Flag a metric as regressed when it is more than this many percent worse.
    #[arg(long, default_value_t = 10.0, value_name = "PCT")]
    threshold: f64,
    /// ...and at least this many milliseconds worse (time metrics).
    #[arg(long, default_value_t = 5.0, value_name = "MS")]
    min_delta_ms: f64,
    /// ...and at least this many MB worse (peak RSS).
    #[arg(long, default_value_t = 2.0, value_name = "MB")]
    min_delta_mb: f64,
    /// Cargo profile used to build the measured binary.
    #[arg(long, default_value = "release")]
    profile: String,
    /// Measure with this executable as-is instead of building --profile (for tests; debug
    /// numbers are meaningless).
    #[arg(long)]
    use_current_exe: bool,
}

#[derive(clap::Args)]
pub struct RunArgs {
    #[arg(long, value_enum)]
    target: Target,
    #[arg(long)]
    root: PathBuf,
}

/// Default directory for results files (ignored by git via the root allowlist in .gitignore).
fn results_dir() -> PathBuf {
    repo_root().join("bench-results")
}

/// Entry point of the hidden `bench-run` subcommand: one run, one JSON line on stdout.
pub fn run_child(args: &RunArgs) -> anyhow::Result<ExitCode> {
    timing::enable(true);
    let record = targets::run(args.target, &args.root)?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{}", record.to_json()).context("writing run record")?;
    Ok(ExitCode::SUCCESS)
}

pub fn run(args: &Args) -> anyhow::Result<ExitCode> {
    if !(args.threshold >= 0.0 && args.min_delta_ms >= 0.0 && args.min_delta_mb >= 0.0) {
        bail!("--threshold, --min-delta-ms and --min-delta-mb must be non-negative numbers");
    }
    let builtins = corpus::builtin(repo_root(), |k| std::env::var(k).ok());
    let corpora = corpus::select(&args.corpora, &builtins)?;
    let mut targets_sel: Vec<Target> = Vec::with_capacity(args.targets.len());
    for t in &args.targets {
        if !targets_sel.contains(t) {
            targets_sel.push(*t);
        }
    }
    let exe = measuring_exe(args)?;

    let mut results: Vec<Summary> = Vec::new();
    for c in &corpora {
        if !c.root.is_dir() {
            let hint = c.hint.map(|h| format!(" (set {h})")).unwrap_or_default();
            eprintln!("note: skipping corpus {}: {} not found{hint}", c.name, c.root.display());
            continue;
        }
        for &target in &targets_sel {
            let mut runs: Vec<RunRecord> = Vec::with_capacity(args.runs as usize);
            for i in 0..args.warmup + args.runs {
                let (label, k, of) =
                    if i < args.warmup { ("warm-up", i + 1, args.warmup) } else { ("run", i + 1 - args.warmup, args.runs) };
                eprintln!("bench: {} {} {label} {k}/{of}", c.name, target.name());
                let record = run_once(&exe, target, &c.root, args.threads)
                    .with_context(|| format!("{} on corpus {}", target.name(), c.name))?;
                if i >= args.warmup {
                    runs.push(record);
                }
            }
            let summary = Summary::from_runs(&c.name, target.name(), &c.root, &runs).context("no runs")?;
            if summary.errors > 0 {
                eprintln!("note: {} {}: {} unreadable files/dirs skipped", c.name, target.name(), summary.errors);
            }
            results.push(summary);
        }
    }

    let mut stdout = std::io::stdout().lock();
    if results.is_empty() {
        eprintln!("note: no corpus available; nothing measured");
        return Ok(ExitCode::SUCCESS);
    }
    stdout.write_all(report::render_table(&results).as_bytes())?;

    let file = ResultsFile {
        created_unix: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        git_rev: git_rev(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        results,
    };
    let text = file.to_json_string();
    let out = args.out.clone().unwrap_or_else(|| results_dir().join("latest.json"));
    write_file(&out, &text)?;
    writeln!(stdout, "\nresults written to {}", out.display())?;

    let baseline = args.baseline.clone().unwrap_or_else(|| results_dir().join("baseline.json"));
    let mut regressions = 0;
    if !args.no_compare {
        match fs::read_to_string(&baseline) {
            Ok(base_text) => {
                let base = ResultsFile::parse(&base_text)
                    .with_context(|| format!("reading baseline {}", baseline.display()))?;
                let th = Thresholds {
                    pct: args.threshold,
                    min_delta_s: args.min_delta_ms / 1e3,
                    min_delta_bytes: args.min_delta_mb * 1e6,
                };
                let cmp = report::compare(&base.results, &file.results, &th);
                regressions = cmp.regressions();
                let rev = base.git_rev.as_deref().unwrap_or("unknown rev");
                writeln!(
                    stdout,
                    "\ncompared with {} ({rev}); regression = >{}% and >={} ms / >={} MB worse",
                    baseline.display(),
                    args.threshold,
                    args.min_delta_ms,
                    args.min_delta_mb
                )?;
                stdout.write_all(report::render_comparison(&cmp).as_bytes())?;
                writeln!(stdout, "{regressions} regression(s)")?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                writeln!(stdout, "no baseline at {}; use --save-baseline to create one", baseline.display())?;
            }
            Err(e) => return Err(e).with_context(|| format!("reading baseline {}", baseline.display())),
        }
    }
    if args.save_baseline {
        write_file(&baseline, &text)?;
        writeln!(stdout, "baseline saved to {}", baseline.display())?;
    }
    Ok(if regressions > 0 { ExitCode::from(EXIT_REGRESSION) } else { ExitCode::SUCCESS })
}

/// Runs one measurement in a fresh process and parses its record.
fn run_once(exe: &Path, target: Target, root: &Path, threads: Option<u32>) -> anyhow::Result<RunRecord> {
    let mut cmd = Command::new(exe);
    cmd.arg("bench-run").arg("--target").arg(target.name()).arg("--root").arg(root);
    if let Some(n) = threads {
        cmd.env("RAYON_NUM_THREADS", n.to_string());
    }
    let out = cmd.output().with_context(|| format!("spawning {}", exe.display()))?;
    if !out.status.success() {
        bail!("measured process failed ({}): {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().rev().find(|l| !l.trim().is_empty()).context("measured process printed nothing")?;
    let value: serde_json::Value = serde_json::from_str(line).context("parsing run record")?;
    RunRecord::from_json(&value)
}

/// The binary whose children do the measuring: this one if it already is the requested profile
/// (or `--use-current-exe`), otherwise a freshly built `--profile` xtask in the same target dir.
fn measuring_exe(args: &Args) -> anyhow::Result<PathBuf> {
    let why = format!("bench: building xtask with --profile {} (measurements need an optimised build)", args.profile);
    build::profile_exe(&args.profile, args.use_current_exe, &why)
}

fn git_rev() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--always", "--dirty", "--abbrev=12"])
        .current_dir(repo_root())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let rev = String::from_utf8(out.stdout).ok()?;
    let rev = rev.trim();
    if rev.is_empty() { None } else { Some(rev.to_owned()) }
}

fn write_file(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}
