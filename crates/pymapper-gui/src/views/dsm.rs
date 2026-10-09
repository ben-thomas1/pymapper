//! Dependency structure matrix: modules x modules, ordered to expose layering and cycles.
//!
//! Row `r` depends on column `c` when cell `(r, c)` is marked (imports and / or calls rolled up
//! to packages and modules). The order puts dependencies first:
//!
//! 1. strongly connected components of the module graph (core Tarjan), condensed;
//! 2. components in reverse topological order (sinks first) by Kahn's algorithm, ties broken
//!    by the smallest node id (pre-order), so modules of one package stay together whenever
//!    the layering allows;
//! 3. inside a cyclic component, members in reverse Eades-Lin-Smyth order (a greedy minimum
//!    feedback arc set heuristic: sinks to the back, sources to the front, otherwise the node
//!    with the largest out-degree minus in-degree).
//!
//! A layered (acyclic) system is then strictly lower triangular. Every mark above the
//! diagonal lies inside a cyclic component's diagonal block and is a feedback dependency (a
//! cycle edge). Cost: O(V + E) for the components plus O((V + E) log V) for the orders.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use pymapper_core::graph::{Condensed, Graph};
use pymapper_core::index::{NodeId, NodeKind};
use pymapper_core::rollup::{Level, LevelScratch};

use super::graph::Weight;
use crate::codemap::CodeMap;

/// What the matrix shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DsmParams {
    pub weight: Weight,
    /// Include third-party and stdlib packages / modules (and distributions).
    pub external: bool,
    /// Only modules in this node's subtree (`None`: everything).
    pub scope: Option<NodeId>,
}

impl Default for DsmParams {
    fn default() -> DsmParams {
        DsmParams { weight: Weight::All, external: false, scope: None }
    }
}

/// One marked cell (16 bytes): row depends on column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DsmCell {
    pub row: u32,
    pub col: u32,
    pub sites: u32,
    pub imports: u32,
}

impl DsmCell {
    #[inline]
    pub fn weight(&self, w: Weight) -> u32 {
        match w {
            Weight::Sites | Weight::Pairs => self.sites,
            Weight::Imports => self.imports,
            Weight::All => self.sites.saturating_add(self.imports),
        }
    }

    /// Above the diagonal: a feedback (cycle) dependency.
    #[inline]
    pub fn is_feedback(&self) -> bool {
        self.col > self.row
    }
}

/// An ordered matrix.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dsm {
    /// Node of every row / column, in matrix order.
    pub nodes: Vec<NodeId>,
    /// Marked cells with a positive weight, sorted by (row, col); no diagonal.
    pub cells: Vec<DsmCell>,
    /// Cyclic components as `start..end` ranges of rows, ascending.
    pub blocks: Vec<(u32, u32)>,
    /// Cells above the diagonal.
    pub feedback: usize,
    pub params: DsmParams,
}

impl Dsm {
    /// Row of node `v`, if it is in the matrix (linear in the row count: called on selection
    /// changes only; see [`Dsm::row_index`] for repeated lookups).
    pub fn row_of(&self, v: NodeId) -> Option<u32> {
        self.nodes.iter().position(|&x| x == v).map(|i| i as u32)
    }

    /// `(node, row)` pairs sorted by node: binary-searchable rows.
    pub fn row_index(&self) -> Vec<(NodeId, u32)> {
        let mut ix: Vec<(NodeId, u32)> = self.nodes.iter().enumerate().map(|(r, &v)| (v, r as u32)).collect();
        ix.sort_unstable();
        ix
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        4 * self.nodes.capacity() + 16 * self.cells.capacity() + 8 * self.blocks.capacity()
    }
}

/// Builds and orders the matrix of `cm` (see the module docs).
pub fn build(cm: &CodeMap, p: DsmParams) -> Dsm {
    let ix = &cm.analysis.index;
    let n = ix.node_count();
    // Rows: packages and modules (distributions with external code), in scope.
    let (lo, hi) = p.scope.filter(|&s| s < n).map_or((0, n), |s| (s, ix.subtree(s).end));
    let mut nodes: Vec<NodeId> = Vec::new();
    let mut v = lo;
    while v < hi {
        let kind = ix.kind(v);
        let ext = cm.layout.muted(v);
        if ext && !p.external {
            v = ix.subtree(v).end;
            continue;
        }
        match kind {
            NodeKind::Package => nodes.push(v),
            NodeKind::Module => nodes.push(v),
            NodeKind::Distribution if ext => nodes.push(v),
            NodeKind::Distribution => {}
            NodeKind::Class | NodeKind::Function => {
                v = ix.subtree(v).end;
                continue;
            }
        }
        v += 1;
    }
    let mut rows = Vec::new();
    cm.analysis.rollup.level_with(ix, Level::Module, &mut LevelScratch::default(), &mut rows);
    // Local edges with a positive weight (rows are sorted by (src, dst): so are these).
    let mut edges: Vec<(u32, u32, u32, u32)> = Vec::new();
    for e in &rows {
        let w = match p.weight {
            Weight::Sites | Weight::Pairs => e.sites,
            Weight::Imports => e.imports,
            Weight::All => e.sites.saturating_add(e.imports),
        };
        if w == 0 {
            continue;
        }
        if let (Ok(a), Ok(b)) = (nodes.binary_search(&e.src), nodes.binary_search(&e.dst)) {
            edges.push((a as u32, b as u32, e.sites, e.imports));
        }
    }
    drop(rows);
    let k = nodes.len() as u32;
    let g = Graph::build(k, || edges.iter().map(|e| (e.0, e.1)));
    let order = order(&g);
    let mut pos = vec![0u32; k as usize];
    for (r, &l) in order.iter().enumerate() {
        pos[l as usize] = r as u32;
    }
    let mut cells: Vec<DsmCell> = edges.iter().map(|&(a, b, sites, imports)| DsmCell { row: pos[a as usize], col: pos[b as usize], sites, imports }).collect();
    cells.sort_unstable_by_key(|c| (c.row, c.col));
    let feedback = cells.iter().filter(|c| c.is_feedback()).count();
    let cond = Condensed::of(&g);
    let scc = cond.scc();
    let mut blocks = Vec::new();
    let mut r = 0usize;
    while r < order.len() {
        let c = scc.comp(order[r]);
        let size = scc.size(c) as usize;
        if size > 1 {
            blocks.push((r as u32, (r + size) as u32));
        }
        r += size.max(1);
    }
    let nodes = order.iter().map(|&l| nodes[l as usize]).collect();
    Dsm { nodes, cells, blocks, feedback, params: p }
}

