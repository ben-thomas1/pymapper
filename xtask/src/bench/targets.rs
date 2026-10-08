//! Benchmark targets. Each runs once inside a fresh child process and returns a [`RunRecord`].
//!
//! Phases are reported through `pymapper_core::timing`, so phases recorded by core code under
//! test show up alongside the ones recorded here.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Instant;

use anyhow::Context;
use clap::ValueEnum;
use pymapper_core::extract::{self, Visitor};
use pymapper_core::extract::events::{Call, Def, Diagnostic};
use pymapper_core::lex::{Lexer, TokenKind};
use pymapper_core::timing;
use rayon::prelude::*;

use pymapper_core::walk::{self as core_walk, DiagnosticKind, PathList, WalkOptions};

use super::rusage;
use super::walk::{PathStore, walk_python};

/// A named benchmark target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, ValueEnum)]
pub enum Target {
    /// Walk + parallel read of every file: the I/O floor.
    Read,
    /// The front end in the loader's shape: walk, then per thread read -> lex + parse + facts
    /// + metrics (interned) -> drop the source, keeping all facts ([`super::extract`]).
    ParseOnly,
    /// Parallel walk only (`pymapper_core::walk`, corpus options).
    Walk,
    /// Sequential reference walk only (single thread, same file set as `walk`).
    WalkSeq,
    /// Parallel walk only with first-party options: gitignore files honoured, hidden
    /// directories skipped.
    WalkGit,
    /// Import-root discovery (`pymapper_core::roots`) for the venv enclosing the corpus (or the
    /// corpus as a project without one), plus the module map over every `RECORD` file.
    Roots,
    /// Walk + parallel read + lex every file to EOF ([`lex_file`]): lexer throughput.
    Lex,
    /// Lexer micro benchmark: walk, load the whole corpus into memory (`load`), then lex it
    /// from memory in parallel (`lex`) with no I/O in the measured phase. Use `-j 1` for
    /// single-core throughput. Peak RSS includes the loaded corpus.
    LexMem,
    /// Parser micro benchmark: like `lex-mem`, but runs the parser (`pymapper_core::extract`)
    /// with a counting visitor in the measured phase `parse`.
    ParseMem,
    /// Like `parse-mem`, plus the per-definition metrics visitor
    /// (`pymapper_core::extract::metrics`) in the same pass; metrics rows are kept.
    MetricsMem,
    /// Like `parse-mem`, but runs the full front end of `parse-only` (facts + metrics, interned
    /// names) from memory: the extractor micro benchmark (use `-j 1` for one core, and for
    /// profiling: `samply record xtask bench-run --target extract-mem --root DIR`).
    ExtractMem,
    /// End-to-end streaming load (`pymapper_core::index::load`): walk, then read -> extract ->
    /// intern -> drop the source per file in parallel, merge into the global tables and build
    /// the hierarchy. The corpus root is the import root of one first-party group. Peak RSS
    /// includes the whole index.
    Load,
    /// `load` through the facts cache (`pymapper_core::cache::load`) after deleting the cache:
    /// extraction plus encoding and writing every segment. Inputs come from import roots when
    /// the corpus is a venv's site-packages (one trusted segment per distribution) or a stdlib
    /// (one trusted segment); any other corpus is one first-party group validated per file
    /// (stat). The cache lives in `$TMPDIR/pymapper-bench-cache/<corpus hash>`.
    LoadCold,
    /// Like `load-cold` but keeps the cache: with the default warm-up run, every file is a
    /// cache hit (no read, no parse).
    LoadWarm,
}

impl Target {
    pub fn name(self) -> &'static str {
        match self {
            Target::Read => "read",
            Target::ParseOnly => "parse-only",
            Target::Walk => "walk",
            Target::WalkSeq => "walk-seq",
            Target::WalkGit => "walk-git",
            Target::Roots => "roots",
            Target::Lex => "lex",
            Target::LexMem => "lex-mem",
            Target::ParseMem => "parse-mem",
            Target::MetricsMem => "metrics-mem",
            Target::ExtractMem => "extract-mem",
            Target::Load => "load",
            Target::LoadCold => "load-cold",
            Target::LoadWarm => "load-warm",
        }
    }
}

/// Result of one measured run.
#[derive(Clone, Debug, PartialEq)]
pub struct RunRecord {
    pub files: u64,
    pub bytes: u64,
    /// Files or directories that could not be read (skipped, not fatal).
    pub errors: u64,
    pub threads: u64,
    /// Phase wall times in seconds, in first-recorded order.
    pub phases: Vec<(String, f64)>,
    /// Wall time of the whole target in seconds.
    pub total_s: f64,
    pub peak_rss_bytes: u64,
    /// Target-specific counts (e.g. tokens), in a fixed order.
    pub counters: Vec<(String, u64)>,
}

