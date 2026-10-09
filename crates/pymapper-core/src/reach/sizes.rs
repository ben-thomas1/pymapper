//! Per-node reach sizes: how many symbols and source lines each node reaches (itself
//! included), approximately via HyperLogLog sketches propagated over the condensed DAG, or
//! exactly (gated: see [`SizeMode::Exact`]).

use super::bits::BitSet;
use super::exact;
use super::hll::{self, HIST};
use crate::extract::facts::NO_PARENT;
use crate::graph::Condensed;
use crate::index::Index;

/// Default HLL precision: 1,024 registers (1 KB per dense sketch), 3.25% relative standard
/// error.
pub const DEFAULT_PRECISION: u8 = 10;

/// How reach sizes are computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeMode {
    /// HyperLogLog sketches at this precision ([`hll::MIN_PRECISION`]`..=`
    /// [`hll::MAX_PRECISION`]), merged sinks first. A reach set of at most `2^p / 4` nodes and
    /// `4 * 2^p` lines is kept exactly (sorted node ids), a larger one as two sketches of `2^p`
    /// one-byte registers (symbols, lines). Costs and memory: see the module docs of
    /// [`crate::reach`].
    Approx(u8),
    /// Exact sets: one depth-first search over the DAG per component, in parallel.
    /// O(sum over components of the DAG part they reach): up to O(k (k + E)) for `k`
    /// components, quadratic on graphs where most nodes reach a large common part. Memory
    /// O(k) per thread. Gate it (`analysis.exact-reachability`).
    Exact,
}

impl Default for SizeMode {
    fn default() -> Self {
        SizeMode::Approx(DEFAULT_PRECISION)
    }
}

/// What each node contributes to reach sizes: whether it counts as a symbol, and its own
/// source lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Weights {
    /// Nodes counted as symbols.
    pub symbol: BitSet,
    /// Own lines per node.
    pub lines: Vec<u32>,
}

impl Weights {
    /// Every node is a symbol with `lines` lines.
    pub fn uniform(n: u32, lines: u32) -> Weights {
        let mut symbol = BitSet::new(n);
        for v in 0..n {
            symbol.insert(v);
        }
        Weights { symbol, lines: vec![lines; n as usize] }
    }

    /// Weights of an index: classes and functions are symbols; a node's lines are its
    /// exclusive lines, the lines of its span not inside a nested def (for a package/module
    /// node, the file's lines outside top-level defs). The exclusive lines of a set of nodes
    /// add up to the distinct source lines they cover: a reached class contributes its body
    /// outside methods, each reached method its own lines. Distributions and namespace
    /// packages have no lines. O(defs).
    pub fn of_index(ix: &Index) -> Weights {
        let n = ix.node_count();
        let mut symbol = BitSet::new(n);
        let mut lines = vec![0u32; n as usize];
        let defs = &ix.facts().defs;
        for f in 0..ix.file_count() {
            let ff = ix.file_facts(f);
            let m = ix.file_node(f);
            let base = ff.defs.start as usize;
            lines[m as usize] = ff.lines;
            for d in 1..ff.defs.len {
                let row = base + d as usize;
                let v = (m + d) as usize;
                let span = (defs.end_line[row].saturating_sub(defs.start_line[row])) + 1;
                lines[v] += span;
                symbol.insert(m + d);
                let p = defs.parent[row];
                if p != NO_PARENT {
                    let pv = (m + p) as usize;
                    lines[pv] = lines[pv].saturating_sub(span);
                }
            }
        }
        Weights { symbol, lines }
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.symbol.heap_bytes() + self.lines.capacity() * 4
    }
}

/// Counters of a sketch propagation (all zero in exact mode).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SizeStats {
    /// Components whose reach set ended dense (estimated).
    pub dense_sets: u64,
    /// Most reach sets held at once (sparse or dense).
    pub peak_live: u64,
    /// Most dense buffers allocated at once (live plus pooled), `2 (2^p + 136)` bytes each.
    pub peak_dense: u64,
    /// Peak bytes of live sparse sets.
    pub peak_sparse_bytes: u64,
}