/// The matrix order of `g`'s nodes (see the module docs): component members are contiguous.
pub fn order(g: &Graph) -> Vec<u32> {
    let cond = Condensed::of(g);
    let (scc, dag) = (cond.scc(), cond.dag());
    let k = cond.count();
    // Kahn from the sinks, smallest member first.
    let mut out_deg: Vec<u32> = (0..k).map(|c| dag.succ(c).len() as u32).collect();
    let mut heap: BinaryHeap<Reverse<(u32, u32)>> = (0..k).filter(|&c| out_deg[c as usize] == 0).map(|c| Reverse((scc.members(c)[0], c))).collect();
    let mut order = Vec::with_capacity(g.node_count() as usize);
    let mut els = Els::default();
    while let Some(Reverse((_, c))) = heap.pop() {
        let m = scc.members(c);
        if m.len() == 1 {
            order.push(m[0]);
        } else {
            els.order(g, scc.comps(), c, m, &mut order);
        }
        for &p in dag.pred(c) {
            out_deg[p as usize] -= 1;
            if out_deg[p as usize] == 0 {
                heap.push(Reverse((scc.members(p)[0], p)));
            }
        }
    }
    debug_assert_eq!(order.len(), g.node_count() as usize);
    order
}

/// Scratch of the Eades-Lin-Smyth ordering (reused across components).
#[derive(Default)]
struct Els {
    out_d: Vec<u32>,
    in_d: Vec<u32>,
    gone: Vec<bool>,
    front: Vec<u32>,
    back: Vec<u32>,
    zero: Vec<u32>,
    heap: BinaryHeap<(i64, Reverse<u32>)>,
}

impl Els {
    /// Appends the members `m` (ascending) of cyclic component `c` to `out`, dependencies
    /// first: the reverse of the ELS sequence, in which most edges point forward.
    fn order(&mut self, g: &Graph, comp: &[u32], c: u32, m: &[u32], out: &mut Vec<u32>) {
        let k = m.len();
        let local = |v: u32| m.binary_search(&v).map_or(u32::MAX, |i| i as u32);
        self.out_d.clear();
        self.in_d.clear();
        self.out_d.resize(k, 0);
        self.in_d.resize(k, 0);
        self.gone.clear();
        self.gone.resize(k, false);
        for (i, &v) in m.iter().enumerate() {
            for &w in g.succ(v) {
                if w != v && comp[w as usize] == c {
                    self.out_d[i] += 1;
                    self.in_d[local(w) as usize] += 1;
                }
            }
        }
        self.front.clear();
        self.back.clear();
        self.heap.clear();
        self.zero.clear();
        for i in 0..k as u32 {
            self.heap.push((self.delta(i), Reverse(i)));
        }
        let mut left = k;
        while left > 0 {
            // Sinks and sources first (pushed as their degrees reach zero).
            let pick = if let Some(i) = self.zero.pop() {
                if self.gone[i as usize] {
                    continue;
                }
                i
            } else {
                let Some((d, Reverse(i))) = self.heap.pop() else { break };
                if self.gone[i as usize] || d != self.delta(i) {
                    continue;
                }
                i
            };
            let i = pick as usize;
            // A sink goes to the back; anything else (source or max delta) to the front.
            if self.out_d[i] == 0 {
                self.back.push(pick);
            } else {
                self.front.push(pick);
            }
            self.gone[i] = true;
            left -= 1;
            let v = m[i];
            for &w in g.succ(v) {
                if w == v || comp[w as usize] != c {
                    continue;
                }
                let j = local(w) as usize;
                if self.gone[j] {
                    continue;
                }
                self.in_d[j] -= 1;
                self.touch(j as u32);
            }
            for &u in g.pred(v) {
                if u == v || comp[u as usize] != c {
                    continue;
                }
                let j = local(u) as usize;
                if self.gone[j] {
                    continue;
                }
                self.out_d[j] -= 1;
                self.touch(j as u32);
            }
        }
        // ELS sequence: front, then back reversed. Dependencies first: its reverse.
        out.extend(self.back.iter().map(|&i| m[i as usize]));
        out.extend(self.front.iter().rev().map(|&i| m[i as usize]));
    }

    #[inline]
    fn delta(&self, i: u32) -> i64 {
        i64::from(self.out_d[i as usize]) - i64::from(self.in_d[i as usize])
    }

    /// Re-queues node `j` after a degree change.
    #[inline]
    fn touch(&mut self, j: u32) {
        let i = j as usize;
        if self.out_d[i] == 0 || self.in_d[i] == 0 {
            self.zero.push(j);
        } else {
            self.heap.push((self.delta(j), Reverse(j)));
        }
    }
}
