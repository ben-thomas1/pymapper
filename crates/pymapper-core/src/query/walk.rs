//! Callers, callees and impact: breadth-first walks over the call graph.

use std::ops::Range;

use super::{Marks, Query, Scope, Scratch, edge};
use crate::analysis::testfiles;
use crate::graph::Csr;
use crate::index::{NONE, NodeId, NodeKind};
use crate::resolve::Confidence;
use crate::rollup::Level;

/// Options of [`Query::callers`] / [`Query::callees`] / [`Query::impact`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalkOptions {
    /// Hops from the target (1: direct callers / callees; `u32::MAX`: unbounded).
    pub depth: u32,
    /// Least sure edge followed (`Possible` follows every edge with a target).
    pub min_conf: Confidence,
    /// Follow override edges (base method -> override: dynamic dispatch).
    pub overrides: bool,
    pub scope: Scope,
    /// Stop after this many nodes ([`Walk::truncated`] is then set).
    pub limit: usize,
}

impl Default for WalkOptions {
    fn default() -> Self {
        WalkOptions { depth: 1, min_conf: Confidence::Possible, overrides: true, scope: Scope::Auto, limit: usize::MAX }
    }
}

impl WalkOptions {
    /// Every edge with a target, unbounded depth.
    pub fn transitive() -> WalkOptions {
        WalkOptions { depth: u32::MAX, ..WalkOptions::default() }
    }
}

/// One node reached by a walk (20 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hop {
    pub node: NodeId,
    /// The neighbour one hop closer to the target (a target node at depth 1).
    pub via: NodeId,
    /// Row of `via` in [`Walk::hops`] (`NONE` when `via` is a target node).
    pub parent: u32,
    /// Hops from the target (1 = direct).
    pub depth: u32,
    /// Best confidence of the edge between `node` and `via`.
    pub conf: Confidence,
    /// Least sure edge on the path back to the target through `via`.
    pub worst: Confidence,
    /// The edge to `via` is only an override edge (dynamic dispatch, not a call).
    pub dispatch: bool,
}

/// Result of [`Query::callers`] / [`Query::callees`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Walk {
    /// Reached nodes in breadth-first order (by depth; within a depth in discovery order,
    /// which is deterministic: CSR rows are sorted).
    pub hops: Vec<Hop>,
    /// The limit stopped the walk before it was complete.
    pub truncated: bool,
}

impl Walk {
    /// Number of reached nodes at each depth (`[0]` is depth 1).
    pub fn by_depth(&self) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        for h in &self.hops {
            let d = h.depth as usize - 1;
            if out.len() <= d {
                out.resize(d + 1, 0);
            }
            out[d] += 1;
        }
        out
    }

    /// The path from `hops[i]` back to the target: `hops[i].node`, its `via`, ..., ending
    /// with a target node (`depth + 1` nodes). O(depth).
    pub fn path_from(&self, i: usize) -> Vec<NodeId> {
        let mut cur = self.hops[i];
        let mut out = Vec::with_capacity(cur.depth as usize + 1);
        out.push(cur.node);
        while cur.parent != NONE {
            cur = self.hops[cur.parent as usize];
            out.push(cur.node);
        }
        out.push(cur.via);
        out
    }
}

/// Breadth-first walk from the nodes of `seeds` over `csr`; appends reached nodes to `out`
/// (also the queue). Returns whether the limit stopped it.
fn bfs(csr: &Csr, seeds: Range<NodeId>, opts: &WalkOptions, marks: &mut Marks, out: &mut Vec<Hop>) -> bool {
    marks.begin(csr.node_count() as usize);
    out.clear();
    if opts.depth == 0 {
        return false;
    }
    let mut expand = |u: NodeId, parent: u32, depth: u32, worst: Confidence, out: &mut Vec<Hop>| -> bool {
        let row = csr.range(u);
        for (&w, &l) in csr.targets()[row.clone()].iter().zip(&csr.labels()[row]) {
            if seeds.contains(&w) || marks.seen(w) {
                continue;
            }
            let Some((conf, dispatch)) = edge(l, opts.overrides).filter(|&(c, _)| c <= opts.min_conf) else { continue };
            if out.len() >= opts.limit {
                return true;
            }
            marks.set(w, out.len() as u32);
            out.push(Hop { node: w, via: u, parent, depth, conf, worst: worst.max(conf), dispatch });
        }
        false
    };
    for s in seeds.clone() {
        if expand(s, NONE, 1, Confidence::Certain, out) {
            return true;
        }
    }
    let mut head = 0;
    while head < out.len() {
        let h = out[head];
        head += 1;
        if h.depth >= opts.depth {
            // BFS order: every later hop is at least as deep.
            break;
        }
        if expand(h.node, head as u32 - 1, h.depth + 1, h.worst, out) {
            return true;
        }
    }
    false
}

