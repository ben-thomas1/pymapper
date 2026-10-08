//! CPU-side tests of the map renderer: grid, LOD bands, scene build, hit testing, text,
//! state updates. GPU rendering is covered by tests/offscreen.rs.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use super::band::{BandBatch, QueryScratch};
use super::camera::Camera;
use super::grid::{GridGeom, limit_ranges, push_merged, sort_by_bin};
use super::hit::{self, Bands, HitKind, HitScratch, rect_contains};
use super::scene::*;
use super::stress::{self, Rng};
use super::text::{ShelfPacker, TextSystem};
use super::{CanvasInput, MapState};

fn rand_rect(rng: &mut Rng, world: f32) -> WorldRect {
    // Mostly small, some medium, a few huge (spanning levels up to the top cell).
    let r = rng.f32();
    let s = if r < 0.8 { world * 0.01 } else if r < 0.97 { world * 0.2 } else { world * 1.5 };
    let (w, h) = (rng.f32() * s, rng.f32() * s);
    let (x, y) = (rng.f32() * world - w * 0.5, rng.f32() * world - h * 0.5);
    WorldRect::new(x, y, x + w, y + h)
}

fn items_in(geom: &GridGeom, starts: &[u32], q: &WorldRect) -> Vec<u32> {
    let mut bins = Vec::new();
    geom.bin_ranges(q, &mut bins);
    let mut out = Vec::new();
    for b in bins {
        out.extend(starts[b.start as usize]..starts[b.end as usize]);
    }
    out
}

#[test]
fn grid_query_finds_every_intersecting_item() {
    let mut rng = Rng::new(7);
    for &n in &[0usize, 1, 5, 300, 5000] {
        let rects: Vec<WorldRect> = (0..n).map(|_| rand_rect(&mut rng, 1000.0)).collect();
        let bounds = rects.iter().fold(WorldRect::EMPTY, |a, r| a.union(r));
        let geom = GridGeom::new(bounds, n);
        let (perm, starts) = sort_by_bin(&geom, rects.iter().copied(), n);
        assert_eq!(perm.len(), n);
        assert_eq!(*starts.last().unwrap() as usize, n);
        // Every sorted item sits in the bin it was assigned.
        for b in 0..geom.bins() as usize {
            for k in starts[b]..starts[b + 1] {
                assert_eq!(geom.bin_of(&rects[perm[k as usize] as usize]) as usize, b);
            }
        }
        for _ in 0..200 {
            let q = rand_rect(&mut rng, 1000.0);
            let got: HashSet<u32> = items_in(&geom, &starts, &q).into_iter().map(|k| perm[k as usize]).collect();
            for (i, r) in rects.iter().enumerate() {
                if r.intersects(&q) {
                    assert!(got.contains(&(i as u32)), "n={n} item {i} {r:?} missed by {q:?}");
                }
            }
            // Point queries too (hit testing).
            let p = [rng.f32() * 1000.0, rng.f32() * 1000.0];
            let pq = WorldRect { min: p, max: p };
            let got: HashSet<u32> = items_in(&geom, &starts, &pq).into_iter().map(|k| perm[k as usize]).collect();
            for (i, r) in rects.iter().enumerate() {
                if r.contains(p) {
                    assert!(got.contains(&(i as u32)));
                }
            }
        }
    }
}

#[test]
fn grid_query_is_local_for_small_items() {
    // 100k unit items on a grid: a small query must not return a large fraction.
    let n = 100_000;
    let side = 316;
    let rects: Vec<WorldRect> = (0..n).map(|i| {
        let (x, y) = ((i % side) as f32, (i / side) as f32);
        WorldRect::new(x, y, x + 0.9, y + 0.9)
    }).collect();
    let bounds = rects.iter().fold(WorldRect::EMPTY, |a, r| a.union(r));
    let geom = GridGeom::new(bounds, n);
    let (_, starts) = sort_by_bin(&geom, rects.iter().copied(), n);
    let got = items_in(&geom, &starts, &WorldRect::new(100.0, 100.0, 110.0, 110.0));
    assert!(got.len() >= 100 && got.len() < 1000, "{}", got.len());
    assert!(geom.levels.len() >= 3, "{:?}", geom.levels);
    assert_eq!(geom.levels.last().unwrap().cols, 1);
}

