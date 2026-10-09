//! Hierarchical edge bundling (Holten 2006) over the treemap.
//!
//! An edge `A -> B` is routed along the hierarchy: its control polygon is `A`'s anchor, the
//! centres of `A`'s ancestors up to the lowest common ancestor, the LCA's centre, the
//! centres of `B`'s ancestors down from it, and `B`'s anchor. Edges between the same two
//! subtrees share their inner control points, so they bundle. The polygon is straightened
//! by the bundling strength `beta` (1 = follow the hierarchy, 0 = straight line), thinned
//! to at most [`MAX_POINTS`] points, and drawn as a cubic B-spline converted to Bezier
//! segments (one curve instance each):
//!
//! ```text
//!   2 points  one straight segment
//!   3 points  one cubic (degree-elevated quadratic through the middle control point)
//!   n points  n - 1 uniform B-spline segments, clamped to the endpoints
//! ```
//!
//! A collapsed endpoint is anchored at its rect's centre; an open one (it stands for its own
//! code while its children are shown) at the centre of its header band, where its label is.

use pymapper_core::index::{Index, NodeId};
use pymapper_core::layout::{Layout, Rect};

/// Most control points per edge (so at most `MAX_POINTS - 1` segments).
pub const MAX_POINTS: usize = 8;

/// Default bundling strength.
pub const DEFAULT_BETA: f32 = 0.85;

pub type Point = [f32; 2];
/// A cubic Bezier: `p0, c0, c1, p1`.
pub type Cubic = [Point; 4];

#[inline]
fn center(r: &Rect) -> Point {
    [(r.x0 + r.x1) * 0.5, (r.y0 + r.y1) * 0.5]
}

