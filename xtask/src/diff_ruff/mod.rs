//! `cargo xtask diff-ruff <dir>`: compare extracted facts against ruff's parser.
//!
//! Pipeline per file (in parallel over files, sorted for deterministic examples):
//! read -> [`LineIndex`] -> oracle facts ([`oracle::RuffOracle`]) -> candidate facts ->
//! [`diff::diff_file`] -> per-worker [`Report`], merged in path order.
//!
//! The candidate (our extractor) is wired in by S1.5 by implementing [`FactSource`] (plus its
//! per-thread [`Extractor`]) and passing it as `candidate` to [`compare`]; until
//! then the command reports ruff-side totals, and `--self-check` diffs ruff against itself to
//! exercise the whole pipeline.

pub mod diff;
pub mod facts;
mod hash;
pub mod oracle;
pub mod report;
mod walk;

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::Context;
use rayon::prelude::*;

use diff::{Category, DiffScratch, Sink};
use facts::{Extractor, FactSource, FileFacts, LineIndex};
use oracle::RuffOracle;
use report::Report;

#[derive(clap::Args)]
pub struct Args {
    /// Directory (or single file) to scan for .py / .pyi files.
    pub dir: PathBuf,
    /// Examples shown per mismatch category.
    #[arg(long, default_value_t = 10)]
    pub examples: usize,
    /// Print every ruff fact per file instead of the report (sequential).
    #[arg(long)]
    pub dump: bool,
    /// Diff ruff against itself to exercise the full pipeline (expects zero mismatches).
    #[arg(long)]
    pub self_check: bool,
}

/// Entry point: exit 0 = no mismatches (or nothing compared), 1 = mismatches; errors are
/// fatal (exit 2, reported by `main`).
pub fn run(args: &Args) -> anyhow::Result<ExitCode> {
    let start = Instant::now();
    let walked = walk::walk(&args.dir).with_context(|| format!("cannot scan {}", args.dir.display()))?;
    for e in &walked.errors {
        eprintln!("warning: {e}");
    }
    let root = args.dir.as_path();
    if args.dump {
        return dump(root, &walked.files, &RuffOracle);
    }
    let walk_time = start.elapsed();
    let candidate: Option<&dyn FactSource> = if args.self_check { Some(&RuffOracle) } else { None };
    let work_start = Instant::now();
    let report = compare(root, &walked.files, &RuffOracle, candidate, args.examples);
    let work_time = work_start.elapsed();
    let mut out = BufWriter::new(io::stdout().lock());
    let written = report
        .write(&mut out, &root.display().to_string(), candidate.map(FactSource::name), walk_time, work_time)
        .and_then(|()| out.flush());
    ignore_broken_pipe(written)?;
    Ok(if report.mismatches() > 0 { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

fn ignore_broken_pipe(r: io::Result<()>) -> io::Result<()> {
    match r {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        r => r,
    }
}

/// Path shown in reports: relative to the scan root when possible.
fn rel<'a>(root: &Path, path: &'a Path) -> &'a Path {
    match path.strip_prefix(root) {
        Ok(p) if !p.as_os_str().is_empty() => p,
        _ => path,
    }
}

/// Per-thread state: reusable buffers plus a partial report.
struct Worker<'a> {
    root: &'a Path,
    oracle: Box<dyn Extractor + 'a>,
    candidate: Option<Box<dyn Extractor + 'a>>,
    src: Vec<u8>,
    lines: LineIndex,
    o: FileFacts,
    c: FileFacts,
    scratch: DiffScratch,
    report: Report,
}

impl<'a> Worker<'a> {
    fn new(root: &'a Path, oracle: &'a dyn FactSource, candidate: Option<&'a dyn FactSource>, max_examples: usize) -> Self {
        Worker {
            root,
            oracle: oracle.extractor(),
            candidate: candidate.map(FactSource::extractor),
            src: Vec::new(),
            lines: LineIndex::default(),
            o: FileFacts::default(),
            c: FileFacts::default(),
            scratch: DiffScratch::default(),
            report: Report::new(max_examples),
        }
    }

    fn file(&mut self, path: &Path) {
        let shown = rel(self.root, path);
        self.report.files_seen += 1;
        self.src.clear();
        if let Err(e) = File::open(path).and_then(|mut f| f.read_to_end(&mut self.src)) {
            self.report.read_error(shown, &e);
            return;
        }
        self.report.bytes += self.src.len() as u64;
        self.lines.reset(&self.src);
        self.o.clear();
        if let Err(skip) = self.oracle.extract(path, &self.src, &self.lines, &mut self.o) {
            self.report.oracle_skip(shown, skip.kind, skip.line, &skip.message);
            return;
        }
        self.report.oracle.add(&self.o);
        let Some(candidate) = self.candidate.as_mut() else { return };
        self.c.clear();
        match candidate.extract(path, &self.src, &self.lines, &mut self.c) {
            Ok(()) => {
                self.report.candidate.add(&self.c);
                diff::diff_file(&self.o, &self.c, &mut self.scratch, &mut self.report.sink(shown));
            }
            Err(skip) => self.report.sink(shown).mismatch(
                Category::CandidateSkip,
                skip.line,
                format_args!("{}: {}", skip.kind.as_str(), skip.message),
            ),
        }
    }
}