#[test]
fn degenerate_grids() {
    let g = GridGeom::new(WorldRect::EMPTY, 10);
    assert_eq!(g.bins(), 1);
    assert_eq!(g.bin_of(&WorldRect::new(0.0, 0.0, 1.0, 1.0)), 0);
    let g = GridGeom::new(WorldRect::new(f32::NAN, 0.0, 1.0, 1.0), 10);
    assert_eq!(g.bins(), 1);
    // Zero-width bounds (a vertical line of items).
    let g = GridGeom::new(WorldRect::new(5.0, 0.0, 5.0, 100.0), 1000);
    let b = g.bin_of(&WorldRect::new(5.0, 50.0, 5.0, 51.0));
    let mut bins = Vec::new();
    g.bin_ranges(&WorldRect::new(4.0, 49.0, 6.0, 52.0), &mut bins);
    assert!(bins.iter().any(|r| r.contains(&b)));
    // NaN query returns nothing; NaN item lands in a valid bin.
    bins.clear();
    g.bin_ranges(&WorldRect::new(f32::NAN, 0.0, 1.0, 1.0), &mut bins);
    assert!(bins.is_empty());
    assert!(g.bin_of(&WorldRect::new(f32::NAN, f32::NAN, f32::NAN, f32::NAN)) < g.bins());
}

#[test]
fn push_merged_and_limit_ranges() {
    let mut v = Vec::new();
    push_merged(&mut v, 0..2);
    push_merged(&mut v, 2..4);
    push_merged(&mut v, 4..4);
    push_merged(&mut v, 6..7);
    assert_eq!(v, vec![0..4, 6..7]);

    let mut rng = Rng::new(3);
    let mut gaps = Vec::new();
    for _ in 0..200 {
        let n = 1 + rng.below(40) as usize;
        let mut ranges: Vec<Range<u32>> = Vec::new();
        let mut at = rng.below(5);
        for _ in 0..n {
            at += 1 + rng.below(20);
            let len = 1 + rng.below(10);
            ranges.push(at..at + len);
            at += len;
        }
        let max = 1 + rng.below(12) as usize;
        let orig = ranges.clone();
        let mut gap_sizes: Vec<u32> = orig.windows(2).map(|w| w[1].start - w[0].end).collect();
        limit_ranges(&mut ranges, max, &mut gaps);
        assert!(ranges.len() <= max && !ranges.is_empty());
        assert!(ranges.windows(2).all(|w| w[0].end < w[1].start));
        assert_eq!(ranges[0].start, orig[0].start);
        assert_eq!(ranges.last().unwrap().end, orig.last().unwrap().end);
        for r in &orig {
            assert!(ranges.iter().any(|m| m.start <= r.start && r.end <= m.end));
        }
        // Optimal: the extra items drawn are exactly the gaps that were not kept as splits.
        gap_sizes.sort_unstable();
        let keep = (max - 1).min(gap_sizes.len());
        let extra: u32 = gap_sizes[..gap_sizes.len() - keep].iter().sum();
        let covered: u32 = ranges.iter().map(|r| r.end - r.start).sum();
        let orig_len: u32 = orig.iter().map(|r| r.end - r.start).sum();
        assert_eq!(covered, orig_len + extra);
    }
}

#[test]
fn lod_for_size_and_bands() {
    assert_eq!(band_for_zoom(1.0), 128);
    assert_eq!(band_for_zoom(0.0), 0);
    assert_eq!(band_for_zoom(f32::NAN), 0);
    assert_eq!(band_for_zoom(f32::MAX), MAX_BAND);
    assert_eq!(band_for_zoom(f32::MIN_POSITIVE), 2);
    assert_eq!(band_zoom(129), 2.0);
    // Size 1 needs zoom 2 to be 2 px: band 129.
    assert_eq!(Lod::for_size(1.0, 2.0), Lod { min: 129, max: LOD_UNBOUNDED });
    assert_eq!(Lod::for_size(0.5, 2.0).min, 130);
    // Size 3: 1.5 px at band 127 (zoom 0.5), 3 px at band 128.
    assert_eq!(Lod::for_size(3.0, 2.0).min, 128);
    assert_eq!(Lod::for_size(f32::NAN, 2.0), Lod::NEVER);
    assert_eq!(Lod::for_size(0.0, 2.0), Lod::NEVER);
    assert_eq!(Lod::for_size(f32::INFINITY, 2.0), Lod::ALWAYS);
    assert_eq!(Lod::for_size(1e30, 2.0).min, 30);
    assert_eq!(Lod::for_size(1e-30, 2.0), Lod { min: 229, max: LOD_UNBOUNDED });
    // Every band at or above `min` shows the item at >= min_px.
    for &s in &[0.013f32, 0.5, 1.0, 7.0, 1234.5] {
        let l = Lod::for_size(s, 2.0);
        assert!(f64::from(s) * band_zoom(l.min) >= 2.0);
        assert!(f64::from(s) * band_zoom(l.min - 1) < 2.0);
        assert!(l.contains(l.min) && !l.contains(l.min - 1) && l.contains(MAX_BAND));
    }
}

