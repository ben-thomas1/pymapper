//! Widest-path propagation of liveness levels (bucket queue) and the def-level step rules.

use super::{Why, level};
use crate::graph::{CallFilter, Graph, call_graph};
use crate::index::{Index, NodeId, NodeKind};
use crate::intern::FxHashMap;
use crate::resolve::{Classes, Confidence, Reason, ResolvedCalls};

/// Levels 0..=100 and why, per node of some graph, with the pending queue.
pub(super) struct Levels {
    pub level: Vec<u8>,
    pub why: Vec<u8>,
    /// One stack per level; a node is pushed each time its level rises.
    queue: Vec<Vec<u32>>,
    pub expansions: u64,
}

/// What a node's expansion does: raise its successors (see the module docs of `deadcode`).
pub(super) trait Expand {
    fn expand(&self, lv: &mut Levels, v: u32, l: u8);
}

impl Levels {
    pub fn new(n: u32) -> Levels {
        Levels {
            level: vec![0; n as usize],
            why: vec![Why::Dead as u8; n as usize],
            queue: (0..=level::LIVE).map(|_| Vec::new()).collect(),
            expansions: 0,
        }
    }

    /// Raises `v` to level `l` (no-op unless higher), queueing it for expansion.
    #[inline]
    pub fn raise(&mut self, v: u32, l: u8, why: Why) {
        let cur = &mut self.level[v as usize];
        if l > *cur {
            *cur = l;
            self.why[v as usize] = why as u8;
            self.queue[l as usize].push(v);
        }
    }

    /// Expands queued nodes, highest level first, until the queue is empty. A node popped at
    /// level `l` has its final level `l` (every step caps the target at the source's level, and
    /// higher levels were all expanded before), so the result is the widest-path level from
    /// the seeds. Stale entries (the node rose since) are skipped. Can be called again after
    /// more seeds are raised: levels only rise.
    pub fn run(&mut self, g: &impl Expand) {
        for l in (1..=level::LIVE).rev() {
            while let Some(v) = self.queue[l as usize].pop() {
                if self.level[v as usize] != l {
                    continue;
                }
                self.expansions += 1;
                g.expand(self, v, l);
            }
        }
    }

    /// Nodes at `level::LIVE`.
    pub fn live_count(&self) -> u64 {
        self.level.iter().filter(|&&l| l == level::LIVE).count() as u64
    }
}

/// The def-level graph: calls, overrides, members, siblings and child seeds.
pub(super) struct DefGraph<'a> {
    pub ix: &'a Index,
    pub classes: &'a Classes,
    /// Call (and decorator) edges without override edges; label = best [`Confidence`].
    pub calls: Graph,
    /// Override edges: base method -> overriding method.
    pub over: Graph,
    /// Per node: the [`Why`] of its child seed (0 = none), applied when its parent expands.
    pub seed: Vec<u8>,
    /// Next def in a ring of same-name siblings (only defs that have one).
    pub siblings: FxHashMap<NodeId, NodeId>,
}

impl<'a> DefGraph<'a> {
    /// The call and override graphs of `calls` (seeds and siblings are filled by `seeds`).
    pub fn new(ix: &'a Index, calls: &'a ResolvedCalls) -> DefGraph<'a> {
        let n = ix.node_count();
        let cg = call_graph(ix, calls, CallFilter::default().without(Reason::Override));
        let edges = calls.edges();
        let over = Graph::build(n, || edges.iter().filter(|e| e.reason == Reason::Override).map(|e| (e.src, e.dst)));
        DefGraph {
            ix,
            classes: calls.classes(),
            calls: cg,
            over,
            seed: vec![0; n as usize],
            siblings: FxHashMap::default(),
        }
    }

    /// Heap bytes (capacity) of the graphs and seeds.
    pub fn heap_bytes(&self) -> usize {
        self.calls.heap_bytes() + self.over.heap_bytes() + self.seed.capacity() + self.siblings.capacity() * 9
    }
}

impl Expand for DefGraph<'_> {
    fn expand(&self, lv: &mut Levels, v: u32, l: u8) {
        let ix = self.ix;
        let fwd = self.calls.forward();
        let labels = fwd.neighbour_labels(v);
        for (i, &w) in fwd.neighbours(v).iter().enumerate() {
            if labels[i] <= Confidence::Likely as u8 {
                lv.raise(w, l, Why::Call);
            } else {
                lv.raise(w, l.min(level::POSSIBLE), Why::PossibleCall);
            }
        }
        let kind = ix.kind(v);
        if kind != NodeKind::Distribution {
            for c in ix.children(v) {
                let s = self.seed[c as usize];
                if s != 0 {
                    let why = Why::from_u8(s);
                    lv.raise(c, l.min(why.seed_strength()), why);
                }
            }
        }
        match kind {
            NodeKind::Class => {
                if let Some(c) = self.classes.id(ix, v) {
                    for &b in self.classes.bases(c) {
                        lv.raise(self.classes.node(b), l, Why::Subclass);
                    }
                }
                // Methods overriding a live base method are live as far as the class is.
                for m in ix.children(v) {
                    let mut best = 0;
                    for &b in self.over.pred(m) {
                        best = best.max(lv.level[b as usize]);
                    }
                    if best > 0 {
                        lv.raise(m, l.min(best), Why::Override);
                    }
                }
            }
            NodeKind::Function => {
                for &m in self.over.succ(v) {
                    if let Some(c) = ix.parent(m) {
                        let lc = lv.level[c as usize];
                        lv.raise(m, l.min(lc), Why::Override);
                    }
                }
                for &b in self.over.pred(v) {
                    lv.raise(b, l, Why::Override);
                }
            }
            _ => {}
        }
        if kind.is_def()
            && let Some(p) = ix.parent(v)
            && ix.kind(p) == NodeKind::Class
        {
            lv.raise(p, l, Why::Method);
        }
        if let Some(&t) = self.siblings.get(&v) {
            lv.raise(t, l, Why::Sibling);
        }
    }
}