#[inline]
fn lerp(a: Point, b: Point, t: f32) -> Point {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

/// Reusable buffers for [`Bundler::route`].
#[derive(Debug, Default)]
pub struct Bundler {
    left: Vec<NodeId>,
    right: Vec<NodeId>,
    pts: Vec<Point>,
    thin: Vec<Point>,
}

impl Bundler {
    /// Appends the Bezier segments of the edge `src -> dst` to `out` and returns how many.
    /// `*_open`: the endpoint is shown open (anchored at its header band). Returns 0 when an
    /// endpoint has no rect in `layout`.
    #[allow(clippy::too_many_arguments)]
    pub fn route(&mut self, ix: &Index, layout: &Layout, src: NodeId, src_open: bool, dst: NodeId, dst_open: bool, beta: f32, out: &mut Vec<Cubic>) -> usize {
        if !self.polygon(ix, layout, src, src_open, dst, dst_open) {
            return 0;
        }
        straighten(&mut self.pts, beta);
        thin(&self.pts, MAX_POINTS, &mut self.thin);
        spline(&self.thin, out)
    }

    /// The control polygon into `self.pts` (see [`Bundler::points`]); false if a needed rect is
    /// missing.
    pub(crate) fn polygon(&mut self, ix: &Index, layout: &Layout, src: NodeId, src_open: bool, dst: NodeId, dst_open: bool) -> bool {
        let anchor = |v: NodeId, open: bool| -> Option<Point> {
            let r = layout.rect(v)?;
            if open {
                let h = layout.config().header_band(&r);
                if h.h() > 0.0 {
                    return Some(center(&h));
                }
            }
            Some(center(&r))
        };
        let (Some(a0), Some(b0)) = (anchor(src, src_open), anchor(dst, dst_open)) else { return false };
        self.left.clear();
        self.right.clear();
        let (mut a, mut b) = (src, dst);
        while ix.depth(a) > ix.depth(b) {
            let Some(p) = ix.parent(a) else { break };
            a = p;
            self.left.push(a);
        }
        while ix.depth(b) > ix.depth(a) {
            let Some(p) = ix.parent(b) else { break };
            b = p;
            self.right.push(b);
        }
        let mut lca = None;
        loop {
            if a == b {
                lca = Some(a);
                break;
            }
            match (ix.parent(a), ix.parent(b)) {
                (Some(pa), Some(pb)) => {
                    a = pa;
                    b = pb;
                    self.left.push(a);
                    self.right.push(b);
                }
                _ => break,
            }
        }
        if let Some(l) = lca {
            // The LCA ends both climbs (or is an endpoint itself): keep it once, in the middle,
            // unless it is an endpoint.
            if self.left.last() == Some(&l) {
                self.left.pop();
            }
            if self.right.last() == Some(&l) {
                self.right.pop();
            }
        }
        self.pts.clear();
        self.pts.push(a0);
        for &v in &self.left {
            let Some(r) = layout.rect(v) else { return false };
            self.pts.push(center(&r));
        }
        match lca {
            Some(l) if l != src && l != dst => {
                let Some(r) = layout.rect(l) else { return false };
                self.pts.push(center(&r));
            }
            Some(_) => {}
            None => {
                // Different distributions: the regions act as the roots of the forest, so
                // edges between distributions bundle through the region centres.
                let (ra, rb) = (layout.region_of(a), layout.region_of(b));
                if let Some(r) = layout.region(ra) {
                    self.pts.push(center(&r));
                }
                if rb != ra
                    && let Some(r) = layout.region(rb)
                {
                    self.pts.push(center(&r));
                }
            }
        }
        for &v in self.right.iter().rev() {
            let Some(r) = layout.rect(v) else { return false };
            self.pts.push(center(&r));
        }
        self.pts.push(b0);
        true
    }
}

impl Bundler {
    /// The control polygon of the last [`Bundler::route`] (before straightening for
    /// `polygon` only; after it for `route`).
    pub fn points(&self) -> &[Point] {
        &self.pts
    }
}

/// Pulls the inner points toward the chord: `p_i = beta p_i + (1 - beta) chord(i / (n-1))`.
pub fn straighten(pts: &mut [Point], beta: f32) {
    let n = pts.len();
    if n < 3 {
        return;
    }
    let beta = if beta.is_nan() { DEFAULT_BETA } else { beta.clamp(0.0, 1.0) };
    let (p0, pn) = (pts[0], pts[n - 1]);
    let last = (n - 1) as f32;
    for (i, p) in pts.iter_mut().enumerate().take(n - 1).skip(1) {
        let c = lerp(p0, pn, i as f32 / last);
        *p = lerp(c, *p, beta);
    }
}

/// At most `max` (>= 2) points of `pts` into `out`: the endpoints plus evenly spaced inner
/// points.
pub fn thin(pts: &[Point], max: usize, out: &mut Vec<Point>) {
    out.clear();
    let n = pts.len();
    let max = max.max(2);
    if n <= max {
        out.extend_from_slice(pts);
        return;
    }
    for k in 0..max {
        // Rounded even spacing; first and last map to the endpoints exactly.
        let i = (k * (n - 1) + (max - 1) / 2) / (max - 1);
        out.push(pts[i]);
    }
}

/// Bezier segments of the clamped cubic B-spline through the control points `pts` (see the
/// module docs), appended to `out`. Returns the number of segments.
pub fn spline(pts: &[Point], out: &mut Vec<Cubic>) -> usize {
    let n = pts.len();
    match n {
        0 | 1 => 0,
        2 => {
            out.push([pts[0], lerp(pts[0], pts[1], 1.0 / 3.0), lerp(pts[0], pts[1], 2.0 / 3.0), pts[1]]);
            1
        }
        3 => {
            out.push([pts[0], lerp(pts[0], pts[1], 2.0 / 3.0), lerp(pts[2], pts[1], 2.0 / 3.0), pts[2]]);
            1
        }
        _ => {
            // Q = [P0, P0, P1, ..., Pn-1, Pn-1]; segment i uses Q[i..i + 4].
            let q = |i: usize| pts[i.saturating_sub(1).min(n - 1)];
            let comb = |a: Point, wa: f32, b: Point, wb: f32, c: Point, wc: f32| {
                [a[0] * wa + b[0] * wb + c[0] * wc, a[1] * wa + b[1] * wb + c[1] * wc]
            };
            for i in 0..n - 1 {
                let (q0, q1, q2, q3) = (q(i), q(i + 1), q(i + 2), q(i + 3));
                let b0 = if i == 0 { pts[0] } else { comb(q0, 1.0 / 6.0, q1, 4.0 / 6.0, q2, 1.0 / 6.0) };
                let b1 = lerp(q1, q2, 1.0 / 3.0);
                let b2 = lerp(q1, q2, 2.0 / 3.0);
                let b3 = if i == n - 2 { pts[n - 1] } else { comb(q1, 1.0 / 6.0, q2, 4.0 / 6.0, q3, 1.0 / 6.0) };
                out.push([b0, b1, b2, b3]);
            }
            n - 1
        }
    }
}
