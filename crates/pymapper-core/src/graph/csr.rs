//! Compressed sparse row adjacency and the forward + reverse [`Graph`] built from edge lists.

use std::ops::Range;

/// One direction of a graph in compressed sparse row form: the neighbours of `v` are
/// `adj[start[v]..start[v + 1]]`, sorted ascending without duplicates. `lab` is empty or holds
/// one `u8` label per entry of `adj`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Csr {
    start: Vec<u32>,
    adj: Vec<u32>,
    lab: Vec<u8>,
}

impl Csr {
    /// Number of nodes.
    #[inline]
    pub fn node_count(&self) -> u32 {
        (self.start.len().max(1) - 1) as u32
    }

    /// Number of edges.
    #[inline]
    pub fn edge_count(&self) -> u32 {
        self.adj.len() as u32
    }

    /// Entry range of `v` in [`Csr::targets`] / [`Csr::labels`].
    #[inline]
    pub fn range(&self, v: u32) -> Range<usize> {
        self.start[v as usize] as usize..self.start[v as usize + 1] as usize
    }

    /// Neighbours of `v`, ascending.
    #[inline]
    pub fn neighbours(&self, v: u32) -> &[u32] {
        &self.adj[self.range(v)]
    }

    /// Labels of the neighbours of `v` (empty if the graph has no labels).
    #[inline]
    pub fn neighbour_labels(&self, v: u32) -> &[u8] {
        if self.lab.is_empty() { &[] } else { &self.lab[self.range(v)] }
    }

    /// Out-degree of `v`.
    #[inline]
    pub fn degree(&self, v: u32) -> u32 {
        self.start[v as usize + 1] - self.start[v as usize]
    }

    /// Whether the edge `v -> w` exists (binary search in `v`'s row).
    #[inline]
    pub fn contains(&self, v: u32, w: u32) -> bool {
        self.neighbours(v).binary_search(&w).is_ok()
    }

    /// The label of `v -> w` (`None` if there is no such edge or no labels).
    #[inline]
    pub fn label(&self, v: u32, w: u32) -> Option<u8> {
        let r = self.range(v);
        let i = self.adj[r.clone()].binary_search(&w).ok()?;
        self.lab.get(r.start + i).copied()
    }

    /// Row offsets (`node_count + 1` entries).
    #[inline]
    pub fn offsets(&self) -> &[u32] {
        &self.start
    }

    /// Every neighbour entry, rows concatenated in node order.
    #[inline]
    pub fn targets(&self) -> &[u32] {
        &self.adj
    }

    /// Labels aligned with [`Csr::targets`] (empty if the graph has no labels).
    #[inline]
    pub fn labels(&self) -> &[u8] {
        &self.lab
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.start.capacity() * 4 + self.adj.capacity() * 4 + self.lab.capacity()
    }

    /// The transpose of `self` into `out` (buffers reused): rows come out sorted because the
    /// source rows are scanned in node order. Duplicates stay duplicates.
    fn transpose_into(&self, out: &mut Csr) {
        let n = self.node_count() as usize;
        let labelled = !self.lab.is_empty();
        out.start.clear();
        out.start.resize(n + 1, 0);
        for &w in &self.adj {
            out.start[w as usize + 1] += 1;
        }
        for i in 0..n {
            out.start[i + 1] += out.start[i];
        }
        out.adj.clear();
        out.adj.resize(self.adj.len(), 0);
        out.lab.clear();
        if labelled {
            out.lab.resize(self.adj.len(), 0);
        }
        // `start[w]` is used as the write cursor of row `w`, then shifted back below.
        for v in 0..n {
            for i in self.range(v as u32) {
                let w = self.adj[i] as usize;
                let at = out.start[w] as usize;
                out.adj[at] = v as u32;
                if labelled {
                    out.lab[at] = self.lab[i];
                }
                out.start[w] += 1;
            }
        }
        for i in (1..=n).rev() {
            out.start[i] = out.start[i - 1];
        }
        out.start[0] = 0;
    }

    /// Removes adjacent duplicates in every row (rows must be sorted), folding their labels
    /// with `merge` (in edge order). Compacts in place.
    fn dedup_sorted_rows(&mut self, merge: fn(u8, u8) -> u8) {
        let n = self.node_count() as usize;
        let labelled = !self.lab.is_empty();
        let mut w = 0usize;
        let mut row_begin = 0usize;
        for v in 0..n {
            let row_end = self.start[v + 1] as usize;
            let out_begin = w;
            for i in row_begin..row_end {
                let t = self.adj[i];
                if w > out_begin && self.adj[w - 1] == t {
                    if labelled {
                        self.lab[w - 1] = merge(self.lab[w - 1], self.lab[i]);
                    }
                    continue;
                }
                self.adj[w] = t;
                if labelled {
                    self.lab[w] = self.lab[i];
                }
                w += 1;
            }
            row_begin = row_end;
            self.start[v + 1] = w as u32;
        }
        self.adj.truncate(w);
        self.lab.truncate(if labelled { w } else { 0 });
    }

    fn shrink(&mut self) {
        self.start.shrink_to_fit();
        self.adj.shrink_to_fit();
        self.lab.shrink_to_fit();
    }
}

