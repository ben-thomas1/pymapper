//! The `parse-only` target: the front end in the real loader's shape.
//!
//! Each rayon worker owns one [`Worker`] ([`PerThread`]): a read buffer, a [`FactStore`], an
//! [`Interner`] and the metrics state. Per file: read into the reused buffer -> lex + parse +
//! facts + metrics in one pass ([`extract_with`], interned names) -> keep the [`FileFacts`]
//! header; the source is dropped (the buffer is overwritten by the next file). Everything
//! extracted stays alive to the end of the run, so peak RSS includes the retained facts.

use std::fs::File;
use std::io::Read;
use std::time::Instant;

use pymapper_core::arena::PerThread;
use pymapper_core::extract::facts::{FactStore, FileFacts, SpanStrs, extract_with};
use pymapper_core::extract::{self, MetricsScratch, MetricsTable, MetricsVisitor, NoopVisitor};
use pymapper_core::intern::Interner;
use pymapper_core::timing;
use pymapper_core::walk::PathList;
use rayon::prelude::*;

use super::targets::{ReadStats, load_files};

/// What `extract-mem` runs per file (environment variable `XTASK_EXTRACT_STAGES`), to measure
/// the cost of each part of the front end. `parse-only` always runs [`Stages::All`].
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum Stages {
    /// Facts (interned) + metrics: the real front end.
    #[default]
    All,
    /// Lexer + parser with a no-op visitor.
    Parse,
    /// Parser + metrics visitor.
    Metrics,
    /// Parser + facts with span strings (no interning).
    FactsSpans,
    /// Parser + facts with interned strings.
    Facts,
}

impl Stages {
    /// Parses `XTASK_EXTRACT_STAGES` (unset: [`Stages::All`]).
    fn from_env() -> anyhow::Result<Stages> {
        let Ok(v) = std::env::var("XTASK_EXTRACT_STAGES") else { return Ok(Stages::All) };
        Ok(match v.as_str() {
            "all" => Stages::All,
            "parse" => Stages::Parse,
            "metrics" => Stages::Metrics,
            "facts-spans" => Stages::FactsSpans,
            "facts" => Stages::Facts,
            _ => anyhow::bail!("XTASK_EXTRACT_STAGES: expected all, parse, metrics, facts-spans or facts, got {v:?}"),
        })
    }
}

/// Per-thread state of the `parse-only` target.
#[derive(Default)]
struct Worker {
    stages: Stages,
    spans: SpanStrs,
    buf: Vec<u8>,
    store: FactStore,
    interner: Interner,
    scratch: MetricsScratch,
    metrics: MetricsTable,
    files: Vec<FileFacts>,
    stats: ReadStats,
    /// Time inside `extract_with` (lex + parse + facts + metrics).
    nanos: u64,
}

/// Counters of a `parse-only` run, in report order.
pub fn parse_only(paths: &PathList) -> (ReadStats, Vec<(String, u64)>) {
    let workers = timing::time("read+parse", || {
        let workers = PerThread::new(Worker::default);
        (0..paths.len()).into_par_iter().for_each(|i| workers.with(|_, w| w.file(paths.get(i))));
        workers.into_vec()
    });
    summarise(&workers)
}

/// `extract-mem`: loads every file (phase `load`), then extracts from memory (phase `parse`)
/// with the same per-thread workers as `parse-only`. Peak RSS includes the loaded corpus.
pub fn extract_mem(paths: &PathList) -> anyhow::Result<(ReadStats, Vec<(String, u64)>)> {
    let stages = Stages::from_env()?;
    let (read, loaded) = timing::time("load", || load_files(paths));
    let workers = timing::time("parse", || {
        let workers = PerThread::new(|| Worker { stages, ..Worker::default() });
        loaded.files.par_iter().for_each(|&(b, s, e)| {
            workers.with(|_, w| w.extract(&loaded.bufs[b as usize][s..e]));
        });
        workers.into_vec()
    });
    let (mut stats, counters) = summarise(&workers);
    stats.errors += read.errors;
    Ok((stats, counters))
}

impl Worker {
    fn file(&mut self, path: &std::path::Path) {
        let mut buf = std::mem::take(&mut self.buf);
        buf.clear();
        if File::open(path).and_then(|mut f| f.read_to_end(&mut buf)).is_ok() {
            self.extract(&buf);
        } else {
            self.stats.errors += 1;
        }
        self.buf = buf;
    }

    #[inline]
    fn extract(&mut self, src: &[u8]) {
        let t0 = Instant::now();
        let ff = match self.stages {
            Stages::All => {
                let m = MetricsVisitor::new(src, &mut self.scratch, &mut self.metrics);
                Some(extract_with(src, &mut self.store, &mut self.interner, m).0)
            }
            Stages::Parse => {
                extract::parse(src, NoopVisitor);
                None
            }
            Stages::Metrics => {
                extract::parse(src, MetricsVisitor::new(src, &mut self.scratch, &mut self.metrics));
                None
            }
            Stages::FactsSpans => Some(extract_with(src, &mut self.store, &mut self.spans, NoopVisitor).0),
            Stages::Facts => Some(extract_with(src, &mut self.store, &mut self.interner, NoopVisitor).0),
        };
        self.nanos += t0.elapsed().as_nanos() as u64;
        self.files.extend(ff);
        self.stats.files += 1;
        self.stats.bytes += src.len() as u64;
    }
}

/// Totals over all workers: read stats plus the counters reported for `parse-only`.
fn summarise(workers: &[Worker]) -> (ReadStats, Vec<(String, u64)>) {
    let mut stats = ReadStats::default();
    let (mut nanos, mut loc, mut defs, mut calls, mut diags) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut facts, mut metric_rows, mut intern) = (0u64, 0u64, 0u64);
    for w in workers {
        stats.files += w.stats.files;
        stats.bytes += w.stats.bytes;
        stats.errors += w.stats.errors;
        nanos += w.nanos;
        for ff in &w.files {
            loc += u64::from(ff.lines);
        }
        defs += u64::from(w.store.defs.len()) - w.files.len() as u64;
        calls += u64::from(w.store.calls.len());
        diags += u64::from(w.store.diags.len());
        facts += (w.store.row_bytes() + w.files.len() * size_of::<FileFacts>()) as u64;
        metric_rows += w.metrics.len() as u64;
        intern += w.interner.heap_bytes() as u64;
    }
    let metrics = metric_rows * MetricsTable::ROW_BYTES as u64;
    let per_kloc = |b: u64| (b * 1000).checked_div(loc).unwrap_or(0);
    let counters = vec![
        ("loc".to_owned(), loc),
        ("defs".to_owned(), defs),
        ("calls".to_owned(), calls),
        ("diagnostics".to_owned(), diags),
        ("parse_cpu_ms".to_owned(), nanos / 1_000_000),
        // MB (10^6 bytes) per second of extraction time: throughput of one core.
        ("parse_MBps_core".to_owned(), (stats.bytes * 1000).checked_div(nanos).unwrap_or(0)),
        // Retained bytes (rows, no spare capacity) per 1000 LOC, i.e. milli-bytes per LOC.
        ("facts_B_per_kLOC".to_owned(), per_kloc(facts)),
        ("metrics_B_per_kLOC".to_owned(), per_kloc(metrics)),
        ("facts_kb".to_owned(), facts / 1000),
        ("metrics_kb".to_owned(), metrics / 1000),
        ("intern_kb".to_owned(), intern / 1000),
    ];
    (stats, counters)
}