/// Per node: the number of symbols and of source lines reachable from it, itself included
/// (estimates in [`SizeMode::Approx`]). Nodes of one strongly connected component share
/// their values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReachSizes {
    symbols: Vec<u32>,
    lines: Vec<u32>,
    stats: SizeStats,
}

impl ReachSizes {
    /// Symbols reachable from `v`.
    #[inline]
    pub fn symbols(&self, v: u32) -> u32 {
        self.symbols[v as usize]
    }

    /// Source lines reachable from `v` ("LOC reached").
    #[inline]
    pub fn lines(&self, v: u32) -> u32 {
        self.lines[v as usize]
    }

    pub fn stats(&self) -> &SizeStats {
        &self.stats
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        (self.symbols.capacity() + self.lines.capacity()) * 4
    }
}

/// Reach sizes of every node of the graph condensed into `cd`, with `w` over its nodes.
///
/// # Panics
/// If `w` does not cover the graph's nodes, or the precision is out of range.
pub fn reach_sizes(cd: &Condensed, w: &Weights, mode: SizeMode) -> ReachSizes {
    let n = cd.scc().comps().len();
    assert!(w.symbol.len() as usize == n && w.lines.len() == n, "weights cover {} nodes, graph has {n}", w.lines.len());
    let k = cd.count() as usize;
    let mut comp = vec![[0u32; 2]; k];
    let stats = match mode {
        SizeMode::Exact => {
            exact::component_sizes(cd, w, &mut comp);
            SizeStats::default()
        }
        SizeMode::Approx(p) => {
            assert!((hll::MIN_PRECISION..=hll::MAX_PRECISION).contains(&p), "HLL precision {p} out of range");
            propagate(cd, w, p, &mut comp)
        }
    };
    let mut symbols = vec![0u32; n];
    let mut lines = vec![0u32; n];
    for (v, &c) in cd.scc().comps().iter().enumerate() {
        symbols[v] = comp[c as usize][0];
        lines[v] = comp[c as usize][1];
    }
    ReachSizes { symbols, lines, stats }
}

/// The reach set of a component: exact while small (sorted unique node ids), else two HLL
/// sketches (symbol keys and line keys) in one buffer.
enum Sketch {
    Sparse(Box<[u32]>),
    Dense(Box<[u8]>),
}

fn empty() -> Sketch {
    Sketch::Sparse(Box::default())
}

/// Bytes of a register histogram stored after the registers of a dense sketch.
const HIST_BYTES: usize = 4 * HIST;

/// Layout of a dense buffer: the symbol sketch (`m` registers, then its histogram as `HIST`
/// little-endian `u32`s), then the line sketch in the same shape.
#[derive(Clone, Copy)]
struct Layout {
    p: u8,
    m: usize,
}

impl Layout {
    /// Bytes of one sketch (registers + histogram).
    #[inline]
    fn part(self) -> usize {
        self.m + HIST_BYTES
    }

    /// Registers of sketch `which` (0 = symbols, 1 = lines).
    #[inline]
    fn regs(self, which: usize) -> std::ops::Range<usize> {
        which * self.part()..which * self.part() + self.m
    }
}

/// Sketch storage of a propagation: the live sets of components that still have unprocessed
/// predecessors, and a pool of free dense buffers.
struct Store {
    lay: Layout,
    /// A sparse set holds at most this many nodes...
    max_nodes: usize,
    /// ...and at most this many lines (bounds the keys inserted when it meets a dense set).
    max_lines: u64,
    /// Live sets; `free` lists unused slots.
    slots: Vec<Sketch>,
    free: Vec<u32>,
    pool: Vec<Box<[u8]>>,
    dense_allocated: u64,
    live: u64,
    sparse_bytes: u64,
    stats: SizeStats,
}

