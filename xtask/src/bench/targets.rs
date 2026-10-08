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
    /// Walk + parallel read + per-file parse hook ([`parse_file`]).
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
        out
    }
}

/// The parse-only hook: called once per file with its full contents, which are dropped (the
/// buffer is reused) as soon as this returns. Plug the lexer/extractor in here.
#[inline]
pub fn parse_file(src: &[u8], acc: &mut ParseAcc) {
    // Until the lexer lands this measures walk + read only.
    let _ = (src, acc);
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
struct Loaded {
    bufs: Vec<Vec<u8>>,
    files: Vec<(u32, usize, usize)>,
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

/// Reads every file in parallel, appending each job's files to one growing buffer.
fn load_files(paths: &PathList) -> (ReadStats, Loaded) {
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
        Target::Read | Target::ParseOnly | Target::Lex | Target::LexMem => {
            let (files, walk_errors, _) = walk_corpus(root, &corpus_options())?;
            let (mut stats, acc) = match target {
                Target::Read => timing::time("read", || read_files(&files, no_hook)),
                Target::ParseOnly => timing::time("read+parse", || read_files(&files, parse_file)),
                Target::Lex => timing::time("read+lex", || read_files(&files, lex_file)),
                _ => lex_mem(&files),
            };
            stats.errors += walk_errors;
            (stats, acc.counters())
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
        let (stats, _) = read_files(&paths, parse_file);
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
        for target in [Target::Read, Target::ParseOnly, Target::Walk, Target::WalkSeq, Target::WalkGit, Target::Lex, Target::LexMem] {
            let rec = run(target, &root).expect("run");
            // The corpus has no ignore files or hidden directories, so all walks agree.
            assert_eq!(rec.files, paths.len() as u64, "{target:?}");
            let read = matches!(target, Target::Read | Target::ParseOnly | Target::Lex | Target::LexMem);
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
                Target::Walk | Target::WalkSeq | Target::WalkGit => &["walk"],
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
            } else if !matches!(target, Target::Walk | Target::WalkGit) {
                assert!(rec.counters.is_empty(), "{target:?}");
            }
        }
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
