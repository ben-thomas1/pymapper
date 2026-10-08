//! Parallel streaming extraction and the deterministic merge.

use std::fs::File;
use std::io::{self, Read};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use super::{FileId, Index, Inputs, build};
use crate::arena::PerThread;
use crate::extract::facts::{FactStore, FileFacts, TableBases, extract_with};
use crate::extract::{MetricsScratch, MetricsTable, MetricsVisitor};
use crate::intern::Interner;
use crate::timing;

/// A file that could not be read. It is loaded as an empty module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadError {
    pub file: FileId,
    pub kind: io::ErrorKind,
}

/// Counters and phase times of one [`load`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadStats {
    pub files: u64,
    /// Source bytes read.
    pub bytes: u64,
    /// Lines of all files.
    pub lines: u64,
    /// Read + extract, wall time.
    pub extract: Duration,
    /// Interner merge + remap + concatenation, wall time.
    pub merge: Duration,
    /// Hierarchy + lookup tables, wall time.
    pub build: Duration,
}

/// Per-thread state: a reused read buffer, the fact store (rows are moved out per chunk, the
/// allocations and extractor scratch stay), the worker interner and metrics state.
#[derive(Default)]
struct Worker {
    buf: Vec<u8>,
    store: FactStore,
    interner: Interner,
    scratch: MetricsScratch,
    metrics: MetricsTable,
}

/// The facts of consecutive files `first..first + files.len()`, with `Sym`s of worker
/// `worker`'s interner until remapped.
struct Chunk {
    worker: usize,
    store: FactStore,
    metrics: MetricsTable,
    files: Vec<FileFacts>,
    errors: Vec<ReadError>,
    bytes: u64,
}

/// Files per chunk: small enough to balance uneven file sizes over the threads, large enough
/// to amortise the per-chunk store.
fn chunk_size(files: usize) -> usize {
    let threads = rayon::current_num_threads().max(1);
    (files / (threads * 16)).clamp(1, 64)
}

/// Loads `inputs`: reads and extracts every file in parallel (sources are dropped right
/// after extraction), merges into global tables and builds the hierarchy. Never fails:
/// unreadable files become [`ReadError`]s and load as empty modules. Deterministic (see the
/// module docs). Runs on the current rayon pool.
pub fn load(inputs: Inputs) -> Index {
    let n = inputs.len();
    let t0 = Instant::now();
    let workers = PerThread::new(Worker::default);
    let size = chunk_size(n);
    let mut chunks: Vec<Chunk> = timing::time("load.extract", || {
        (0..n.div_ceil(size))
            .into_par_iter()
            .map(|c| {
                let range = c * size..((c + 1) * size).min(n);
                workers.with(|wi, w| w.chunk(wi, &inputs, range))
            })
            .collect()
    });
    let extract = t0.elapsed();

    let t1 = Instant::now();
    let (strings, facts, metrics, files, read_errors, bytes) = timing::time("load.merge", || {
        let interners: Vec<Interner> = workers.into_vec().into_iter().map(|w| w.interner).collect();
        let (strings, remaps) = Interner::merge_all(interners);
        chunks.par_iter_mut().for_each(|c| {
            let r = &remaps[c.worker];
            if r.is_identity() {
                return;
            }
            for ff in &c.files {
                c.store.map_syms(ff, |s| r.get(s));
            }
        });
        drop(remaps);
        let (facts, metrics, files, errors, bytes) = concat(chunks);
        (strings, facts, metrics, files, errors, bytes)
    });
    let merge = t1.elapsed();

    let lines = files.iter().map(|ff| u64::from(ff.lines)).sum();
    let stats = LoadStats { files: n as u64, bytes, lines, extract, merge, build: Duration::ZERO };
    let t2 = Instant::now();
    let mut index = timing::time("load.build", || build::build(inputs, strings, facts, metrics, files, read_errors, stats));
    index.stats.build = t2.elapsed();
    index
}

impl Worker {
    fn chunk(&mut self, wi: usize, inputs: &Inputs, range: std::ops::Range<usize>) -> Chunk {
        debug_assert!(self.store.defs.is_empty() && self.metrics.is_empty());
        let mut files = Vec::with_capacity(range.len());
        let mut errors = Vec::new();
        let mut bytes = 0u64;
        let mut buf = std::mem::take(&mut self.buf);
        for f in range {
            buf.clear();
            let res = File::open(inputs.paths.get(f)).and_then(|mut h| h.read_to_end(&mut buf));
            if let Err(e) = res {
                errors.push(ReadError { file: f as FileId, kind: e.kind() });
                buf.clear();
            }
            bytes += buf.len() as u64;
            let src = &buf[..];
            let m = MetricsVisitor::new(src, &mut self.scratch, &mut self.metrics);
            files.push(extract_with(src, &mut self.store, &mut self.interner, m).0);
        }
        // Keep the buffer for the next chunk unless one huge file grew it.
        if buf.capacity() <= 16 << 20 {
            self.buf = buf;
        }
        // Move the rows out into exact-size tables; the worker keeps its allocations.
        let mut store = FactStore::new();
        store.append(&mut self.store);
        let mut metrics = MetricsTable::new();
        metrics.append(&self.metrics);
        self.metrics.clear();
        debug_assert_eq!(metrics.len(), store.defs.len() as usize - files.len());
        Chunk { worker: wi, store, metrics, files, errors, bytes }
    }
}

fn add_bases(a: &mut TableBases, b: &TableBases) {
    a.defs += b.defs;
    a.decorators += b.decorators;
    a.bases += b.bases;
    a.params += b.params;
    a.imports += b.imports;
    a.calls += b.calls;
    a.bindings += b.bindings;
    a.decls += b.decls;
    a.names += b.names;
    a.exports += b.exports;
    a.evidence += b.evidence;
    a.diags += b.diags;
    a.suppressions += b.suppressions;
}

type Merged = (FactStore, MetricsTable, Vec<FileFacts>, Vec<ReadError>, u64);

/// Concatenates the chunks in order into exactly-sized global tables, freeing each chunk as
/// soon as it is copied (peak: one copy of the facts plus one chunk).
fn concat(chunks: Vec<Chunk>) -> Merged {
    let mut total = TableBases::default();
    let (mut nfiles, mut nmetrics, mut nerrors) = (0usize, 0usize, 0usize);
    for c in &chunks {
        add_bases(&mut total, &c.store.bases());
        nfiles += c.files.len();
        nmetrics += c.metrics.len();
        nerrors += c.errors.len();
    }
    let mut facts = FactStore::new();
    facts.reserve_exact(&total);
    let mut metrics = MetricsTable::new();
    reserve_metrics(&mut metrics, nmetrics);
    let mut files = Vec::with_capacity(nfiles);
    let mut errors = Vec::with_capacity(nerrors);
    let mut bytes = 0;
    for mut c in chunks {
        let b = facts.append(&mut c.store);
        for mut ff in c.files {
            ff.shift(&b);
            files.push(ff);
        }
        metrics.append(&c.metrics);
        errors.extend_from_slice(&c.errors);
        bytes += c.bytes;
    }
    (facts, metrics, files, errors, bytes)
}

fn reserve_metrics(m: &mut MetricsTable, n: usize) {
    m.cyclomatic.reserve_exact(n);
    m.cognitive.reserve_exact(n);
    m.max_nesting.reserve_exact(n);
    m.loc.reserve_exact(n);
    m.distinct_operators.reserve_exact(n);
    m.distinct_operands.reserve_exact(n);
    m.operators.reserve_exact(n);
    m.operands.reserve_exact(n);
    m.params.reserve_exact(n);
}
