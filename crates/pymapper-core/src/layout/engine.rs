//! Level-by-level layout: regions and distributions at level 0, then each level from the rects
//! of the one above.

use std::sync::Arc;

use rayon::prelude::*;

use super::squarify::{R64, squarify};
use super::{LayoutConfig, LayoutTree, Order, Rect, Region};
use crate::index::NodeId;

/// Levels with at most this many nodes are laid out on the calling thread.
const PAR_MIN_NODES: usize = 32 * 1024;
/// Parents per parallel task (at least).
const PAR_MIN_PARENTS: usize = 2048;
/// Id of the filler cell (a parent's weight beyond its children) when sorting.
const FILLER: u32 = u32::MAX;

/// A treemap layout of a [`LayoutTree`], computed lazily by depth (see the module docs).
#[derive(Clone, Debug)]
pub struct Layout {
    tree: Arc<LayoutTree>,
    cfg: LayoutConfig,
    regions: [Option<Rect>; 3],
    /// Rects of level `d`, indexed by slot.
    levels: Vec<Arc<[Rect]>>,
}

impl Layout {
    /// An empty layout (no level computed yet; the regions are known).
    pub fn new(tree: Arc<LayoutTree>, cfg: LayoutConfig) -> Layout {
        let regions = regions(&tree, &cfg);
        Layout { tree, cfg, regions, levels: Vec::new() }
    }

    /// A layout starting from previously computed levels (the on-disk cache), which must have
    /// been computed for the same tree and config. Levels with the wrong length, and every
    /// level after one, are dropped.
    pub fn with_levels(tree: Arc<LayoutTree>, cfg: LayoutConfig, levels: Vec<Arc<[Rect]>>) -> Layout {
        let mut l = Layout::new(tree, cfg);
        for (d, lv) in levels.into_iter().enumerate() {
            if d as u32 >= l.tree.levels() || lv.len() != l.tree.level(d as u32).len() {
                break;
            }
            l.levels.push(lv);
        }
        l
    }

    #[inline]
    pub fn tree(&self) -> &Arc<LayoutTree> {
        &self.tree
    }

    #[inline]
    pub fn config(&self) -> &LayoutConfig {
        &self.cfg
    }

    /// Number of levels computed (levels `0..computed()` are available).
    #[inline]
    pub fn computed(&self) -> u32 {
        self.levels.len() as u32
    }

    /// Every level of the tree is computed.
    #[inline]
    pub fn is_complete(&self) -> bool {
        self.computed() >= self.tree.levels()
    }

    /// Computes levels up to and including `depth` (clamped to the deepest level).
    pub fn ensure(&mut self, depth: u32) {
        while self.computed() <= depth && !self.is_complete() {
            self.step();
        }
    }

    /// Computes the next level; returns it, or `None` when every level is done.
    pub fn step(&mut self) -> Option<&Arc<[Rect]>> {
        if self.is_complete() {
            return None;
        }
        let d = self.computed();
        let lv = compute_level(&self.tree, &self.cfg, &self.regions, self.levels.last().map(|l| &l[..]), d);
        self.levels.push(lv);
        self.levels.last()
    }

    /// Rects of level `d` by slot (see [`LayoutTree::level`]), if computed.
    #[inline]
    pub fn level(&self, d: u32) -> Option<&Arc<[Rect]>> {
        self.levels.get(d as usize)
    }

    /// All computed levels.
    #[inline]
    pub fn levels(&self) -> &[Arc<[Rect]>] {
        &self.levels
    }

    /// Rect of node `v`, if its level is computed.
    #[inline]
    pub fn rect(&self, v: NodeId) -> Option<Rect> {
        let lv = self.levels.get(self.tree.depth(v) as usize)?;
        Some(lv[self.tree.slot(v) as usize])
    }

    /// Content rect of `v` (where its children go), if its level is computed.
    #[inline]
    pub fn content(&self, v: NodeId) -> Option<Rect> {
        self.rect(v).map(|r| self.cfg.content(&r))
    }

    /// The region `v` lies in (that of its distribution).
    pub fn region_of(&self, v: NodeId) -> Region {
        region_of(&self.tree, v)
    }

    /// `v` belongs to third-party or stdlib code (draw it muted).
    #[inline]
    pub fn muted(&self, v: NodeId) -> bool {
        self.region_of(v) != Region::Main
    }

