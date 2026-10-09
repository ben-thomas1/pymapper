//! Force-directed layout: ForceAtlas2-style forces with Barnes-Hut repulsion over the own
//! [`QuadTree`], and Fruchterman-Reingold style cooling, stepped one iteration at a time so
//! the background worker can publish progressive snapshots and cancel between iterations.
//!
//! Forces on node `i` (mass `m_i = 1 + degree`):
//!
//! ```text
//!   repulsion   k_r m_i m_j / d        from every other node (Barnes-Hut, theta)
//!   attraction  w_e d                  along each edge e (w_e in (0, 1], log of the weight)
//!   gravity     k_g m_i                towards the origin (keeps components together)
//! ```
//!
//! Each iteration moves a node along its force by at most the temperature, which decays
//! geometrically from a tenth of the initial extent; the layout stops when the temperature is
//! below `min_temp` of the extent, after `max_iters`, or when the mean move is below
//! `tolerance` (all relative to the ideal edge length 1). Cost per iteration:
//! O(n log n) for the tree and the repulsion (parallel above [`PARALLEL_MIN`] nodes) plus
//! O(m) for the edges. Results do not depend on the thread count: every node's force is
//! summed by one thread in a fixed order.

use rayon::prelude::*;

use super::quadtree::QuadTree;

/// Repulsion runs in parallel from this many nodes.
pub const PARALLEL_MIN: usize = 1024;

/// Layout parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ForceParams {
    /// Barnes-Hut opening angle (larger: faster, coarser).
    pub theta: f32,
    pub repulsion: f32,
    pub gravity: f32,
    pub max_iters: u32,
    /// Temperature decay per iteration.
    pub cooling: f32,
    /// Stop below this temperature (fraction of the initial extent).
    pub min_temp: f32,
    /// Stop when the mean move per node is below this (edge lengths).
    pub tolerance: f32,
}

impl Default for ForceParams {
    fn default() -> ForceParams {
        ForceParams { theta: 0.9, repulsion: 1.0, gravity: 0.05, max_iters: 400, cooling: 0.985, min_temp: 0.002, tolerance: 0.002 }
    }
}

/// The graph to lay out: `n` nodes and weighted edges (weight in `(0, 1]`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ForceGraph {
    pub n: u32,
    pub edges: Vec<(u32, u32, f32)>,
}

impl ForceGraph {
    /// Node masses: 1 + degree.
    pub fn masses(&self) -> Vec<f32> {
        let mut m = vec![1.0f32; self.n as usize];
        for &(a, b, _) in &self.edges {
            m[a as usize] += 1.0;
            m[b as usize] += 1.0;
        }
        m
    }
}

/// Layout state between iterations.
#[derive(Debug, Clone)]
pub struct ForceLayout {
    pub pos: Vec<[f32; 2]>,
    disp: Vec<[f32; 2]>,
    mass: Vec<f32>,
    tree: QuadTree,
    pub params: ForceParams,
    pub iter: u32,
    pub temp: f32,
    t_min: f32,
    /// Mean move of the last iteration.
    pub last_move: f32,
    done: bool,
}

