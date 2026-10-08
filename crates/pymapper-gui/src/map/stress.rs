//! Deterministic synthetic scene for stress tests and measurements: a three-level
//! treemap-like hierarchy (packages -> modules -> symbols) with labels, plus Bezier edges
//! between symbols (mostly local, some global), and a scripted camera path.

use std::fmt::Write as _;

use super::scene::{Lod, Rgba, RectStyle, Scene, SceneBuilder, WorldRect};

/// Seed used by `--stress` and the benchmarks.
pub const SEED: u64 = 0x5EED_CA9E;

/// World extent of the stress scene (well inside the f32 precision budget, see camera.rs).
pub const WORLD: WorldRect = WorldRect::new(0.0, 0.0, 16_000.0, 10_000.0);

/// SplitMix64: tiny, fast, deterministic.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in [0, 1).
    pub fn f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    /// Uniform in [0, n) (n > 0).
    pub fn below(&mut self, n: u32) -> u32 {
        (((self.next_u64() >> 32) * u64::from(n)) >> 32) as u32
    }
}

/// Splits `r` into `k` cells, row by row (rows of near-equal counts, so the cells tile the
/// whole rect), with jittered row heights and column widths, after reserving a header strip
/// at the top. Pushes cells into `out`.
fn split(r: WorldRect, k: usize, header: f32, rng: &mut Rng, out: &mut Vec<WorldRect>, weights: &mut Vec<f32>) {
    out.clear();
    if k == 0 {
        return;
    }
    let pad = (r.width().min(r.height()) * 0.02).max(0.0);
    let inner = WorldRect::new(r.min[0] + pad, r.min[1] + header.max(pad), r.max[0] - pad, r.max[1] - pad);
    if !inner.is_positive() {
        return;
    }
    let aspect = inner.width() / inner.height();
    let rows = ((k as f32 / aspect).sqrt().round() as usize).clamp(1, k);
    let gap = (inner.width() * inner.height() / k as f32).sqrt() * 0.04;
    // Cumulative jittered cuts of [lo, hi] into n parts, written to `cuts`.
    let mut cut = |n: usize, lo: f32, hi: f32, cuts: &mut Vec<f32>| {
        weights.clear();
        weights.extend((0..n).map(|_| 0.6 + 0.8 * rng.f32()));
        let total: f32 = weights.iter().sum();
        cuts.clear();
        cuts.push(lo);
        let mut acc = lo;
        for w in weights.iter() {
            acc += (hi - lo) * w / total;
            cuts.push(acc);
        }
        cuts[n] = hi;
    };
    let mut ys = Vec::with_capacity(rows + 1);
    let mut xs = Vec::new();
    cut(rows, inner.min[1], inner.max[1], &mut ys);
    for row in 0..rows {
        let n = k / rows + usize::from(row < k % rows);
        cut(n, inner.min[0], inner.max[0], &mut xs);
        for c in 0..n {
            out.push(WorldRect::new(xs[c] + gap * 0.5, ys[row] + gap * 0.5, xs[c + 1] - gap * 0.5, ys[row + 1] - gap * 0.5));
        }
    }
}

fn hsl(h: f32, s: f32, l: f32) -> Rgba {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h.rem_euclid(1.0)) * 6.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c * 0.5;
    let q = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgba::rgb(q(r), q(g), q(b))
}

