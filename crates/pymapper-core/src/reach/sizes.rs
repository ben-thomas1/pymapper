//! Per-node reach sizes: how many symbols and source lines each node reaches (itself
//! included), approximately via HyperLogLog sketches propagated over the condensed DAG, or
//! exactly (gated: see [`SizeMode::Exact`]).

use super::bits::BitSet;
use super::exact;
use super::hll;
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
    /// [`hll::MAX_PRECISION`]), merged sinks first. Sets of up to `2^p / 4` elements are kept
    /// exactly (sparse), larger ones as `2^p` one-byte registers. O(V + E + total lines) plus
    /// `O(2^p)` per DAG edge into a dense sketch; memory: the live sketches (see the module
    /// docs of [`crate::reach`]).
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
    /// Components whose symbol or line set ended dense (one count per set).
    pub dense_sets: u64,
    /// Most sketches held at once (each component holds two: symbols and lines).
    pub peak_live: u64,
    /// Most dense register arrays allocated at once (live plus pooled).
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

/// A set of element keys: exact (sorted unique keys) while small, else HLL registers.
enum Sketch {
    Sparse(Box<[u32]>),
    Dense(Box<[u8]>),
}

fn empty() -> Sketch {
    Sketch::Sparse(Box::default())
}

/// Sketch storage of a propagation: the live sketch pairs (symbols, lines) of components that
/// still have unprocessed predecessors, and a pool of free register arrays.
struct Store {
    p: u8,
    m: usize,
    /// Largest sparse set; one more key and it turns dense (sparse bytes > dense bytes).
    threshold: usize,
    /// Live sketch pairs; `free` lists unused slots.
    slots: Vec<[Sketch; 2]>,
    free: Vec<u32>,
    pool: Vec<Box<[u8]>>,
    dense_allocated: u64,
    live: u64,
    sparse_bytes: u64,
    stats: SizeStats,
}

impl Store {
    /// A zeroed register array (pooled when possible).
    fn take_dense(&mut self) -> Box<[u8]> {
        match self.pool.pop() {
            Some(mut b) => {
                b.fill(0);
                b
            }
            None => {
                self.dense_allocated += 1;
                self.stats.peak_dense = self.stats.peak_dense.max(self.dense_allocated);
                vec![0u8; self.m].into_boxed_slice()
            }
        }
    }

    fn release(&mut self, s: Sketch) {
        match s {
            Sketch::Dense(b) => self.pool.push(b),
            Sketch::Sparse(k) => self.sparse_bytes -= (k.len() * 4) as u64,
        }
    }

    /// Stores a pair; returns its slot.
    fn put(&mut self, pair: [Sketch; 2]) -> u32 {
        for s in &pair {
            if let Sketch::Sparse(k) = s {
                self.sparse_bytes += (k.len() * 4) as u64;
            }
        }
        self.live += 2;
        self.stats.peak_live = self.stats.peak_live.max(self.live);
        self.stats.peak_sparse_bytes = self.stats.peak_sparse_bytes.max(self.sparse_bytes);
        match self.free.pop() {
            Some(h) => {
                self.slots[h as usize] = pair;
                h
            }
            None => {
                self.slots.push(pair);
                (self.slots.len() - 1) as u32
            }
        }
    }

    /// Frees slot `h`, recycling its register arrays.
    fn drop_slot(&mut self, h: u32) {
        let [a, b] = std::mem::replace(&mut self.slots[h as usize], [empty(), empty()]);
        self.release(a);
        self.release(b);
        self.live -= 2;
        self.free.push(h);
    }
}

/// The set being built for one component: keys while small, registers once large.
struct Acc {
    keys: Vec<u32>,
    dense: Option<Box<[u8]>>,
}

impl Acc {
    fn to_dense(&mut self, st: &mut Store) {
        if self.dense.is_none() {
            let mut regs = st.take_dense();
            for &k in &self.keys {
                hll::insert(&mut regs, st.p, k);
            }
            self.keys.clear();
            self.dense = Some(regs);
        }
    }

    /// Sorts and deduplicates the keys; goes dense when still above the threshold.
    fn compact(&mut self, st: &mut Store) {
        self.keys.sort_unstable();
        self.keys.dedup();
        if self.keys.len() > st.threshold {
            self.to_dense(st);
        }
    }