/// Per-thread state of the parse hook. Later steps add lexer/extractor state and counters here.
#[derive(Default)]
pub struct ParseAcc {
    lex: LexAcc,
    parse: ParserAcc,
}

/// Counters of the `parse-mem` target.
#[derive(Default)]
struct ParserAcc {
    files: u64,
    bytes: u64,
    defs: u64,
    calls: u64,
    diagnostics: u64,
    /// Time spent inside the parser, summed over threads.
    nanos: u64,
    /// `metrics-mem`: metrics rows kept and the heap bytes of their tables.
    metric_rows: u64,
    metric_bytes: u64,
}

/// Visitor of the `parse-mem` target: counts a few events (cheap, so the
/// measurement is dominated by the parser).
#[derive(Default)]
struct CountVisitor {
    defs: u64,
    calls: u64,
    diagnostics: u64,
}

impl Visitor for CountVisitor {
    fn def_enter(&mut self, _: &Def<'_>) {
        self.defs += 1;
    }
    fn call(&mut self, _: &Call<'_>) {
        self.calls += 1;
    }
    fn diagnostic(&mut self, _: &Diagnostic) {
        self.diagnostics += 1;
    }
}

/// Counters of the `lex` target.
#[derive(Default)]
struct LexAcc {
    files: u64,
    bytes: u64,
    tokens: u64,
    error_tokens: u64,
    /// Time spent inside the lexer, summed over threads (excludes walk and I/O).
    nanos: u64,
}

impl ParseAcc {
    fn merge(&mut self, other: ParseAcc) {
        let (a, b) = (&mut self.lex, other.lex);
        a.files += b.files;
        a.bytes += b.bytes;
        a.tokens += b.tokens;
        a.error_tokens += b.error_tokens;
        a.nanos += b.nanos;
        let (a, b) = (&mut self.parse, other.parse);
        a.files += b.files;
        a.bytes += b.bytes;
        a.defs += b.defs;
        a.calls += b.calls;
        a.diagnostics += b.diagnostics;
        a.nanos += b.nanos;
        a.metric_rows += b.metric_rows;
        a.metric_bytes += b.metric_bytes;
    }

    fn counters(&self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        let lx = &self.lex;
        if lx.files > 0 {
            out.push(("tokens".to_owned(), lx.tokens));
            out.push(("error_tokens".to_owned(), lx.error_tokens));
            out.push(("lex_cpu_ms".to_owned(), lx.nanos / 1_000_000));
            // MB (10^6 bytes) per second of lexer time: throughput of one core.
            out.push(("lex_MBps_core".to_owned(), (lx.bytes * 1000).checked_div(lx.nanos).unwrap_or(0)));
        }
        let p = &self.parse;
        if p.files > 0 {
            out.push(("defs".to_owned(), p.defs));
            out.push(("calls".to_owned(), p.calls));
            out.push(("diagnostics".to_owned(), p.diagnostics));
            out.push(("parse_cpu_ms".to_owned(), p.nanos / 1_000_000));
            out.push(("parse_MBps_core".to_owned(), (p.bytes * 1000).checked_div(p.nanos).unwrap_or(0)));
        }
        if p.metric_rows > 0 {
            out.push(("metric_rows".to_owned(), p.metric_rows));
            out.push(("metrics_kb".to_owned(), p.metric_bytes / 1000));
        }
        out
    }
}

/// The `lex` hook: lexes the whole file (parser mode, no trivia) and counts tokens.
pub fn lex_file(src: &[u8], acc: &mut ParseAcc) {
    let t0 = Instant::now();
    let (tokens, errors) = lex_count(src);
    let lx = &mut acc.lex;
    lx.nanos += t0.elapsed().as_nanos() as u64;
    lx.files += 1;
    lx.bytes += src.len() as u64;
    lx.tokens += tokens;
    lx.error_tokens += errors;
}

/// Lexes `src` to EOF; returns (tokens incl. EndOfFile, error tokens).
#[inline]
fn lex_count(src: &[u8]) -> (u64, u64) {
    let mut lexer = Lexer::new(src);
    let mut tokens = 0u64;
    let mut errors = 0u64;
    loop {
        let t = lexer.next_token();
        tokens += 1;
        errors += u64::from(t.kind.is_error());
        if t.kind == TokenKind::EndOfFile {
            return (tokens, errors);
        }
    }
}

