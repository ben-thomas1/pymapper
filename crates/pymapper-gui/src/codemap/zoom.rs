//! Semantic zoom: which hierarchy cut the map shows at each LOD band.
//!
//! A node's children are revealed once the node is [`REVEAL_PX`] physical pixels big on
//! screen (the square root of its area), so the rollup level follows on-screen size per
//! subtree, not one global depth. Per node:
//!
//! ```text
//!   rev[v]   first band in which v is shown: 0 for distributions, else open[parent(v)]
//!   open[v]  first band in which v's children are shown (UNBOUNDED for leaves and for
//!            nodes whose children are not laid out); open[v] >= rev[v]
//! ```
//!
//! The *cut* at band `b` maps every node to its deepest shown ancestor-or-self. A shown node
//! `x` is an edge endpoint in one of two states:
//!
//! - collapsed (`rev[x] <= b < open[x]`): it stands for its whole subtree;
//! - open (`b >= open[x]`): its children are shown and stand for themselves, so `x` stands
//!   only for its own code (module-level statements, a class body).
//!
//! The edge `A -> B` of the cut therefore aggregates the same base pairs in every band where
//! both endpoints keep their state, i.e. on the intersection of their state intervals. Each
//! such edge is emitted once, at the first band of that interval, with that interval as its
//! LOD: the renderer then switches rollup levels per band with no CPU work beyond its band
//! filter (see [`crate::map`]).

use pymapper_core::index::{Index, NodeId};
use pymapper_core::layout::{Layout, Rect};

use crate::map::scene::{Band, LOD_UNBOUNDED, Lod};

/// A node's children are shown once the node is this many physical pixels big (square root
/// of its on-screen area).
pub const REVEAL_PX: f32 = 160.0;

/// Reveal bands of every node (see the module docs). 2 bytes per node.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reveal {
    pub rev: Vec<Band>,
    pub open: Vec<Band>,
}

/// State interval of a cut endpoint at some band.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// Shown open (children visible): the node stands only for its own code.
    pub open: bool,
    /// Bands in which the node keeps this state.
    pub lod: Lod,
}

impl Reveal {
    /// Reveal bands of `ix`'s nodes from the computed levels of `layout` (a layout of the same
    /// index). Nodes in levels not computed yet are never shown (`rev = UNBOUNDED`), and
    /// their parents never open. One pre-order pass.
    pub fn new(ix: &Index, layout: &Layout) -> Reveal {
        let n = ix.node_count() as usize;
        let tree = layout.tree();
        assert_eq!(tree.len(), n, "layout of another index");
        let computed = layout.computed();
        let mut rev = Vec::with_capacity(n);
        let mut open = Vec::with_capacity(n);
        for v in 0..n as NodeId {
            let d = ix.depth(v);
            let r = match ix.parent(v) {
                None if d < computed => 0,
                Some(p) => open[p as usize],
                None => LOD_UNBOUNDED,
            };
            rev.push(r);
            let o = if r == LOD_UNBOUNDED || d + 1 >= computed || !tree.has_children(v) {
                LOD_UNBOUNDED
            } else {
                layout.rect(v).map_or(LOD_UNBOUNDED, |rect| reveal_band(&rect).max(r))
            };
            open.push(o);
        }
        Reveal { rev, open }
    }

    /// A copy whose nodes below `depth` in third-party and stdlib code never open (edges into
    /// external code stop at that depth: see [`super::edges`]). `external(v)` tells external
    /// nodes.
    pub fn clamped(&self, ix: &Index, depth: u32, external: impl Fn(NodeId) -> bool) -> Reveal {
        let mut r = self.clone();
        for v in 0..ix.node_count() {
            let d = ix.depth(v);
            if d < depth || !external(v) {
                continue;
            }
            let i = v as usize;
            r.open[i] = LOD_UNBOUNDED;
            if d > depth {
                r.rev[i] = LOD_UNBOUNDED;
            }
        }
        r
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.rev.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.rev.is_empty()
    }

    /// Every band at which the cut changes, ascending (the first is 0 for a non-empty index).
    pub fn change_bands(&self) -> Vec<Band> {
        let mut seen = [false; 256];
        for (&r, &o) in self.rev.iter().zip(&self.open) {
            seen[usize::from(r)] = true;
            seen[usize::from(o)] = true;
        }
        seen[usize::from(LOD_UNBOUNDED)] = false;
        (0..=255u8).filter(|&b| seen[usize::from(b)]).collect()
    }

    /// The cut at band `b` into `map` (resized): the deepest ancestor-or-self of every node
    /// that is shown at `b`. One pre-order pass; distributions are always shown.
    pub fn fill_cut(&self, ix: &Index, b: Band, map: &mut Vec<NodeId>) {
        map.clear();
        map.reserve(self.rev.len());
        for v in 0..self.rev.len() as NodeId {
            let m = match ix.parent(v) {
                Some(p) if self.rev[v as usize] > b => map[p as usize],
                _ => v,
            };
            map.push(m);
        }
    }

    /// State of shown node `x` at band `b` (`rev[x] <= b`).
    #[inline]
    pub fn endpoint(&self, x: NodeId, b: Band) -> Endpoint {
        let (r, o) = (self.rev[x as usize], self.open[x as usize]);
        debug_assert!(r <= b, "node {x} is not shown at band {b}");
        if o <= b {
            Endpoint { open: true, lod: Lod { min: o, max: LOD_UNBOUNDED } }
        } else {
            Endpoint { open: false, lod: Lod { min: r, max: o } }
        }
    }

    /// Bands in which rect `v` is drawn: from its reveal band, once it is at least
    /// `min_px` pixels (its auto LOD `size_lod`).
    #[inline]
    pub fn rect_lod(&self, v: NodeId, size_lod: Lod) -> Lod {
        Lod { min: self.rev[v as usize].max(size_lod.min), max: size_lod.max }
    }
}

/// The band from which a rect is [`REVEAL_PX`] big (square root of its area).
pub fn reveal_band(r: &Rect) -> Band {
    let side = (r.w().max(0.0) * r.h().max(0.0)).sqrt();
    Lod::for_size(side, REVEAL_PX).min
}