/// A directed graph over nodes `0..n` with forward and reverse [`Csr`] adjacency. Parallel
/// edges are merged (the minimum label wins); self loops are kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Graph {
    fwd: Csr,
    rev: Csr,
    labelled: bool,
}

impl Graph {
    /// Builds a graph over `0..n` from an edge list without labels. `edges` is called twice
    /// (count, then scatter) and must yield the same edges both times; nothing is materialised
    /// besides the two CSRs. Panics if an endpoint is `>= n` or there are `>= 2^32` edges.
    pub fn build<I>(n: u32, edges: impl Fn() -> I) -> Graph
    where
        I: Iterator<Item = (u32, u32)>,
    {
        build(n, false, std::cmp::min, || edges().map(|(s, d)| (s, d, 0)))
    }

    /// Like [`Graph::build`] with a `u8` label per edge; parallel edges keep the minimum label.
    pub fn build_labelled<I>(n: u32, edges: impl Fn() -> I) -> Graph
    where
        I: Iterator<Item = (u32, u32, u8)>,
    {
        build(n, true, std::cmp::min, edges)
    }

    /// Like [`Graph::build_labelled`], parallel edges folding their labels with `merge`
    /// (commutative and associative: the fold order follows the scatter order).
    pub fn build_labelled_with<I>(n: u32, merge: fn(u8, u8) -> u8, edges: impl Fn() -> I) -> Graph
    where
        I: Iterator<Item = (u32, u32, u8)>,
    {
        build(n, true, merge, edges)
    }

    /// Builds from a slice of `(src, dst)` pairs (tests and small graphs).
    pub fn from_edges(n: u32, edges: &[(u32, u32)]) -> Graph {
        Graph::build(n, || edges.iter().copied())
    }

    #[inline]
    pub fn node_count(&self) -> u32 {
        self.fwd.node_count()
    }

    /// Number of distinct edges (self loops included).
    #[inline]
    pub fn edge_count(&self) -> u32 {
        self.fwd.edge_count()
    }

    /// Successors of `v`, ascending.
    #[inline]
    pub fn succ(&self, v: u32) -> &[u32] {
        self.fwd.neighbours(v)
    }

    /// Predecessors of `v`, ascending.
    #[inline]
    pub fn pred(&self, v: u32) -> &[u32] {
        self.rev.neighbours(v)
    }

    /// Forward adjacency.
    #[inline]
    pub fn forward(&self) -> &Csr {
        &self.fwd
    }

    /// Reverse adjacency.
    #[inline]
    pub fn reverse(&self) -> &Csr {
        &self.rev
    }

    /// Whether the edge `v -> w` exists.
    #[inline]
    pub fn has_edge(&self, v: u32, w: u32) -> bool {
        self.fwd.contains(v, w)
    }

    /// Label of `v -> w` (`None` without such an edge or without labels).
    #[inline]
    pub fn label(&self, v: u32, w: u32) -> Option<u8> {
        self.fwd.label(v, w)
    }

    /// Whether the graph carries edge labels.
    #[inline]
    pub fn is_labelled(&self) -> bool {
        self.labelled
    }

    /// Heap bytes (capacity) of both directions.
    pub fn heap_bytes(&self) -> usize {
        self.fwd.heap_bytes() + self.rev.heap_bytes()
    }
}

/// Counting-sort build: scatter by source (rows unsorted), transpose (reverse rows sorted by
/// source, duplicates adjacent), dedup, transpose back (forward rows sorted by target). O(n + m)
/// time, no comparison sort, peak memory two CSRs.
fn build<I>(n: u32, labelled: bool, merge: fn(u8, u8) -> u8, edges: impl Fn() -> I) -> Graph
where
    I: Iterator<Item = (u32, u32, u8)>,
{
    let nu = n as usize;
    let mut tmp = Csr { start: vec![0u32; nu + 1], adj: Vec::new(), lab: Vec::new() };
    let mut m = 0usize;
    for (s, d, _) in edges() {
        assert!(s < n && d < n, "edge ({s}, {d}) out of range for {n} nodes");
        tmp.start[s as usize + 1] += 1;
        m += 1;
    }
    assert!(m < u32::MAX as usize, "too many edges for u32 offsets");
    for i in 0..nu {
        tmp.start[i + 1] += tmp.start[i];
    }
    tmp.adj.resize(m, 0);
    if labelled {
        tmp.lab.resize(m, 0);
    }
    // Write cursors per row: reuse `start` shifted by one (start[v] is the cursor of row v).
    for (s, d, l) in edges() {
        let at = tmp.start[s as usize] as usize;
        tmp.adj[at] = d;
        if labelled {
            tmp.lab[at] = l;
        }
        tmp.start[s as usize] += 1;
    }
    for i in (1..=nu).rev() {
        tmp.start[i] = tmp.start[i - 1];
    }
    tmp.start[0] = 0;
    let mut rev = Csr::default();
    tmp.transpose_into(&mut rev);
    rev.dedup_sorted_rows(merge);
    rev.shrink();
    // Reuse the scatter buffers for the forward direction.
    rev.transpose_into(&mut tmp);
    tmp.shrink();
    Graph { fwd: tmp, rev, labelled }
}