/// The whole corpus in memory: per-job concatenated buffers plus (buffer, start, end) per file.
pub(super) struct Loaded {
    pub(super) bufs: Vec<Vec<u8>>,
    pub(super) files: Vec<(u32, usize, usize)>,
}

/// `lex-mem`: loads every file (phase `load`), then lexes from memory (phase `lex`).
fn lex_mem(paths: &PathList) -> (ReadStats, ParseAcc) {
    let (stats, loaded) = timing::time("load", || load_files(paths));
    let acc = timing::time("lex", || {
        loaded
            .files
            .par_iter()
            .fold(ParseAcc::default, |mut acc, &(b, s, e)| {
                let src = &loaded.bufs[b as usize][s..e];
                let t0 = Instant::now();
                let (tokens, errors) = lex_count(src);
                let lx = &mut acc.lex;
                lx.nanos += t0.elapsed().as_nanos() as u64;
                lx.files += 1;
                lx.bytes += src.len() as u64;
                lx.tokens += tokens;
                lx.error_tokens += errors;
                acc
            })
            .reduce(ParseAcc::default, |mut a, b| {
                a.merge(b);
                a
            })
    });
    (stats, acc)
}

/// `parse-mem`: loads every file (phase `load`), then parses from memory (phase `parse`).
fn parse_mem(paths: &PathList) -> (ReadStats, ParseAcc) {
    let (stats, loaded) = timing::time("load", || load_files(paths));
    let acc = timing::time("parse", || {
        loaded
            .files
            .par_iter()
            .fold(ParseAcc::default, |mut acc, &(b, s, e)| {
                let src = &loaded.bufs[b as usize][s..e];
                let t0 = Instant::now();
                let v = extract::parse(src, CountVisitor::default());
                let p = &mut acc.parse;
                p.nanos += t0.elapsed().as_nanos() as u64;
                p.files += 1;
                p.bytes += src.len() as u64;
                p.defs += v.defs;
                p.calls += v.calls;
                p.diagnostics += v.diagnostics;
                acc
            })
            .reduce(ParseAcc::default, |mut a, b| {
                a.merge(b);
                a
            })
    });
    (stats, acc)
}

/// `metrics-mem`: like `parse-mem`, with the metrics visitor teed in. Each fold job keeps one
/// scratch and appends to one metrics table; all tables stay alive until the end of the phase
/// (as the extraction pipeline keeps metrics with the def records), so peak RSS includes them.
fn metrics_mem(paths: &PathList) -> (ReadStats, ParseAcc) {
    use pymapper_core::extract::{MetricsScratch, MetricsTable, MetricsVisitor, Tee};
    struct Acc {
        acc: ParseAcc,
        scratch: MetricsScratch,
        table: MetricsTable,
    }
    let (stats, loaded) = timing::time("load", || load_files(paths));
    let (mut acc, tables) = timing::time("parse", || {
        loaded
            .files
            .par_iter()
            .fold(
                || Acc { acc: ParseAcc::default(), scratch: MetricsScratch::new(), table: MetricsTable::new() },
                |mut a, &(b, s, e)| {
                    let src = &loaded.bufs[b as usize][s..e];
                    let t0 = Instant::now();
                    let m = MetricsVisitor::new(src, &mut a.scratch, &mut a.table);
                    let v = extract::parse(src, Tee(CountVisitor::default(), m)).0;
                    let p = &mut a.acc.parse;
                    p.nanos += t0.elapsed().as_nanos() as u64;
                    p.files += 1;
                    p.bytes += src.len() as u64;
                    p.defs += v.defs;
                    p.calls += v.calls;
                    p.diagnostics += v.diagnostics;
                    a
                },
            )
            .map(|a| (a.acc, vec![a.table]))
            .reduce(
                || (ParseAcc::default(), Vec::new()),
                |(mut a, mut ta), (b, tb)| {
                    a.merge(b);
                    ta.extend(tb);
                    (a, ta)
                },
            )
    });
    for t in &tables {
        acc.parse.metric_rows += t.len() as u64;
        acc.parse.metric_bytes += t.heap_bytes() as u64;
    }
    (stats, acc)
}

