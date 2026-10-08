//! Bottom-up node aggregates over the pre-order hierarchy.

use super::is_file_level;
use crate::index::{Index, NodeId, NodeKind};

/// Aggregates of one node (a copy of its row; see the module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NodeAgg {
    pub loc: u32,
    pub symbols: u32,
    pub cc_sum: u32,
    pub cc_max: u16,
    pub cog_sum: u32,
    pub cog_max: u16,
    /// Findings in the subtree (0 until [`Aggregates::set_findings`]).
    pub findings: u32,
    /// `None` until [`Aggregates::set_scores`].
    pub score: Option<f32>,
}

/// Per-node aggregates, struct of arrays indexed by [`NodeId`] (see the module docs).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Aggregates {
    loc: Vec<u32>,
    symbols: Vec<u32>,
    cc_sum: Vec<u32>,
    cog_sum: Vec<u32>,
    cc_max: Vec<u16>,
    cog_max: Vec<u16>,
    /// Empty until set.
    findings: Vec<u32>,
    /// Empty until set.
    score: Vec<f32>,
}

impl Aggregates {
    /// Computes every column of `ix`: own values in pre-order, then each node into its parent
    /// in reverse pre-order (children come after their parent). O(nodes), no scratch.
    pub fn build(ix: &Index) -> Aggregates {
        let n = ix.node_count() as usize;
        let mx = ix.metrics();
        let mut a = Aggregates {
            loc: Vec::with_capacity(n),
            symbols: Vec::with_capacity(n),
            cc_sum: Vec::with_capacity(n),
            cog_sum: Vec::with_capacity(n),
            cc_max: Vec::with_capacity(n),
            cog_max: Vec::with_capacity(n),
            findings: Vec::new(),
            score: Vec::new(),
        };
        for v in 0..n as NodeId {
            let kind = ix.kind(v);
            let (mut loc, mut sym, mut cc, mut cog) = (0, 0, 0, 0);
            if kind.is_def() {
                sym = 1;
                if let Some(m) = ix.metrics_row(v) {
                    loc = mx.loc[m];
                    if kind == NodeKind::Function {
                        cc = mx.cyclomatic[m];
                        cog = mx.cognitive[m];
                    }
                }
            } else if let Some(f) = ix.file(v) {
                loc = ix.file_facts(f).lines;
            }
            a.loc.push(loc);
            a.symbols.push(sym);
            a.cc_sum.push(u32::from(cc));
            a.cog_sum.push(u32::from(cog));
            a.cc_max.push(cc);
            a.cog_max.push(cog);
        }
        for v in (0..n as NodeId).rev() {
            let Some(p) = ix.parent(v) else { continue };
            let (v, p, pk) = (v as usize, p as usize, ix.kind(p as NodeId));
            if is_file_level(ix.kind(v as NodeId)) {
                a.loc[p] = a.loc[p].saturating_add(a.loc[v]);
            }
            a.symbols[p] += a.symbols[v];
            // A function's own score already holds everything nested in it.
            if pk != NodeKind::Function {
                a.cc_sum[p] = a.cc_sum[p].saturating_add(a.cc_sum[v]);
                a.cog_sum[p] = a.cog_sum[p].saturating_add(a.cog_sum[v]);
            }
            a.cc_max[p] = a.cc_max[p].max(a.cc_max[v]);
            a.cog_max[p] = a.cog_max[p].max(a.cog_max[v]);
        }
        a
    }

    /// Number of nodes.
    #[inline]
    pub fn len(&self) -> usize {
        self.loc.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.loc.is_empty()
    }

    /// The row of node `v`.
    pub fn get(&self, v: NodeId) -> NodeAgg {
        let i = v as usize;
        NodeAgg {
            loc: self.loc[i],
            symbols: self.symbols[i],
            cc_sum: self.cc_sum[i],
            cc_max: self.cc_max[i],
            cog_sum: self.cog_sum[i],
            cog_max: self.cog_max[i],
            findings: self.findings(v),
            score: self.score(v),
        }
    }

    /// Lines per node (the treemap size).
    #[inline]
    pub fn loc(&self) -> &[u32] {
        &self.loc
    }

    /// Classes and functions in each subtree, the node included.
    #[inline]
    pub fn symbols(&self) -> &[u32] {
        &self.symbols
    }

    /// Cyclomatic complexity summed over the outermost functions of each subtree.
    #[inline]
    pub fn cc_sum(&self) -> &[u32] {
        &self.cc_sum
    }

    /// Maximum cyclomatic complexity of a function in each subtree.
    #[inline]
    pub fn cc_max(&self) -> &[u16] {
        &self.cc_max
    }

    /// Cognitive complexity summed over the outermost functions of each subtree.
    #[inline]
    pub fn cog_sum(&self) -> &[u32] {
        &self.cog_sum
    }

    /// Maximum cognitive complexity of a function in each subtree.
    #[inline]
    pub fn cog_max(&self) -> &[u16] {
        &self.cog_max
    }

    /// Findings in the subtree of `v` (0 before [`Aggregates::set_findings`]).
    #[inline]
    pub fn findings(&self, v: NodeId) -> u32 {
        self.findings.get(v as usize).copied().unwrap_or(0)
    }

    /// Score of `v` (`None` before [`Aggregates::set_scores`]).
    #[inline]
    pub fn score(&self, v: NodeId) -> Option<f32> {
        self.score.get(v as usize).copied()
    }

    /// Replaces the finding counts: one finding at each node of `at` (repeats allowed), then
    /// subtree totals. O(nodes + findings).
    pub fn set_findings(&mut self, ix: &Index, at: impl IntoIterator<Item = NodeId>) {
        let n = self.len();
        self.findings.clear();
        self.findings.resize(n, 0);
        for v in at {
            self.findings[v as usize] += 1;
        }
        for v in (0..n as NodeId).rev() {
            if let Some(p) = ix.parent(v) {
                self.findings[p as usize] += self.findings[v as usize];
            }
        }
    }

    /// Sets the score column (placeholder for a later scoring step).
    ///
    /// # Panics
    /// If `score` does not have one value per node.
    pub fn set_scores(&mut self, score: Vec<f32>) {
        assert_eq!(score.len(), self.len(), "one score per node");
        self.score = score;
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        4 * (self.loc.capacity()
            + self.symbols.capacity()
            + self.cc_sum.capacity()
            + self.cog_sum.capacity()
            + self.findings.capacity()
            + self.score.capacity())
            + 2 * (self.cc_max.capacity() + self.cog_max.capacity())
    }
}
