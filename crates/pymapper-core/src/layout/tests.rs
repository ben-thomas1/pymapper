use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use super::squarify::{R64, squarify};
use super::*;
use crate::index::{Inputs, NodeId, load};
use crate::roots::{FileStatus, Origin};
use crate::rollup::Aggregates;

// -- helpers ------------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-layout-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, rel: &str, contents: &str) -> String {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, contents).expect("write");
        p.to_str().expect("utf-8 temp path").to_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn r(x0: f64, y0: f64, x1: f64, y1: f64) -> R64 {
    R64 { x0, y0, x1, y1 }
}

fn area(q: &R64) -> f64 {
    q.w() * q.h()
}

fn overlap(a: &Rect, b: &Rect) -> f32 {
    let w = (a.x1.min(b.x1) - a.x0.max(b.x0)).max(0.0);
    let h = (a.y1.min(b.y1) - a.y0.max(b.y0)).max(0.0);
    w * h
}

fn sq(weights: &[f64], rect: R64) -> Vec<R64> {
    let mut out = vec![rect.corner(); weights.len()];
    squarify(weights, rect, &mut out);
    out
}

/// Checks tiling: every cell inside `rect`, cell areas proportional to weight, cells
/// pairwise disjoint (up to rounding), total area equal to the rect's.
fn check_tiling(weights: &[f64], rect: R64, out: &[R64]) {
    let total: f64 = weights.iter().sum();
    let a = area(&rect);
    let eps = 1e-9 * a.max(1.0);
    let mut sum = 0.0;
    for (w, q) in weights.iter().zip(out) {
        assert!(q.x0 >= rect.x0 - 1e-9 && q.y0 >= rect.y0 - 1e-9, "{q:?} outside {rect:?}");
        assert!(q.x1 <= rect.x1 + 1e-9 && q.y1 <= rect.y1 + 1e-9, "{q:?} outside {rect:?}");
        assert!(q.w() >= 0.0 && q.h() >= 0.0, "negative cell {q:?}");
        let want = w / total * a;
        assert!((area(q) - want).abs() <= 1e-6 * a.max(1.0), "area {} want {want}", area(q));
        sum += area(q);
    }
    assert!((sum - a).abs() <= 1e-6 * a, "cells cover {sum} of {a}");
    for i in 0..out.len() {
        for j in i + 1..out.len() {
            let (p, q) = (&out[i], &out[j]);
            let w = (p.x1.min(q.x1) - p.x0.max(q.x0)).max(0.0);
            let h = (p.y1.min(q.y1) - p.y0.max(q.y0)).max(0.0);
            assert!(w * h <= eps, "cells {i} {p:?} and {j} {q:?} overlap");
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// -- squarify -----------------------------------------------------------------------------

#[test]
fn squarify_paper_example() {
    // Bruls et al., figure 2: 6 6 4 3 2 2 1 into a 6 x 4 rectangle.
    let w = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
    let rect = r(0.0, 0.0, 6.0, 4.0);
    let out = sq(&w, rect);
    check_tiling(&w, rect, &out);
    // First row: the two 6s stacked in a 3-wide column on the left.
    assert_eq!(out[0], r(0.0, 0.0, 3.0, 2.0));
    assert_eq!(out[1], r(0.0, 2.0, 3.0, 4.0));
    // Second row: 4 and 3 in the remaining 3 x 4, a row along the top (width 3 < height 4).
    assert!((out[2].h() - 7.0 / 3.0).abs() < 1e-12 && (out[3].h() - 7.0 / 3.0).abs() < 1e-12);
    // Then 2, 2 and 1 each in a column of the remaining 3 x 5/3 (adding a cell to a column
    // would make it worse); the 1 is the worst cell, 0.6 x 5/3 (aspect 25/9).
    for q in &out[4..] {
        assert!((q.h() - 5.0 / 3.0).abs() < 1e-12, "{q:?}");
    }
    for q in &out {
        let ar = (q.w() / q.h()).max(q.h() / q.w());
        assert!(ar <= 25.0 / 9.0 + 1e-9, "aspect {ar} of {q:?}");
    }
}

#[test]
fn squarify_edge_cases() {
    let rect = r(10.0, 20.0, 110.0, 70.0);
    assert!(sq(&[], rect).is_empty());
    assert_eq!(sq(&[5.0], rect), vec![rect]);
    // Zero weights get zero-area cells, the rest tiles the rect.
    let w = [3.0, 0.0, 2.0, 0.0, 0.0];
    let out = sq(&w, rect);
    check_tiling(&w, rect, &out);
    assert_eq!(area(&out[1]), 0.0);
    // All zero, or a degenerate rect: zero-area cells at the corner.
    for q in sq(&[0.0, 0.0], rect) {
        assert_eq!(q, rect.corner());
    }
    let flat = r(0.0, 0.0, 10.0, 0.0);
    for q in sq(&[1.0, 2.0], flat) {
        assert_eq!(area(&q), 0.0);
    }
    // Equal weights in a square: a perfect grid of squares.
    let out = sq(&[1.0; 4], r(0.0, 0.0, 2.0, 2.0));
    for q in &out {
        assert!((q.w() - 1.0).abs() < 1e-12 && (q.h() - 1.0).abs() < 1e-12, "{q:?}");
    }
}

#[test]
fn squarify_random_tilings() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for case in 0..300 {
        let n = 1 + rng.below(if case % 10 == 0 { 2000 } else { 40 }) as usize;
        let mut w: Vec<f64> = (0..n).map(|_| (1 + rng.below(1000)) as f64).collect();
        if case % 3 == 0 {
            // Heavy-tailed sizes, as real LOC.
            for x in &mut w {
                *x = (*x / 10.0).powi(3).max(1.0);
            }
        }
        w.sort_by(|a, b| b.total_cmp(a));
        let rect = r(0.0, 0.0, 1.0 + rng.below(5000) as f64, 1.0 + rng.below(5000) as f64);
        let out = sq(&w, rect);
        check_tiling(&w, rect, &out);
    }
}

#[test]
fn squarify_beats_slicing_on_aspect_ratio() {
    let mut rng = Rng(7);
    let mut w: Vec<f64> = (0..500).map(|_| (1 + rng.below(100)) as f64).collect();
    w.sort_by(|a, b| b.total_cmp(a));
    let out = sq(&w, r(0.0, 0.0, 1600.0, 1000.0));
    let mut mean = 0.0;
    for q in &out {
        mean += (q.w() / q.h()).max(q.h() / q.w());
    }
    mean /= out.len() as f64;
    // Slicing 500 cells across 1600 would give aspect ratios in the hundreds.
    assert!(mean < 2.0, "mean aspect ratio {mean}");
}

// -- tree ---------------------------------------------------------------------------------

/// root(0) [a(1) [a1(2) a2(3)] b(4)] ext(5) [c(6)]
fn small_tree() -> LayoutTree {
    let depths = [0, 1, 2, 2, 1, 0, 1];
    let metric = [0, 50, 10, 30, 20, 0, 7];
    LayoutTree::from_preorder(&depths, &metric, &[Region::Main, Region::ThirdParty], 1)
}

#[test]
fn tree_shape_weights_and_levels() {
    let t = small_tree();
    assert_eq!(t.len(), 7);
    assert_eq!(t.levels(), 3);
    assert_eq!(t.subtree(0), 0..5);
    assert_eq!(t.subtree(1), 1..4);
    assert_eq!(t.subtree(5), 5..7);
    assert!(t.has_children(1) && !t.has_children(2));
    // a = max(50, 10 + 30); root = 50 + 20; ext = 7.
    assert_eq!(t.weight(1), 50);
    assert_eq!(t.weight(0), 70);
    assert_eq!(t.weight(5), 7);
    assert_eq!(t.level(0), &[0, 5]);
    assert_eq!(t.level(1), &[1, 4, 6]);
    assert_eq!(t.level(2), &[2, 3]);
    assert!(t.level(3).is_empty());
    assert_eq!((t.slot(4), t.slot(6), t.slot(3)), (1, 2, 1));
    assert_eq!(t.root_region(1), Region::ThirdParty);
    // min_weight lifts zeros; a parent is never lighter than its children.
    let z = LayoutTree::from_preorder(&[0, 1, 1], &[0, 0, 0], &[Region::Main], 5);
    assert_eq!((z.weight(0), z.weight(1)), (10, 5));
    let z0 = LayoutTree::from_preorder(&[0, 1], &[0, 0], &[Region::Main], 0);
    assert_eq!(z0.weight(0), 0);
}

#[test]
fn tree_hash_tracks_shape_weights_and_regions() {
    let a = small_tree();
    assert_eq!(a.hash(), small_tree().hash());
    let w = LayoutTree::from_preorder(&[0, 1, 2, 2, 1, 0, 1], &[0, 50, 10, 30, 21, 0, 7], &[Region::Main, Region::ThirdParty], 1);
    let s = LayoutTree::from_preorder(&[0, 1, 2, 1, 1, 0, 1], &[0, 50, 10, 30, 20, 0, 7], &[Region::Main, Region::ThirdParty], 1);
    let g = LayoutTree::from_preorder(&[0, 1, 2, 2, 1, 0, 1], &[0, 50, 10, 30, 20, 0, 7], &[Region::Main, Region::Stdlib], 1);
    for o in [&w, &s, &g] {
        assert_ne!(o.hash(), a.hash());
    }
}

#[test]
#[should_panic(expected = "depth jumps")]
fn tree_rejects_depth_jumps() {
    LayoutTree::from_preorder(&[0, 2], &[1, 1], &[Region::Main], 1);
}

#[test]
fn empty_tree() {
    let t = Arc::new(LayoutTree::from_preorder(&[], &[], &[], 1));
    assert!(t.is_empty() && t.levels() == 0);
    let mut l = Layout::new(Arc::clone(&t), LayoutConfig::default());
    l.ensure(10);
    assert!(l.is_complete() && l.computed() == 0);
    assert!(l.region(Region::Main).is_none());
}

// -- layout -------------------------------------------------------------------------------

/// A random hierarchy: `roots` distributions, packages, modules and defs below, heavy-tailed
/// LOC. Returns (depths, metric).
fn gen_tree(seed: u64, roots: usize, fanout: u64, max_depth: u16) -> (Vec<u16>, Vec<u32>) {
    fn rec(rng: &mut Rng, d: u16, fanout: u64, max_depth: u16, depths: &mut Vec<u16>, metric: &mut Vec<u32>) {
        depths.push(d);
        let loc = (1 + rng.below(40)).pow(2) as u32;
        metric.push(if d == 0 { 0 } else { loc });
        if d < max_depth && (d < 2 || rng.below(3) != 0) {
            let kids = rng.below(fanout + 1);
            for _ in 0..kids {
                rec(rng, d + 1, fanout, max_depth, depths, metric);
            }
        }
    }
    let mut rng = Rng(seed | 1);
    let (mut depths, mut metric) = (Vec::new(), Vec::new());
    for _ in 0..roots {
        rec(&mut rng, 0, fanout, max_depth, &mut depths, &mut metric);
    }
    (depths, metric)
}

fn regions_for(depths: &[u16], cycle: &[Region]) -> Vec<Region> {
    let n = depths.iter().filter(|&&d| d == 0).count();
    (0..n).map(|i| cycle[i % cycle.len()]).collect()
}

fn grow(r: &Rect, e: f32) -> Rect {
    Rect::new(r.x0 - e, r.y0 - e, r.x1 + e, r.y1 + e)
}

/// Every computed rect lies in its parent's content rect (or its region), siblings do not
/// overlap, and each cell's share of its parent's content area is its share of the parent's
/// weight.
fn check_layout(l: &Layout) {
    let t = l.tree();
    let cfg = l.config();
    for reg in [Region::Main, Region::ThirdParty, Region::Stdlib] {
        if let Some(q) = l.region(reg) {
            assert!(cfg.world.contains(&q), "region {reg:?} {q:?} outside the world");
        }
    }
    if l.computed() > 0 {
        for (s, &v) in t.level(0).iter().enumerate() {
            let q = l.rect(v).expect("level 0");
            let reg = l.region(t.root_region(s as u32)).expect("region of a root");
            assert!(grow(&reg, 1e-2).contains(&q), "root {v} {q:?} outside {reg:?}");
        }
    }
    let mut kids: Vec<NodeId> = Vec::new();
    for d in 1..l.computed() {
        for &p in t.level(d - 1) {
            if !t.has_children(p) {
                continue;
            }
            let c = l.content(p).expect("parent level");
            let ca = f64::from(c.area());
            let pw = f64::from(t.weight(p));
            kids.clear();
            let mut v = p + 1;
            while v < t.subtree(p).end {
                kids.push(v);
                v = t.subtree(v).end;
            }
            let e = 1e-5 * c.w().max(c.h()).max(1.0) + 1e-3;
            for (i, &a) in kids.iter().enumerate() {
                let qa = l.rect(a).expect("child level");
                assert!(grow(&c, e).contains(&qa), "{a} {qa:?} outside parent {p} content {c:?}");
                if pw > 0.0 {
                    let want = f64::from(t.weight(a)) / pw * ca;
                    let got = f64::from(qa.area());
                    assert!((got - want).abs() <= 1e-4 * ca + 1e-3 * want + 1e-3, "{a}: area {got} want {want}");
                }
                for &b in &kids[i + 1..] {
                    let qb = l.rect(b).expect("child level");
                    assert!(f64::from(overlap(&qa, &qb)) <= 1e-5 * ca + 1e-4, "siblings {a} {qa:?} {b} {qb:?} overlap");
                }
            }
        }
    }
}

fn bits(l: &Layout) -> Vec<u32> {
    let mut out = Vec::new();
    for lv in l.levels() {
        for q in lv.iter() {
            out.extend([q.x0.to_bits(), q.y0.to_bits(), q.x1.to_bits(), q.y1.to_bits()]);
        }
    }
    out
}

#[test]
fn small_layout_is_nested_and_proportional() {
    let t = Arc::new(small_tree());
    let cfg = LayoutConfig { external_share: 0.5, ..LayoutConfig::default() };
    let mut l = Layout::new(Arc::clone(&t), cfg);
    assert_eq!(l.computed(), 0);
    assert_eq!(l.rect(0), None);
    l.ensure(0);
    assert_eq!(l.computed(), 1);
    assert!(l.rect(0).is_some() && l.rect(1).is_none());
    l.ensure(99);
    assert!(l.is_complete());
    assert_eq!(l.computed(), 3);
    check_layout(&l);
    // root 70 vs ext 7: the external region gets 7/77 of the long side, not the 0.5 cap.
    let (m, e) = (l.region(Region::Main).expect("main"), l.region(Region::ThirdParty).expect("ext"));
    let span = cfg.world.w() - cfg.region_gap;
    assert!((e.w() - span * 7.0 / 77.0).abs() < 0.5, "{e:?}");
    assert!((m.x1 + cfg.region_gap - e.x0).abs() < 1e-3);
    assert!(l.region(Region::Stdlib).is_none());
    assert!(!l.muted(0) && !l.muted(3) && l.muted(5) && l.muted(6));
    assert_eq!(l.region_of(6), Region::ThirdParty);
    // The single root of each region fills it.
    assert_eq!(l.rect(0), Some(m));
    assert_eq!(l.rect(5), Some(e));
    // a (50) and b (20) share the root's content area 5:2.
    let (a, b) = (l.rect(1).expect("a"), l.rect(4).expect("b"));
    assert!((a.area() / b.area() - 2.5).abs() < 1e-3);
    // a's children (10 + 30) leave a filler of 10: they cover 4/5 of a's content.
    let ca = l.content(1).expect("a content");
    let kids = l.rect(2).expect("a1").area() + l.rect(3).expect("a2").area();
    assert!((kids / ca.area() - 0.8).abs() < 1e-3);
    // The header band sits on top of the content.
    let hb = cfg.header_band(&a);
    assert_eq!((hb.y0, hb.y1), (a.y0, ca.y0));
    assert!(hb.h() > 0.0);
}

#[test]
fn external_share_caps_large_venvs() {
    // Main weighs 10, third-party 1000, stdlib 3000.
    let depths = [0, 1, 0, 1, 0, 1];
    let metric = [0, 10, 0, 1000, 0, 3000];
    let t = Arc::new(LayoutTree::from_preorder(&depths, &metric, &[Region::Main, Region::ThirdParty, Region::Stdlib], 1));
    let cfg = LayoutConfig::default();
    let mut l = Layout::new(t, cfg);
    l.ensure(1);
    check_layout(&l);
    let m = l.region(Region::Main).expect("main");
    let (tp, sl) = (l.region(Region::ThirdParty).expect("tp"), l.region(Region::Stdlib).expect("stdlib"));
    let span = cfg.world.w() - cfg.region_gap;
    assert!((m.w() - span * (1.0 - cfg.external_share)).abs() < 0.5, "{m:?}");
    // The external strip is tall: third-party on top, stdlib below, 1:3 by weight.
    assert!(tp.y1 < sl.y0 && tp.x0 == sl.x0 && tp.x1 == sl.x1);
    let hs = tp.h() + sl.h();
    assert!((tp.h() / hs - 0.25).abs() < 1e-3);
    for v in [3, 5] {
        assert!(l.muted(v));
    }
}

#[test]
fn only_external_or_only_main_takes_the_world() {
    let cfg = LayoutConfig::default();
    let t = Arc::new(LayoutTree::from_preorder(&[0, 1, 0], &[0, 5, 9], &[Region::Stdlib, Region::ThirdParty], 1));
    let l = Layout::new(t, cfg);
    assert!(l.region(Region::Main).is_none());
    let (tp, sl) = (l.region(Region::ThirdParty).expect("tp"), l.region(Region::Stdlib).expect("stdlib"));
    // A landscape world splits left/right.
    assert_eq!((tp.x0, sl.x1), (cfg.world.x0, cfg.world.x1));
    let t = Arc::new(LayoutTree::from_preorder(&[0, 0], &[3, 4], &[Region::Main, Region::Main], 1));
    let mut l = Layout::new(t, cfg);
    assert_eq!(l.region(Region::Main), Some(cfg.world));
    l.ensure(0);
    check_layout(&l);
    let total = l.rect(0).expect("a").area() + l.rect(1).expect("b").area();
    assert!((total - cfg.world.area()).abs() / cfg.world.area() < 1e-5);
}

#[test]
fn random_layouts_hold_the_invariants() {
    let cycle = [Region::Main, Region::ThirdParty, Region::Main, Region::Stdlib];
    for seed in 0..12u64 {
        let (depths, metric) = gen_tree(seed * 77 + 3, 1 + (seed % 5) as usize, 6, 6);
        let t = Arc::new(LayoutTree::from_preorder(&depths, &metric, &regions_for(&depths, &cycle), (seed % 2) as u32));
        let order = if seed % 3 == 0 { Order::Preorder } else { Order::Size };
        let mut l = Layout::new(t, LayoutConfig { order, ..LayoutConfig::default() });
        l.ensure(u32::MAX);
        check_layout(&l);
    }
}

#[test]
fn preorder_keeps_sibling_order() {
    // Children 1, 5, 3: by size the 5 would come first.
    let t = Arc::new(LayoutTree::from_preorder(&[0, 1, 1, 1], &[0, 1, 5, 3], &[Region::Main], 1));
    let world = Rect::new(0.0, 0.0, 900.0, 100.0);
    let cfg = LayoutConfig { world, pad: 0.0, header: 0.0, order: Order::Preorder, ..LayoutConfig::default() };
    let mut l = Layout::new(Arc::clone(&t), cfg);
    l.ensure(1);
    let xs: Vec<f32> = (1..4).map(|v| l.rect(v).expect("child").x0).collect();
    assert!(xs[0] < xs[1] && xs[1] < xs[2], "{xs:?}");
    let mut s = Layout::new(t, LayoutConfig { order: Order::Size, ..cfg });
    s.ensure(1);
    assert!(s.rect(2).expect("5").x0 < s.rect(3).expect("3").x0);
    assert!(s.rect(3).expect("3").x0 < s.rect(1).expect("1").x0);
}

#[test]
fn padding_insets_children_and_caps_on_small_rects() {
    let cfg = LayoutConfig::default();
    let big = Rect::new(0.0, 0.0, 1000.0, 1000.0);
    let c = cfg.content(&big);
    assert_eq!(c, Rect::new(cfg.pad, cfg.header, 1000.0 - cfg.pad, 1000.0 - cfg.pad));
    // A small rect: pad and header scale down with it instead of eating it.
    let small = Rect::new(0.0, 0.0, 10.0, 10.0);
    let c = cfg.content(&small);
    assert!((c.x0 - 0.2).abs() < 1e-6 && (c.y0 - 0.8).abs() < 1e-6, "{c:?}");
    assert!(c.area() > 0.8 * small.area());
    // Nothing left after the inset: the content is the rect itself.
    let tight = LayoutConfig { pad: 10.0, pad_frac: 1.0, header: 10.0, header_frac: 1.0, ..cfg };
    assert_eq!(tight.content(&small), small);
    assert_eq!(tight.header_band(&small).h(), 0.0);
}

#[test]
fn layout_is_deterministic_and_thread_count_independent() {
    // Big enough for the parallel path (levels above 32k nodes).
    let (depths, metric) = gen_tree(42, 3, 40, 4);
    assert!(depths.len() > 80_000, "{} nodes", depths.len());
    let regions = regions_for(&depths, &[Region::Main, Region::ThirdParty, Region::Stdlib]);
    let t = Arc::new(LayoutTree::from_preorder(&depths, &metric, &regions, 1));
    assert!((0..t.levels()).any(|d| t.level(d).len() > 32 * 1024));
    let run = |threads: usize| {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().expect("pool");
        pool.install(|| {
            let mut l = Layout::new(Arc::clone(&t), LayoutConfig::default());
            l.ensure(u32::MAX);
            l
        })
    };
    let one = run(1);
    check_layout(&one);
    let b1 = bits(&one);
    assert_eq!(b1, bits(&run(7)));
    assert_eq!(b1, bits(&run(1)));
    // A rebuilt tree from the same input lays out identically.
    let t2 = Arc::new(LayoutTree::from_preorder(&depths, &metric, &regions, 1));
    assert_eq!(t2.hash(), t.hash());
    let mut l2 = Layout::new(t2, LayoutConfig::default());
    l2.ensure(u32::MAX);
    assert_eq!(b1, bits(&l2));
}

#[test]
fn toggling_external_code_keeps_first_party_geography() {
    // Main alone vs main + a venv: first-party rects only stretch with the main region.
    let (depths, metric) = gen_tree(9, 1, 6, 4);
    let main_only = Arc::new(LayoutTree::from_preorder(&depths, &metric, &[Region::Main], 1));
    let mut d2 = depths.clone();
    let mut m2 = metric.clone();
    d2.extend([0, 1, 1]);
    m2.extend([0, 400, 300]);
    let with_ext = Arc::new(LayoutTree::from_preorder(&d2, &m2, &[Region::Main, Region::ThirdParty], 1));
    let cfg = LayoutConfig::default();
    let mut a = Layout::new(main_only, cfg);
    let mut b = Layout::new(with_ext, cfg);
    a.ensure(1);
    b.ensure(1);
    let (ma, mb) = (a.region(Region::Main).expect("a"), b.region(Region::Main).expect("b"));
    assert!(mb.w() < ma.w() && mb.h() == ma.h());
    let ext = b.region(Region::ThirdParty).expect("ext");
    for &v in a.tree().level(1) {
        let (qa, qb) = (a.rect(v).expect("a"), b.rect(v).expect("b"));
        assert!(grow(&mb, 1e-2).contains(&qb) && overlap(&qb, &ext) == 0.0);
        // Same share of the main region.
        let (fa, fb) = (qa.area() / ma.area(), qb.area() / mb.area());
        assert!((fa - fb).abs() <= 1e-4 * fa.max(1e-3), "node {v}: {fa} vs {fb}");
    }
}

// -- store --------------------------------------------------------------------------------

fn medium() -> (Arc<LayoutTree>, Layout) {
    let (depths, metric) = gen_tree(5, 3, 8, 5);
    let regions = regions_for(&depths, &[Region::Main, Region::Stdlib]);
    let t = Arc::new(LayoutTree::from_preorder(&depths, &metric, &regions, 1));
    let mut l = Layout::new(Arc::clone(&t), LayoutConfig::default());
    l.ensure(u32::MAX);
    (t, l)
}

#[test]
fn store_round_trip_and_partial_levels() {
    let dir = TempDir::new("store");
    let (t, l) = medium();
    let cfg = *l.config();
    let path = cache_path(&dir.path().join("sub"), &t, &cfg);
    let name = path.file_name().and_then(|n| n.to_str()).expect("name").to_owned();
    assert!(name.starts_with("layout-") && name.ends_with(".pml") && name.len() == 7 + 16 + 4, "{name}");
    save_levels(&path, &t, &cfg, l.levels()).expect("save");
    let back = load_levels(&path, &t, &cfg).expect("load");
    let lb = Layout::with_levels(Arc::clone(&t), cfg, back);
    assert!(lb.is_complete());
    assert_eq!(bits(&lb), bits(&l));
    // Only the first two levels stored: the rest is computed on demand, identically.
    save_levels(&path, &t, &cfg, &l.levels()[..2]).expect("save");
    let mut part = Layout::with_levels(Arc::clone(&t), cfg, load_levels(&path, &t, &cfg).expect("load"));
    assert_eq!(part.computed(), 2);
    part.ensure(u32::MAX);
    assert_eq!(bits(&part), bits(&l));
}

#[test]
fn store_rejects_stale_or_damaged_files() {
    let dir = TempDir::new("store-bad");
    let (t, l) = medium();
    let cfg = *l.config();
    let path = cache_path(dir.path(), &t, &cfg);
    save_levels(&path, &t, &cfg, l.levels()).expect("save");
    // Another config or tree: another key (and file name).
    let other = LayoutConfig { pad: 3.0, ..cfg };
    assert_ne!(cache_key(&t, &other), cache_key(&t, &cfg));
    assert_ne!(cache_path(dir.path(), &t, &other), path);
    assert!(matches!(load_levels(&path, &t, &other), Err(StoreError::Invalid("key"))));
    let t2 = LayoutTree::from_preorder(&[0, 1], &[0, 1], &[Region::Main], 1);
    assert!(matches!(load_levels(&path, &t2, &cfg), Err(StoreError::Invalid("key"))));
    // A flipped rect byte fails the checksum; a truncated file fails to read.
    let good = fs::read(&path).expect("read");
    let mut bad = good.clone();
    let mid = bad.len() / 2;
    bad[mid] ^= 0x40;
    fs::write(&path, &bad).expect("write");
    assert!(matches!(load_levels(&path, &t, &cfg), Err(StoreError::Invalid("checksum"))));
    fs::write(&path, &good[..good.len() - 3]).expect("write");
    assert!(matches!(load_levels(&path, &t, &cfg), Err(StoreError::Io(_))));
    let mut long = good.clone();
    long.push(0);
    fs::write(&path, &long).expect("write");
    assert!(matches!(load_levels(&path, &t, &cfg), Err(StoreError::Invalid("trailing bytes"))));
    fs::write(&path, b"garbage").expect("write");
    assert!(load_levels(&path, &t, &cfg).is_err());
    assert!(matches!(load_levels(&dir.path().join("missing.pml"), &t, &cfg), Err(StoreError::Io(_))));
}

// -- worker -------------------------------------------------------------------------------

#[test]
fn worker_computes_lazily_and_matches_the_sync_layout() {
    let (t, l) = medium();
    let notified = Arc::new(AtomicUsize::new(0));
    let n2 = Arc::clone(&notified);
    let w = LayoutWorker::spawn(Arc::clone(&t), *l.config(), None, 1, Box::new(move || {
        n2.fetch_add(1, Ordering::Relaxed);
    }))
    .expect("spawn");
    assert_eq!(w.region(Region::Main), l.region(Region::Main));
    assert!(w.wait_for(1) >= 2);
    // Nothing deeper than requested is computed.
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(w.computed(), 2);
    assert!(w.level(2).is_none());
    assert_eq!(w.wait_for(u32::MAX), t.levels());
    assert_eq!(bits(&w.snapshot()), bits(&l));
    assert_eq!(notified.load(Ordering::Relaxed), t.levels() as usize);
    assert_eq!(w.from_cache(), 0);
    assert_eq!(&w.levels()[0][..], &l.levels()[0][..]);
}

#[test]
fn worker_saves_and_reuses_the_cache() {
    let dir = TempDir::new("worker");
    let (t, l) = medium();
    let cfg = *l.config();
    {
        let w = LayoutWorker::spawn(Arc::clone(&t), cfg, Some(dir.path().to_path_buf()), 2, Box::new(|| {})).expect("spawn");
        assert_eq!(w.wait_for(2), 3);
        // Saved once idle; drop joins the thread, so poll for the file first.
        let path = cache_path(dir.path(), &t, &cfg);
        for _ in 0..500 {
            if load_levels(&path, &t, &cfg).is_ok_and(|lv| lv.len() == 3) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    let w = LayoutWorker::spawn(Arc::clone(&t), cfg, Some(dir.path().to_path_buf()), 0, Box::new(|| {})).expect("spawn");
    assert!(w.wait_for(0) >= 3);
    assert_eq!(w.from_cache(), 3);
    assert_eq!(w.wait_for(u32::MAX), t.levels());
    assert_eq!(bits(&w.snapshot()), bits(&l));
}

#[test]
fn worker_on_empty_tree_and_early_drop() {
    let t = Arc::new(LayoutTree::from_preorder(&[], &[], &[], 1));
    let w = LayoutWorker::spawn(t, LayoutConfig::default(), None, 5, Box::new(|| {})).expect("spawn");
    assert_eq!(w.wait_for(5), 0);
    drop(w);
    let (t, _) = medium();
    let w = LayoutWorker::spawn(t, LayoutConfig::default(), None, u32::MAX, Box::new(|| {})).expect("spawn");
    drop(w);
}

// -- from an index ------------------------------------------------------------------------

#[test]
fn tree_from_index_follows_the_hierarchy() {
    let dir = TempDir::new("index");
    let mut inp = Inputs::new();
    let proj = inp.add_group(b"proj", Origin::FirstParty, None);
    let lib = inp.add_group(b"lib", Origin::ThirdParty, None);
    let std = inp.add_group(b"stdlib", Origin::Stdlib, None);
    let big = "def f():\n    x = 1\n    return x\n\n\nclass C:\n    def m(self):\n        pass\n\n    def n(self):\n        pass\n";
    let files = [
        (dir.file("p/pkg/__init__.py", ""), proj, "pkg"),
        (dir.file("p/pkg/a.py", big), proj, "pkg.a"),
        (dir.file("p/pkg/b.py", "def g():\n    pass\n"), proj, "pkg.b"),
        (dir.file("l/lib.py", big), lib, "lib"),
        (dir.file("s/os.py", "def getcwd():\n    pass\n"), std, "os"),
    ];
    for (p, g, m) in &files {
        inp.push(p.as_bytes(), *g, m.as_bytes(), FileStatus::Active).expect("push");
    }
    let ix = load(inp);
    let agg = Aggregates::build(&ix);
    let t = LayoutTree::from_index(&ix, &agg, TreeOptions::default());
    assert_eq!(t.len(), ix.node_count() as usize);
    for v in 0..ix.node_count() {
        assert_eq!(t.depth(v), ix.depth(v));
        assert_eq!(t.subtree(v), ix.subtree(v));
        assert!(t.weight(v) >= agg.loc()[v as usize].max(1));
    }
    let a = ix.file_node(1);
    assert_eq!(t.weight(a), 11);
    let mut l = Layout::new(Arc::new(t), LayoutConfig::default());
    l.ensure(u32::MAX);
    check_layout(&l);
    for g in 0..3 {
        let v = ix.group_node(g);
        let want = Region::of(ix.groups()[g as usize].origin);
        assert_eq!(l.region_of(v), want);
        assert_eq!(l.muted(v), want != Region::Main);
    }
    assert!(!l.muted(a) && l.muted(ix.file_node(3)) && l.muted(ix.file_node(4)));
    // Hash: stable for the same index, changes with the metric.
    let again = LayoutTree::from_index(&ix, &agg, TreeOptions::default());
    assert_eq!(again.hash(), l.tree().hash());
    let uni = LayoutTree::from_index(&ix, &agg, TreeOptions { metric: SizeMetric::Uniform, min_weight: 1 });
    assert_ne!(uni.hash(), l.tree().hash());
    // Uniform: a parent weighs as many units as it has leaves (here: f, m, n under pkg.a).
    assert_eq!(uni.weight(a), 3);
    for m in [SizeMetric::Symbols, SizeMetric::CcSum, SizeMetric::CogSum] {
        let t = Arc::new(LayoutTree::from_index(&ix, &agg, TreeOptions { metric: m, min_weight: 1 }));
        let mut l = Layout::new(t, LayoutConfig::default());
        l.ensure(u32::MAX);
        check_layout(&l);
    }
}