#[test]
fn change_points() {
    let mut c = ChangePoints::default();
    assert!(!c.changes_between(0, 254));
    c.add(Lod { min: 130, max: 140 });
    assert!(c.changes_between(129, 130));
    assert!(c.changes_between(130, 129));
    assert!(!c.changes_between(130, 139));
    assert!(c.changes_between(139, 140));
    assert!(!c.changes_between(140, 200));
    assert!(!c.changes_between(5, 5));
    c.add(Lod::ALWAYS);
    assert!(!c.changes_between(0, 1), "band 0 is not a transition into band 1");
    assert!(c.changes_between(254, 100));
}

fn style(fill: Rgba) -> RectStyle {
    RectStyle { fill, stroke: Rgba::TRANSPARENT, radius: 0.0, stroke_width: 0.0 }
}

#[test]
fn scene_build_sorts_by_layer_and_bin() {
    let mut rng = Rng::new(11);
    let mut b = SceneBuilder::new();
    for i in 0..2000u32 {
        let r = rand_rect(&mut rng, 500.0);
        let layer = (i % 3) as u8 * 2;
        let label = format!("n{i}");
        b.add_rect(i, layer, r, style(Rgba::rgb(1, 2, 3)), i.is_multiple_of(2).then_some(label.as_bytes()));
    }
    b.add_line(7, [0.0, 0.0], [10.0, 10.0], Rgba::rgb(9, 9, 9), 2.0, Lod::ALWAYS);
    b.add_curve(8, [[0.0, 0.0], [5.0, -5.0], [15.0, 5.0], [20.0, 0.0]], Rgba::rgb(9, 9, 9), 3.0, Lod::ALWAYS);
    let s = b.build();
    assert_eq!(s.rects.len(), 2000);
    assert_eq!(s.lines.len(), 1);
    assert_eq!(s.curves.len(), 1);
    assert_eq!(s.max_edge_width, 3.0);
    assert_eq!(s.rects.layers.iter().map(|l| l.layer).collect::<Vec<_>>(), vec![0, 2, 4]);
    let ids: HashSet<u32> = s.rects.id.iter().copied().collect();
    assert_eq!(ids.len(), 2000);
    for l in &s.rects.layers {
        assert_eq!(l.bin_start.len(), l.grid.bins() as usize + 1);
        assert_eq!(l.bin_start[0], l.items.start);
        assert_eq!(*l.bin_start.last().unwrap(), l.items.end);
        for bin in 0..l.grid.bins() as usize {
            for i in l.bin_start[bin]..l.bin_start[bin + 1] {
                let inst = &s.rects.inst[i as usize];
                assert_eq!(l.grid.bin_of(&WorldRect { min: inst.min, max: inst.max }) as usize, bin);
                assert_eq!(u32::from(l.layer), (s.rects.id[i as usize] % 3) * 2);
            }
        }
    }
    // Labels follow the permutation.
    for i in 0..s.rects.len() {
        let id = s.rects.id[i];
        let want = if id.is_multiple_of(2) { format!("n{id}") } else { String::new() };
        assert_eq!(s.label(i), want.as_bytes());
    }
    assert!(s.bounds.is_positive());
    assert!(s.heap_bytes() >= 2000 * size_of::<RectInstance>());
}

