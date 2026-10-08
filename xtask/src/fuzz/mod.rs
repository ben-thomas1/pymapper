//! `cargo xtask fuzz`: a stable-Rust mutation fuzzer for the lexer/parser (docs/fuzz.md).
//!
//! The command is a supervisor: it builds xtask with the `fuzz` profile (optimised, panics
//! unwind, debug assertions and overflow checks on) and runs the fuzzing in a child process.
//! The child runs the target under `catch_unwind` on many threads with a hang watchdog and saves
//! crashing/hanging inputs (raw and minimised) to `target/fuzz-artifacts/<target>/`. If the
//! child dies from something `catch_unwind` cannot catch (stack overflow, abort, OOM kill), the
//! supervisor recovers the in-flight inputs from a shared-memory file, finds the culprit by
//! replaying each in a fresh process, minimises it, and restarts fuzzing where it left off.

mod engine;
mod inflight;
mod minimise;
mod mutate;
mod rng;
mod targets;

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};

use crate::bench::walk::{PathStore, walk_python};
use crate::build::{self, repo_root};
use engine::{Config, Finding, Kind, fnv1a};
use inflight::{Inflight, MAX_ITER, Snapshot, read_snapshot};
use minimise::{Executor, Outcome, minimise};
use mutate::{Mutator, Scratch, Seeds};
use targets::FuzzTarget;

/// Exit code when the fuzzer found problems.
const EXIT_FINDINGS: u8 = 1;
const DEFAULT_MAX_LEN: u64 = 65536;
/// Executions allowed when minimising in fresh processes (hangs and aborts; each costs a
/// process start, and a hang the whole budget).
const MAX_PROCESS_MINIMISE_EXECS: u32 = 500;
/// Wall-clock limit for minimising one hang.
const HANG_MINIMISE_TIME: Duration = Duration::from_secs(60);