/// Runs the comparison over `files` (sorted) in parallel. Examples are the first
/// `max_examples` per category in `files` order regardless of scheduling.
pub fn compare(
    root: &Path,
    files: &[PathBuf],
    oracle: &dyn FactSource,
    candidate: Option<&dyn FactSource>,
    max_examples: usize,
) -> Report {
    files
        .par_iter()
        .fold(
            || Worker::new(root, oracle, candidate, max_examples),
            |mut w, p| {
                w.file(p);
                w
            },
        )
        .map(|w| w.report)
        .reduce(|| Report::new(max_examples), Report::merge)
}

/// `--dump`: every fact of every file, sequentially, to stdout.
fn dump(root: &Path, files: &[PathBuf], source: &dyn FactSource) -> anyhow::Result<ExitCode> {
    let mut out = BufWriter::new(io::stdout().lock());
    let mut src = Vec::new();
    let mut lines = LineIndex::default();
    let mut facts = FileFacts::default();
    let mut extractor = source.extractor();
    let result = (|| -> io::Result<()> {
        for path in files {
            writeln!(out, "== {}", rel(root, path).display())?;
            src.clear();
            if let Err(e) = File::open(path).and_then(|mut f| f.read_to_end(&mut src)) {
                writeln!(out, "error {e}")?;
                continue;
            }
            lines.reset(&src);
            facts.clear();
            match extractor.extract(path, &src, &lines, &mut facts) {
                Ok(()) => facts.dump(&mut out)?,
                Err(s) => writeln!(out, "skip {} line={}: {}", s.kind.as_str(), s.line, s.message)?,
            }
        }
        out.flush()
    })();
    ignore_broken_pipe(result)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::facts::{Skip, SkipKind};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use super::*;

    /// Candidate that drops every call named `drop_me` and fails on files containing `FAIL`.
    /// Counts extractor creations to check that state is per worker, not per file.
    #[derive(Default)]
    struct Lossy {
        extractors: AtomicUsize,
    }

    impl FactSource for Lossy {
        fn name(&self) -> &'static str {
            "lossy"
        }

        fn extractor(&self) -> Box<dyn Extractor + '_> {
            self.extractors.fetch_add(1, Ordering::Relaxed);
            Box::new(LossyExtractor)
        }
    }

    struct LossyExtractor;

    impl Extractor for LossyExtractor {
        fn extract(&mut self, path: &Path, src: &[u8], lines: &LineIndex, out: &mut FileFacts) -> Result<(), Skip> {
            if src.windows(4).any(|w| w == b"FAIL") {
                return Err(Skip { kind: SkipKind::Syntax, line: 1, message: "boom".to_owned() });
            }
            RuffOracle.extract(path, src, lines, out)?;
            let text = &out.text;
            out.calls.retain(|c| &text[c.callee.off as usize..(c.callee.off + c.callee.len) as usize] != b"drop_me");
            Ok(())
        }
    }

    fn corpus() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/corpus")
    }

    #[test]
    fn self_check_on_corpus_is_clean() {
        let root = corpus();
        let w = walk::walk(&root).unwrap();
        assert!(w.files.len() >= 28);
        let r = compare(&root, &w.files, &RuffOracle, Some(&RuffOracle), 5);
        assert_eq!(r.mismatches(), 0);
        assert_eq!(r.files_seen, w.files.len() as u64);
        assert_eq!(r.oracle, r.candidate);
        assert!(r.oracle.defs > 0 && r.oracle.imports > 0 && r.oracle.calls > 0);
    }

    #[test]
    fn detects_candidate_losses_with_file_and_line() {
        let dir = std::env::temp_dir().join(format!("pymapper-diff-ruff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.py"), "def f():\n    drop_me()\n    keep()\n").unwrap();
        std::fs::write(dir.join("b.py"), "x = 1  # FAIL\n").unwrap();
        std::fs::write(dir.join("c.py"), "def broken(:\n").unwrap();
        let w = walk::walk(&dir).unwrap();
        let lossy = Lossy::default();
        let r = compare(&dir, &w.files, &RuffOracle, Some(&lossy), 5);
        assert!(lossy.extractors.load(Ordering::Relaxed) >= 1);
        assert_eq!(r.files_seen, 3);
        assert_eq!(r.counts[Category::CallMissing as usize], 1);
        assert_eq!(r.counts[Category::CandidateSkip as usize], 1);
        assert_eq!(r.mismatches(), 2);
        assert_eq!(r.oracle_skipped(), 1);
        let ex = &r.examples[Category::CallMissing as usize][0];
        assert_eq!((ex.path.as_str(), ex.line, ex.msg.as_str()), ("a.py", 2, "`drop_me` in `f`"));
        let ex = &r.examples[Category::CandidateSkip as usize][0];
        assert_eq!((ex.path.as_str(), ex.msg.as_str()), ("b.py", "syntax: boom"));
        assert_eq!(r.oracle_skip_examples[0].path, "c.py");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