#[test]
fn instance_sizes_have_no_padding() {
    assert_eq!(size_of::<RectInstance>(), 32);
    assert_eq!(size_of::<EdgeInstance>(), 40);
    assert_eq!(size_of::<GlyphInstance>(), 32);
    assert_eq!(size_of::<Rgba>(), 4);
    let r = RectInstance { min: [1.0, 2.0], max: [3.0, 4.0], fill: Rgba([5, 6, 7, 8]), stroke: Rgba([9, 10, 11, 12]), radius: 0.5, stroke_width: 0.25 };
    let bytes = as_bytes(std::slice::from_ref(&r));
    assert_eq!(bytes.len(), 32);
    assert_eq!(&bytes[16..24], &[5, 6, 7, 8, 9, 10, 11, 12]);
    assert_eq!(&bytes[0..4], &1.0f32.to_ne_bytes());
}

fn lod_scene() -> Scene {
    // Rects of sizes 1, 2, 4, ... 512 in a row; curves visible in a band window.
    let mut b = SceneBuilder::new();
    let mut x = 0.0;
    for i in 0..10u32 {
        let s = (1u32 << i) as f32;
        b.add_rect(i, 0, WorldRect::new(x, 0.0, x + s, s), style(Rgba::rgb(200, 200, 200)), Some(b"label"));
        x += s + 1.0;
    }
    b.add_curve(100, [[0.0, 0.0], [10.0, 10.0], [20.0, 10.0], [30.0, 0.0]], Rgba::rgb(0, 0, 0), 1.0, Lod { min: 126, max: 129 });
    b.build()
}

#[test]
fn band_rebuild_filters_by_lod() {
    let s = lod_scene();
    let mut bb = BandBatch::default();
    for band in [120u8, 125, 128, 129, 134, MAX_BAND] {
        bb.rebuild(&s.rects, band);
        let want: Vec<u32> = (0..s.rects.len() as u32).filter(|&i| s.rects.lod[i as usize].contains(band)).collect();
        assert_eq!(bb.slots, want, "band {band}");
        assert_eq!(*bb.tables[0].last().unwrap() as usize, want.len());
        let mut covered = Vec::new();
        bb.for_each_run(|slot, run| {
            assert_eq!(slot as usize, covered.len());
            covered.extend(run);
        });
        assert_eq!(covered, want);
    }
    // At band 128 (zoom 1..2) only rects >= 2 world units show.
    bb.rebuild(&s.rects, 128);
    assert!(bb.slots.iter().all(|&i| s.rects.inst[i as usize].max[1] >= 2.0));
    assert_eq!(bb.len(), 9);
    let mut cb = BandBatch::default();
    cb.rebuild(&s.curves, 128);
    assert_eq!(cb.len(), 1);
    cb.rebuild(&s.curves, 129);
    assert!(cb.is_empty());
}

#[test]
fn band_query_matches_brute_force() {
    let s = stress::scene(20_000, 0, 1);
    let mut bands = Bands::default();
    let mut scratch = QueryScratch::default();
    let mut out = Vec::new();
    let mut rng = Rng::new(5);
    for band in [118u8, 121, 124, 127] {
        bands.rects.rebuild(&s.rects, band);
        for _ in 0..30 {
            let q = rand_rect(&mut rng, 16_000.0);
            let mut got = HashSet::new();
            for li in 0..s.rects.layers.len() {
                bands.rects.query(&s.rects, li, &q, super::gpu::MAX_RANGES_PER_LAYER, &mut scratch, &mut out);
                assert!(out.len() <= super::gpu::MAX_RANGES_PER_LAYER);
                for r in &out {
                    got.extend(r.clone().map(|slot| bands.rects.slots[slot as usize]));
                }
            }
            for &i in &bands.rects.slots {
                let inst = &s.rects.inst[i as usize];
                if (WorldRect { min: inst.min, max: inst.max }).intersects(&q) {
                    assert!(got.contains(&i), "band {band}: rect {i} missed");
                }
            }
        }
    }
}

