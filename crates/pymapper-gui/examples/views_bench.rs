//! Secondary views benchmark (CPU; the renderer is measured by `canvas_bench` and
//! `codemap_bench`): loads a project like `codemap_bench`, then times the graph view's cut,
//! Barnes-Hut layout and scenes at several code map zoom levels, synthetic force layouts of
//! growing size, the dependency matrix, and the focus view's adjacencies and N-hop walks.
//!
//! ```text
//! cargo run --release -p pymapper-gui --example views_bench -- PATH [--[no-]deps] [--[no-]stdlib]
//!     [--isolated] [--cache-dir DIR] [--no-cache] [--synthetic N,N,...]
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use pymapper_core::index::NodeKind;
use pymapper_gui::codemap::load::{LoadParams, load};
use pymapper_gui::codemap::metrics::ColorMetric;
use pymapper_gui::codemap::{CodeMap, MapParams};
use pymapper_gui::map::scene::{WorldRect, band_for_zoom};
use pymapper_gui::map::stress::Rng;
use pymapper_gui::stats::process_usage;
use pymapper_gui::views::dsm::{self, DsmParams};
use pymapper_gui::views::force::{ForceGraph, ForceLayout, ForceParams};
use pymapper_gui::views::graph::{Adjacency, FocusParams, Weight, focus_graph, level_graph, radial_layout};
use pymapper_gui::views::quadtree::QuadTree;
use pymapper_gui::views::scene::{self, NodeLinkStyle};

/// The canvas the zoom levels are computed for (physical pixels).
const CANVAS: [f32; 2] = [2880.0, 1800.0];

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn mib(b: usize) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn rss() -> f64 {
    mib(process_usage().peak_rss_bytes as usize)
}

fn main() -> Result<()> {
    let mut lp = LoadParams::default();
    let mut path = None;
    let mut synthetic = vec![1_000usize, 10_000, 50_000];
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--deps" | "--no-deps" => lp.deps = Some(a == "--deps"),
            "--stdlib" | "--no-stdlib" => lp.stdlib = Some(a == "--stdlib"),
            "--isolated" => lp.isolated = true,
            "--no-cache" => lp.no_cache = true,
            "--cache-dir" => lp.cache_dir = Some(PathBuf::from(args.next().context("--cache-dir DIR")?)),
            "--synthetic" => synthetic = args.next().context("--synthetic N,N")?.split(',').map(str::parse).collect::<Result<_, _>>()?,
            _ if a.starts_with('-') => bail!("unknown option {a}"),
            _ => path = Some(PathBuf::from(a)),
        }
    }
    lp.path = std::fs::canonicalize(path.unwrap_or_else(|| PathBuf::from(".")))?;
    let t = Instant::now();
    let analysis = Arc::new(load(&lp, &|_| {})?);
    let ix = analysis.index();
    println!("views_bench: {}", lp.path.display());
    println!("load: {:.0} ms ({} files, {} nodes, {} call pairs); peak RSS {:.1} MiB", ms(t.elapsed()), ix.file_count(), ix.node_count(), analysis.rollup.stats().call_pairs, rss());
    let t = Instant::now();
    let map = Arc::new(CodeMap::build(Arc::clone(&analysis), MapParams::default()));
    println!("code map build: {:.1} ms; peak RSS {:.1} MiB", ms(t.elapsed()), rss());

    graph_views(&map);
    synthetic_force(&synthetic);
    matrix(&map);
    focus(&map);
    println!("peak RSS {:.1} MiB", rss());
    Ok(())
}