/// Result of [`Query::impact`]: everything that transitively calls the target.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Impact {
    /// The reverse walk (every affected node with its path back to the target).
    pub walk: Walk,
    /// Affected classes and functions.
    pub symbols: u32,
    /// Affected `<module>` scopes (module-level code calling into the target on import).
    pub module_scopes: u32,
    /// Affected nodes in test files ([`testfiles::is_test_file`]).
    pub tests: u32,
    /// Distinct files of the affected nodes.
    pub files: u32,
    /// Affected nodes per package/module, most affected first (ties: node order).
    pub modules: Vec<(NodeId, u32)>,
    /// Modules that import the target's modules, directly or transitively, in breadth-first
    /// order (every import edge, type-only and lazy ones included).
    pub importers: Vec<NodeId>,
}

impl Query<'_> {
    /// Nodes that call into `target` up to `opts.depth` hops.
    pub fn callers(&self, target: NodeId, opts: &WalkOptions, scratch: &mut Scratch) -> Walk {
        let mut out = Walk::default();
        out.truncated = bfs(self.graph.reverse(), self.target(target, opts.scope), opts, &mut scratch.a, &mut out.hops);
        out
    }

    /// Nodes that `target` calls up to `opts.depth` hops.
    pub fn callees(&self, target: NodeId, opts: &WalkOptions, scratch: &mut Scratch) -> Walk {
        let mut out = Walk::default();
        out.truncated = bfs(self.graph.forward(), self.target(target, opts.scope), opts, &mut scratch.a, &mut out.hops);
        out
    }

    /// Reverse reachability of `target` with counts (see [`Impact`]). `opts.depth` bounds the
    /// walk (use [`WalkOptions::transitive`] for everything).
    pub fn impact(&self, target: NodeId, opts: &WalkOptions, scratch: &mut Scratch) -> Impact {
        let ix = self.ix;
        let walk = self.callers(target, opts, scratch);
        let mut out = Impact::default();
        // Module of every hop, then counted by sort + run length (no hash map).
        let mods = &mut scratch.buf;
        mods.clear();
        let mut files = Vec::with_capacity(walk.hops.len());
        for h in &walk.hops {
            match ix.kind(h.node) {
                NodeKind::Class | NodeKind::Function => out.symbols += 1,
                _ => out.module_scopes += 1,
            }
            if let Some(f) = ix.file(h.node) {
                files.push(f);
            }
            mods.push(Level::Module.apply(ix, h.node));
        }
        // Per distinct file: one test check, counting every hop in it.
        files.sort_unstable();
        let mut i = 0;
        while i < files.len() {
            let j = i + files[i..].partition_point(|&f| f == files[i]);
            out.files += 1;
            if testfiles::is_test_file(ix, files[i]) {
                out.tests += (j - i) as u32;
            }
            i = j;
        }
        mods.sort_unstable();
        let mut i = 0;
        while i < mods.len() {
            let j = i + mods[i..].partition_point(|&m| m == mods[i]);
            out.modules.push((mods[i], (j - i) as u32));
            i = j;
        }
        out.modules.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out.importers = self.importers(target, &mut scratch.b, &mut scratch.queue);
        out.walk = walk;
        out
    }

    /// Modules importing the modules of `target` (its own module for a def), transitively.
    fn importers(&self, target: NodeId, marks: &mut Marks, queue: &mut Vec<u32>) -> Vec<NodeId> {
        let ix = self.ix;
        let mg = self.import_graph();
        let range = self.module_range(target);
        // Local ids of the target's modules: module nodes are ascending, so a contiguous run.
        let lo = mg.nodes().partition_point(|&v| v < range.start) as u32;
        let hi = mg.nodes().partition_point(|&v| v < range.end) as u32;
        marks.begin(mg.len() as usize);
        queue.clear();
        for m in lo..hi {
            marks.set(m, 0);
            queue.push(m);
        }
        let mut head = 0;
        let mut out = Vec::new();
        let rev = mg.graph().reverse();
        while head < queue.len() {
            let m = queue[head];
            head += 1;
            for &p in rev.neighbours(m) {
                if !marks.seen(p) {
                    marks.set(p, 0);
                    queue.push(p);
                    out.push(mg.node(p));
                }
            }
        }
        debug_assert!(out.iter().all(|&v| !ix.is_descendant(target, v) || ix.kind(target).is_def()));
        out
    }

    /// Package/module nodes standing for `target` in module graphs: its subtree for a
    /// package/module/distribution, its file's module for a def.
    pub(super) fn module_range(&self, target: NodeId) -> Range<NodeId> {
        let ix = self.ix;
        if ix.kind(target).is_def() {
            match ix.file(target) {
                Some(f) => {
                    let m = ix.file_node(f);
                    m..m + 1
                }
                None => target..target,
            }
        } else {
            ix.subtree(target)
        }
    }
}