/// Tiny built-in seeds, always present (so fuzzing works with an empty corpus).
const BUILTIN_SEEDS: &[&[u8]] = &[
    b"",
    b"x = 1\n",
    b"def f(a, *args, b=2, **kw):\n    return a\n",
    b"class C(B, metaclass=M):\n    @property\n    def p(self): ...\n",
    b"s = f'{x!r:>{w}} {y=}' + rb'\\d' + '''\nmulti\n'''\n",
    b"match p:\n    case [a, *_] if a:\n        pass\n    case {'k': v}:\n        pass\n",
    b"async def g():\n    async with a as b, c:\n        await b\n",
    b"if x:\n\tpass\nelif y: \\\n    z\nelse:\n    (a,\n b)\n",
];

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Extra seed inputs: directories (their *.py/*.pyi files) or individual files.
    #[arg(value_name = "SEEDS")]
    seeds: Vec<PathBuf>,
    /// Target to fuzz (see --list); default: the first registered target.
    #[arg(short, long)]
    target: Option<String>,
    /// List the registered targets and exit.
    #[arg(long)]
    list: bool,
    /// RNG seed (default: derived from the clock; printed so a run can be repeated).
    #[arg(short, long)]
    seed: Option<u64>,
    /// Number of inputs to run (default: unlimited, bounded by --time).
    #[arg(short = 'n', long)]
    iterations: Option<u64>,
    /// Wall-clock limit in seconds (default: 60 unless --iterations is given).
    #[arg(short = 'T', long, value_name = "SECS")]
    time: Option<f64>,
    /// Worker threads (default: all cores).
    #[arg(short, long, value_parser = clap::value_parser!(u32).range(1..))]
    jobs: Option<u32>,
    /// Per-input time budget; an input running longer is saved as a hang.
    #[arg(long, default_value_t = 1000, value_name = "MS", value_parser = clap::value_parser!(u64).range(1..))]
    budget_ms: u64,
    /// Maximum input length in bytes (seeds are truncated to it).
    #[arg(long, default_value_t = DEFAULT_MAX_LEN, value_parser = clap::value_parser!(u64).range(1..=(1 << 30)))]
    max_len: u64,
    /// Stop after this many hangs (each abandons a stuck thread).
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..))]
    max_hangs: u32,
    /// Executions allowed per minimisation (0: save raw inputs only).
    #[arg(long, default_value_t = 4000)]
    minimise_execs: u32,
    /// Do not seed from tests/corpus.
    #[arg(long)]
    no_corpus: bool,
    /// Artifact directory (default: target/fuzz-artifacts in the repository).
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,
    /// Run the target once on each file (e.g. a saved artifact) and report the outcome.
    #[arg(long, value_name = "FILE", num_args = 1..)]
    reproduce: Vec<PathBuf>,
    /// Write generated input number ITER (for --seed and the same seeds) to stdout and exit.
    #[arg(long, value_name = "ITER")]
    replay: Option<u64>,
    /// Stack size of worker threads in MiB.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..=4096))]
    stack_mb: u32,
    /// Restart the fuzzing process at most this many times after it aborts.
    #[arg(long, default_value_t = 3)]
    max_restarts: u32,
    /// Cargo profile to build the fuzzing process with (must unwind on panic).
    #[arg(long, default_value = "fuzz")]
    profile: String,
    /// Fuzz with this executable as-is instead of building --profile.
    #[arg(long)]
    use_current_exe: bool,
    /// Internal: this process is the fuzzing child.
    #[arg(long, hide = true)]
    child: bool,
    /// Internal: first iteration number.
    #[arg(long, hide = true, default_value_t = 0)]
    start_iter: u64,
    /// Internal: shared in-flight file written by the child.
    #[arg(long, hide = true)]
    inflight: Option<PathBuf>,
}

impl Args {
    fn target(&self) -> anyhow::Result<&'static FuzzTarget> {
        match &self.target {
            None => Ok(&targets::TARGETS[0]),
            Some(name) => targets::find(name).with_context(|| {
                let names: Vec<&str> = targets::TARGETS.iter().map(|t| t.name).collect();
                format!("unknown fuzz target {name:?}: expected one of {}", names.join(", "))
            }),
        }
    }

    fn jobs(&self) -> usize {
        match self.jobs {
            Some(j) => j as usize,
            None => std::thread::available_parallelism().map_or(1, |n| n.get()),
        }
    }

    fn budget(&self) -> Duration {
        Duration::from_millis(self.budget_ms)
    }

    fn stack_size(&self) -> usize {
        self.stack_mb as usize * 1024 * 1024
    }

    fn duration(&self) -> anyhow::Result<Option<Duration>> {
        match (self.time, self.iterations) {
            (Some(t), _) if !(t.is_finite() && t > 0.0) => bail!("--time must be a positive number of seconds"),
            (Some(t), _) => Ok(Some(Duration::from_secs_f64(t))),
            (None, Some(_)) => Ok(None),
            (None, None) => Ok(Some(Duration::from_secs(60))),
        }
    }

    /// `--out`, or target/fuzz-artifacts in the repository.
    fn artifact_base(&self) -> PathBuf {
        self.out.clone().unwrap_or_else(|| repo_root().join("target/fuzz-artifacts"))
    }

    fn artifact_dir(&self, target: &FuzzTarget) -> PathBuf {
        self.artifact_base().join(target.name)
    }
}

/// Loads the built-in seeds, tests/corpus (unless disabled) and the extra seed paths.
fn load_seeds(args: &Args) -> anyhow::Result<Seeds> {
    let max_len = args.max_len as usize;
    let mut seeds = Seeds::default();
    for s in BUILTIN_SEEDS {
        seeds.push(s, max_len)?;
    }
    let mut paths = PathStore::default();
    if !args.no_corpus {
        let corpus = repo_root().join("tests/corpus");
        walk_python(&corpus, &mut paths).with_context(|| format!("walking {}", corpus.display()))?;
    }
    let mut buf = Vec::new();
    for p in &args.seeds {
        if p.is_dir() {
            walk_python(p, &mut paths).with_context(|| format!("walking {}", p.display()))?;
        } else {
            read_seed(p, &mut buf, &mut seeds, max_len)?;
        }
    }
    // Directory order is filesystem-dependent; sort so a seed number means the same file
    // everywhere (inputs are regenerated from (seed, iteration) and the seed list).
    let mut order: Vec<usize> = (0..paths.len()).collect();
    order.sort_unstable_by(|&a, &b| paths.get(a).cmp(paths.get(b)));
    for i in order {
        read_seed(paths.get(i), &mut buf, &mut seeds, max_len)?;
    }
    Ok(seeds)
}

/// Reads one seed file through the reusable `buf`.
fn read_seed(path: &Path, buf: &mut Vec<u8>, seeds: &mut Seeds, max_len: usize) -> anyhow::Result<()> {
    buf.clear();
    let mut f = fs::File::open(path).with_context(|| format!("opening seed {}", path.display()))?;
    f.read_to_end(buf).with_context(|| format!("reading seed {}", path.display()))?;
    seeds.push(buf, max_len)
}

fn default_seed() -> u64 {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    rng::splitmix64(t ^ u64::from(std::process::id()).rotate_left(32))
}

pub fn run(args: &Args) -> anyhow::Result<ExitCode> {
    if args.list {
        let mut out = std::io::stdout().lock();
        for t in targets::TARGETS {
            let tag = if t.selftest { " (self-test)" } else { "" };
            writeln!(out, "{:<16} {}{tag}", t.name, t.about)?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    let target = args.target()?;
    if !args.reproduce.is_empty() {
        return reproduce(args, target);
    }
    if let Some(iter) = args.replay {
        let seed = args.seed.context("--replay needs --seed")?;
        let seeds = load_seeds(args)?;
        let mut input = Vec::new();
        Mutator::new(&seeds, seed, args.max_len as usize).generate(iter, &mut input, &mut Scratch::default());
        std::io::stdout().lock().write_all(&input)?;
        return Ok(ExitCode::SUCCESS);
    }
    if args.child {
        return run_child(args, target);
    }
    supervise(args, target)
}

fn check_unwind() -> anyhow::Result<()> {
    if cfg!(panic = "abort") {
        bail!("this xtask build aborts on panic, so crashes cannot be caught; use a profile that unwinds (--profile fuzz)");
    }
    Ok(())
}

/// `--reproduce`: run the target once per file and report what happened.
fn reproduce(args: &Args, target: &FuzzTarget) -> anyhow::Result<ExitCode> {
    check_unwind()?;
    let mut ex = Executor::new(target.run, args.budget(), args.stack_size(), args.reproduce.len());
    let mut problems = 0;
    let mut out = std::io::stdout().lock();
    let mut data = Vec::new();
    for path in &args.reproduce {
        data.clear();
        if path.as_os_str() == "-" {
            std::io::stdin().lock().read_to_end(&mut data).context("reading stdin")?;
        } else {
            let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
            f.read_to_end(&mut data).with_context(|| format!("reading {}", path.display()))?;
        }
        match ex.run(&data).context("could not run the target")? {
            Outcome::Ok => writeln!(out, "{}: ok", path.display())?,
            Outcome::Crash(_) => {
                problems += 1;
                let (location, message) = ex.last_panic();
                writeln!(out, "{}: crash at {location}: {message}", path.display())?;
            }
            Outcome::Hang => {
                problems += 1;
                writeln!(out, "{}: hang (over {} ms)", path.display(), args.budget_ms)?;
            }
        }
    }
    Ok(if problems > 0 { ExitCode::from(EXIT_FINDINGS) } else { ExitCode::SUCCESS })
}

/// Writes the artifacts of findings into one directory.
struct Artifacts<'a> {
    dir: &'a Path,
    target: &'a FuzzTarget,
    seed: u64,
    /// Options that select the seed inputs, for the `--replay` hint.
    seed_flags: String,
}

impl<'a> Artifacts<'a> {
    fn new(dir: &'a Path, target: &'a FuzzTarget, seed: u64, args: &Args) -> Artifacts<'a> {
        use std::fmt::Write as _;
        let mut seed_flags = String::new();
        if args.no_corpus {
            seed_flags.push_str(" --no-corpus");
        }
        if args.max_len != DEFAULT_MAX_LEN {
            let _ = write!(seed_flags, " --max-len {}", args.max_len);
        }
        if !args.seeds.is_empty() {
            seed_flags.push_str(" --");
            for p in &args.seeds {
                let _ = write!(seed_flags, " {}", p.display());
            }
        }
        Artifacts { dir, target, seed, seed_flags }
    }

    /// File stem, e.g. `crash-0123456789abcdef`.
    fn stem(f: &Finding) -> String {
        format!("{}-{:016x}", f.kind.name(), f.sig)
    }

    fn write(&self, name: &str, data: &[u8]) -> anyhow::Result<PathBuf> {
        fs::create_dir_all(self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.dir.join(name);
        fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    /// Saves the raw input as soon as a finding is known.
    fn save_raw(&self, f: &Finding) -> anyhow::Result<PathBuf> {
        self.write(&format!("{}.raw.py", Self::stem(f)), &f.input)
    }

    /// Removes the raw input of a finding that turned out to be a duplicate.
    fn remove_raw(&self, f: &Finding) {
        // Best effort: a leftover raw file is harmless.
        let _ = fs::remove_file(self.dir.join(format!("{}.raw.py", Self::stem(f))));
    }

    /// Saves the (possibly minimised) input as `<stem>.py` plus a `<stem>.txt` description.
    fn save_final(&self, f: &Finding, data: &[u8], note: &str) -> anyhow::Result<PathBuf> {
        use std::fmt::Write as _;
        let stem = Self::stem(f);
        let path = self.write(&format!("{stem}.py"), data)?;
        let mut info = String::new();
        let _ = writeln!(info, "kind: {}", f.kind.name());
        let _ = writeln!(info, "target: {}", self.target.name);
        let _ = writeln!(info, "seed: {}", self.seed);
        let _ = writeln!(info, "iteration: {}", f.iter);
        if !f.location.is_empty() {
            let _ = writeln!(info, "location: {}", f.location);
        }
        let _ = writeln!(info, "message: {}", f.message);
        let _ = writeln!(info, "raw: {stem}.raw.py ({} bytes)", f.input.len());
        let _ = writeln!(info, "input: {stem}.py ({} bytes; {note})", data.len());
        let _ = writeln!(info, "reproduce: cargo xtask fuzz -t {} --reproduce {}", self.target.name, path.display());
        let _ = writeln!(
            info,
            "regenerate: cargo xtask fuzz -t {} --seed {} --replay {}{}",
            self.target.name, self.seed, f.iter, self.seed_flags
        );
        self.write(&format!("{stem}.txt"), info.as_bytes())?;
        Ok(path)
    }
}

/// Final inputs already saved, by content hash. Hangs and aborts are found per input (they have
/// no panic location), so many findings minimise to the same input; only the first is kept.
type Saved = HashMap<u64, PathBuf>;

/// Minimises `f` with `still_fails` (unless `max_execs` is 0) and saves the result. Returns the
/// new artifact, or `None` if the result duplicates one in `saved` (its raw file is removed).
fn finish(
    art: &Artifacts,
    f: &Finding,
    max_execs: u32,
    saved: &mut Saved,
    still_fails: &mut dyn FnMut(&[u8]) -> Option<bool>,
) -> anyhow::Result<Option<PathBuf>> {
    let (data, note) = if max_execs == 0 {
        (Cow::Borrowed(&f.input[..]), "not minimised".to_owned())
    } else {
        let m = minimise(&f.input, max_execs as usize, still_fails);
        let note = format!("minimised in {} execs{}", m.execs, if m.aborted { ", stopped early" } else { "" });
        (Cow::Owned(m.data), note)
    };
    match saved.entry(fnv1a(&data)) {
        Entry::Occupied(e) => {
            eprintln!("fuzz: {} (iteration {}) reduces to the same input as {}", f.kind.name(), f.iter, e.get().display());
            art.remove_raw(f);
            Ok(None)
        }
        Entry::Vacant(e) => {
            let path = art.save_final(f, &data, &note)?;
            e.insert(path.clone());
            Ok(Some(path))
        }
    }
}

/// The fuzzing child: run the engine, save and minimise findings, print a summary.
fn run_child(args: &Args, target: &'static FuzzTarget) -> anyhow::Result<ExitCode> {
    check_unwind()?;
    let seed = args.seed.unwrap_or_else(default_seed);
    let seeds = Arc::new(load_seeds(args)?);
    let jobs = args.jobs();
    let slots = jobs + args.max_hangs as usize;
    let inflight = Arc::new(match &args.inflight {
        Some(p) => Inflight::mapped(p, slots, seed, args.start_iter)?,
        None => Inflight::heap(slots, seed, args.start_iter),
    });
    let dir = args.artifact_dir(target);
    let art = Artifacts::new(&dir, target, seed, args);
    let cfg = Config {
        seed,
        start_iter: args.start_iter,
        iterations: args.iterations,
        duration: args.duration()?,
        jobs,
        budget: args.budget(),
        max_len: args.max_len as usize,
        max_hangs: args.max_hangs as usize,
        stack_size: args.stack_size(),
        progress: Some(Duration::from_secs(5)),
    };
    eprintln!(
        "fuzz: target {} | seed {seed} | {} seed inputs ({} bytes) | {jobs} threads | budget {} ms | from iteration {}",
        target.name,
        seeds.len(),
        seeds.total_bytes(),
        args.budget_ms,
        args.start_iter
    );
    let mut save_err: Option<anyhow::Error> = None;
    let report = engine::fuzz(target.run, Arc::clone(&seeds), inflight, &cfg, &mut |f| {
        let at = if f.location.is_empty() { String::new() } else { format!(" at {}", f.location) };
        eprintln!("fuzz: new {} (iteration {}){at}: {}", f.kind.name(), f.iter, f.message);
        if let Err(e) = art.save_raw(f) {
            save_err.get_or_insert(e);
        }
    })?;
    if let Some(e) = save_err {
        return Err(e);
    }

    // Crashes are minimised in-process (fast); hangs in fresh processes, because confirming a
    // hang in-process would abandon another stuck thread each time.
    let mut ex = Executor::new(target.run, args.budget(), args.stack_size(), args.max_hangs as usize);
    let exe = std::env::current_exe().context("locating the xtask executable")?;
    let mut seen = Saved::new();
    let mut saved = Vec::with_capacity(report.findings.len());
    for f in &report.findings {
        let path = match f.kind {
            Kind::Crash => {
                let want = Outcome::Crash(f.sig);
                finish(&art, f, args.minimise_execs, &mut seen, &mut |c| ex.run(c).map(|o| o == want))?
            }
            Kind::Hang | Kind::Abort => {
                let deadline = Instant::now() + HANG_MINIMISE_TIME;
                let execs = args.minimise_execs.min(MAX_PROCESS_MINIMISE_EXECS);
                finish(&art, f, execs, &mut seen, &mut |c| {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    probe(&exe, args, target, c).ok().and_then(|p| p.result.is(Replay::Hang))
                })?
            }
        };
        saved.extend(path);
    }

    let mut out = std::io::stdout().lock();
    let secs = report.elapsed.as_secs_f64();
    writeln!(
        out,
        "fuzz {}: {} execs in {secs:.1}s ({:.0}/s, {jobs} threads), seed {seed}, iterations {}..{}",
        target.name,
        report.execs,
        report.execs as f64 / secs.max(1e-9),
        args.start_iter,
        report.next_iter
    )?;
    let distinct = report.findings.iter().filter(|f| f.kind == Kind::Crash).count();
    writeln!(out, "crashing inputs: {} ({distinct} distinct), hangs: {}", report.crashes, report.hangs)?;
    if report.hang_limit_hit {
        writeln!(out, "stopped early: --max-hangs {} reached", args.max_hangs)?;
    }
    for p in &saved {
        writeln!(out, "saved {}", p.display())?;
    }
    Ok(if report.findings.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(EXIT_FINDINGS) })
}

// ---------------------------------------------------------------------------------------------
// Supervisor

/// Whether a child's exit status means the process died (signal, abort, OOM kill, or a panic
/// outside the target) rather than exiting with one of our codes.
fn died(status: ExitStatus) -> bool {
    !matches!(status.code(), Some(0..=2))
}

/// Per-launch parameters of a fuzzing child.
struct ChildRun<'a> {
    seed: u64,
    start: u64,
    iterations: Option<u64>,
    time: Option<Duration>,
    inflight: &'a Path,
    out_base: &'a Path,
}

/// The fuzzing child's command line: the user's options plus where to start.
fn child_command(exe: &Path, args: &Args, target: &FuzzTarget, run: &ChildRun) -> Command {
    let mut cmd = Command::new(exe);
    cmd.args(["fuzz", "--child", "--target", target.name]);
    cmd.arg("--seed").arg(run.seed.to_string());
    cmd.arg("--start-iter").arg(run.start.to_string());
    cmd.arg("--inflight").arg(run.inflight);
    cmd.arg("--out").arg(run.out_base);
    cmd.arg("--budget-ms").arg(args.budget_ms.to_string());
    cmd.arg("--max-len").arg(args.max_len.to_string());
    cmd.arg("--max-hangs").arg(args.max_hangs.to_string());
    cmd.arg("--minimise-execs").arg(args.minimise_execs.to_string());
    cmd.arg("--stack-mb").arg(args.stack_mb.to_string());
    if let Some(j) = args.jobs {
        cmd.arg("--jobs").arg(j.to_string());
    }
    if let Some(n) = run.iterations {
        cmd.arg("--iterations").arg(n.to_string());
    }
    if let Some(t) = run.time {
        cmd.arg("--time").arg(format!("{:.3}", t.as_secs_f64().max(0.001)));
    }
    if args.no_corpus {
        cmd.arg("--no-corpus");
    }
    cmd.arg("--").args(&args.seeds);
    cmd
}

/// What replaying one input in a fresh process did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Replay {
    Ok,
    Crash,
    Hang,
    /// The process died (signal, abort).
    Died,
    /// The process failed with a usage or fatal error (exit code 2).
    Failed,
}

impl Replay {
    /// Minimisation predicate: whether this is `want`; `None` (stop) on `Failed`.
    fn is(self, want: Replay) -> Option<bool> {
        if self == Replay::Failed { None } else { Some(self == want) }
    }
}

struct Probe {
    result: Replay,
    status: ExitStatus,
    stderr: Vec<u8>,
}

/// Runs `input` once in a fresh `--reproduce -` process.
fn probe(exe: &Path, args: &Args, target: &FuzzTarget, input: &[u8]) -> anyhow::Result<Probe> {
    let mut child = Command::new(exe)
        .args(["fuzz", "--target", target.name])
        .arg("--budget-ms")
        .arg(args.budget_ms.to_string())
        .arg("--stack-mb")
        .arg(args.stack_mb.to_string())
        .args(["--reproduce", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {}", exe.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        // The child reads all of stdin before running the target; a write error means it died
        // early, which its exit status reports.
        let _ = stdin.write_all(input);
    }
    let out = child.wait_with_output().context("waiting for the replay process")?;
    let result = match out.status.code() {
        _ if died(out.status) => Replay::Died,
        Some(0) => Replay::Ok,
        // `--reproduce` prints "<path>: hang (over N ms)" or "<path>: crash at ...".
        Some(1) if out.stdout.windows(7).any(|w| w == b": hang ") => Replay::Hang,
        Some(1) => Replay::Crash,
        _ => Replay::Failed,
    };
    Ok(Probe { result, status: out.status, stderr: out.stderr })
}

/// "killed the fuzzing process (<status>): <last stderr lines>".
fn death_message(status: ExitStatus, stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let mut lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let keep = lines.len().saturating_sub(2);
    lines.drain(..keep);
    if lines.is_empty() {
        format!("killed the process ({status})")
    } else {
        format!("killed the process ({status}): {}", lines.join(" | "))
    }
}

/// After a child died: replays its in-flight inputs one per process to find the one that kills
/// the process, then minimises and saves it. `Ok(false)` when no in-flight input reproduces.
fn recover(
    exe: &Path,
    args: &Args,
    art: &Artifacts,
    seeds: &Seeds,
    snap: &Snapshot,
    saved: &mut Saved,
) -> anyhow::Result<bool> {
    let target = art.target;
    let mutator = Mutator::new(seeds, art.seed, args.max_len as usize);
    let mut scratch = Scratch::default();
    let mut input = Vec::new();
    for &iter in &snap.running {
        mutator.generate(iter, &mut input, &mut scratch);
        let p = probe(exe, args, target, &input)?;
        if p.result != Replay::Died {
            continue;
        }
        let f = Finding {
            kind: Kind::Abort,
            sig: fnv1a(&input),
            iter,
            input,
            location: String::new(),
            message: death_message(p.status, &p.stderr),
        };
        eprintln!("fuzz: new abort (iteration {iter}): {}", f.message);
        art.save_raw(&f)?;
        let mut probe_err: Option<anyhow::Error> = None;
        let execs = args.minimise_execs.min(MAX_PROCESS_MINIMISE_EXECS);
        finish(art, &f, execs, saved, &mut |c| match probe(exe, args, target, c) {
            Ok(p) => p.result.is(Replay::Died),
            Err(e) => {
                probe_err.get_or_insert(e);
                None
            }
        })?;
        if let Some(e) = probe_err {
            eprintln!("fuzz: minimisation stopped: {e:#}");
        }
        return Ok(true);
    }
    Ok(false)
}

/// Runs fuzzing children until the iteration/time limit, restarting after a child dies (at
/// most `--max-restarts` times) and recovering the input that killed it.
fn supervise(args: &Args, target: &'static FuzzTarget) -> anyhow::Result<ExitCode> {
    let why = format!("fuzz: building xtask with --profile {} (optimised; panics unwind)", args.profile);
    let exe = build::profile_exe(&args.profile, args.use_current_exe, &why)?;
    let seed = args.seed.unwrap_or_else(default_seed);
    let duration = args.duration()?;
    let end_iter = args.iterations.map(|n| args.start_iter.saturating_add(n).min(MAX_ITER));
    let out_base = args.artifact_base();
    let dir = out_base.join(target.name);
    let art = Artifacts::new(&dir, target, seed, args);
    let inflight = std::env::temp_dir().join(format!("pymapper-fuzz-{}.inflight", std::process::id()));
    let begin = Instant::now();
    let mut start = args.start_iter;
    let mut findings = false;
    let mut deaths = 0u32;
    let mut culprits = 0u32;
    let mut aborts = Saved::new();
    let mut seeds: Option<Seeds> = None;
    let result = loop {
        let iterations = match end_iter {
            Some(end) if start >= end => break Ok(()),
            Some(end) => Some(end - start),
            None => None,
        };
        let time = match duration {
            Some(d) if begin.elapsed() >= d => break Ok(()),
            Some(d) => Some(d - begin.elapsed().min(d)),
            None => None,
        };
        // A stale file must not be mistaken for this child's state if it dies early.
        let _ = fs::remove_file(&inflight);
        let run = ChildRun { seed, start, iterations, time, inflight: &inflight, out_base: &out_base };
        let status = child_command(&exe, args, target, &run)
            .status()
            .with_context(|| format!("spawning {}", exe.display()))?;
        if !died(status) {
            match status.code() {
                Some(0) => break Ok(()),
                Some(1) => {
                    findings = true;
                    break Ok(());
                }
                _ => break Err(anyhow::anyhow!("the fuzzing process failed ({status})")),
            }
        }
        deaths += 1;
        eprintln!("fuzz: the fuzzing process died ({status}); replaying its in-flight inputs");
        let snap = match read_snapshot(&inflight) {
            Ok(s) if s.seed == seed => s,
            Ok(_) => break Err(anyhow::anyhow!("in-flight file has a different seed")),
            Err(e) => break Err(e.context("the fuzzing process died before it started fuzzing")),
        };
        let seeds = match &mut seeds {
            Some(s) => &*s,
            None => &*seeds.insert(load_seeds(args)?),
        };
        if recover(&exe, args, &art, seeds, &snap, &mut aborts)? {
            culprits += 1;
        } else {
            eprintln!("fuzz: none of the in-flight inputs {:?} kills a fresh process", snap.running);
        }
        if deaths > args.max_restarts {
            eprintln!("fuzz: not restarting (--max-restarts {} reached)", args.max_restarts);
            break Ok(());
        }
        // Inputs claimed by workers but not yet run when the child died are skipped.
        start = snap.next_iter.max(start);
        eprintln!("fuzz: restarting from iteration {start}");
    };
    let _ = fs::remove_file(&inflight);
    result?;
    if deaths > 0 {
        let mut out = std::io::stdout().lock();
        writeln!(out, "process deaths: {deaths}, explained by an input: {culprits}, distinct inputs: {}", aborts.len())?;
        let mut paths: Vec<&PathBuf> = aborts.values().collect();
        paths.sort_unstable();
        for p in paths {
            writeln!(out, "saved {}", p.display())?;
        }
    }
    Ok(if findings || deaths > 0 { ExitCode::from(EXIT_FINDINGS) } else { ExitCode::SUCCESS })
}