#[test]
fn rect_contains_respects_rounded_corners() {
    let r = RectInstance { min: [0.0, 0.0], max: [10.0, 10.0], fill: Rgba::TRANSPARENT, stroke: Rgba::TRANSPARENT, radius: 4.0, stroke_width: 0.0 };
    // zoom 1: radius 4 world units.
    assert!(rect_contains(&r, [5.0, 5.0], 1.0, 1.0));
    assert!(rect_contains(&r, [0.0, 5.0], 1.0, 1.0));
    assert!(!rect_contains(&r, [0.2, 0.2], 1.0, 1.0), "outside the corner arc");
    assert!(rect_contains(&r, [1.5, 1.5], 1.0, 1.0));
    // zoom 10: radius 0.4 world units, so that point is inside.
    assert!(!rect_contains(&r, [0.2, 0.2], 10.0, 20.0), "radius is in points: 8 world units at 2x");
    assert!(rect_contains(&r, [0.2, 0.2], 10.0, 1.0));
    assert!(!rect_contains(&r, [10.5, 5.0], 1.0, 1.0));
    assert!(!rect_contains(&r, [f32::NAN, 5.0], 1.0, 1.0));
}

#[test]
fn edge_distance() {
    let line = EdgeInstance { p0: [0.0, 0.0], c0: [0.0, 0.0], c1: [10.0, 0.0], p1: [10.0, 0.0], color: Rgba::TRANSPARENT, width: 1.0 };
    assert_eq!(hit::edge_dist2(&line, [5.0, 3.0], 1), 9.0);
    assert_eq!(hit::edge_dist2(&line, [-3.0, 4.0], 1), 25.0);
    let curve = EdgeInstance { p0: [0.0, 0.0], c0: [0.0, 10.0], c1: [10.0, 10.0], p1: [10.0, 0.0], color: Rgba::TRANSPARENT, width: 1.0 };
    let top = hit::bezier(&curve, 0.5);
    assert_eq!(top, [5.0, 7.5]);
    assert!(hit::edge_dist2(&curve, top, hit::CURVE_SEGMENTS) < 1e-6);
    assert!(hit::edge_dist2(&curve, [5.0, 0.0], hit::CURVE_SEGMENTS) > 10.0);
}

/// DPI factor used by the hit-test checks.
const PPP: f32 = 2.0;

/// Brute-force topmost rect under `p` among the band's rects (highest layer, then latest).
fn brute_rect(s: &Scene, bands: &Bands, p: [f32; 2], zoom: f32) -> Option<u32> {
    bands.rects.slots.iter().rev().copied().find(|&i| rect_contains(&s.rects.inst[i as usize], p, zoom, PPP))
}

#[test]
fn hit_test_matches_brute_force() {
    let scratch = check_hits(&stress::scene(30_000, 3_000, 2), 400);
    assert_eq!(scratch.parallel_scans, 0);
}

#[test]
fn dense_edge_hit_test_matches_brute_force() {
    // Enough long edges that the candidate scan takes the parallel path.
    let scratch = check_hits(&stress::scene(3_000, 80_000, 6), 30);
    assert!(scratch.parallel_scans > 0);
}

