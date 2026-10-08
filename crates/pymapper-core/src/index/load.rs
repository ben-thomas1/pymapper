//! Parallel streaming extraction and the deterministic merge.

use std::fs::File;
use std::io::{self, Read};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use super::{FileId, Index, Inputs, build};
use crate::arena::PerThread;
use crate::cache::{Lookup, Probe, SegRemap, Stamp};
use crate::extract::facts::{FactStore, FileFacts, TableBases, extract_with};
use crate::extract::{MetricsScratch, MetricsTable, MetricsVisitor};
use crate::intern::{Interner, hash_bytes};
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
    /// Source bytes (read, or the cached size of files loaded from the cache).
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
    /// Local ids of the cache segment being copied -> ids of `interner`.
    remap: SegRemap,
}

/// The facts of consecutive files `first..first + files.len()`, with `Sym`s of worker
/// `worker`'s interner until remapped.
struct Chunk {
    worker: usize,
    store: FactStore,
    metrics: MetricsTable,
    files: Vec<FileFacts>,
    /// Per file, when loading with a cache.
    stamps: Vec<Stamp>,
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
///
/// Memory: a finished chunk is appended to the global tables as soon as every earlier chunk
/// is, so only out-of-order chunks (about one per thread) wait in memory. The global tables
/// are reserved once from an early estimate; capacity that is never written is never touched
/// (no RSS) and is released at the end.
pub fn load(inputs: Inputs) -> Index {
    load_with(inputs, None).0
}

/// [`load`] with cached facts: files the cache `lookup` has are copied from it instead of
/// read and extracted. Also returns the [`Stamp`] of every file (empty without a cache), for
/// writing the cache back. Same result as [`load`] for the same file contents.
pub(crate) fn load_with(inputs: Inputs, lookup: Option<&mut Lookup>) -> (Index, Vec<Stamp>) {
    let n = inputs.len();
    let t0 = Instant::now();
    let workers = PerThread::new(Worker::default);
    let size = chunk_size(n);
    let nchunks = n.div_ceil(size);
    let slots: Vec<Mutex<Option<Chunk>>> = (0..nchunks).map(|_| Mutex::new(None)).collect();
    let sink = Mutex::new(Sink { chunks: nchunks, ..Sink::default() });
    // Chunks are handed out in FileId order from a counter (not split recursively the rayon
    // way), so they finish roughly in order and few wait for an earlier one.
    let next = AtomicUsize::new(0);
    let cache = lookup.as_deref();
    timing::time("load.extract", || {
        (0..rayon::current_num_threads().min(nchunks)).into_par_iter().for_each(|_| {
            loop {
                let c = next.fetch_add(1, Ordering::Relaxed);
                if c >= nchunks {
                    break;
                }
                let range = c * size..((c + 1) * size).min(n);
                let chunk = workers.with(|wi, w| w.chunk(wi, &inputs, range, cache));
                *lock(&slots[c]) = Some(chunk);
                drain(&sink, &slots);
            }
        });
    });
    let mut sink = sink.into_inner().unwrap_or_else(PoisonError::into_inner);
    // Chunks stranded by a lost try_lock race.
    sink.drain_ready(&slots);
    assert_eq!(sink.next, nchunks, "every chunk is merged");
    let extract = t0.elapsed();

    let t1 = Instant::now();
    let strings = timing::time("load.merge", || {
        let interners: Vec<Interner> = workers.into_vec().into_iter().map(|w| w.interner).collect();
        let (strings, remaps) = Interner::merge_all(interners);
        for (f, ff) in sink.files.iter().enumerate() {
            let r = &remaps[sink.worker[f] as usize];
            if !r.is_identity() {
                sink.facts.map_syms(ff, |s| r.get(s));
            }
        }
        sink.facts.shrink_to_fit();
        shrink_metrics(&mut sink.metrics);
        sink.files.shrink_to_fit();
        drop(std::mem::take(&mut sink.worker));
        strings
    });
    let merge = t1.elapsed();

    let Sink { facts, metrics, files, errors, bytes, stamps, .. } = sink;
    let lines = files.iter().map(|ff| u64::from(ff.lines)).sum();
    let stats = LoadStats { files: n as u64, bytes, lines, extract, merge, build: Duration::ZERO };
    let t2 = Instant::now();
    let mut index = timing::time("load.build", || build::build(inputs, strings, facts, metrics, files, errors, stats));
    index.stats.build = t2.elapsed();
    (index, stamps)
}

/// Locks a mutex, ignoring poisoning (a panicking worker aborts the load anyway).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Appends every ready chunk in order, unless another thread is already doing it. Loops so
/// that a chunk published while the previous drainer was releasing the lock is not left
/// behind (any that still are get merged after the parallel phase).
fn drain(sink: &Mutex<Sink>, slots: &[Mutex<Option<Chunk>>]) {
    loop {
        let mut s = match sink.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        s.drain_ready(slots);
        let next = s.next;
        drop(s);
        if next >= slots.len() || lock(&slots[next]).is_none() {
            return;
        }
    }
}

impl Worker {
    fn chunk(&mut self, wi: usize, inputs: &Inputs, range: std::ops::Range<usize>, cache: Option<&Lookup>) -> Chunk {
        debug_assert!(self.store.defs.is_empty() && self.metrics.is_empty());
        let mut files = Vec::with_capacity(range.len());
        let mut stamps = Vec::with_capacity(if cache.is_some() { range.len() } else { 0 });
        let mut errors = Vec::new();
        let mut bytes = 0u64;
        let mut buf = std::mem::take(&mut self.buf);
        for f in range {
            let path = inputs.paths.get(f);
            let g = inputs.group[f];
            let probe = cache.map_or(Probe::Miss, |c| c.probe(g, path));
            if let (Probe::Hit(i, st), Some(c)) = (probe, cache)
                && let Some(ff) = c.copy(g, i, false, &mut self.store, &mut self.metrics, &mut self.interner, &mut self.remap)
            {
                files.push(ff);
                bytes += st.size;
                stamps.push(st);
                c.done(g);
                continue;
            }
            buf.clear();
            let mut mtime = 0;
            let res = File::open(path).and_then(|mut h| {
                if cache.is_some() {
                    mtime = crate::cache::mtime_ns(&h.metadata()?);
                }
                h.read_to_end(&mut buf)
            });
            let read_ok = res.is_ok();
            if let Err(e) = res {
                errors.push(ReadError { file: f as FileId, kind: e.kind() });
                buf.clear();
            }
            bytes += buf.len() as u64;
            let src = &buf[..];
            if let Some(c) = cache {
                let st = Stamp { size: src.len() as u64, mtime_ns: mtime, hash: hash_bytes(src) };
                stamps.push(st);
                if let Probe::Verify(i, old) = probe
                    && read_ok
                    && old.size == st.size
                    && old.hash == st.hash
                    && let Some(ff) = c.copy(g, i, true, &mut self.store, &mut self.metrics, &mut self.interner, &mut self.remap)
                {
                    files.push(ff);
                    c.done(g);
                    continue;
                }
            }
            let m = MetricsVisitor::new(src, &mut self.scratch, &mut self.metrics);
            files.push(extract_with(src, &mut self.store, &mut self.interner, m).0);
            if let Some(c) = cache {
                c.done(g);
            }
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
        Chunk { worker: wi, store, metrics, files, stamps, errors, bytes }
    }
}

/// The global tables being filled, in chunk order.
#[derive(Default)]
struct Sink {
    /// Number of chunks.
    chunks: usize,
    /// Next chunk to append.
    next: usize,
    /// The tables were reserved for the estimated total (see [`Sink::reserve_estimate`]).
    reserved: bool,
    facts: FactStore,
    metrics: MetricsTable,
    files: Vec<FileFacts>,
    /// Worker (interner) of each file's `Sym`s, until remapped.
    worker: Vec<u32>,
    stamps: Vec<Stamp>,
    errors: Vec<ReadError>,
    bytes: u64,
}

impl Sink {
    /// Appends chunks `next..` while they are ready.
    fn drain_ready(&mut self, slots: &[Mutex<Option<Chunk>>]) {
        while self.next < slots.len() {
            let Some(c) = lock(&slots[self.next]).take() else { break };
            self.absorb(c);
            self.next += 1;
            if !self.reserved && self.next * 16 >= self.chunks {
                self.reserve_estimate();
            }
        }
    }