    #[inline]
    fn push(&mut self, st: &mut Store, key: u32) {
        match &mut self.dense {
            Some(regs) => hll::insert(regs, st.p, key),
            None => {
                self.keys.push(key);
                if self.keys.len() >= 4 * st.threshold {
                    self.compact(st);
                }
            }
        }
    }

    fn add_keys(&mut self, st: &mut Store, keys: &[u32]) {
        match &mut self.dense {
            Some(regs) => {
                for &k in keys {
                    hll::insert(regs, st.p, k);
                }
            }
            None => {
                self.keys.extend_from_slice(keys);
                if self.keys.len() >= 4 * st.threshold {
                    self.compact(st);
                }
            }
        }
    }

    fn add_dense(&mut self, st: &mut Store, regs: &[u8]) {
        self.to_dense(st);
        if let Some(d) = &mut self.dense {
            hll::merge(d, regs);
        }
    }

    /// The finished set's size (exact or estimated) and, when `keep`, the set itself.
    fn finish(&mut self, st: &mut Store, keep: bool) -> (u32, Option<Sketch>) {
        if self.dense.is_none() {
            self.compact(st);
        }
        match self.dense.take() {
            Some(regs) => {
                st.stats.dense_sets += 1;
                let est = hll::estimate(&regs, st.p).round().min(f64::from(u32::MAX)) as u32;
                if keep {
                    (est, Some(Sketch::Dense(regs)))
                } else {
                    st.pool.push(regs);
                    (est, None)
                }
            }
            None => {
                let len = self.keys.len() as u32;
                let set = keep.then(|| Sketch::Sparse(self.keys.as_slice().into()));
                self.keys.clear();
                (len, set)
            }
        }
    }
}

/// Sketch propagation over the condensed DAG, sinks first: a component's set is its members'
/// elements plus its successors' sets. A component's sketches live until its last predecessor
/// has merged them (reference count = DAG in-degree); the last consumer of a dense sketch takes
/// its registers instead of copying them.
fn propagate(cd: &Condensed, w: &Weights, p: u8, out: &mut [[u32; 2]]) -> SizeStats {
    let dag = cd.dag();
    let scc = cd.scc();
    let k = cd.count();
    let m = hll::registers(p);
    let mut st = Store {
        p,
        m,
        threshold: (m / 4).max(1),
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
    let mut acc = Acc { keys: Vec::new(), dense: None };
    for c in cd.reverse_topo() {
        let succ = dag.succ(c);
        let keep = pending[c as usize] > 0;
        let mut pair = [empty(), empty()];
        for which in 0..2 {
            // Start from a dense successor: steal it if this is its last consumer.
            let mut base = None;
            for &s in succ {
                if let Sketch::Dense(_) = st.slots[slot[s as usize] as usize][which] {
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
                    if let Sketch::Dense(regs) = std::mem::replace(&mut st.slots[h][which], empty()) {
                        acc.dense = Some(regs);
                    }
                } else {
                    let mut regs = st.take_dense();
                    if let Sketch::Dense(src) = &st.slots[h][which] {
                        regs.copy_from_slice(src);
                    }
                    acc.dense = Some(regs);
                }
            }
            for &s in succ {
                if Some(s) == base {
                    continue;
                }
                let h = slot[s as usize] as usize;
                // Move the sketch out while merging so `st` can be borrowed mutably.
                let set = std::mem::replace(&mut st.slots[h][which], empty());
                match &set {
                    Sketch::Sparse(keys) => acc.add_keys(&mut st, keys),
                    Sketch::Dense(regs) => acc.add_dense(&mut st, regs),
                }
                st.slots[h][which] = set;
            }
            for &v in scc.members(c) {
                if which == 0 {
                    if w.symbol.contains(v) {
                        acc.push(&mut st, hll::key(v, 0));
                    }
                } else {
                    for i in 0..w.lines[v as usize] {
                        acc.push(&mut st, hll::key(v, i));
                    }
                }
            }
            let (size, set) = acc.finish(&mut st, keep);
            out[c as usize][which] = size;
            if let Some(set) = set {
                pair[which] = set;
            }
        }
        for &s in succ {
            pending[s as usize] -= 1;
            if pending[s as usize] == 0 {
                st.drop_slot(slot[s as usize]);
                slot[s as usize] = u32::MAX;
            }
        }
        if keep {
            slot[c as usize] = st.put(pair);
        }
    }
    st.stats
}