fn check_hits(s: &Scene, points: usize) -> HitScratch {
    let mut cam = Camera { viewport: [1600.0, 1000.0], ..Camera::default() };
    cam.set_limits(&s.bounds);
    cam.fit(&s.bounds, 1.0);
    let mut bands = Bands::default();
    let mut scratch = HitScratch::default();
    let mut rng = Rng::new(9);
    let mut edge_hits = 0;
    for zoom_mul in [1.0f32, 4.0, 32.0] {
        cam.zoom = cam.min_zoom * 4.0 * zoom_mul;
        let band = cam.band();
        bands.rects.rebuild(&s.rects, band);
        bands.lines.rebuild(&s.lines, band);
        bands.curves.rebuild(&s.curves, band);
        for _ in 0..points {
            let screen = [rng.f32() * 1600.0, rng.f32() * 1000.0];
            let p = cam.screen_to_world(screen);
            let got = hit::hit_test(s, &bands, &cam, screen, 3.0, PPP, &mut scratch);
            // Brute-force edges: the nearest band curve within reach.
            let mut best_edge: Option<(f32, u32)> = None;
            for &i in &bands.curves.slots {
                let e = &s.curves.inst[i as usize];
                let lim = (hit::edge_px(e.width, PPP) * 0.5 + 3.0) / cam.zoom;
                let d2 = hit::edge_dist2(e, p, hit::CURVE_SEGMENTS);
                if d2 <= lim * lim && best_edge.is_none_or(|(bd, _)| d2 <= bd) {
                    best_edge = Some((d2, i));
                }
            }
            match (got, best_edge) {
                (Some(h), Some((d_want, _))) => {
                    assert_eq!(h.kind, HitKind::Curve);
                    let d_got = hit::edge_dist2(&s.curves.inst[h.index as usize], p, hit::CURVE_SEGMENTS);
                    // Subdivision and direct evaluation differ by float rounding: allow a
                    // few ulps of the world coordinates (~16k, ulp ~1e-3).
                    assert!((d_got.sqrt() - d_want.sqrt()).abs() <= 4e-3, "{d_got} vs {d_want}");
                    edge_hits += 1;
                }
                (Some(h), None) if h.kind == HitKind::Curve => {
                    // Only acceptable right at the tolerance boundary (rounding).
                    let e = &s.curves.inst[h.index as usize];
                    let lim = (hit::edge_px(e.width, PPP) * 0.5 + 3.0) / cam.zoom;
                    let d = hit::edge_dist2(e, p, hit::CURVE_SEGMENTS).sqrt();
                    assert!((d - lim).abs() <= 4e-3, "spurious edge hit at {p:?}: {d} vs {lim}");
                }
                (got, None) => {
                    let want = brute_rect(s, &bands, p, cam.zoom);
                    assert_eq!(got.map(|h| h.index), want, "zoom {} at {p:?}", cam.zoom);
                    if let Some(h) = got {
                        assert_eq!(h.kind, HitKind::Rect);
                        assert_eq!(h.id, s.rects.id[h.index as usize]);
                    }
                }
                (None, Some((d_want, i))) => {
                    // Only acceptable right at the tolerance boundary (rounding).
                    let lim = (hit::edge_px(s.curves.inst[i as usize].width, PPP) * 0.5 + 3.0) / cam.zoom;
                    assert!(lim - d_want.sqrt() <= 4e-3, "edge missed at {p:?}");
                }
            }
        }
    }
    assert!(edge_hits > 0, "the sample should hit some edges");
    scratch
}

#[test]
fn shelf_packer_never_overlaps() {
    let mut p = ShelfPacker::new(64, 64);
    let mut rng = Rng::new(4);
    let mut placed: Vec<[u32; 4]> = Vec::new();
    for _ in 0..500 {
        let (w, h) = (1 + rng.below(12), 1 + rng.below(12));
        if let Some([x, y]) = p.alloc(w, h) {
            assert!(x + w <= 64 && y + h <= 64);
            for q in &placed {
                let sep = x + w <= q[0] || q[0] + q[2] <= x || y + h <= q[1] || q[1] + q[3] <= y;
                assert!(sep, "overlap {:?} vs {q:?}", [x, y, w, h]);
            }
            placed.push([x, y, w, h]);
        }
    }
    assert!(placed.len() > 20);
    assert_eq!(p.alloc(65, 1), None);
    p.clear();
    assert_eq!(p.alloc(10, 10), Some([0, 0]));
}