    /// Reserves every table for 1.5x its extrapolated final length, once about 1/16 of the
    /// chunks are in. Without it the tables grow by doubling, and allocators keep the freed
    /// smaller blocks resident (macOS keeps blocks up to 8 MiB): about one more copy of the
    /// facts in RSS. Overshoot costs only untouched virtual memory (released by the final
    /// `shrink_to_fit`); undershoot falls back to doubling from a large base.
    fn reserve_estimate(&mut self) {
        self.reserved = true;
        let (done, all) = (self.next as u64, self.chunks as u64);
        let more = |len: u32| -> u32 {
            let est = u64::from(len) * all * 3 / (2 * done.max(1));
            u32::try_from(est.saturating_sub(u64::from(len))).unwrap_or(u32::MAX - len)
        };
        let b = self.facts.bases();
        let extra = TableBases {
            defs: more(b.defs),
            decorators: more(b.decorators),
            bases: more(b.bases),
            params: more(b.params),
            imports: more(b.imports),
            calls: more(b.calls),
            bindings: more(b.bindings),
            decls: more(b.decls),
            names: more(b.names),
            exports: more(b.exports),
            evidence: more(b.evidence),
            diags: more(b.diags),
            suppressions: more(b.suppressions),
        };
        self.facts.reserve_exact(&extra);
        let m = more(self.metrics.len() as u32) as usize;
        reserve_metrics(&mut self.metrics, m);
        let rest = self.chunks.saturating_sub(self.next);
        let per_chunk = self.files.len() / self.next.max(1) + 1;
        self.files.reserve_exact(rest * per_chunk);
        self.worker.reserve_exact(rest * per_chunk);
    }

    fn absorb(&mut self, mut c: Chunk) {
        let b = self.facts.append(&mut c.store);
        for mut ff in c.files {
            ff.shift(&b);
            self.files.push(ff);
            self.worker.push(c.worker as u32);
        }
        self.stamps.extend_from_slice(&c.stamps);
        self.metrics.append(&c.metrics);
        self.errors.extend_from_slice(&c.errors);
        self.bytes += c.bytes;
    }
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

fn shrink_metrics(m: &mut MetricsTable) {
    m.cyclomatic.shrink_to_fit();
    m.cognitive.shrink_to_fit();
    m.max_nesting.shrink_to_fit();
    m.loc.shrink_to_fit();
    m.distinct_operators.shrink_to_fit();
    m.distinct_operands.shrink_to_fit();
    m.operators.shrink_to_fit();
    m.operands.shrink_to_fit();
    m.params.shrink_to_fit();
}
