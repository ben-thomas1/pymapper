//! Interaction benchmark on a real project (no GPU): loads it in-process, builds the code map,
//! its first scene and scene index, then times what the interaction does per user action.
//!
//! ```text
//! cargo run --release -p pymapper-gui --example interact_bench -- PATH [--[no-]deps] [--[no-]stdlib]
//!     [--isolated] [--cache-dir DIR] [--no-cache] [--samples N]
//! ```
//!
//! Reported: scene index build and size; query state (call graph + search index) build;
//! findings pass; search latency; hover highlight build (random cells and the largest
//! subtrees); selection / path highlight; marks overlays; shortest paths; source pane reads;
//! peak RSS.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use pymapper_core::index::{NodeId, NodeKind};
use pymapper_core::query::{PathOptions, Query, Scratch};
use pymapper_gui::codemap::load::{LoadParams, load};
use pymapper_gui::codemap::scene::SceneParams;
use pymapper_gui::codemap::{CodeMap, MapParams};
use pymapper_gui::interact::findings;
use pymapper_gui::interact::highlight::{self, Base, Focus, HighlightSpec};
use pymapper_gui::interact::marks::{self, MarksSpec};
use pymapper_gui::interact::sceneindex::{NO_CELL, SceneIndex};
use pymapper_gui::interact::search::SearchIndex;
use pymapper_gui::interact::source;
use pymapper_gui::map::stress::Rng;
use pymapper_gui::stats::{Samples, process_usage};

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn mib(b: usize) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn report(label: &str, s: &Samples, extra: &str) {
    let mut scratch = Vec::new();
    let x = s.summary(&mut scratch);
    println!(
        "{label:<34} n={:<6} mean {:>8.3}  p50 {:>8.3}  p95 {:>8.3}  max {:>8.3} ms  {extra}",
        x.count,
        ms(x.mean),
        ms(x.p50),
        ms(x.p95),
        ms(x.max)
    );
}