impl Store {
    /// A zeroed dense buffer (pooled when possible); histograms are set by [`Acc`].
    fn take_dense(&mut self) -> Box<[u8]> {
        match self.pool.pop() {
            Some(mut b) => {
                b.fill(0);
                b
            }
            None => {
                self.dense_allocated += 1;
                self.stats.peak_dense = self.stats.peak_dense.max(self.dense_allocated);
                vec![0u8; 2 * self.lay.part()].into_boxed_slice()
            }
        }
    }

    /// Stores a set; returns its slot.
    fn put(&mut self, set: Sketch) -> u32 {
        if let Sketch::Sparse(k) = &set {
            self.sparse_bytes += (k.len() * 4) as u64;
        }
        self.live += 1;
        self.stats.peak_live = self.stats.peak_live.max(self.live);
        self.stats.peak_sparse_bytes = self.stats.peak_sparse_bytes.max(self.sparse_bytes);
        match self.free.pop() {
            Some(h) => {
                self.slots[h as usize] = set;
                h
            }
            None => {
                self.slots.push(set);
                (self.slots.len() - 1) as u32
            }
        }
    }

    /// Frees slot `h`, recycling its dense buffer.
    fn drop_slot(&mut self, h: u32) {
        match std::mem::replace(&mut self.slots[h as usize], empty()) {
            Sketch::Dense(b) => self.pool.push(b),
            Sketch::Sparse(k) => self.sparse_bytes -= (k.len() * 4) as u64,
        }
        self.live -= 1;
        self.free.push(h);
    }
}

/// Adds `key` to registers `regs`, keeping their histogram `hist` in sync.
#[inline]
fn insert_tracked(regs: &mut [u8], hist: &mut [u32; HIST], p: u8, key: u32) {
    let (idx, rank) = hll::slot(key, p);
    let old = regs[idx];
    if rank > old {
        regs[idx] = rank;
        hist[usize::from(old)] -= 1;
        hist[usize::from(rank)] += 1;
    }
}

/// `dst = max(dst, src)` register-wise, keeping `dst`'s histogram in sync. Merged sketches
/// usually change few registers, so blocks of 16 registers are first compared as a whole
/// (vectorised) and only blocks with a change are updated one by one.
fn merge_tracked(dst: &mut [u8], src: &[u8], hist: &mut [u32; HIST]) {
    debug_assert_eq!(dst.len() % 16, 0);
    for (d, s) in dst.as_chunks_mut::<16>().0.iter_mut().zip(src.as_chunks::<16>().0) {
        let mut any = 0u8;
        for j in 0..16 {
            any |= u8::from(s[j] > d[j]);
        }
        if any != 0 {
            for j in 0..16 {
                let (old, new) = (d[j], s[j]);
                if new > old {
                    d[j] = new;
                    hist[usize::from(old)] -= 1;
                    hist[usize::from(new)] += 1;
                }
            }
        }
    }
}

fn read_hist(buf: &[u8], at: usize) -> [u32; HIST] {
    let mut hist = [0u32; HIST];
    for (i, h) in hist.iter_mut().enumerate() {
        let o = at + 4 * i;
        *h = u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    }
    hist
}

fn write_hist(buf: &mut [u8], at: usize, hist: &[u32; HIST]) {
    for (i, h) in hist.iter().enumerate() {
        let o = at + 4 * i;
        buf[o..o + 4].copy_from_slice(&h.to_le_bytes());
    }
}

/// Adds node `v`'s keys (its symbol key, one key per own line) to a dense buffer.
#[inline]
fn insert_node(buf: &mut [u8], hist: &mut [[u32; HIST]; 2], lay: Layout, w: &Weights, v: u32) {
    if w.symbol.contains(v) {
        insert_tracked(&mut buf[lay.regs(0)], &mut hist[0], lay.p, hll::key(v, 0));
    }
    let lines = &mut buf[lay.regs(1)];
    for i in 0..w.lines[v as usize] {
        insert_tracked(lines, &mut hist[1], lay.p, hll::key(v, i));
    }
}

