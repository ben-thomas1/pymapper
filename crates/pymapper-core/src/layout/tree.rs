//! The layout's view of the hierarchy: pre-order shape, effective weights, nodes by depth.

use std::hash::Hasher;
use std::ops::Range;

use super::Region;
use crate::index::{Index, NodeId};
use crate::intern::FxHasher;
use crate::rollup::Aggregates;

/// What sizes a node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SizeMetric {
    /// Lines of code (the default: stable geography).
    #[default]
    Loc,
    /// Classes and functions in the subtree.
    Symbols,
    /// Cyclomatic complexity summed over outermost functions.
    CcSum,
    /// Cognitive complexity summed over outermost functions.
    CogSum,
    /// Every leaf weighs [`TreeOptions::min_weight`] (a parent is the sum of its leaves).
    Uniform,
}

/// How [`LayoutTree::from_index`] weighs nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TreeOptions {
    pub metric: SizeMetric,
    /// Smallest weight of any node, so empty modules and zero-complexity functions still get
    /// a cell. 0 lets them collapse to zero area.
    pub min_weight: u32,
}

impl Default for TreeOptions {
    fn default() -> TreeOptions {
        TreeOptions { metric: SizeMetric::Loc, min_weight: 1 }
    }
}

/// The hierarchy as the layout needs it (see the module docs). Struct of arrays indexed by
/// node id (pre-order) unless noted; 18 bytes per node.
#[derive(Clone, Debug)]
pub struct LayoutTree {
    /// Subtree of `v` is `v..end[v]`.
    pub(crate) end: Vec<u32>,
    pub(crate) depth: Vec<u16>,
    /// Effective weight: max(metric, min_weight, sum of children).
    pub(crate) weight: Vec<u32>,
    /// Position of a node in its level.
    pub(crate) slot: Vec<u32>,
    /// Nodes grouped by depth, pre-order inside a depth: level `d` is
    /// `by_level[level_start[d]..level_start[d + 1]]`.
    pub(crate) by_level: Vec<NodeId>,
    pub(crate) level_start: Vec<u32>,
    /// Region of each depth-0 node, by slot.
    pub(crate) root_region: Vec<u8>,
    hash: u64,
}

impl LayoutTree {
    /// The tree of an index, weighed by `opts` from the aggregates of the same index.
    ///
    /// # Panics
    /// If `agg` does not have one row per node of `ix`.
    pub fn from_index(ix: &Index, agg: &Aggregates, opts: TreeOptions) -> LayoutTree {
        let n = ix.node_count() as usize;
        assert_eq!(agg.len(), n, "aggregates of another index");
        let mut end = Vec::with_capacity(n);
        let mut depth = Vec::with_capacity(n);
        let mut metric = Vec::with_capacity(n);
        let mut h = FxHasher::default();
        for v in 0..n as NodeId {
            end.push(ix.subtree(v).end);
            let d = ix.depth(v) as u16;
            depth.push(d);
            let m = match opts.metric {
                SizeMetric::Loc => agg.loc()[v as usize],
                SizeMetric::Symbols => agg.symbols()[v as usize],
                SizeMetric::CcSum => agg.cc_sum()[v as usize],
                SizeMetric::CogSum => agg.cog_sum()[v as usize],
                SizeMetric::Uniform => 0,
            };
            metric.push(m);
            let name = ix.name(v);
            h.write_u8(ix.kind(v) as u8);
            h.write_u32(name.len() as u32);
            h.write(name);
            h.write_u16(ix.dup(v));
        }
        // Distributions are the depth-0 nodes; their slots follow node order.
        let mut roots = Vec::with_capacity(ix.groups().len());
        for v in 0..n as NodeId {
            if depth[v as usize] == 0 {
                roots.push(v);
            }
        }
        // Index invariant: every group has exactly one distribution node (`group_node`), and
        // every depth-0 node is one. A root no group maps to would stay in the main region.
        debug_assert_eq!(roots.len(), ix.groups().len(), "depth-0 nodes are the groups' distributions");
        let mut region = vec![Region::Main as u8; roots.len()];
        for (g, grp) in ix.groups().iter().enumerate() {
            let v = ix.group_node(g as u32);
            debug_assert!(roots.binary_search(&v).is_ok(), "group {g} maps to a depth-0 node");
            if let Ok(s) = roots.binary_search(&v) {
                region[s] = Region::of(grp.origin) as u8;
            }
        }
        LayoutTree::build(end, depth, metric, region, opts.min_weight, h.finish())
    }