fn main() -> Result<()> {
    let mut lp = LoadParams::default();
    let mut samples = 2000usize;
    let mut path = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--deps" | "--no-deps" => lp.deps = Some(a == "--deps"),
            "--stdlib" | "--no-stdlib" => lp.stdlib = Some(a == "--stdlib"),
            "--isolated" => lp.isolated = true,
            "--no-cache" => lp.no_cache = true,
            "--cache-dir" => lp.cache_dir = Some(PathBuf::from(args.next().context("--cache-dir DIR")?)),
            "--samples" => samples = args.next().context("--samples N")?.parse()?,
            _ => path = Some(PathBuf::from(a)),
        }
    }
    lp.path = path.context("usage: interact_bench PATH [options]")?;
    let t = Instant::now();
    let a = Arc::new(load(&lp, &|_| {})?);
    println!("load {:.0} ms, {} nodes, {} files", ms(t.elapsed()), a.index.node_count(), a.index.file_count());
    let map = CodeMap::build(Arc::clone(&a), MapParams::default());
    let (scene, _, drawn) = map.scene(&SceneParams::default());
    let t = Instant::now();
    let sx = SceneIndex::build(&scene, a.index.node_count(), &map.edges, &drawn);
    println!(
        "scene index {:.1} ms, {:.1} MiB ({} rects, {} drawn edges, {} segments)",
        ms(t.elapsed()),
        mib(sx.heap_bytes()),
        scene.rects.len(),
        drawn.len(),
        scene.curves.len()
    );
    let ix = &a.index;
    let t = Instant::now();
    let q = Query::new(&a.index, &a.imports, &a.calls);
    let si = SearchIndex::build(&q);
    println!(
        "query state {:.1} ms (call graph + {} distinct names), {:.1} MiB",
        ms(t.elapsed()),
        si.distinct_names(),
        mib(q.heap_bytes() + si.heap_bytes())
    );
    let t = Instant::now();
    let ins = findings::analyse(&a, &q);
    println!(
        "findings {:.0} ms: {} findings ({}), {} badges, {} cycles, {} violations, {:.1} MiB",
        ms(t.elapsed()),
        ins.findings.len(),
        ins.passes.join(", "),
        ins.badges.list.len(),
        ins.cycles.len(),
        ins.layer_violations.len(),
        mib(ins.heap_bytes())
    );

    // Search: typed prefixes of real names, plus fuzzy and qualified forms.
    let mut rng = Rng::new(7);
    let n = ix.node_count();
    let mut s = Samples::with_capacity(samples.max(1));
    let mut hits = 0usize;
    for _ in 0..samples.min(400) {
        let v = rng.below(n);
        let name = String::from_utf8_lossy(ix.name(v)).into_owned();
        let k = (name.len() / 2).max(1).min(name.len());
        let text = &name[..name.floor_char_boundary(k)];
        let t = Instant::now();
        hits += si.search(&q, text, 200).len();
        s.push(t.elapsed());
    }
    report("search (half a real name)", &s, &format!("{} hits avg", hits / samples.clamp(1, 400)));
    s.clear();
    for text in ["init", "get_cfg", "Engine", "test_", "parse", "a", "x.y:z", "core.models:Base"] {
        let t = Instant::now();
        si.search(&q, text, 200);
        s.push(t.elapsed());
    }
    report("search (fixed set incl. 1 char)", &s, "");

    // Hover highlight: random cells, then the 20 largest subtrees (worst cases).
    let cells: Vec<NodeId> = (0..n).filter(|&v| sx.cell(v) != NO_CELL).collect();
    let base = Base { scene: &scene, sx: &sx, edges: &map.edges, ix };
    s.clear();
    let mut segs = 0usize;
    for _ in 0..samples {
        let v = cells[rng.below(cells.len() as u32) as usize];
        let t = Instant::now();
        let (sc, _) = highlight::build(&base, &HighlightSpec { hover: Some(Focus::Node(v)), ..HighlightSpec::default() });
        s.push(t.elapsed());
        segs += sc.curves.len();
    }
    report("hover highlight (random cells)", &s, &format!("{} segments avg", segs / samples.max(1)));
    let first: Vec<NodeId> = cells.iter().copied().filter(|&v| !map.layout.muted(v)).collect();
    s.clear();
    segs = 0;
    for _ in 0..samples {
        let Some(&v) = first.get(rng.below(first.len().max(1) as u32) as usize) else { break };
        let t = Instant::now();
        let (sc, _) = highlight::build(&base, &HighlightSpec { hover: Some(Focus::Node(v)), ..HighlightSpec::default() });
        s.push(t.elapsed());
        segs += sc.curves.len();
    }
    report("hover highlight (first-party cells)", &s, &format!("{} segments avg", segs / s.len().max(1)));
    let mut big: Vec<NodeId> = first.iter().copied().filter(|&v| ix.kind(v) != NodeKind::Function).collect();
    big.sort_unstable_by_key(|&v| std::cmp::Reverse(ix.subtree(v).len()));
    big.truncate(20);
    s.clear();
    let (mut worst, mut skipped) = (0usize, 0u32);
    for &v in &big {
        let t = Instant::now();
        let (sc, st) = highlight::build(&base, &HighlightSpec { hover: Some(Focus::Node(v)), ..HighlightSpec::default() });
        s.push(t.elapsed());
        worst = worst.max(sc.curves.len());
        skipped = skipped.max(st.skipped);
    }
    report("hover highlight (20 largest 1st-p.)", &s, &format!("max {worst} segments, {skipped} edges over the cap"));
    s.clear();
    for _ in 0..samples.min(500) {
        let v = cells[rng.below(cells.len() as u32) as usize];
        let t = Instant::now();
        highlight::build(&base, &HighlightSpec { selected: Some(v), ..HighlightSpec::default() });
        s.push(t.elapsed());
    }
    report("selection highlight (+ markers)", &s, "");

    // Marks.
    let all = MarksSpec { cycles: true, layers: true, badges: true, min_conf: 0 };
    s.clear();
    let mut items = 0;
    for _ in 0..5 {
        let t = Instant::now();
        let (sc, _) = marks::build(&base, &map, &ins, &ins.badges, &all);
        s.push(t.elapsed());
        items = sc.rects.len() + sc.curves.len();
    }
    report("marks (cycles + violations + badges)", &s, &format!("{items} items"));
    s.clear();
    for _ in 0..5 {
        let t = Instant::now();
        let b = ins.badges_at(ix, 80);
        s.push(t.elapsed());
        items = b.list.len();
    }
    report("badge recount (min conf 80)", &s, &format!("{items} badges"));

    // Paths between random functions (calls), found or not.
    let funcs: Vec<NodeId> = (0..n).filter(|&v| ix.kind(v) == NodeKind::Function).collect();
    let mut scratch = Scratch::default();
    s.clear();
    let mut found = 0;
    if !funcs.is_empty() {
        for _ in 0..samples.min(500) {
            let (x, y) = (funcs[rng.below(funcs.len() as u32) as usize], funcs[rng.below(funcs.len() as u32) as usize]);
            let t = Instant::now();
            found += usize::from(q.path(x, y, &PathOptions::default(), &mut scratch).is_some());
            s.push(t.elapsed());
        }
    }
    report("path (random function pairs)", &s, &format!("{found} found"));

    // Source pane: read + highlight random defs.
    s.clear();
    let mut bytes = 0usize;
    for _ in 0..samples.min(500) {
        let Some(&v) = funcs.get(rng.below(funcs.len().max(1) as u32) as usize) else { break };
        let (Some(f), Some(r)) = (ix.file(v), ix.def_row(v)) else { continue };
        let d = &ix.facts().defs;
        let t = Instant::now();
        if let Ok(h) = source::load(ix.file_path(f), d.start[r], d.end[r], d.start_line[r]) {
            bytes += h.text.len();
        }
        s.push(t.elapsed());
    }
    report("source read + highlight (functions)", &s, &format!("{} B avg", bytes / s.len().max(1)));
    s.clear();
    let mods: Vec<NodeId> = (0..n).filter(|&v| ix.kind(v) == NodeKind::Module).collect();
    for _ in 0..samples.min(200) {
        let Some(&v) = mods.get(rng.below(mods.len().max(1) as u32) as usize) else { break };
        let (Some(f), Some(r)) = (ix.file(v), ix.def_row(v)) else { continue };
        let d = &ix.facts().defs;
        let t = Instant::now();
        let _ = source::load(ix.file_path(f), d.start[r], d.end[r], d.start_line[r]);
        s.push(t.elapsed());
    }
    report("source read + highlight (modules)", &s, "");
    println!("peak RSS {:.0} MiB", mib(process_usage().peak_rss_bytes as usize));
    Ok(())
}
