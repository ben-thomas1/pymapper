//! Cycles containing a target: import cycles of its modules, call cycles (recursion) through
//! a node.
//!
//! - Imports: the graph of [`crate::analysis::cycles`] (first-party runtime files, type-only
//!   edges dropped unless configured), condensed once on first use; a query reads the
//!   components of the target's modules and searches a witness through the target in each
//!   (BFS inside the component). O(component) per query.
//! - Calls: the strongly connected component of node `t` is the set of nodes both reachable
//!   from `t` and reaching `t`. A forward BFS from `t` marks its reach (and finds the shortest
//!   cycle through `t`: the first processed node with an edge back to `t`); a reverse BFS from
//!   `t` restricted to marked nodes collects the component (a node outside the forward reach
//!   cannot reach `t` through a node inside it, so pruning is exact). O(forward reach of `t`),
//!   no whole-graph Tarjan.

use super::walk::WalkOptions;
use super::{Query, Scratch, follows};
use crate::analysis::cycles::witness;
use crate::index::{NONE, NodeId};

/// A cycle through the target.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CycleHit {
    /// Every node of the strongly connected component, ascending.
    pub members: Vec<NodeId>,
    /// A shortest cycle through the target's node: each node imports (calls) the next and the
    /// last one the first. Import cycles start at the module whose import closes the cycle (as
    /// PM301); call cycles start at the target.
    pub witness: Vec<NodeId>,
}

impl Query<'_> {
    /// Import cycles containing a module of `target` (its module for a def; every module of
    /// a package), one per cyclic component, in component order.
    pub fn import_cycles(&self, target: NodeId, scratch: &mut Scratch) -> Vec<CycleHit> {
        let (mg, scc) = self.cycle_graph();
        let g = mg.graph();
        let range = self.module_range(target);
        let lo = mg.nodes().partition_point(|&v| v < range.start) as u32;
        let hi = mg.nodes().partition_point(|&v| v < range.end) as u32;
        let mut seen: Vec<u32> = Vec::new();
        let (parent, queue, local) = (&mut scratch.parent, &mut scratch.queue, &mut scratch.buf);
        let mut out = Vec::new();
        for m in lo..hi {
            let c = scc.comp(m);
            if seen.contains(&c) || !scc.is_cyclic(g, c) {
                continue;
            }
            seen.push(c);
            // `parent` is all `NONE` between queries (`witness` resets what it sets).
            if parent.len() < mg.len() as usize {
                parent.resize(mg.len() as usize, NONE);
            }
            local.clear();
            // `m` is the lowest module of the target in this component.
            witness(g, scc, c, m, parent, queue, local);
            let mut members: Vec<NodeId> = scc.members(c).iter().map(|&l| mg.node(l)).collect();
            members.sort_unstable();
            out.push(CycleHit { members, witness: local.iter().map(|&l| mg.node(l)).collect() });
        }
        out
    }

    /// The call cycle through node `t` (recursion, direct or mutual) over the edges `opts`
    /// keeps (`opts.depth`, `scope` and `limit` are ignored); `None` if `t` is in no cycle.
    pub fn call_cycle(&self, t: NodeId, opts: &WalkOptions, scratch: &mut Scratch) -> Option<CycleHit> {
        let g = &self.graph;
        let n = g.node_count() as usize;
        let keep = |l: u8| follows(l, opts.min_conf, opts.overrides);
        // Forward BFS: `a.pos` = BFS parent.
        let (fa, queue) = (&mut scratch.a, &mut scratch.queue);
        fa.begin(n);
        fa.set(t, NONE);
        queue.clear();
        queue.push(t);
        let mut closing = NONE;
        let mut head = 0;
        let fwd = g.forward();
        while head < queue.len() {
            let u = queue[head];
            head += 1;
            let row = fwd.range(u);
            for (&w, &l) in fwd.targets()[row.clone()].iter().zip(&fwd.labels()[row]) {
                if !keep(l) {
                    continue;
                }
                if w == t && closing == NONE {
                    closing = u;
                }
                if !fa.seen(w) {
                    fa.set(w, u);
                    queue.push(w);
                }
            }
        }
        if closing == NONE {
            return None;
        }
        let mut cycle = Vec::new();
        let mut v = closing;
        while v != NONE {
            cycle.push(v);
            v = fa.get(v).unwrap_or(NONE);
        }
        // `closing, parent(closing), ..., t` reversed: `t, ..., closing`.
        cycle.reverse();
        // Reverse BFS restricted to the forward reach.
        let fb = &mut scratch.b;
        fb.begin(n);
        fb.set(t, 0);
        queue.clear();
        queue.push(t);
        let mut head = 0;
        let rev = g.reverse();
        while head < queue.len() {
            let u = queue[head];
            head += 1;
            let row = rev.range(u);
            for (&w, &l) in rev.targets()[row.clone()].iter().zip(&rev.labels()[row]) {
                if keep(l) && fa.seen(w) && !fb.seen(w) {
                    fb.set(w, 0);
                    queue.push(w);
                }
            }
        }
        let mut members = queue.clone();
        members.sort_unstable();
        Some(CycleHit { members, witness: cycle })
    }
}