/// `out = a ∪ b` for sorted unique `a` and `b`; returns the symbols and lines of the nodes
/// only in `b` (what the union adds to `a`).
fn union_weighted(a: &[u32], b: &[u32], out: &mut Vec<u32>, w: &Weights) -> (u32, u64) {
    out.clear();
    out.reserve(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    let (mut syms, mut lines) = (0u32, 0u64);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        if y < x {
            syms += u32::from(w.symbol.contains(y));
            lines += u64::from(w.lines[y as usize]);
        }
        out.push(x.min(y));
        i += usize::from(x <= y);
        j += usize::from(y <= x);
    }
    out.extend_from_slice(&a[i..]);
    for &y in &b[j..] {
        syms += u32::from(w.symbol.contains(y));
        lines += u64::from(w.lines[y as usize]);
    }
    out.extend_from_slice(&b[j..]);
    (syms, lines)
}

/// The set being built for one component: sorted node ids with exact totals while small,
/// dense sketches once large. Register histograms are kept in sync on every insert and merge,
/// so an estimate costs O(32) instead of a pass over the registers.
struct Acc<'w> {
    w: &'w Weights,
    /// Sorted unique node ids while sparse.
    nodes: Vec<u32>,
    /// Merge output, swapped with `nodes`.
    tmp: Vec<u32>,
    /// Exact symbols and lines of `nodes`.
    syms: u32,
    lines: u64,
    dense: Option<Box<[u8]>>,
    hist: [[u32; HIST]; 2],
}

impl Acc<'_> {
    fn make_dense(&mut self, st: &mut Store) {
        if self.dense.is_some() {
            return;
        }
        let lay = st.lay;
        let mut buf = st.take_dense();
        self.hist = [[0; HIST]; 2];
        self.hist[0][0] = lay.m as u32;
        self.hist[1][0] = lay.m as u32;
        for &v in &self.nodes {
            insert_node(&mut buf, &mut self.hist, lay, self.w, v);
        }
        self.nodes.clear();
        self.dense = Some(buf);
    }

    /// Starts from the dense buffer `buf` (stolen or copied, histograms in it).
    fn adopt(&mut self, lay: Layout, buf: Box<[u8]>) {
        self.hist = [read_hist(&buf, lay.m), read_hist(&buf, lay.part() + lay.m)];
        self.dense = Some(buf);
    }

    /// Adds sorted unique node ids: a linear merge while sparse (going dense past either
    /// limit), key inserts once dense.
    fn add_nodes(&mut self, st: &mut Store, nodes: &[u32]) {
        if let Some(buf) = &mut self.dense {
            for &v in nodes {
                insert_node(buf, &mut self.hist, st.lay, self.w, v);
            }
            return;
        }
        let (syms, lines) = if self.nodes.is_empty() {
            self.nodes.extend_from_slice(nodes);
            let mut t = (0u32, 0u64);
            for &v in nodes {
                t.0 += u32::from(self.w.symbol.contains(v));
                t.1 += u64::from(self.w.lines[v as usize]);
            }
            t
        } else {
            let t = union_weighted(&self.nodes, nodes, &mut self.tmp, self.w);
            std::mem::swap(&mut self.nodes, &mut self.tmp);
            t
        };
        self.syms += syms;
        self.lines += lines;
        if self.nodes.len() > st.max_nodes || self.lines > st.max_lines {
            self.make_dense(st);
        }
    }

    /// Adds a dense set.
    fn merge_dense(&mut self, st: &mut Store, src: &[u8]) {
        self.make_dense(st);
        let lay = st.lay;
        if let Some(d) = &mut self.dense {
            for which in 0..2 {
                let r = lay.regs(which);
                merge_tracked(&mut d[r.clone()], &src[r], &mut self.hist[which]);
            }
        }
    }

    /// The finished set's `[symbols, lines]` (exact or estimated) and, when `keep`, the set.
    fn finish(&mut self, st: &mut Store, keep: bool) -> ([u32; 2], Option<Sketch>) {
        let lay = st.lay;
        let out = match self.dense.take() {
            Some(mut buf) => {
                st.stats.dense_sets += 1;
                let mut est = [0u32; 2];
                for (which, e) in est.iter_mut().enumerate() {
                    debug_assert_eq!(self.hist[which], hll::histogram(&buf[lay.regs(which)], lay.p), "histogram");
                    *e = hll::estimate_histogram(&self.hist[which], lay.p).round().min(f64::from(u32::MAX)) as u32;
                }
                if keep {
                    write_hist(&mut buf, lay.m, &self.hist[0]);
                    write_hist(&mut buf, lay.part() + lay.m, &self.hist[1]);
                    (est, Some(Sketch::Dense(buf)))
                } else {
                    st.pool.push(buf);
                    (est, None)
                }
            }
            None => {
                let sizes = [self.syms, self.lines.min(u64::from(u32::MAX)) as u32];
                (sizes, keep.then(|| Sketch::Sparse(self.nodes.as_slice().into())))
            }
        };
        self.nodes.clear();
        self.syms = 0;
        self.lines = 0;
        out
    }
}

