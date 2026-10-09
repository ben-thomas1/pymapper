//! Lookup tables of one published scene, for interaction (built once per scene, on the map
//! worker thread):
//!
//! ```text
//!   cell[v]                 node -> its cell in scene.rects (label carriers skipped)
//!   drawn                   drawn edges (indices into CodeMap::edges), ascending
//!   seg_start / segs        drawn edge k -> its Bezier segments in scene.curves (CSR)
//!   by_src / by_dst         drawn edges sorted by (src, dst) / (dst, src): the edges with an
//!                           endpoint in a subtree are one binary-searched range (pre-order ids)
//! ```
//!
//! Memory: 4 bytes per node, 16 bytes per drawn edge, 4 bytes per segment. Build: O(nodes +
//! rect instances + segments log drawn + drawn log drawn).

use std::ops::Range;

use pymapper_core::index::NodeId;

use crate::codemap::edges::MapEdge;
use crate::map::scene::Scene;

/// "No cell" in [`SceneIndex::cell`].
pub const NO_CELL: u32 = u32::MAX;

/// See the module docs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SceneIndex {
    cell: Vec<u32>,
    drawn: Vec<u32>,
    seg_start: Vec<u32>,
    segs: Vec<u32>,
    by_src: Vec<u32>,
    by_dst: Vec<u32>,
}

/// How an edge touches a node set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Dir {
    /// From inside to outside (callees / imported).
    Out,
    /// From outside to inside (callers / importers).
    In,
    /// Both endpoints inside.
    Internal,
}

impl SceneIndex {
    /// Indexes `scene` (built from `edges` with drawn edge list `drawn`, ascending; curve ids
    /// are edge indices) for an index of `nodes` nodes. Rect ids are node ids.
    pub fn build(scene: &Scene, nodes: u32, edges: &[MapEdge], drawn: &[u32]) -> SceneIndex {
        debug_assert!(drawn.windows(2).all(|w| w[0] < w[1]), "drawn edges must ascend");
        let mut cell = vec![NO_CELL; nodes as usize];
        let rects = &scene.rects;
        for (i, (inst, &id)) in rects.inst.iter().zip(&rects.id).enumerate() {
            // Cells have a visible fill; label carriers are fully transparent.
            if inst.fill.0[3] == 0 && inst.stroke.0[3] == 0 {
                continue;
            }
            if let Some(c) = cell.get_mut(id as usize)
                && *c == NO_CELL
            {
                *c = i as u32;
            }
        }
        // Segments per drawn edge (counting sort by position in `drawn`).
        let curves = &scene.curves;
        let pos: Vec<u32> = curves.id.iter().map(|&id| drawn.binary_search(&id).map_or(u32::MAX, |p| p as u32)).collect();
        let mut seg_start = vec![0u32; drawn.len() + 1];
        for &p in &pos {
            if p != u32::MAX {
                seg_start[p as usize + 1] += 1;
            }
        }
        for k in 1..seg_start.len() {
            seg_start[k] += seg_start[k - 1];
        }
        let mut fill = seg_start.clone();
        let mut segs = vec![0u32; *seg_start.last().unwrap_or(&0) as usize];
        for (i, &p) in pos.iter().enumerate() {
            if p != u32::MAX {
                let f = &mut fill[p as usize];
                segs[*f as usize] = i as u32;
                *f += 1;
            }
        }
        drop((pos, fill));
        let mut by_src: Vec<u32> = (0..drawn.len() as u32).collect();
        let mut by_dst = by_src.clone();
        let e = |k: u32| &edges[drawn[k as usize] as usize];
        by_src.sort_unstable_by_key(|&k| (e(k).src, e(k).dst, k));
        by_dst.sort_unstable_by_key(|&k| (e(k).dst, e(k).src, k));
        SceneIndex { cell, drawn: drawn.to_vec(), seg_start, segs, by_src, by_dst }
    }

    /// The cell of node `v` in the scene's rects ([`NO_CELL`] if not drawn).
    #[inline]
    pub fn cell(&self, v: NodeId) -> u32 {
        self.cell.get(v as usize).copied().unwrap_or(NO_CELL)
    }

    /// Drawn edges (indices into the code map's edge table), ascending.
    #[inline]
    pub fn drawn(&self) -> &[u32] {
        &self.drawn
    }

    /// Position of edge `e` among the drawn edges.
    #[inline]
    pub fn drawn_pos(&self, e: u32) -> Option<usize> {
        self.drawn.binary_search(&e).ok()
    }

    /// Curve instances (indices into `scene.curves`) of edge `e` (empty if not drawn).
    pub fn segments(&self, e: u32) -> &[u32] {
        match self.drawn_pos(e) {
            Some(k) => &self.segs[self.seg_start[k] as usize..self.seg_start[k + 1] as usize],
            None => &[],
        }
    }

    /// Calls `f(edge, dir)` for every drawn edge with an endpoint in the node range `r` (a
    /// subtree), each edge once. O(log drawn + matches).
    pub fn edges_touching(&self, edges: &[MapEdge], r: Range<NodeId>, mut f: impl FnMut(u32, Dir)) {
        let inside = |v: NodeId| r.contains(&v);
        let e = |k: u32| &edges[self.drawn[k as usize] as usize];
        let lo = self.by_src.partition_point(|&k| e(k).src < r.start);
        let hi = lo + self.by_src[lo..].partition_point(|&k| e(k).src < r.end);
        for &k in &self.by_src[lo..hi] {
            let dir = if inside(e(k).dst) { Dir::Internal } else { Dir::Out };
            f(self.drawn[k as usize], dir);
        }
        let lo = self.by_dst.partition_point(|&k| e(k).dst < r.start);
        let hi = lo + self.by_dst[lo..].partition_point(|&k| e(k).dst < r.end);
        for &k in &self.by_dst[lo..hi] {
            if !inside(e(k).src) {
                f(self.drawn[k as usize], Dir::In);
            }
        }
    }

    /// Heap bytes.
    pub fn heap_bytes(&self) -> usize {
        4 * (self.cell.capacity() + self.drawn.capacity() + self.seg_start.capacity() + self.segs.capacity() + self.by_src.capacity() + self.by_dst.capacity())
    }
}