    /// Rect of a region, `None` when it has no distribution.
    #[inline]
    pub fn region(&self, r: Region) -> Option<Rect> {
        self.regions[r as usize]
    }

    /// Heap bytes of the computed levels.
    pub fn heap_bytes(&self) -> usize {
        self.levels.iter().map(|l| l.len() * size_of::<Rect>()).sum::<usize>()
            + self.levels.capacity() * size_of::<Arc<[Rect]>>()
    }
}

/// A zeroed level of `n` rects, allocated once in its final place (no `Vec` -> `Arc` copy:
/// `repeat_n` has an exact length).
pub(super) fn new_level(n: usize) -> Arc<[Rect]> {
    std::iter::repeat_n(Rect::default(), n).collect()
}

/// Level `d` of `tree`: level 0 from the regions, deeper levels from `prev` (level `d - 1`).
pub(super) fn compute_level(
    tree: &LayoutTree,
    cfg: &LayoutConfig,
    regions: &[Option<Rect>; 3],
    prev: Option<&[Rect]>,
    d: u32,
) -> Arc<[Rect]> {
    let mut lv = new_level(tree.level(d).len());
    let out = Arc::get_mut(&mut lv).unwrap_or_else(|| unreachable!("fresh Arc"));
    match prev {
        Some(prev) if d > 0 => next_level(tree, cfg, prev, d, out),
        _ => level0(tree, cfg, regions, out),
    }
    lv
}

pub(super) fn region_of(tree: &LayoutTree, v: NodeId) -> Region {
    let roots = tree.level(0);
    let s = roots.partition_point(|&r| r <= v);
    if s == 0 {
        return Region::Main;
    }
    tree.root_region(s as u32 - 1)
}

#[inline]
fn r64(r: &Rect) -> R64 {
    R64 { x0: f64::from(r.x0), y0: f64::from(r.y0), x1: f64::from(r.x1), y1: f64::from(r.y1) }
}

#[inline]
fn r32(r: &R64) -> Rect {
    Rect::new(r.x0 as f32, r.y0 as f32, r.x1 as f32, r.y1 as f32)
}

/// Splits `r` along its long side into a part of share `a` (first) and the rest, with `gap`
/// between them (the gap never takes more than half the side).
fn split(r: R64, a: f64, gap: f64) -> (R64, R64) {
    let horiz = r.w() >= r.h();
    let len = if horiz { r.w() } else { r.h() };
    let gap = gap.clamp(0.0, len * 0.5);
    let first = (len - gap) * a.clamp(0.0, 1.0);
    if horiz {
        let x = r.x0 + first;
        (R64 { x1: x, ..r }, R64 { x0: x + gap, ..r })
    } else {
        let y = r.y0 + first;
        (R64 { y1: y, ..r }, R64 { y0: y + gap, ..r })
    }
}

/// Region rects (see the module docs): main, third-party, stdlib.
fn regions(tree: &LayoutTree, cfg: &LayoutConfig) -> [Option<Rect>; 3] {
    let mut w = [0u64; 3];
    let mut present = [false; 3];
    for s in 0..tree.level(0).len() as u32 {
        let r = tree.root_region(s) as usize;
        present[r] = true;
        w[r] += u64::from(tree.weight(tree.level(0)[s as usize]));
    }
    let world = r64(&cfg.world);
    let gap = f64::from(cfg.region_gap);
    let ext_present = present[1] || present[2];
    let (main, ext) = match (present[0], ext_present) {
        (false, false) => return [None; 3],
        (true, false) => (Some(world), None),
        (false, true) => (None, Some(world)),
        (true, true) => {
            let ext_w = (w[1] + w[2]) as f64;
            let total = w[0] as f64 + ext_w;
            let cap = f64::from(cfg.external_share).clamp(0.0, 1.0);
            let share = if total > 0.0 { cap.min(ext_w / total) } else { cap };
            let (m, e) = split(world, 1.0 - share, gap);
            (Some(m), Some(e))
        }
    };
    let mut out = [main.as_ref().map(r32), None, None];
    if let Some(e) = ext {
        match (present[1], present[2]) {
            (true, true) => {
                let t = (w[1] + w[2]) as f64;
                let a = if t > 0.0 { w[1] as f64 / t } else { 0.5 };
                let (tp, sl) = split(e, a, gap);
                out[1] = Some(r32(&tp));
                out[2] = Some(r32(&sl));
            }
            (true, false) => out[1] = Some(r32(&e)),
            _ => out[2] = Some(r32(&e)),
        }
    }
    out
}

