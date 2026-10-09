//! Program summary: counts per origin, resolution rates, cycles and top hotspots.
//!
//! One pass over the nodes and files (counts), the resolution statistics already kept by
//! [`Imports`](crate::resolve::Imports) and [`ResolvedCalls`](crate::resolve::ResolvedCalls),
//! the SCCs of the cycle import graph (shared with [`Query::import_cycles`]) and bounded heaps
//! for the rankings. O(V + E), no allocation per node.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use super::{Query, edge};
use crate::analysis::hotspot::{Metric, top_functions};
use crate::analysis::testfiles;
use crate::index::{FileId, NodeId, NodeKind};
use crate::resolve::ImportStatus;
use crate::roots::Origin;

/// Options of [`Query::summary`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SummaryOptions {
    /// Entries per ranking.
    pub top: usize,
    /// Rank only first-party code (counts always cover every origin).
    pub first_party_only: bool,
    /// Rank test code too.
    pub include_tests: bool,
}

impl Default for SummaryOptions {
    fn default() -> Self {
        SummaryOptions { top: 10, first_party_only: true, include_tests: false }
    }
}

/// Counts of one origin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OriginCounts {
    pub files: u32,
    pub lines: u64,
    /// Packages, namespace packages included.
    pub packages: u32,
    pub modules: u32,
    pub classes: u32,
    pub functions: u32,
}

/// One ranked node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ranked {
    pub node: NodeId,
    pub value: u32,
}

/// Result of [`Query::summary`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Indexed by [`Origin`] as `usize` (first-party, third-party, stdlib).
    pub origins: [OriginCounts; 3],
    pub import_rows: u64,
    /// Import rows naming a missing module or name (or beyond the top-level package).
    pub unresolved_imports: u64,
    /// Module import edges.
    pub import_edges: u64,
    pub call_sites: u64,
    /// Call sites per [`Confidence`](crate::resolve::Confidence) of their best edge.
    pub calls_by_conf: [u64; 4],
    /// Distinct (caller, callee) pairs.
    pub call_edges: u64,
    /// Cyclic components of the cycle import graph and the size of the largest.
    pub import_cycles: u32,
    pub largest_import_cycle: u32,
    /// Functions with the highest cognitive / cyclomatic complexity.
    pub cognitive: Vec<Ranked>,
    pub cyclomatic: Vec<Ranked>,
    /// Classes and functions with the most distinct callers (real calls only: a base method
    /// that only dispatches to an override does not count).
    pub most_called: Vec<Ranked>,
    /// Packages and modules with the most importing modules.
    pub most_imported: Vec<Ranked>,
}

/// Bounded top-k by (value desc, node asc).
struct TopK {
    k: usize,
    heap: BinaryHeap<Reverse<(u32, Reverse<NodeId>)>>,
}

impl TopK {
    fn new(k: usize) -> TopK {
        TopK { k, heap: BinaryHeap::with_capacity(k + 1) }
    }

    #[inline]
    fn push(&mut self, node: NodeId, value: u32) {
        if self.k == 0 || value == 0 {
            return;
        }
        let item = Reverse((value, Reverse(node)));
        if self.heap.len() < self.k {
            self.heap.push(item);
        } else if let Some(mut weakest) = self.heap.peek_mut()
            && item < *weakest
        {
            *weakest = item;
        }
    }

    fn into_vec(self) -> Vec<Ranked> {
        self.heap.into_sorted_vec().into_iter().map(|Reverse((value, Reverse(node)))| Ranked { node, value }).collect()
    }
}

impl Query<'_> {
    /// Summary of the whole program (see [`Summary`]).
    pub fn summary(&self, opts: &SummaryOptions) -> Summary {
        let ix = self.ix;
        let mut s = Summary::default();
        for f in 0..ix.file_count() {
            let c = &mut s.origins[ix.file_origin(f) as usize];
            c.files += 1;
            c.lines += u64::from(ix.file_facts(f).lines);
        }
        // Origin of every distribution node, sorted by node (distributions precede their
        // subtree in pre-order).
        let mut dists: Vec<(NodeId, Origin)> =
            ix.groups().iter().enumerate().map(|(g, gr)| (ix.group_node(g as u32), gr.origin)).collect();
        dists.sort_unstable_by_key(|d| d.0);
        let rankable = |f: FileId| {
            (!opts.first_party_only || ix.file_origin(f) == Origin::FirstParty)
                && (opts.include_tests || !testfiles::is_test_file(ix, f))
        };
        let mut called = TopK::new(opts.top);
        let mut origin = Origin::FirstParty;
        let mut next_dist = 0;
        for v in 0..ix.node_count() {
            let kind = ix.kind(v);
            if kind == NodeKind::Distribution {
                while next_dist < dists.len() && dists[next_dist].0 <= v {
                    origin = dists[next_dist].1;
                    next_dist += 1;
                }
                continue;
            }
            let c = &mut s.origins[origin as usize];
            match kind {
                NodeKind::Package => c.packages += 1,
                NodeKind::Module => c.modules += 1,
                NodeKind::Class => c.classes += 1,
                _ => c.functions += 1,
            }
            if kind.is_def() && ix.file(v).is_some_and(rankable) {
                let real = self.graph.reverse().neighbour_labels(v).iter().filter(|&&l| edge(l, false).is_some()).count();
                called.push(v, real as u32);
            }
        }
        let ist = self.imp.stats();
        s.import_rows = ist.rows;
        s.import_edges = ist.module_edges;
        s.unresolved_imports = [ImportStatus::MissingName, ImportStatus::MissingModule, ImportStatus::BeyondTop]
            .iter()
            .map(|&k| ist.by_status[k as usize])
            .sum();
        let cst = self.calls.stats();
        s.call_sites = cst.calls;
        s.calls_by_conf = cst.by_conf;
        s.call_edges = u64::from(self.graph.edge_count());
        let (cg, scc) = self.cycle_graph();
        for c in 0..scc.count() {
            if scc.is_cyclic(cg.graph(), c) {
                s.import_cycles += 1;
                s.largest_import_cycle = s.largest_import_cycle.max(scc.size(c));
            }
        }
        let to_ranked = |v: Vec<(NodeId, u32)>| v.into_iter().map(|(node, value)| Ranked { node, value }).collect();
        s.cognitive = to_ranked(top_functions(ix, Metric::Cognitive, opts.top, rankable));
        s.cyclomatic = to_ranked(top_functions(ix, Metric::Mccabe, opts.top, rankable));
        s.most_called = called.into_vec();
        let mg = self.import_graph();
        let mut imported = TopK::new(opts.top);
        for m in 0..mg.len() {
            let v = mg.node(m);
            let ok = match ix.file(v) {
                Some(f) => !opts.first_party_only || ix.file_origin(f) == Origin::FirstParty,
                None => !opts.first_party_only,
            };
            if ok {
                imported.push(v, mg.graph().reverse().degree(m));
            }
        }
        s.most_imported = imported.into_vec();
        s
    }
}