#[test]
fn text_layout_and_atlas() {
    let mut t = TextSystem::new(14.0, 256);
    assert!(t.line_height() >= 14.0 && t.ascent() > 0.0 && t.ascent() < t.line_height());
    let rect = WorldRect::new(0.0, 0.0, 100.0, 20.0);
    let white = Rgba::rgb(255, 255, 255);
    let mut out = Vec::new();
    let w = t.layout(b"hello_world", rect, white, 1000.0, &mut out);
    assert!(w > 40.0 && w < 140.0, "{w}");
    assert_eq!(out.len(), 11, "every character has ink");
    assert!(out.windows(2).all(|g| g[1].offset[0] > g[0].offset[0]));
    assert!(out.iter().all(|g| g.rect_min == rect.min && g.size[0] > 0));
    assert!(out.iter().filter(|g| g.offset[1] < 0).count() >= 10, "glyphs rise above the baseline");
    assert!(!t.pending.is_empty() && !t.atlas_full);
    let pending = t.pending.len();
    // Cached: laying out again rasterises nothing new.
    out.clear();
    t.layout(b"hello_world", rect, white, 1000.0, &mut out);
    assert_eq!(t.pending.len(), pending);
    // Spaces advance without glyphs; max width truncates; non-ASCII and invalid UTF-8 work.
    out.clear();
    let w2 = t.layout(b"a b", rect, white, 1000.0, &mut out);
    assert_eq!(out.len(), 2);
    assert!(w2 > 0.0);
    // Label LOD: short labels need their full width, long ones their first few characters.
    assert!(out.iter().all(|g| g.need_px == w2.ceil() as u16));
    out.clear();
    let w3 = t.layout(b"hello_world", rect, white, 1000.0, &mut out);
    let need = out[0].need_px;
    assert!(out.iter().all(|g| g.need_px == need));
    assert!(f32::from(need) < w3 * 0.6 && need > 10, "{need} of {w3}");
    out.clear();
    t.layout(b"abcdefghijklmnopqrstuvwxyz", rect, white, 30.0, &mut out);
    assert!(out.len() < 10, "{}", out.len());
    out.clear();
    t.layout("caf\u{e9} \u{2192} \u{1f600}".as_bytes(), rect, white, 1000.0, &mut out);
    assert!(out.len() >= 5, "{}", out.len());
    out.clear();
    t.layout(&[b'a', 0xff, 0xfe, b'b'], rect, white, 1000.0, &mut out);
    assert!(out.len() >= 2);
    // A tiny atlas fills up and says so.
    let mut small = TextSystem::new(40.0, 32);
    out.clear();
    small.layout(b"ABCDEFGHIJ", rect, white, 1e9, &mut out);
    assert!(small.atlas_full);
    small.reset_atlas(1024, 1024);
    assert!(!small.atlas_full && small.pending.is_empty());
}

fn state_with(scene: Scene) -> MapState {
    let mut m = MapState::new(Arc::new(scene));
    m.update([1600.0, 1000.0], 2.0);
    m
}

#[test]
fn panning_within_a_band_rebuilds_nothing() {
    let mut m = state_with(stress::scene(50_000, 20_000, 3));
    assert_eq!(m.stats.band_rebuilds.count, 1);
    assert_eq!(m.stats.label_rebuilds.count, 1);
    let band_gen = m.band_generation;
    let label_gen = m.labels.generation;
    assert!(m.stats.labels > 0 && m.stats.glyphs > 0);
    // Small pans stay inside the label region (one viewport of margin).
    for _ in 0..20 {
        m.camera.pan_px(30.0, -20.0);
        assert!(!m.update([1600.0, 1000.0], 2.0));
    }
    assert_eq!((m.band_generation, m.labels.generation), (band_gen, label_gen));
    // Leaving the label region relays out labels but keeps the band.
    m.camera.pan_px(-5000.0, 0.0);
    assert!(m.update([1600.0, 1000.0], 2.0));
    assert_eq!(m.band_generation, band_gen);
    assert_eq!(m.labels.generation, label_gen + 1);
    // Zooming across band boundaries rebuilds the band.
    let c = [m.camera.viewport[0] * 0.5, m.camera.viewport[1] * 0.5];
    m.camera.zoom_at(c, 4.0);
    assert!(m.update([1600.0, 1000.0], 2.0));
    assert!(m.band_generation > band_gen);
    // force_all draws everything.
    m.force_all = true;
    m.update([1600.0, 1000.0], 2.0);
    assert_eq!(m.bands.rects.len(), 50_000);
    assert_eq!(m.bands.curves.len(), 20_000);
    let g = m.band_generation;
    m.update([1600.0, 1000.0], 2.0);
    assert_eq!(m.band_generation, g, "no rebuild while forced and unchanged");
    m.force_all = false;
    m.update([1600.0, 1000.0], 2.0);
    assert!(m.band_generation > g);
    let band = m.camera.band();
    let want = m.scene.rects.lod.iter().filter(|l| l.contains(band)).count();
    assert_eq!(m.bands.rects.len(), want);
}

#[test]
fn zooming_out_hides_small_rects_and_labels_fit() {
    let m = state_with(stress::scene(50_000, 0, 4));
    // Fit view: leaves are tiny, so only packages/modules (and few leaves) are in the band.
    let fit_count = m.bands.rects.len();
    assert!(fit_count < 50_000, "{fit_count}");
    for &i in &m.bands.rects.slots {
        let r = &m.scene.rects.inst[i as usize];
        let size = (r.max[0] - r.min[0]).max(r.max[1] - r.min[1]);
        assert!(size * m.camera.zoom >= MIN_RECT_PX * 0.999, "{size}");
    }
    // Every label candidate can show a few glyphs somewhere in the band.
    let max_zoom = band_zoom(m.camera.band() + 1) as f32;
    let mut seen = HashSet::new();
    for g in &m.labels.glyphs {
        let w = (g.rect_max[0] - g.rect_min[0]) * max_zoom;
        assert!(w >= super::MIN_LABEL_PX * 0.999);
        seen.insert(g.rect_min.map(f32::to_bits));
    }
    assert_eq!(seen.len(), m.stats.labels as usize);
}