/// Reads every file in parallel, appending each job's files to one growing buffer.
pub(super) fn load_files(paths: &PathList) -> (ReadStats, Loaded) {
    struct Job {
        buf: Vec<u8>,
        spans: Vec<(usize, usize)>,
        stats: ReadStats,
    }
    let jobs: Vec<Job> = (0..paths.len())
        .into_par_iter()
        .fold(
            || Job { buf: Vec::new(), spans: Vec::new(), stats: ReadStats::default() },
            |mut job, i| {
                let start = job.buf.len();
                match File::open(paths.get(i)).and_then(|mut f| f.read_to_end(&mut job.buf)) {
                    Ok(n) => {
                        job.stats.files += 1;
                        job.stats.bytes += n as u64;
                        job.spans.push((start, start + n));
                    }
                    Err(_) => {
                        job.buf.truncate(start);
                        job.stats.errors += 1;
                    }
                }
                job
            },
        )
        .collect();
    let mut stats = ReadStats::default();
    let mut loaded = Loaded { bufs: Vec::with_capacity(jobs.len()), files: Vec::new() };
    for job in jobs {
        stats.files += job.stats.files;
        stats.bytes += job.stats.bytes;
        stats.errors += job.stats.errors;
        let b = loaded.bufs.len() as u32;
        for &(s, e) in &job.spans {
            loaded.files.push((b, s, e));
        }
        loaded.bufs.push(job.buf);
    }
    (stats, loaded)
}

#[inline]
fn no_hook(_src: &[u8], _acc: &mut ParseAcc) {}

/// Runs `target` once over `root`. Requires `timing` to be enabled by the caller to get phases.
pub fn run(target: Target, root: &Path) -> anyhow::Result<RunRecord> {
    let _ = timing::take();
    let t0 = Instant::now();
    let (stats, counters) = match target {
        Target::WalkSeq => {
            let mut paths = PathStore::default();
            let errors = timing::time("walk", || walk_python(root, &mut paths))
                .with_context(|| format!("walking {}", root.display()))?;
            (ReadStats { files: paths.len() as u64, bytes: 0, errors }, Vec::new())
        }
        Target::Walk | Target::WalkGit => {
            let opts = if target == Target::Walk { corpus_options() } else { WalkOptions::default() };
            let (files, errors, counters) = walk_corpus(root, &opts)?;
            (ReadStats { files: files.len() as u64, bytes: 0, errors }, counters)
        }
        Target::Roots => roots_target(root)?,
        Target::Load => load_target(root)?,
        Target::LoadCold | Target::LoadWarm => cache_target(root, target == Target::LoadCold)?,
        Target::Read
        | Target::ParseOnly
        | Target::Lex
        | Target::LexMem
        | Target::ParseMem
        | Target::MetricsMem
        | Target::ExtractMem => {
            let (files, walk_errors, _) = walk_corpus(root, &corpus_options())?;
            let with_acc = |(stats, acc): (ReadStats, ParseAcc)| (stats, acc.counters());
            let (mut stats, counters) = match target {
                Target::Read => with_acc(timing::time("read", || read_files(&files, no_hook))),
                Target::ParseOnly => super::extract::parse_only(&files),
                Target::Lex => with_acc(timing::time("read+lex", || read_files(&files, lex_file))),
                Target::ParseMem => with_acc(parse_mem(&files)),
                Target::MetricsMem => with_acc(metrics_mem(&files)),
                Target::ExtractMem => super::extract::extract_mem(&files)?,
                _ => with_acc(lex_mem(&files)),
            };
            stats.errors += walk_errors;
            (stats, counters)
        }
    };
    let total_s = t0.elapsed().as_secs_f64();
    let phases = timing::take().into_iter().map(|p| (p.name.to_owned(), p.elapsed.as_secs_f64())).collect();
    Ok(RunRecord {
        files: stats.files,
        bytes: stats.bytes,
        errors: stats.errors,
        threads: rayon::current_num_threads() as u64,
        phases,
        total_s,
        peak_rss_bytes: rusage::peak_rss_bytes().context("getrusage")?,
        counters,
    })
}

/// Named counters of a run, in a fixed order.
type Counters = Vec<(String, u64)>;

/// Walk options for the benchmark corpora: every Python file outside `__pycache__` and `.git`,
/// hidden directories included, no gitignore (the same file set as the sequential reference
/// walker, so `walk` and `walk-seq` are comparable).
fn corpus_options() -> WalkOptions {
    WalkOptions::everything()
}