/// Reusable buffers of one thread.
#[derive(Default)]
struct Scratch {
    /// (weight, node id) of the cells of one parent, then sorted.
    cells: Vec<(u32, u32)>,
    weights: Vec<f64>,
    rects: Vec<R64>,
}

impl Scratch {
    /// Orders the gathered cells, squarifies them into `r` and calls `put(id, rect)` for every
    /// cell but the filler.
    fn lay(&mut self, order: Order, r: &Rect, mut put: impl FnMut(u32, Rect)) {
        if order == Order::Size {
            self.cells.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        self.weights.clear();
        self.weights.extend(self.cells.iter().map(|c| f64::from(c.0)));
        self.rects.clear();
        self.rects.resize(self.cells.len(), r64(r));
        squarify(&self.weights, r64(r), &mut self.rects);
        for (c, q) in self.cells.iter().zip(&self.rects) {
            if c.1 != FILLER {
                put(c.1, r32(q));
            }
        }
    }
}

/// Distributions into their region rects.
fn level0(tree: &LayoutTree, cfg: &LayoutConfig, regions: &[Option<Rect>; 3], out: &mut [Rect]) {
    let roots = tree.level(0);
    let mut sc = Scratch::default();
    for (ri, reg) in regions.iter().enumerate() {
        let Some(reg) = reg else { continue };
        sc.cells.clear();
        for (s, &v) in roots.iter().enumerate() {
            if tree.root_region(s as u32) as usize == ri {
                // The id is the slot here, so `put` can index `out` directly.
                sc.cells.push((tree.weight(v), s as u32));
            }
        }
        sc.lay(cfg.order, reg, |s, r| out[s as usize] = r);
    }
}

/// Level `d` (>= 1) from the rects of level `d - 1`.
fn next_level(tree: &LayoutTree, cfg: &LayoutConfig, prev: &[Rect], d: u32, out: &mut [Rect]) {
    let parents = tree.level(d - 1);
    let nodes = tree.level(d);
    if nodes.len() <= PAR_MIN_NODES {
        lay_parents(tree, cfg, prev, parents, out, 0);
        return;
    }
    // Parallel over chunks of parents: a chunk's children are a contiguous run of the level,
    // starting at the first depth-`d` node after its first parent.
    let threads = rayon::current_num_threads().max(1);
    let per = parents.len().div_ceil(threads * 8).max(PAR_MIN_PARENTS);
    let mut tasks = Vec::with_capacity(parents.len().div_ceil(per));
    let mut rest: &mut [Rect] = out;
    let mut base = 0usize;
    let mut i = 0usize;
    while i < parents.len() {
        let j = (i + per).min(parents.len());
        let stop = if j < parents.len() { nodes.partition_point(|&c| c < parents[j]) } else { nodes.len() };
        let (head, tail) = std::mem::take(&mut rest).split_at_mut(stop - base);
        tasks.push((&parents[i..j], head, base));
        rest = tail;
        base = stop;
        i = j;
    }
    tasks.into_par_iter().for_each(|(ps, o, base)| lay_parents(tree, cfg, prev, ps, o, base));
}

/// Lays out the children of `parents` into `out`, which holds the level slots from `base`.
fn lay_parents(tree: &LayoutTree, cfg: &LayoutConfig, prev: &[Rect], parents: &[NodeId], out: &mut [Rect], base: usize) {
    let mut sc = Scratch::default();
    for &p in parents {
        if !tree.has_children(p) {
            continue;
        }
        sc.cells.clear();
        let end = tree.subtree(p).end;
        let mut sum = 0u64;
        let mut c = p + 1;
        while c < end {
            let w = tree.weight(c);
            sum += u64::from(w);
            sc.cells.push((w, c));
            c = tree.subtree(c).end;
        }
        let pw = u64::from(tree.weight(p));
        if pw > sum {
            sc.cells.push(((pw - sum) as u32, FILLER));
        }
        let content = cfg.content(&prev[tree.slot(p) as usize]);
        sc.lay(cfg.order, &content, |c, r| out[tree.slot(c) as usize - base] = r);
    }
}
