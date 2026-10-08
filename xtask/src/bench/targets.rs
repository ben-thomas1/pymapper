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

use super::rusage;
use super::walk::{PathStore, walk_python};

/// A named benchmark target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, ValueEnum)]
pub enum Target {
    /// Walk + parallel read of every file: the I/O floor.
    Read,
    /// Walk + parallel read + per-file parse hook ([`parse_file`]).
    ParseOnly,
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
fn lex_mem(paths: &PathStore) -> (ReadStats, ParseAcc) {
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
fn load_files(paths: &PathStore) -> (ReadStats, Loaded) {
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
    let mut paths = PathStore::default();
    let walk_errors = timing::time("walk", || walk_python(root, &mut paths))
        .with_context(|| format!("walking {}", root.display()))?;
    let (stats, acc) = match target {
        Target::Read => timing::time("read", || read_files(&paths, no_hook)),
        Target::ParseOnly => timing::time("read+parse", || read_files(&paths, parse_file)),
        Target::Lex => timing::time("read+lex", || read_files(&paths, lex_file)),
        Target::LexMem => lex_mem(&paths),
    };
    drop(paths);
    let total_s = t0.elapsed().as_secs_f64();
    let phases = timing::take().into_iter().map(|p| (p.name.to_owned(), p.elapsed.as_secs_f64())).collect();
    Ok(RunRecord {
        files: stats.files,
        bytes: stats.bytes,
        errors: walk_errors + stats.errors,
        threads: rayon::current_num_threads() as u64,
        phases,
        total_s,
        peak_rss_bytes: rusage::peak_rss_bytes().context("getrusage")?,
        counters: acc.counters(),
    })
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
pub fn read_files<F: Fn(&[u8], &mut ParseAcc) + Sync>(paths: &PathStore, hook: F) -> (ReadStats, ParseAcc) {
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
        let mut paths = PathStore::default();
        walk_python(&dir, &mut paths).unwrap();
        paths.push_join(&dir, std::ffi::OsStr::new("vanished.py")).unwrap();
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
        for target in [Target::Read, Target::ParseOnly, Target::Lex, Target::LexMem] {
            let rec = run(target, &root).expect("run");
            assert_eq!(rec.files, paths.len() as u64, "{target:?}");
            assert_eq!(rec.bytes, bytes, "{target:?}");
            assert_eq!(rec.errors, 0);
            assert!(rec.threads >= 1);
            assert!(rec.peak_rss_bytes > 0);
            let names: Vec<&str> = rec.phases.iter().map(|(n, _)| n.as_str()).collect();
            let expected: &[&str] = match target {
                Target::Read => &["walk", "read"],
                Target::ParseOnly => &["walk", "read+parse"],
                Target::Lex => &["walk", "read+lex"],
                Target::LexMem => &["walk", "load", "lex"],
            };
            assert_eq!(names, expected, "{target:?}");
            let phase_sum: f64 = rec.phases.iter().map(|(_, s)| s).sum();
            assert!(phase_sum <= rec.total_s + 1e-6, "phases {phase_sum} > total {}", rec.total_s);
            let counter = |name: &str| rec.counters.iter().find(|(n, _)| n == name).map(|&(_, v)| v);
            if matches!(target, Target::Lex | Target::LexMem) {
                assert!(counter("tokens").unwrap() > rec.files);
                assert_eq!(counter("error_tokens"), Some(0));
                assert!(counter("lex_MBps_core").is_some());
            } else {
                assert!(rec.counters.is_empty(), "{target:?}");
            }
        }
        // Same test: `run` uses the process-global phase table.
        assert!(run(Target::Read, Path::new("/no/such/xtask/root")).is_err());
    }
}