/// Builds the stress scene with exactly `rects` rects and `edges` curves (seeded).
pub fn scene(rects: usize, edges: usize, seed: u64) -> Scene {
    let mut rng = Rng::new(seed);
    let side = (rects as f64).sqrt();
    let pkgs = ((side / 10.0).round() as usize).clamp(1, rects.max(1));
    let mods_per = ((side / 10.0).round() as usize).max(1);
    let internal = pkgs + pkgs * mods_per;
    let (pkgs, mods_per, leaves) = if rects > internal {
        (pkgs, mods_per, rects - internal)
    } else if rects >= 3 {
        (1, 1, rects - 2)
    } else {
        (rects, 0, 0)
    };
    let tiny = leaves < 2;
    let mut b = SceneBuilder::with_capacity(rects, edges);
    let mut label = String::new();
    let mut cells = Vec::new();
    let mut sub = Vec::new();
    let mut leafs = Vec::new();
    let mut weights = Vec::new();
    // Leaf centre and LOD, for edge generation.
    let mut leaf_pts: Vec<([f32; 2], Lod)> = Vec::with_capacity(leaves);
    let mut id = 0u32;
    let header = |r: &WorldRect| (r.height() * 0.12).min(r.width() * 0.1);
    split(WORLD, pkgs, 0.0, &mut rng, &mut cells, &mut weights);
    let n_mods = pkgs * mods_per;
    let mut mod_index = 0usize;
    for (p, &pr) in cells.iter().enumerate() {
        let hue = p as f32 / pkgs as f32;
        label.clear();
        let _ = write!(label, "package_{p}");
        let style = RectStyle { fill: hsl(hue, 0.35, 0.22), stroke: hsl(hue, 0.4, 0.35), radius_px: 4.0, stroke_px: 1.0 };
        let i = b.add_rect(id, 0, pr, style, Some(label.as_bytes()));
        if tiny {
            // Tiny scenes: containers are edge endpoints too.
            leaf_pts.push((pr.center(), b.rect_lod(i)));
        }
        id += 1;
        split(pr, mods_per, header(&pr), &mut rng, &mut sub, &mut weights);
        for (m, &mr) in sub.iter().enumerate() {
            label.clear();
            let _ = write!(label, "package_{p}.module_{m}");
            let style = RectStyle { fill: hsl(hue, 0.3, 0.32), stroke: hsl(hue, 0.35, 0.45), radius_px: 3.0, stroke_px: 1.0 };
            let i = b.add_rect(id, 1, mr, style, Some(label.as_bytes()));
            if tiny {
                leaf_pts.push((mr.center(), b.rect_lod(i)));
            }
            id += 1;
            // Distribute leaves evenly over modules (the first ones take the remainder).
            let k = leaves.checked_div(n_mods).map_or(0, |q| q + usize::from(mod_index < leaves % n_mods));
            split(mr, k, header(&mr), &mut rng, &mut leafs, &mut weights);
            for (s, &sr) in leafs.iter().enumerate() {
                label.clear();
                let _ = write!(label, "function_{s}");
                let heat = rng.f32();
                let style = RectStyle { fill: hsl(0.33 - 0.33 * heat, 0.45, 0.55), stroke: Rgba::new(0, 0, 0, 90), radius_px: 2.0, stroke_px: 1.0 };
                let i = b.add_rect(id, 2, sr, style, Some(label.as_bytes()));
                leaf_pts.push((sr.center(), b.rect_lod(i)));
                id += 1;
            }
            mod_index += 1;
        }
    }
    // Degenerate splits (tiny inputs) can drop leaves: top up with small filler rects in
    // the top-left corner so the count is exact.
    while b.rect_count() < rects {
        let style = RectStyle { fill: Rgba::rgb(60, 60, 60), stroke: Rgba::TRANSPARENT, radius_px: 0.0, stroke_px: 0.0 };
        let r = WorldRect::new(0.0, 0.0, 1.0, 1.0);
        let i = b.add_rect(id, 3, r, style, None);
        leaf_pts.push((r.center(), b.rect_lod(i)));
        id += 1;
    }
    let n = leaf_pts.len() as u32;
    let diag = (WORLD.width() * WORLD.width() + WORLD.height() * WORLD.height()).sqrt();
    for e in 0..edges {
        if n < 2 {
            break;
        }
        let a = rng.below(n);
        // 70% near neighbours (same or adjacent module), 20% same package range, 10% global.
        let r = rng.f32();
        let spread = if r < 0.7 { 200 } else if r < 0.9 { 20_000 } else { n };
        let off = rng.below(spread.min(n).max(2)) as i64 - i64::from(spread.min(n) / 2);
        let bi = (i64::from(a) + off).rem_euclid(i64::from(n)) as u32;
        let bi = if bi == a { (a + 1) % n } else { bi };
        let (p0, l0) = leaf_pts[a as usize];
        let (p1, l1) = leaf_pts[bi as usize];
        // Bow each edge sideways a little and pull long ones towards the centre (a crude
        // stand-in for bundling), keeping control points inside the world.
        let (dx, dy) = (p1[0] - p0[0], p1[1] - p0[1]);
        let pull = ((dx * dx + dy * dy).sqrt() / diag).min(1.0) * 0.6;
        let ctr = WORLD.center();
        let bow = 0.12;
        let c0 = [p0[0] + dx / 3.0 - dy * bow + (ctr[0] - p0[0]) * pull, p0[1] + dy / 3.0 + dx * bow + (ctr[1] - p0[1]) * pull];
        let c1 = [p1[0] - dx / 3.0 - dy * bow + (ctr[0] - p1[0]) * pull, p1[1] - dy / 3.0 + dx * bow + (ctr[1] - p1[1]) * pull];
        let clamp = |c: [f32; 2]| [c[0].clamp(WORLD.min[0], WORLD.max[0]), c[1].clamp(WORLD.min[1], WORLD.max[1])];
        let (c0, c1) = (clamp(c0), clamp(c1));
        let lod = Lod { min: l0.min.max(l1.min), max: l0.max.min(l1.max) };
        let color = Rgba::new(120, 170, 255, 70);
        b.add_curve(e as u32, [p0, c0, c1, p1], color, 1.0, lod);
    }
    b.build()
}

/// Camera path for measurements, `t` in [0, 1]: zooms from the fit view in by 2^`depth`
/// and back out (log-linear), while drifting over the scene. Returns (centre, zoom).
pub fn camera_path(t: f32, bounds: &WorldRect, fit_zoom: f32, depth: f32) -> ([f32; 2], f32) {
    let t = t.clamp(0.0, 1.0);
    let tri = 1.0 - (2.0 * t - 1.0).abs();
    let zoom = fit_zoom * (depth * tri).exp2();
    let a = std::f32::consts::TAU * t;
    let c = bounds.center();
    let (rx, ry) = (bounds.width() * 0.3, bounds.height() * 0.3);
    ([c[0] + rx * a.sin(), c[1] + ry * (2.0 * a).sin()], zoom)
}