/// Runs the core walker as phase `walk`: files, non-fatal error count and walk counters. An
/// unreadable root is an error.
fn walk_corpus(root: &Path, opts: &WalkOptions) -> anyhow::Result<(PathList, u64, Counters)> {
    let w = timing::time("walk", || core_walk::walk(&[root], opts));
    if let Some(d) = w.diagnostics.iter().find(|d| d.kind == DiagnosticKind::Root || d.path == root) {
        anyhow::bail!("walking {}: {d}", root.display());
    }
    let s = w.stats;
    let counters = vec![
        ("dirs".to_owned(), s.dirs),
        ("entries".to_owned(), s.entries),
        ("ignored".to_owned(), s.ignored),
        ("hidden".to_owned(), s.hidden),
    ];
    Ok((w.files, w.diagnostics.len() as u64, counters))
}

/// `load`: walk (phase `walk`), then [`pymapper_core::index::load`] (phases `load.extract`,
/// `load.merge`, `load.build`). The whole index stays alive until the counters are taken.
fn load_target(root: &Path) -> anyhow::Result<(ReadStats, Counters)> {
    use pymapper_core::index::{Inputs, load};
    use pymapper_core::roots::Origin;
    let (files, walk_errors, _) = walk_corpus(root, &corpus_options())?;
    let inputs = Inputs::from_root_dir(root, &files, Origin::FirstParty, b"corpus");
    drop(files);
    let ix = load(inputs);
    let s = ix.stats();
    let defs = u64::from(ix.facts().defs.len()) - u64::from(ix.file_count());
    let stats = ReadStats { files: s.files, bytes: s.bytes, errors: walk_errors + ix.read_errors().len() as u64 };
    let kb = |b: usize| b as u64 / 1000;
    let counters = vec![
        ("loc".to_owned(), s.lines),
        ("defs".to_owned(), defs),
        ("nodes".to_owned(), u64::from(ix.node_count())),
        ("strings".to_owned(), ix.strings().len() as u64),
        ("index_kb".to_owned(), kb(ix.heap_bytes())),
        ("facts_kb".to_owned(), kb(ix.facts().heap_bytes())),
        ("metrics_kb".to_owned(), kb(ix.metrics().heap_bytes())),
        ("intern_kb".to_owned(), kb(ix.strings().heap_bytes())),
        // Retained index bytes per 1000 LOC (milli-bytes per LOC).
        ("index_B_per_kLOC".to_owned(), (ix.heap_bytes() as u64 * 1000).checked_div(s.lines).unwrap_or(0)),
    ];
    Ok((stats, counters))
}

/// Cache directory of the `load-cold` / `load-warm` targets for `root`.
fn bench_cache_dir(root: &Path) -> std::path::PathBuf {
    let h = pymapper_core::intern::hash_bytes(root.as_os_str().as_encoded_bytes());
    std::env::temp_dir().join("pymapper-bench-cache").join(format!("{h:016x}"))
}

/// Inputs of the cache targets, the import roots they came from and the number of walk errors.
type CacheInputs = (pymapper_core::index::Inputs, Option<pymapper_core::roots::Roots>, u64);

/// The inputs of the cache targets (see [`Target::LoadCold`]):
///
/// - a venv's site-packages: import roots (phase `discover`), then the files of every
///   distribution from its `RECORD` plus a walk of the entries no `RECORD` claims (phase
///   `files`): what the CLI loads, with no walk of distribution files;
/// - a stdlib: a walk without `site-packages` (phase `walk`), then import roots;
/// - anything else: a walk, one first-party group.
fn cache_inputs(root: &Path) -> anyhow::Result<CacheInputs> {
    use pymapper_core::index::Inputs;
    use pymapper_core::roots::{self, Origin, RootOptions};
    if let Some(v) = root.ancestors().take(5).find(|d| d.join("pyvenv.cfg").is_file()) {
        let opts = RootOptions { venv: Some(v.to_path_buf()), include_deps: true, ..RootOptions::default() };
        let r = timing::time("discover", || roots::discover(v.parent().unwrap_or(root), &opts));
        let (files, errors) = timing::time("files", || record_files(&r, root))?;
        return Ok((Inputs::from_roots(&r, &files), Some(r), errors));
    }
    let is_stdlib = root.join("os.py").is_file()
        && root.file_name().is_some_and(|n| n.as_encoded_bytes().starts_with(b"python3."));
    if !is_stdlib {
        let (files, errors, _) = walk_corpus(root, &corpus_options())?;
        return Ok((Inputs::from_root_dir(root, &files, Origin::FirstParty, b"corpus"), None, errors));
    }
    // The stdlib without the base interpreter's site-packages, like `Roots::sources`.
    let opts = WalkOptions { excludes: vec!["/site-packages".to_owned()], ..corpus_options() };
    let (files, errors, _) = walk_corpus(root, &opts)?;
    // The interpreter that owns this stdlib, found through PATH = <prefix>/bin.
    let prefix = root.parent().and_then(Path::parent).context("stdlib prefix")?;
    let version = roots::parse_version_prefix(&root.file_name().unwrap_or_default().as_encoded_bytes()[6..]);
    let project = std::env::temp_dir().join("pymapper-bench-empty-project");
    std::fs::create_dir_all(&project)?;
    let opts = RootOptions {
        include_stdlib: true,
        python_version: version,
        path_env: Some(prefix.join("bin").into_os_string()),
        ..RootOptions::default()
    };
    let r = timing::time("discover", || roots::discover(&project, &opts));
    anyhow::ensure!(r.stdlib.as_deref() == Some(root), "stdlib not discovered for {}", root.display());
    Ok((Inputs::from_roots(&r, &files), Some(r), errors))
}