#[test]
fn input_pans_zooms_and_hovers() {
    let mut m = state_with(stress::scene(5_000, 0, 5));
    let before = m.camera;
    let mut input = CanvasInput { drag: [10.0, 5.0], ..CanvasInput::NONE };
    m.apply_input(&input, 2.0);
    assert!((m.camera.center[0] - (before.center[0] - 20.0 / before.zoom)).abs() < 1e-3);
    input = CanvasInput { zoom: 2.0, zoom_anchor: Some([0.0, 0.0]), ..CanvasInput::NONE };
    let w = m.camera.screen_to_world([0.0, 0.0]);
    m.apply_input(&input, 2.0);
    let w2 = m.camera.screen_to_world([0.0, 0.0]);
    assert!((w[0] - w2[0]).abs() < 1e-2 && (w[1] - w2[1]).abs() < 1e-2);
    input = CanvasInput { fit: true, ..CanvasInput::NONE };
    m.apply_input(&input, 2.0);
    assert_eq!(m.camera.center, m.scene.bounds.center());
    m.update([1600.0, 1000.0], 2.0);
    // Hover in the canvas hits a rect.
    input = CanvasInput { hover: Some([400.0, 250.0]), ..CanvasInput::NONE };
    assert!(m.apply_input(&input, 2.0));
    let h = m.hover.expect("hit");
    assert_eq!(h.kind, HitKind::Rect);
    assert!(!m.apply_input(&input, 2.0), "same hover: unchanged");
    let mut text = String::new();
    m.describe_hover(&mut text);
    assert!(text.starts_with("rect #") && text.contains("package_"), "{text}");
    input.hover = None;
    assert!(m.apply_input(&input, 2.0));
    m.describe_hover(&mut text);
    assert!(text.is_empty());
}

#[test]
fn empty_scene_is_harmless() {
    let mut m = MapState::default();
    m.update([800.0, 600.0], 1.0);
    m.update([800.0, 600.0], 1.0);
    assert!(m.bands.rects.is_empty());
    assert_eq!(m.hit_test([10.0, 10.0], 3.0), None);
    let input = CanvasInput { hover: Some([1.0, 1.0]), drag: [3.0, 3.0], zoom: 1.5, zoom_anchor: None, fit: true };
    assert!(!m.apply_input(&input, 1.0));
    assert!(m.labels.glyphs.is_empty());
}

#[test]
fn stress_scene_is_exact_and_deterministic() {
    for &n in &[0usize, 1, 2, 3, 10, 101, 4321] {
        let s = stress::scene(n, n, 1);
        assert_eq!(s.rects.len(), n, "rects for {n}");
        assert_eq!(s.curves.len(), if n >= 2 { n } else { 0 }, "edges for {n}");
    }
    let a = stress::scene(3000, 3000, 42);
    let b = stress::scene(3000, 3000, 42);
    assert_eq!(a.rects.inst, b.rects.inst);
    assert_eq!(a.curves.inst, b.curves.inst);
    assert_eq!(a.text, b.text);
    let c = stress::scene(3000, 3000, 43);
    assert_ne!(a.rects.inst, c.rects.inst);
    // Edges are visible only when both endpoints are.
    assert!(a.curves.lod.iter().all(|l| l.min > 100 && l.max == LOD_UNBOUNDED));
    // Camera path starts and ends at the fit zoom and peaks in the middle.
    let (_, z0) = stress::camera_path(0.0, &a.bounds, 1.0, 7.0);
    let (_, z1) = stress::camera_path(0.5, &a.bounds, 1.0, 7.0);
    let (_, z2) = stress::camera_path(1.0, &a.bounds, 1.0, 7.0);
    assert_eq!((z0, z1, z2), (1.0, 128.0, 1.0));
}