/// Sketch propagation over the condensed DAG, sinks first: a component's set is its members
/// plus its successors' sets. A component's set lives until its last predecessor has merged
/// it (reference count = DAG in-degree); the last consumer of a dense set takes its buffer
/// instead of copying it.
fn propagate(cd: &Condensed, w: &Weights, p: u8, out: &mut [[u32; 2]]) -> SizeStats {
    let dag = cd.dag();
    let scc = cd.scc();
    let k = cd.count();
    let m = hll::registers(p);
    let mut st = Store {
        lay: Layout { p, m },
        max_nodes: (m / 4).max(1),
        max_lines: 4 * m as u64,
        slots: Vec::new(),
        free: Vec::new(),
        pool: Vec::new(),
        dense_allocated: 0,
        live: 0,
        sparse_bytes: 0,
        stats: SizeStats::default(),
    };
    let mut pending: Vec<u32> = (0..k).map(|c| dag.reverse().degree(c)).collect();
    let mut slot = vec![u32::MAX; k as usize];
    let mut acc = Acc { w, nodes: Vec::new(), tmp: Vec::new(), syms: 0, lines: 0, dense: None, hist: [[0; HIST]; 2] };
    for c in cd.reverse_topo() {
        let succ = dag.succ(c);
        let keep = pending[c as usize] > 0;
        // Start from a dense successor: steal it if this is its last consumer.
        let mut base = None;
        for &s in succ {
            if let Sketch::Dense(_) = st.slots[slot[s as usize] as usize] {
                if pending[s as usize] == 1 {
                    base = Some(s);
                    break;
                }
                base.get_or_insert(s);
            }
        }
        if let Some(b) = base {
            let h = slot[b as usize] as usize;
            if pending[b as usize] == 1 {
                if let Sketch::Dense(buf) = std::mem::replace(&mut st.slots[h], empty()) {
                    acc.adopt(st.lay, buf);
                }
            } else {
                let mut buf = st.take_dense();
                if let Sketch::Dense(src) = &st.slots[h] {
                    buf.copy_from_slice(src);
                }
                acc.adopt(st.lay, buf);
            }
        }
        for &s in succ {
            if Some(s) == base {
                continue;
            }
            let h = slot[s as usize] as usize;
            // Move the set out while merging so `st` can be borrowed mutably.
            let set = std::mem::replace(&mut st.slots[h], empty());
            match &set {
                Sketch::Sparse(nodes) => acc.add_nodes(&mut st, nodes),
                Sketch::Dense(buf) => acc.merge_dense(&mut st, buf),
            }
            st.slots[h] = set;
        }
        acc.add_nodes(&mut st, scc.members(c));
        let (sizes, set) = acc.finish(&mut st, keep);
        out[c as usize] = sizes;
        for &s in succ {
            pending[s as usize] -= 1;
            if pending[s as usize] == 0 {
                st.drop_slot(slot[s as usize]);
                slot[s as usize] = u32::MAX;
            }
        }
        if let Some(set) = set {
            slot[c as usize] = st.put(set);
        }
    }
    st.stats
}