/// The graph view at the fit zoom and zoomed into the centre.
fn graph_views(map: &CodeMap) {
    let mut world = WorldRect::EMPTY;
    for d in 0..map.layout.computed().min(1) {
        for &v in map.layout.tree().level(d) {
            if let Some(r) = map.layout.rect(v) {
                world = world.union(&WorldRect::new(r.x0, r.y0, r.x1, r.y1));
            }
        }
    }
    if !world.is_positive() {
        println!("graph: empty map");
        return;
    }
    let fit = (CANVAS[0] / world.width()).min(CANVAS[1] / world.height()) * 0.95;
    println!("graph view (current level; max 3000 nodes):");
    for zoom_in in [1.0f32, 8.0, 64.0] {
        let z = fit * zoom_in;
        let band = band_for_zoom(z);
        let c = world.center();
        let (hw, hh) = (CANVAS[0] / z * 0.5, CANVAS[1] / z * 0.5);
        let view = WorldRect::new(c[0] - hw, c[1] - hh, c[0] + hw, c[1] + hh);
        let t = Instant::now();
        let mut g = level_graph(map, band, &view, 3000);
        let cut = t.elapsed();
        g.edges.retain(|e| e.weight(Weight::All) > 0);
        let max = g.edges.iter().map(|e| e.weight(Weight::All)).max().unwrap_or(1);
        let lm = (1.0 + max as f32).ln().max(f32::MIN_POSITIVE);
        let fg = ForceGraph { n: g.nodes.len() as u32, edges: g.edges.iter().map(|e| (e.src, e.dst, ((1.0 + e.weight(Weight::All) as f32).ln() / lm).max(0.05))).collect() };
        let init: Vec<[f32; 2]> = g.nodes.iter().map(|&v| map.layout.rect(v).map_or([0.0; 2], |r| [(r.x0 + r.x1) * 0.5, (r.y0 + r.y1) * 0.5])).collect();
        let t = Instant::now();
        let mut l = ForceLayout::new(&fg, &init, ForceParams::default());
        let mut worst = Duration::ZERO;
        while !l.done() {
            let s = Instant::now();
            l.step(&fg);
            worst = worst.max(s.elapsed());
        }
        let layout = t.elapsed();
        let r = scene::radii(map, &g);
        let t = Instant::now();
        let (sc, nl) = scene::node_link(map, &g, &l.pos, &r, &NodeLinkStyle { color: ColorMetric::Kind, weight: Weight::All, focus: None });
        let sb = t.elapsed();
        println!(
            "  zoom {zoom_in:>3}x (band {band}): cut {:.1} ms -> {} nodes, {} edges ({} left out); layout {} iters {:.0} ms (mean {:.2}, max {:.2} ms/iter); scene {:.2} ms ({} rects, {} lines, {:.1} MiB); layout heap {:.1} MiB",
            ms(cut),
            g.nodes.len(),
            g.edges.len(),
            g.dropped,
            l.iter,
            ms(layout),
            ms(layout) / f64::from(l.iter.max(1)),
            ms(worst),
            ms(sb),
            sc.rects.len(),
            sc.lines.len(),
            mib(sc.heap_bytes() + nl.heap_bytes()),
            mib(l.heap_bytes())
        );
    }
}

/// Force layouts of synthetic graphs: clustered (random within groups of 32, plus 10% long
/// range), 3 edges per node.
fn synthetic_force(sizes: &[usize]) {
    println!("synthetic force layouts (3 edges per node, theta 0.9):");
    for &n in sizes {
        let mut rng = Rng::new(0x5eed ^ n as u64);
        let mut edges = Vec::with_capacity(3 * n);
        for v in 0..n as u32 {
            for _ in 0..3 {
                let w = if rng.next_u64().is_multiple_of(10) { (rng.next_u64() % n as u64) as u32 } else { (v / 32 * 32 + (rng.next_u64() % 32) as u32).min(n as u32 - 1) };
                if w != v {
                    edges.push((v, w, 1.0));
                }
            }
        }
        let g = ForceGraph { n: n as u32, edges };
        let init: Vec<[f32; 2]> = (0..n).map(|_| [(rng.next_u64() % (1 << 20)) as f32, (rng.next_u64() % (1 << 20)) as f32]).collect();
        let mut tree = QuadTree::default();
        let mass = g.masses();
        let t = Instant::now();
        tree.build(&init, &mass);
        let build = t.elapsed();
        let mut l = ForceLayout::new(&g, &init, ForceParams { max_iters: 30, tolerance: 0.0, min_temp: 0.0, ..ForceParams::default() });
        let t = Instant::now();
        l.run(&g);
        let per = ms(t.elapsed()) / f64::from(l.iter.max(1));
        println!("  {n:>7} nodes: quadtree build {:.2} ms ({} cells); {per:.2} ms per iteration ({} iterations); heap {:.1} MiB", ms(build), tree.cells(), l.iter, mib(l.heap_bytes()));
    }
}

