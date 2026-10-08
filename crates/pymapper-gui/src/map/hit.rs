//! Hit testing: grid query for candidates, then exact geometry (rounded rects, Bezier
//! ribbons) against what is drawn in the current band.

use std::ops::Range;

use super::band::{BandBatch, QueryScratch};
use super::camera::Camera;
use super::scene::{EdgeInstance, RectInstance, Scene, WorldRect};

/// Segments per curve, shared with the vertex shader (`CURVE_SEGMENTS` in map.wgsl). A
/// power of two, so hit testing can reach exactly these segments by halving.
pub const CURVE_SEGMENTS: u32 = 16;
const CURVE_DEPTH: u32 = CURVE_SEGMENTS.trailing_zeros();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    Rect,
    Line,
    Curve,
}

/// What is under the cursor: kind, sorted index in its batch, and the caller's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub kind: HitKind,
    pub index: u32,
    pub id: u32,
}

/// Band-filtered views of the three batches (what is drawn right now).
#[derive(Debug, Default)]
pub struct Bands {
    pub rects: BandBatch,
    pub lines: BandBatch,
    pub curves: BandBatch,
}

/// True if world point `p` is inside the rounded rect as drawn at `zoom` (corner radius is
/// in pixels, so it depends on zoom).
pub fn rect_contains(r: &RectInstance, p: [f32; 2], zoom: f32) -> bool {
    if !(p[0] >= r.min[0] && p[0] <= r.max[0] && p[1] >= r.min[1] && p[1] <= r.max[1]) {
        return false;
    }
    let hw = (r.max[0] - r.min[0]) * 0.5;
    let hh = (r.max[1] - r.min[1]) * 0.5;
    let rad = (r.radius_px.max(0.0) / zoom).min(hw).min(hh);
    if rad <= 0.0 {
        return true;
    }
    // Distance from the inner (radius-shrunk) box; inside the corner circle counts.
    let qx = ((p[0] - (r.min[0] + hw)).abs() - (hw - rad)).max(0.0);
    let qy = ((p[1] - (r.min[1] + hh)).abs() - (hh - rad)).max(0.0);
    qx * qx + qy * qy <= rad * rad
}

/// Point on the cubic at `t`.
pub fn bezier(e: &EdgeInstance, t: f32) -> [f32; 2] {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    [
        a * e.p0[0] + b * e.c0[0] + c * e.c1[0] + d * e.p1[0],
        a * e.p0[1] + b * e.c0[1] + c * e.c1[1] + d * e.p1[1],
    ]
}

fn seg_dist2(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    let (x, y) = (a[0] + t * dx - p[0], a[1] + t * dy - p[1]);
    x * x + y * y
}

/// Squared world distance from `p` to the edge's centre line as tessellated with
/// `segments` segments (what the shader draws).
pub fn edge_dist2(e: &EdgeInstance, p: [f32; 2], segments: u32) -> f32 {
    let n = segments.max(1);
    let mut best = f32::INFINITY;
    let mut a = e.p0;
    for i in 1..=n {
        let b = if i == n { e.p1 } else { bezier(e, i as f32 / n as f32) };
        best = best.min(seg_dist2(p, a, b));
        a = b;
    }
    best
}

fn mid(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5]
}

/// Minimum squared distance from `p` to the `2^depth`-segment polyline of cubic `c`, folded
/// into `best` (start it at the squared limit). Halves recursively (de Casteljau) and
/// prunes sub-curves whose control-polygon bbox, grown by `sqrt(best)`, misses `p`: the
/// curve and its chords lie inside that hull. Most far candidates die at the first test.
fn curve_dist2(c: &[[f32; 2]; 4], p: [f32; 2], depth: u32, best: &mut f32) {
    let lim = best.sqrt();
    let (x0, x1) = (c[0][0].min(c[1][0]).min(c[2][0]).min(c[3][0]), c[0][0].max(c[1][0]).max(c[2][0]).max(c[3][0]));
    let (y0, y1) = (c[0][1].min(c[1][1]).min(c[2][1]).min(c[3][1]), c[0][1].max(c[1][1]).max(c[2][1]).max(c[3][1]));
    if !(p[0] >= x0 - lim && p[0] <= x1 + lim && p[1] >= y0 - lim && p[1] <= y1 + lim) {
        return;
    }
    if depth == 0 {
        *best = best.min(seg_dist2(p, c[0], c[3]));
        return;
    }
    let (ab, bc, cd) = (mid(c[0], c[1]), mid(c[1], c[2]), mid(c[2], c[3]));
    let (abc, bcd) = (mid(ab, bc), mid(bc, cd));
    let m = mid(abc, bcd);
    curve_dist2(&[c[0], ab, abc, m], p, depth - 1, best);
    curve_dist2(&[m, bcd, cd, c[3]], p, depth - 1, best);
}