    /// A tree from a pre-order list of depths (each node's depth at most one more than the
    /// previous node's, the first 0) with each node's metric and the region of each depth-0
    /// node in order. For tests, benches and callers without an [`Index`].
    ///
    /// # Panics
    /// On an invalid depth sequence, mismatched lengths or a wrong number of regions.
    pub fn from_preorder(depths: &[u16], metric: &[u32], regions: &[Region], min_weight: u32) -> LayoutTree {
        let n = depths.len();
        assert_eq!(metric.len(), n, "one metric per node");
        assert!(n == 0 || depths[0] == 0, "first node must be a root");
        let mut end = vec![0u32; n];
        // Open ancestors; closing a node sets its end.
        let mut stack: Vec<u32> = Vec::new();
        for (v, &d) in depths.iter().enumerate() {
            assert!(d as usize <= stack.len(), "depth jumps by more than one at node {v}");
            while stack.len() > d as usize {
                let u = stack.pop().unwrap_or_default();
                end[u as usize] = v as u32;
            }
            stack.push(v as u32);
        }
        for u in stack {
            end[u as usize] = n as u32;
        }
        let roots = depths.iter().filter(|&&d| d == 0).count();
        assert_eq!(regions.len(), roots, "one region per root");
        let region = regions.iter().map(|&r| r as u8).collect();
        LayoutTree::build(end, depths.to_vec(), metric.to_vec(), region, min_weight, 0)
    }

    fn build(end: Vec<u32>, depth: Vec<u16>, metric: Vec<u32>, root_region: Vec<u8>, min_weight: u32, seed: u64) -> LayoutTree {
        let n = end.len();
        // Effective weights, children before parents (reverse pre-order); `metric` becomes
        // the weight column in place.
        let mut weight = metric;
        for v in (0..n).rev() {
            let mut sum = 0u64;
            let mut c = v + 1;
            while c < end[v] as usize {
                sum += u64::from(weight[c]);
                c = end[c] as usize;
            }
            let s = sum.min(u64::from(u32::MAX)) as u32;
            weight[v] = weight[v].max(min_weight).max(s);
        }
        // Counting sort of nodes by depth (stable: pre-order inside a depth).
        let levels = depth.iter().copied().max().map_or(0, |d| d as usize + 1);
        let mut level_start = vec![0u32; levels + 1];
        for &d in &depth {
            level_start[d as usize + 1] += 1;
        }
        for d in 0..levels {
            level_start[d + 1] += level_start[d];
        }
        let mut fill: Vec<u32> = level_start[..levels].to_vec();
        let mut by_level = vec![0u32; n];
        let mut slot = vec![0u32; n];
        for v in 0..n {
            let d = depth[v] as usize;
            let p = fill[d];
            by_level[p as usize] = v as u32;
            slot[v] = p - level_start[d];
            fill[d] = p + 1;
        }
        let mut h = FxHasher::default();
        h.write_u64(seed);
        h.write_u64(n as u64);
        for v in 0..n {
            h.write_u16(depth[v]);
            h.write_u32(weight[v]);
        }
        h.write(&root_region);
        LayoutTree { end, depth, weight, slot, by_level, level_start, root_region, hash: h.finish() }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.end.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.end.is_empty()
    }

    /// Number of depths (levels).
    #[inline]
    pub fn levels(&self) -> u32 {
        self.level_start.len().saturating_sub(1) as u32
    }

    /// Nodes at depth `d`, in pre-order (empty past the deepest level).
    #[inline]
    pub fn level(&self, d: u32) -> &[NodeId] {
        let d = d as usize;
        if d + 1 >= self.level_start.len() {
            return &[];
        }
        &self.by_level[self.level_start[d] as usize..self.level_start[d + 1] as usize]
    }

    #[inline]
    pub fn depth(&self, v: NodeId) -> u32 {
        u32::from(self.depth[v as usize])
    }

    /// Position of `v` in [`LayoutTree::level`]`(depth(v))`.
    #[inline]
    pub fn slot(&self, v: NodeId) -> u32 {
        self.slot[v as usize]
    }

    #[inline]
    pub fn weight(&self, v: NodeId) -> u32 {
        self.weight[v as usize]
    }

    #[inline]
    pub fn subtree(&self, v: NodeId) -> Range<NodeId> {
        v..self.end[v as usize]
    }

    #[inline]
    pub fn has_children(&self, v: NodeId) -> bool {
        self.end[v as usize] > v + 1
    }

    /// Region of the depth-0 ancestor of `v`'s level-0 slot `s`.
    #[inline]
    pub fn root_region(&self, s: u32) -> Region {
        Region::from_u8(self.root_region[s as usize])
    }

    /// Hash of the hierarchy: shape, kinds, names and duplicate ordinals (from an index),
    /// weights and root regions. Equal trees hash equal on every run.
    #[inline]
    pub fn hash(&self) -> u64 {
        self.hash
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        4 * (self.end.capacity() + self.weight.capacity() + self.slot.capacity() + self.by_level.capacity())
            + 2 * self.depth.capacity()
            + 4 * self.level_start.capacity()
            + self.root_region.capacity()
    }
}