impl ForceLayout {
    /// Starts from `init` (one position per node; rescaled to an extent of about
    /// `sqrt(n)` edge lengths and centred). Coincident start positions are spread on a small
    /// deterministic spiral so no two nodes start on the same point.
    pub fn new(g: &ForceGraph, init: &[[f32; 2]], params: ForceParams) -> ForceLayout {
        let n = g.n as usize;
        assert_eq!(init.len(), n, "one start position per node");
        let mut pos = normalised(init, (n as f32).sqrt().max(1.0));
        spread_duplicates(&mut pos);
        // Break symmetries (treemap centres line up in rows and columns, and collinear starts
        // would never leave their line): a tiny deterministic offset per node.
        for (i, q) in pos.iter_mut().enumerate() {
            let h = (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let a = (h >> 40) as f32 * (std::f32::consts::TAU / (1u64 << 24) as f32);
            q[0] += 0.02 * a.cos();
            q[1] += 0.02 * a.sin();
        }
        let extent = extent(&pos).max(1.0);
        let temp = 0.1 * extent;
        ForceLayout {
            pos,
            disp: vec![[0.0; 2]; n],
            mass: g.masses(),
            tree: QuadTree::default(),
            params,
            iter: 0,
            temp,
            t_min: params.min_temp * extent,
            last_move: f32::INFINITY,
            done: n <= 1,
        }
    }

    /// The layout has converged (or ran out of iterations).
    #[inline]
    pub fn done(&self) -> bool {
        self.done
    }

    /// One iteration; returns the mean move.
    pub fn step(&mut self, g: &ForceGraph) -> f32 {
        if self.done {
            return 0.0;
        }
        let p = self.params;
        self.tree.build(&self.pos, &self.mass);
        let (tree, pos, mass) = (&self.tree, &self.pos, &self.mass);
        let force = |i: usize, d: &mut [f32; 2]| {
            let [fx, fy] = tree.repulsion(i as u32, p.theta, p.repulsion);
            let [x, y] = pos[i];
            let r = (x * x + y * y).sqrt();
            let g = if r > 1e-6 { p.gravity * mass[i] / r } else { 0.0 };
            *d = [fx - g * x, fy - g * y];
        };
        if pos.len() >= PARALLEL_MIN {
            self.disp.par_iter_mut().enumerate().for_each(|(i, d)| force(i, d));
        } else {
            self.disp.iter_mut().enumerate().for_each(|(i, d)| force(i, d));
        }
        for &(a, b, w) in &g.edges {
            let (pa, pb) = (self.pos[a as usize], self.pos[b as usize]);
            let (dx, dy) = ((pb[0] - pa[0]) * w, (pb[1] - pa[1]) * w);
            let da = &mut self.disp[a as usize];
            da[0] += dx;
            da[1] += dy;
            let db = &mut self.disp[b as usize];
            db[0] -= dx;
            db[1] -= dy;
        }
        let t = self.temp;
        let mut moved = 0.0f64;
        for (q, d) in self.pos.iter_mut().zip(&self.disp) {
            let len = (d[0] * d[0] + d[1] * d[1]).sqrt();
            if !(len > 0.0 && len.is_finite()) {
                continue;
            }
            let s = len.min(t) / len;
            q[0] += d[0] * s;
            q[1] += d[1] * s;
            moved += f64::from(len.min(t));
        }
        self.iter += 1;
        self.temp *= p.cooling;
        self.last_move = (moved / self.pos.len().max(1) as f64) as f32;
        self.done = self.iter >= p.max_iters || self.temp < self.t_min || self.last_move < p.tolerance;
        self.last_move
    }

    /// Runs to convergence.
    pub fn run(&mut self, g: &ForceGraph) {
        while !self.done {
            self.step(g);
        }
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        8 * (self.pos.capacity() + self.disp.capacity()) + 4 * self.mass.capacity() + self.tree.heap_bytes()
    }
}

/// Largest absolute coordinate.
pub fn extent(pos: &[[f32; 2]]) -> f32 {
    pos.iter().fold(0.0f32, |m, p| m.max(p[0].abs()).max(p[1].abs()))
}

/// `pos` centred on its bounding box and scaled so its larger side is `2 * size`
/// (non-finite positions become the origin).
pub fn normalised(pos: &[[f32; 2]], size: f32) -> Vec<[f32; 2]> {
    let (mut lo, mut hi) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
    for p in pos.iter().filter(|p| p[0].is_finite() && p[1].is_finite()) {
        lo = [lo[0].min(p[0]), lo[1].min(p[1])];
        hi = [hi[0].max(p[0]), hi[1].max(p[1])];
    }
    if lo[0] > hi[0] {
        return vec![[0.0; 2]; pos.len()];
    }
    let c = [(lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5];
    let half = ((hi[0] - lo[0]).max(hi[1] - lo[1]) * 0.5).max(f32::MIN_POSITIVE);
    let s = if half > 0.0 { size / half } else { 1.0 };
    pos.iter().map(|p| if p[0].is_finite() && p[1].is_finite() { [(p[0] - c[0]) * s, (p[1] - c[1]) * s] } else { [0.0; 2] }).collect()
}

/// Moves nodes that share a position onto a small golden-angle spiral around it (sorted, so
/// the result does not depend on hashing).
fn spread_duplicates(pos: &mut [[f32; 2]]) {
    let mut order: Vec<u32> = (0..pos.len() as u32).collect();
    order.sort_unstable_by(|&a, &b| {
        let (pa, pb) = (pos[a as usize], pos[b as usize]);
        pa[0].total_cmp(&pb[0]).then(pa[1].total_cmp(&pb[1])).then(a.cmp(&b))
    });
    let mut i = 0;
    while i < order.len() {
        let p = pos[order[i] as usize];
        let mut j = i + 1;
        while j < order.len() && pos[order[j] as usize] == p {
            j += 1;
        }
        for (k, &v) in order[i + 1..j].iter().enumerate() {
            let k = (k + 1) as f32;
            let r = 0.05 * k.sqrt();
            let a = k * 2.399_963;
            pos[v as usize] = [p[0] + r * a.cos(), p[1] + r * a.sin()];
        }
        i = j;
    }
}