/// The third-party files under `root` from [`Roots::sources`]: `RECORD` files of every
/// distribution, plus a walk of what no `RECORD` claims. Returns the files and walk errors.
fn record_files(r: &pymapper_core::roots::Roots, root: &Path) -> anyhow::Result<(PathList, u64)> {
    use pymapper_core::roots::Source;
    let full = |_| anyhow::anyhow!("path list full");
    let mut files = PathList::default();
    let mut errors = 0;
    let mut buf = Vec::new();
    for src in r.sources(&[], &WalkOptions::default()) {
        match src {
            Source::Record { dist } => {
                let site = r.dists.site_path(dist);
                if !site.starts_with(root) {
                    continue;
                }
                for rel in r.dists.owned_files(dist) {
                    buf.clear();
                    buf.extend_from_slice(site.as_os_str().as_encoded_bytes());
                    buf.push(b'/');
                    buf.extend_from_slice(rel);
                    files.push(&buf).map_err(full)?;
                }
            }
            Source::Walk { path, opts, .. } if path.starts_with(root) => {
                let w = core_walk::walk(&[&path], &opts);
                errors += w.diagnostics.len() as u64;
                for i in 0..w.files.len() {
                    files.push(w.files.get_bytes(i)).map_err(full)?;
                }
            }
            Source::Walk { .. } => {}
        }
    }
    Ok((files, errors))
}

/// `load-cold` / `load-warm`: the inputs ([`cache_inputs`]), then
/// `pymapper_core::cache::load` (the `load.*` phases, then `cache.write`; segments are opened
/// lazily inside `load.extract`).
fn cache_target(root: &Path, cold: bool) -> anyhow::Result<(ReadStats, Counters)> {
    use pymapper_core::cache::{self, Cache};
    let dir = bench_cache_dir(root);
    let c = Cache::new(&dir);
    if cold {
        c.clear().with_context(|| format!("clearing {}", dir.display()))?;
    }
    let (inputs, roots, walk_errors) = cache_inputs(root)?;
    let (ix, cs) = cache::load(inputs, roots.as_ref(), &c);
    if let Some(e) = &cs.write_error {
        anyhow::bail!("writing the cache: {e}");
    }
    let s = ix.stats();
    let stats = ReadStats { files: s.files, bytes: s.bytes, errors: walk_errors + ix.read_errors().len() as u64 };
    let kb = |b: u64| b / 1000;
    let counters = vec![
        ("loc".to_owned(), s.lines),
        ("segments".to_owned(), u64::from(cs.segments)),
        ("opened".to_owned(), u64::from(cs.opened)),
        ("hits".to_owned(), cs.hits),
        ("verified".to_owned(), cs.verified),
        ("misses".to_owned(), cs.misses),
        ("written".to_owned(), u64::from(cs.written)),
        ("opened_kb".to_owned(), kb(cs.opened_bytes)),
        ("written_kb".to_owned(), kb(cs.written_bytes)),
        ("index_kb".to_owned(), kb(ix.heap_bytes() as u64)),
    ];
    Ok((stats, counters))
}

