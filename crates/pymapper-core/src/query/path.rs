//! Shortest path A -> B: bidirectional breadth-first search over the call graph or the module
//! import graph.
//!
//! Both sides grow one whole level at a time, always the side with the smaller frontier. The
//! first node reached by both sides closes a shortest path: before the level, no node was seen
//! by both, so every path is longer than `df + db`; a meet in the level has length at most
//! `df + 1 + db`. Cost: the nodes and edges visited, typically far fewer than one BFS from A.

use std::ops::Range;

use super::{Marks, Query, Scope, Scratch, edge, follows};
use crate::graph::Csr;
use crate::index::{NONE, NodeId};
use crate::resolve::Confidence;

/// Which graph a path follows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PathGraph {
    /// Calls between nodes.
    #[default]
    Calls,
    /// Imports between packages/modules (every module edge; endpoints that are defs stand for
    /// their module).
    Imports,
}

/// Options of [`Query::path`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathOptions {
    pub graph: PathGraph,
    /// Least sure call edge followed (calls only).
    pub min_conf: Confidence,
    /// Follow override edges (calls only).
    pub overrides: bool,
    /// Longest path searched, in edges.
    pub max_len: u32,
    /// What the endpoints cover (see [`Scope`]); a node of the `from` target and one of the
    /// `to` target are the path's ends.
    pub scope: Scope,
}

impl Default for PathOptions {
    fn default() -> Self {
        PathOptions { graph: PathGraph::Calls, min_conf: Confidence::Possible, overrides: true, max_len: u32::MAX, scope: Scope::Auto }
    }
}

/// A shortest path: `nodes[0]` in the `from` target, the last node in the `to` target, and
/// `nodes[i]` calls (imports) `nodes[i + 1]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortestPath {
    pub graph: PathGraph,
    pub nodes: Vec<NodeId>,
    /// Label of each edge `nodes[i] -> nodes[i + 1]`: calls `conf << 1 | dispatch` (the edge as
    /// the search followed it), imports the edge flags.
    labels: Vec<u8>,
}

impl ShortestPath {
    /// Number of edges.
    pub fn len(&self) -> usize {
        self.labels.len()
    }

    /// A single node (the targets overlap).
    pub fn is_empty(&self) -> bool {
        self.labels.is_empty()
    }

    /// Best confidence of call edge `i` (`None` on an import path).
    pub fn conf(&self, i: usize) -> Option<Confidence> {
        (self.graph == PathGraph::Calls).then(|| Confidence::from_u8(self.labels[i] >> 1))
    }

    /// Call edge `i` is only an override edge (dynamic dispatch).
    pub fn dispatch(&self, i: usize) -> bool {
        self.graph == PathGraph::Calls && self.labels[i] & 1 != 0
    }

    /// [`res_flags`](crate::resolve::res_flags) of import edge `i` (0 on a call path).
    pub fn import_flags(&self, i: usize) -> u8 {
        if self.graph == PathGraph::Imports { self.labels[i] } else { 0 }
    }

    /// Least sure call edge of the path (`None` on an import path or a single node).
    pub fn worst(&self) -> Option<Confidence> {
        (0..self.len()).filter_map(|i| self.conf(i)).max()
    }
}

/// One side of the search: marks (`pos` = the neighbour towards the side's seeds, `NONE` for
/// a seed), the current frontier and the depth reached.
struct Side<'s> {
    csr: &'s Csr,
    marks: &'s mut Marks,
    frontier: Vec<u32>,
    depth: u32,
}

impl Side<'_> {
    fn seed(&mut self, n: usize, seeds: Range<u32>) {
        self.marks.begin(n);
        self.frontier.clear();
        for s in seeds {
            self.marks.set(s, NONE);
            self.frontier.push(s);
        }
    }

    /// Expands one level; returns the first node also seen by `other`.
    fn level(&mut self, other: &Marks, keep: &impl Fn(u8) -> bool, next: &mut Vec<u32>) -> Option<u32> {
        next.clear();
        self.depth += 1;
        for &u in &self.frontier {
            let row = self.csr.range(u);
            for (&w, &l) in self.csr.targets()[row.clone()].iter().zip(&self.csr.labels()[row]) {
                if self.marks.seen(w) || !keep(l) {
                    continue;
                }
                self.marks.set(w, u);
                if other.seen(w) {
                    return Some(w);
                }
                next.push(w);
            }
        }
        std::mem::swap(&mut self.frontier, next);
        None
    }
}