fn matrix(map: &CodeMap) {
    println!("dependency matrix:");
    for (label, external) in [("first-party", false), ("with external", true)] {
        let t = Instant::now();
        let d = dsm::build(map, DsmParams { external, ..DsmParams::default() });
        let build = t.elapsed();
        let t = Instant::now();
        let (sc, _) = scene::matrix(map, &d);
        let sb = t.elapsed();
        let largest = d.blocks.iter().map(|b| b.1 - b.0).max().unwrap_or(0);
        println!(
            "  {label:<13}: build + order {:.1} ms -> {} modules, {} cells, {} cycles (largest {largest}), {} feedback; scene {:.1} ms ({} rects, {:.1} MiB); dsm heap {:.1} MiB",
            ms(build),
            d.nodes.len(),
            d.cells.len(),
            d.blocks.len(),
            d.feedback,
            ms(sb),
            sc.rects.len(),
            mib(sc.heap_bytes()),
            mib(d.heap_bytes())
        );
    }
}

fn focus(map: &CodeMap) {
    let a = &map.analysis;
    let ix = a.index();
    let t = Instant::now();
    let calls = Adjacency::calls(ix.node_count(), &a.rollup);
    let tc = t.elapsed();
    let t = Instant::now();
    let mods = Adjacency::modules(ix, &a.rollup);
    let tm = t.elapsed();
    println!("focus: call adjacency {:.1} ms ({:.1} MiB), module adjacency {:.1} ms ({:.1} MiB)", ms(tc), mib(calls.heap_bytes()), ms(tm), mib(mods.heap_bytes()));
    // Focus on the 20 functions with the most callers plus 20 spread over the index.
    let mut funcs: Vec<u32> = (0..ix.node_count()).filter(|&v| ix.kind(v) == NodeKind::Function).collect();
    if funcs.is_empty() {
        return;
    }
    funcs.sort_unstable_by_key(|&v| std::cmp::Reverse(calls.inc(v).len()));
    let step = (funcs.len() / 20).max(1);
    let mut picks: Vec<u32> = funcs.iter().take(20).copied().collect();
    picks.extend(funcs.iter().step_by(step).take(20).copied());
    for hops in [1u8, 2, 3] {
        let (mut total, mut worst, mut nodes) = (Duration::ZERO, Duration::ZERO, 0usize);
        for &v in &picks {
            let t = Instant::now();
            let g = focus_graph(&calls, v, FocusParams { hops, ..FocusParams::default() });
            let pos = radial_layout(&g);
            let r = scene::radii(map, &g);
            let (sc, _) = scene::node_link(map, &g, &pos, &r, &NodeLinkStyle { color: ColorMetric::Kind, weight: Weight::Sites, focus: g.local(v) });
            let d = t.elapsed();
            std::hint::black_box(sc);
            total += d;
            worst = worst.max(d);
            nodes += g.nodes.len();
        }
        println!("  {hops} hop(s), max 400 nodes: walk + layout + scene mean {:.2} ms, max {:.2} ms, mean {} nodes ({} foci)", ms(total) / picks.len() as f64, ms(worst), nodes / picks.len(), picks.len());
    }
}