/// Squared distance from `p` to the edge as drawn (lines: one segment; curves:
/// [`CURVE_SEGMENTS`] segments) if it is at most `lim2`, else `None`.
pub fn edge_within(e: &EdgeInstance, p: [f32; 2], curve: bool, lim2: f32) -> Option<f32> {
    if !curve {
        let d2 = seg_dist2(p, e.p0, e.p1);
        return (d2 <= lim2).then_some(d2);
    }
    // Start just above the limit so a distance of exactly the limit still counts.
    let mut best = lim2.next_up();
    curve_dist2(&[e.p0, e.c0, e.c1, e.p1], p, CURVE_DEPTH, &mut best);
    (best <= lim2).then_some(best)
}

/// Reusable buffers for [`hit_test`].
#[derive(Debug, Default)]
pub struct HitScratch {
    query: QueryScratch,
    ranges: Vec<Range<u32>>,
}

/// Topmost drawn item under screen point `screen` (canvas pixels): edges first (within
/// `tol_px` of the ribbon), then rects from the highest layer down.
pub fn hit_test(scene: &Scene, bands: &Bands, cam: &Camera, screen: [f32; 2], tol_px: f32, s: &mut HitScratch) -> Option<Hit> {
    let p = cam.screen_to_world(screen);
    if !(p[0].is_finite() && p[1].is_finite()) {
        return None;
    }
    let tol = tol_px.max(0.0);
    for (kind, batch, band) in [(HitKind::Curve, &scene.curves, &bands.curves), (HitKind::Line, &scene.lines, &bands.lines)] {
        let reach = (scene.max_edge_px * 0.5 + tol) / cam.zoom;
        let q = WorldRect { min: [p[0] - reach, p[1] - reach], max: [p[0] + reach, p[1] + reach] };
        let mut best: Option<(f32, u32)> = None;
        for li in (0..batch.layers.len()).rev() {
            band.query(batch, li, &q, usize::MAX, &mut s.query, &mut s.ranges);
            for r in &s.ranges {
                for slot in r.clone() {
                    let i = band.slots[slot as usize];
                    let e = &batch.inst[i as usize];
                    let lim = (e.width_px * 0.5 + tol) / cam.zoom;
                    // Cheap reject first: the edge lies inside its control-point bbox. In
                    // dense scenes nearly all grid candidates fail here.
                    let x0 = e.p0[0].min(e.c0[0]).min(e.c1[0]).min(e.p1[0]) - lim;
                    let x1 = e.p0[0].max(e.c0[0]).max(e.c1[0]).max(e.p1[0]) + lim;
                    let y0 = e.p0[1].min(e.c0[1]).min(e.c1[1]).min(e.p1[1]) - lim;
                    let y1 = e.p0[1].max(e.c0[1]).max(e.c1[1]).max(e.p1[1]) + lim;
                    if p[0] < x0 || p[0] > x1 || p[1] < y0 || p[1] > y1 {
                        continue;
                    }
                    // Later slots draw on top, so ties go to the later one.
                    if let Some(d2) = edge_within(e, p, kind == HitKind::Curve, lim * lim)
                        && best.is_none_or(|(bd, _)| d2 <= bd)
                    {
                        best = Some((d2, i));
                    }
                }
            }
        }
        if let Some((_, i)) = best {
            return Some(Hit { kind, index: i, id: batch.id[i as usize] });
        }
    }
    let q = WorldRect { min: p, max: p };
    for li in (0..scene.rects.layers.len()).rev() {
        bands.rects.query(&scene.rects, li, &q, usize::MAX, &mut s.query, &mut s.ranges);
        // Within a layer the last drawn wins.
        for r in s.ranges.iter().rev() {
            for slot in r.clone().rev() {
                let i = bands.rects.slots[slot as usize];
                if rect_contains(&scene.rects.inst[i as usize], p, cam.zoom) {
                    return Some(Hit { kind: HitKind::Rect, index: i, id: scene.rects.id[i as usize] });
                }
            }
        }
    }
    None
}