/// Shortest path from a node of `src` to a node of `dst` over `g` (`fwd`, `rev`) using edges
/// whose label `keep`s. Returns the nodes.
fn search(
    fwd: &Csr,
    rev: &Csr,
    src: Range<u32>,
    dst: Range<u32>,
    keep: impl Fn(u8) -> bool,
    max_len: u32,
    scratch: &mut Scratch,
) -> Option<Vec<u32>> {
    if src.is_empty() || dst.is_empty() {
        return None;
    }
    let common = src.start.max(dst.start);
    if common < src.end.min(dst.end) {
        return Some(vec![common]);
    }
    let n = fwd.node_count() as usize;
    let mut f = Side { csr: fwd, marks: &mut scratch.a, frontier: std::mem::take(&mut scratch.queue), depth: 0 };
    let mut b = Side { csr: rev, marks: &mut scratch.b, frontier: std::mem::take(&mut scratch.buf), depth: 0 };
    f.seed(n, src);
    b.seed(n, dst);
    let mut next = Vec::new();
    let mut meet = None;
    while !f.frontier.is_empty() && !b.frontier.is_empty() && f.depth + b.depth < max_len {
        let m = if f.frontier.len() <= b.frontier.len() {
            f.level(b.marks, &keep, &mut next)
        } else {
            b.level(f.marks, &keep, &mut next)
        };
        if m.is_some() {
            meet = m;
            break;
        }
    }
    let out = meet.map(|w| {
        let mut nodes = Vec::with_capacity((f.depth + b.depth + 1) as usize);
        let mut v = w;
        while let Some(p) = f.marks.get(v).filter(|&p| p != NONE) {
            nodes.push(p);
            v = p;
        }
        nodes.reverse();
        nodes.push(w);
        let mut v = w;
        while let Some(s) = b.marks.get(v).filter(|&s| s != NONE) {
            nodes.push(s);
            v = s;
        }
        nodes
    });
    scratch.queue = f.frontier;
    scratch.buf = b.frontier;
    out
}

impl Query<'_> {
    /// A shortest path from `from` to `to` (see [`PathOptions`]); `None` when `to` is not
    /// reachable within `opts.max_len` edges.
    pub fn path(&self, from: NodeId, to: NodeId, opts: &PathOptions, scratch: &mut Scratch) -> Option<ShortestPath> {
        match opts.graph {
            PathGraph::Calls => {
                let g = &self.graph;
                let keep = |l: u8| follows(l, opts.min_conf, opts.overrides);
                let nodes = search(g.forward(), g.reverse(), self.target(from, opts.scope), self.target(to, opts.scope), keep, opts.max_len, scratch)?;
                let labels = nodes
                    .windows(2)
                    .map(|w| {
                        let l = g.label(w[0], w[1]).unwrap_or(0);
                        edge(l, opts.overrides).map_or(0, |(c, d)| ((c as u8) << 1) | u8::from(d))
                    })
                    .collect();
                Some(ShortestPath { graph: PathGraph::Calls, nodes, labels })
            }
            PathGraph::Imports => {
                let mg = self.import_graph();
                let local = |r: Range<NodeId>| {
                    let lo = mg.nodes().partition_point(|&v| v < r.start) as u32;
                    let hi = mg.nodes().partition_point(|&v| v < r.end) as u32;
                    lo..hi
                };
                let (src, dst) = (local(self.module_range(from)), local(self.module_range(to)));
                let g = mg.graph();
                let mut nodes = search(g.forward(), g.reverse(), src, dst, |_| true, opts.max_len, scratch)?;
                let labels = nodes.windows(2).map(|w| g.label(w[0], w[1]).unwrap_or(0)).collect();
                for v in &mut nodes {
                    *v = mg.node(*v);
                }
                Some(ShortestPath { graph: PathGraph::Imports, nodes, labels })
            }
        }
    }
}
