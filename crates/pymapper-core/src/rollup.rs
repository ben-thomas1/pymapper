//! Rollups and bottom-up aggregates at any hierarchy level (design: Semantics). Owned by the
//! rollup step.
//!
//! ```text
//!   Index + Imports + ResolvedCalls ──> Rollup        base tables, built once:
//!                                       │             call pairs (src, dst, sites, best/worst
//!                                       │             confidence) sorted by (src, dst), import
//!                                       │             pairs sorted by (src, dst), multi-target
//!                                       │             sites sorted by src
//!                                       └─> level(Level) ──> LevelEdges   per zoom level:
//!                                                            endpoints mapped to their
//!                                                            ancestor at the level, one row
//!                                                            per (A, B), sorted by (A, B)
//!   Index ─────────────────────────────> Aggregates   one bottom-up pass over the pre-order
//!                                                     hierarchy: LOC, symbols, sum/max
//!                                                     cyclomatic and cognitive, findings
//!                                                     (hook), score (placeholder)
//! ```
//!
//! # Levels
//!
//! A [`Level`] maps every node to one representative: [`Level::Depth`]`(k)` to its ancestor at
//! depth `k` (a node above depth `k` stays itself: a module at depth 2 at the method level),
//! [`Level::Module`] to its package/module (defs to the file that defines them). Both maps are
//! monotone in pre-order (`v < w` implies `map(v) <= map(w)`): the representative of `v` is an
//! ancestor-or-self, and pre-order subtrees are intervals. So the base pairs, sorted by source,
//! stay grouped by mapped source after mapping, and a level needs no global sort.
//!
//! # Per-level algorithm
//!
//! One pass over the base tables in source order. For each run of base rows with the same
//! mapped source `A`, rows are merged by mapped destination `B` through a slot array indexed
//! by node (`slot[B]` = output row of `B` in the current run, validated against the run start:
//! no clearing between runs), then the run (one row per distinct `B`, few) is sorted by `B`.
//! Rows with `A == B` (calls and imports inside one representative) are dropped. Cost:
//! O(n) to build the node map plus O(m + sum of d log d) for `m` base rows and `d` distinct
//! destinations per source; scratch is `8 n` bytes (node map + slots), reusable across levels
//! ([`LevelScratch`]).
//!
//! # Edge values
//!
//! For a level edge `A -> B`:
//!
//! - `sites`: distinct call sites (call rows and uncalled decorators) in `A`'s subtree with a
//!   kept edge into `B`'s subtree. A site with several targets (`Foo()` links the class and its
//!   `__init__`; `self.m()` may link several subclass methods) counts once per `B`: the base
//!   counts it once per target, and the level subtracts the excess for the few multi-target
//!   sites (kept apart in the base). Override edges have no site.
//! - `pairs`: distinct (caller node, callee node) pairs behind the edge, override pairs
//!   included.
//! - `best` / `worst`: the most and least sure [`Confidence`] of those pairs (`None` when the
//!   edge has no call pair).
//! - `imports`: module import rows behind the edge (the import flag is `imports > 0`), and
//!   `import_flags`: the [`res_flags::EDGE_ALL`] flags shared by every one of them plus `STAR`
//!   if any is a star import, as in [`ModuleEdge`](crate::resolve::ModuleEdge).
//!
//! Known approximation of `sites`: edges of one site are consecutive in
//! [`ResolvedCalls::edges`], and sites are told apart by (row, caller) only, because call rows
//! and decorator rows are numbered in different tables. When the last call site of a file and
//! its first uncalled decorator have the same row number and the same caller, they count as
//! one site. Decorator rows are numbered far below call rows except in the first files, so this
//! is rare and undercounts by at most one.
//!
//! # Aggregates
//!
//! [`Aggregates::build`] fills per-node columns in two passes over the nodes (own values in
//! pre-order, then children into parents in reverse pre-order):
//!
//! - `loc`: defs: their inclusive line span; packages and modules: the file's lines plus the
//!   lines of their sub-packages and modules; namespace packages and distributions: the sum
//!   over their packages and modules. Defs never add to their parent (their lines are already
//!   in the parent's span).
//! - `symbols`: class and function nodes in the subtree, the node itself included.
//! - `cc_sum` / `cog_sum`: cyclomatic and cognitive complexity summed over the functions that
//!   are not nested in another function. A nested function (also inside a class inside a
//!   function) is already counted in its enclosing function's score (see
//!   [`crate::extract::metrics`]), so sums do not cross a function boundary. Class rows (a
//!   sum over the methods) are not added either.
//! - `cc_max` / `cog_max`: the maximum over every function of the subtree.
//! - `findings`: per-node finding counts, inclusive (empty until
//!   [`Aggregates::set_findings`]).
//! - `score`: placeholder column for a later scoring step ([`Aggregates::set_scores`]).
//!
//! Memory: 20 bytes per node (plus 4 per node for each of findings and scores once set).

mod aggregate;
mod edges;

#[cfg(test)]
mod tests;

pub use aggregate::{Aggregates, NodeAgg};
pub use edges::{LevelEdges, LevelScratch, Rollup, RollupEdge, RollupStats};

use crate::index::{Index, NodeId, NodeKind};
use crate::resolve::{Confidence, res_flags};

/// A zoom level: which representative every node maps to (see the module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Level {
    /// The ancestor at depth `k` (0 = distributions); nodes above `k` map to themselves.
    Depth(u32),
    /// The package/module node: defs map to the file they are defined in, packages, modules
    /// and distributions to themselves.
    #[default]
    Module,
}

impl Level {
    /// The representative of node `v` at this level. O(1).
    #[inline]
    pub fn apply(self, ix: &Index, v: NodeId) -> NodeId {
        match self {
            Level::Depth(k) => {
                if ix.depth(v) <= k {
                    v
                } else {
                    ix.ancestor_at(v, k)
                }
            }
            Level::Module => {
                if ix.kind(v).is_def() {
                    ix.file(v).map_or(v, |f| ix.file_node(f))
                } else {
                    v
                }
            }
        }
    }

    /// Writes the representative of every node of `ix` into `map` (resized to the node
    /// count). One forward pass: parents come before children in pre-order.
    pub fn fill_map(self, ix: &Index, map: &mut Vec<NodeId>) {
        let n = ix.node_count() as usize;
        map.clear();
        map.reserve_exact(n);
        for v in 0..n as NodeId {
            let keep = match self {
                Level::Depth(k) => ix.depth(v) <= k,
                Level::Module => !ix.kind(v).is_def(),
            };
            let r = match ix.parent(v) {
                Some(p) if !keep => map[p as usize],
                _ => v,
            };
            map.push(r);
        }
    }
}

/// Maximum node depth of `ix` (`Level::Depth(k)` for `k` at or beyond it is the identity).
pub fn max_depth(ix: &Index) -> u32 {
    let mut d = 0;
    for v in 0..ix.node_count() {
        d = d.max(ix.depth(v));
    }
    d
}

/// Flags of an edge after adding import rows with flags `f`: AND of the edge-wide flags, OR of
/// `STAR`; everything else dropped (as the module edges of [`crate::resolve`]).
#[inline]
fn merge_import_flags(acc: u8, f: u8) -> u8 {
    (acc & f & res_flags::EDGE_ALL) | ((acc | f) & res_flags::STAR)
}

/// Kinds that add their lines to their parent's.
#[inline]
fn is_file_level(k: NodeKind) -> bool {
    matches!(k, NodeKind::Package | NodeKind::Module)
}

#[inline]
fn conf_of(v: u8) -> Confidence {
    Confidence::from_u8(v)
}