/// `roots`: discovery as phase `discover` (deps and stdlib on), then the module map over every
/// `RECORD` file as phase `module-map`. The venv is the nearest ancestor of `root` (up to 4
/// levels) with a `pyvenv.cfg`, i.e. a site-packages corpus; without one `root` is the project.
/// Files = `RECORD` files mapped.
fn roots_target(root: &Path) -> anyhow::Result<(ReadStats, Counters)> {
    use pymapper_core::roots::{self, ModuleMapBuilder, RootOptions};
    anyhow::ensure!(root.is_dir(), "{} is not a directory", root.display());
    let venv = root.ancestors().take(5).find(|d| d.join("pyvenv.cfg").is_file());
    let project = venv.and_then(Path::parent).unwrap_or(root);
    let opts = RootOptions {
        venv: venv.map(Path::to_path_buf),
        include_deps: true,
        include_stdlib: true,
        path_env: std::env::var_os("PATH"),
        ..RootOptions::default()
    };
    let r = timing::time("discover", || roots::discover(project, &opts));
    let map = timing::time("module-map", || {
        let mut b = ModuleMapBuilder::new();
        for d in r.dists.ids() {
            r.add_record_files(&mut b, d);
        }
        b.finish()
    });
    let counters = vec![
        ("roots".to_owned(), r.roots.len() as u64),
        ("dists".to_owned(), r.dists.len() as u64),
        ("names".to_owned(), map.names().len() as u64),
        ("diags".to_owned(), r.diagnostics.len() as u64),
        ("dists-kb".to_owned(), (r.dists.heap_bytes() / 1000) as u64),
        ("map-kb".to_owned(), (map.heap_bytes() / 1000) as u64),
    ];
    Ok((ReadStats { files: map.file_count() as u64, bytes: 0, errors: 0 }, counters))
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct ReadStats {
    pub files: u64,
    pub bytes: u64,
    pub errors: u64,
}

struct ReadState {
    buf: Vec<u8>,
    stats: ReadStats,
    acc: ParseAcc,
}

/// Reads every file in parallel into a reused per-job buffer and hands it to `hook`.
pub fn read_files<F: Fn(&[u8], &mut ParseAcc) + Sync>(paths: &PathList, hook: F) -> (ReadStats, ParseAcc) {
    let fresh = || ReadState { buf: Vec::new(), stats: ReadStats::default(), acc: ParseAcc::default() };
    let state = (0..paths.len())
        .into_par_iter()
        .fold(fresh, |mut st, i| {
            st.buf.clear();
            match File::open(paths.get(i)).and_then(|mut f| f.read_to_end(&mut st.buf)) {
                Ok(n) => {
                    st.stats.files += 1;
                    st.stats.bytes += n as u64;
                    hook(&st.buf, &mut st.acc);
                }
                Err(_) => st.stats.errors += 1,
            }
            st
        })
        .map(|st| ReadState { buf: Vec::new(), ..st })
        .reduce(fresh, |mut a, b| {
            a.stats.files += b.stats.files;
            a.stats.bytes += b.stats.bytes;
            a.stats.errors += b.stats.errors;
            a.acc.merge(b.acc);
            a
        });
    (state.stats, state.acc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn corpus_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/corpus")
    }

    #[test]
    fn read_files_counts_bytes_and_errors() {
        let dir = std::env::temp_dir().join(format!("xtask-read-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.py"), "abc").unwrap();
        fs::write(dir.join("b.py"), "").unwrap();
        fs::write(dir.join("c.py"), vec![b'x'; 100_000]).unwrap();
        let mut paths = core_walk::walk(&[&dir], &corpus_options()).files;
        paths.push(dir.join("vanished.py").as_os_str().as_encoded_bytes()).unwrap();
        let (stats, _) = read_files(&paths, no_hook);
        assert_eq!(stats, ReadStats { files: 3, bytes: 100_003, errors: 1 });
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn targets_agree_with_the_filesystem() {
        let root = corpus_dir();
        let mut paths = PathStore::default();
        walk_python(&root, &mut paths).unwrap();
        let mut bytes = 0u64;
        for i in 0..paths.len() {
            bytes += fs::metadata(paths.get(i)).unwrap().len();
        }
        timing::enable(true);
        for target in [
            Target::Read,
            Target::ParseOnly,
            Target::Walk,
            Target::WalkSeq,
            Target::WalkGit,
            Target::Lex,
            Target::LexMem,
            Target::ParseMem,
            Target::MetricsMem,
            Target::ExtractMem,
            Target::Load,
            Target::LoadCold,
            Target::LoadWarm,
        ] {
            let rec = run(target, &root).expect("run");
            // The corpus has no ignore files or hidden directories, so all walks agree.
            assert_eq!(rec.files, paths.len() as u64, "{target:?}");
            let read = !matches!(target, Target::Walk | Target::WalkSeq | Target::WalkGit | Target::Roots);
            assert_eq!(rec.bytes, if read { bytes } else { 0 }, "{target:?}");
            assert_eq!(rec.errors, 0);
            assert!(rec.threads >= 1);
            assert!(rec.peak_rss_bytes > 0);
            let names: Vec<&str> = rec.phases.iter().map(|(n, _)| n.as_str()).collect();
            let expected: &[&str] = match target {
                Target::Read => &["walk", "read"],
                Target::ParseOnly => &["walk", "read+parse"],
                Target::Lex => &["walk", "read+lex"],
                Target::LexMem => &["walk", "load", "lex"],
                Target::ParseMem | Target::MetricsMem | Target::ExtractMem => &["walk", "load", "parse"],
                Target::Walk | Target::WalkSeq | Target::WalkGit => &["walk"],
                Target::Load => &["walk", "load.extract", "load.merge", "load.build"],
                Target::LoadCold | Target::LoadWarm => {
                    &["walk", "load.extract", "load.merge", "load.build", "cache.write"]
                }
                Target::Roots => unreachable!("checked below"),
            };
            assert_eq!(names, expected, "{target:?}");
            let counters: Vec<&str> = rec.counters.iter().map(|(n, _)| n.as_str()).collect();
            if matches!(target, Target::Walk | Target::WalkGit) {
                assert_eq!(counters, ["dirs", "entries", "ignored", "hidden"], "{target:?}");
            }
            let phase_sum: f64 = rec.phases.iter().map(|(_, s)| s).sum();
            assert!(phase_sum <= rec.total_s + 1e-6, "phases {phase_sum} > total {}", rec.total_s);
            let counter = |name: &str| rec.counters.iter().find(|(n, _)| n == name).map(|&(_, v)| v);
            if matches!(target, Target::Lex | Target::LexMem) {
                assert!(counter("tokens").unwrap() > rec.files);
                assert_eq!(counter("error_tokens"), Some(0));
                assert!(counter("lex_MBps_core").is_some());
            } else if matches!(target, Target::ParseMem | Target::MetricsMem) {
                assert!(counter("defs").unwrap() > 0);
                assert!(counter("calls").unwrap() > 0);
                assert!(counter("parse_MBps_core").is_some());
                assert_eq!(counter("metric_rows"), if target == Target::MetricsMem { counter("defs") } else { None });
            } else if matches!(target, Target::ParseOnly | Target::ExtractMem) {
                let names: Vec<&str> = rec.counters.iter().map(|(n, _)| n.as_str()).collect();
                assert_eq!(names, [
                    "loc", "defs", "calls", "diagnostics", "parse_cpu_ms", "parse_MBps_core", "facts_B_per_kLOC",
                    "metrics_B_per_kLOC", "facts_kb", "metrics_kb", "intern_kb"
                ]);
                assert!(counter("loc").unwrap() > rec.files);
                assert!(counter("defs").unwrap() > 0 && counter("calls").unwrap() > 0);
                assert!(counter("facts_B_per_kLOC").unwrap() > 1000);
                assert!(counter("metrics_B_per_kLOC").unwrap() > 0);
            } else if target == Target::Load {
                let names: Vec<&str> = rec.counters.iter().map(|(n, _)| n.as_str()).collect();
                assert_eq!(names, [
                    "loc", "defs", "nodes", "strings", "index_kb", "facts_kb", "metrics_kb", "intern_kb",
                    "index_B_per_kLOC"
                ]);
                assert!(counter("loc").unwrap() > rec.files);
                assert!(counter("nodes").unwrap() > counter("defs").unwrap());
                assert!(counter("index_B_per_kLOC").unwrap() > 1000);
            } else if matches!(target, Target::LoadCold | Target::LoadWarm) {
                assert_eq!(counter("segments"), Some(1));
                if target == Target::LoadCold {
                    assert_eq!((counter("misses"), counter("written")), (Some(rec.files), Some(1)));
                } else {
                    // Right after `load-cold`: every file is a hit (or verified, when the
                    // corpus was written within the last seconds).
                    assert_eq!(counter("misses"), Some(0));
                    assert_eq!(counter("hits").unwrap() + counter("verified").unwrap(), rec.files);
                }
            } else if !matches!(target, Target::Walk | Target::WalkGit) {
                assert!(rec.counters.is_empty(), "{target:?}");
            }
        }
        let _ = fs::remove_dir_all(bench_cache_dir(&root));
        // Same test: `run` uses the process-global phase table.
        let rec = run(Target::Roots, &root).expect("roots");
        let names: Vec<&str> = rec.phases.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["discover", "module-map"]);
        assert!(rec.counters.iter().any(|(n, v)| n == "roots" && *v >= 1));
        for target in [Target::Read, Target::Walk, Target::WalkSeq, Target::Roots] {
            assert!(run(target, Path::new("/no/such/xtask/root")).is_err(), "{target:?}");
        }
    }
}
