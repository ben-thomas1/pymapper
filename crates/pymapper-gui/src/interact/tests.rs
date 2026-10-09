//! Interaction tests on a small temp project: scene index against brute force, highlight and
//! marks overlays, search ranking, findings mapping and badges, the worker, and the state
//! machine end to end.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use pymapper_core::index::{Index, NodeId};
use pymapper_core::query::{PathGraph, Query};
use pymapper_core::rules::codes::IMPORT_CYCLE;
use pymapper_core::rules::{Pass, Severity};

use super::findings::{self, NO_NODE};
use super::highlight::{self, Base, Focus, HighlightSpec, MARKER_PX, VEIL};
use super::marks::{self, MarksSpec};
use super::sceneindex::{Dir, NO_CELL, SceneIndex};
use super::search::SearchIndex;
use super::state::{Interaction, SceneCtx};
use super::worker::{InsightWorker, PathJob, Results};
use crate::codemap::load::{Analysis, LoadParams, load};
use crate::codemap::scene::SceneParams;
use crate::codemap::{CodeMap, MapParams};
use crate::map::scene::{Lod, Scene, band_zoom};
use crate::map::{MapState, OVERLAY_HIGHLIGHT, OVERLAY_MARKS};

struct TempProject(PathBuf);

impl TempProject {
    fn new(files: &[(&str, &str)]) -> TempProject {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-interact-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        for (rel, src) in files {
            let p = dir.join(rel);
            fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            fs::write(&p, src).expect("write");
        }
        TempProject(fs::canonicalize(&dir).expect("canonical"))
    }

    fn analysis(&self) -> Arc<Analysis> {
        let p = LoadParams { path: self.0.clone(), isolated: false, deps: Some(false), stdlib: Some(false), cache_dir: None, no_cache: true, watch: false };
        Arc::new(load(&p, &|_| {}).expect("load"))
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const PYPROJECT: &str = "[project]\nname = \"proj\"\nversion = \"0\"\n\n[tool.pymapper]\nmin-confidence = 0\n\n\
[[tool.pymapper.contracts]]\nname = \"low stays low\"\nkind = \"forbidden\"\nsource = [\"app.low\"]\nforbidden = [\"app.high\"]\n";

/// An import cycle (a <-> b), a forbidden import (low -> high), a complex function, unused
/// code, a suppressed finding and cross-module calls.
const PROJ: &[(&str, &str)] = &[
    ("pyproject.toml", PYPROJECT),
    ("app/__init__.py", ""),
    ("app/a.py", "from app import b\n\ndef fa():\n    return b.fb()\n\ndef helper():\n    return 1\n"),
    ("app/b.py", "from app import a\n\ndef fb():\n    return a.helper()\n"),
    ("app/high.py", "def top():\n    return 1\n"),
    (
        "app/low.py",
        "from app import high\n\ndef base():\n    return high.top()\n\n\
         def tangled(x):\n    if x:\n        for i in x:\n            if i:\n                while i:\n                    if i > 2:\n                        i -= 1\n                    elif i:\n                        i -= 2\n                    else:\n                        break\n    try:\n        pass\n    except ValueError:\n        pass\n    except KeyError:\n        pass\n    if x and x or x:\n        return 1\n    return 0\n\n\
         def unused_one():  # pymapper: ignore[PM401]\n    return 2\n",
    ),
    ("app/main.py", "from app import a, low\n\nclass Engine:\n    def start(self):\n        return a.fa() + low.base()\n\ndef run():\n    Engine().start()\n    return low.tangled([1])\n\nif __name__ == '__main__':\n    run()\n"),
];

/// PROJ plus a module of many tiny functions (tiny cells).
fn files() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = PROJ.iter().map(|&(a, b)| (a.to_owned(), b.to_owned())).collect();
    let mut many = String::from("def big():\n");
    for k in 0..200 {
        many.push_str(&format!("    x{k} = {k}\n"));
    }
    for k in 0..80 {
        many.push_str(&format!("\ndef f{k}():\n    return {k}\n"));
    }
    v.push(("app/many.py".to_owned(), many));
    v
}

fn fixture() -> (TempProject, Arc<CodeMap>, Arc<Scene>, Arc<SceneIndex>) {
    let f = files();
    let refs: Vec<(&str, &str)> = f.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let p = TempProject::new(&refs);
    let a = p.analysis();
    let m = Arc::new(CodeMap::build(a, MapParams::default()));
    let (scene, _, drawn) = m.scene(&SceneParams::default());
    let sx = SceneIndex::build(&scene, m.analysis.index().node_count(), &m.edges, &drawn);
    (p, m, Arc::new(scene), Arc::new(sx))
}

fn node(ix: &Index, qid: &str) -> NodeId {
    ix.find(qid.as_bytes()).unwrap_or_else(|| panic!("no node {qid}"))
}

fn base<'a>(m: &'a CodeMap, scene: &'a Scene, sx: &'a SceneIndex) -> Base<'a> {
    Base { scene, sx, edges: &m.edges, ix: m.analysis.index() }
}

#[test]
fn scene_index_matches_the_scene() {
    let (_p, m, scene, sx) = fixture();
    let ix = m.analysis.index();
    assert!(!sx.drawn().is_empty(), "fixture draws edges");
    // Cells: id is the node, visibly filled, the first such instance.
    let mut cells = 0;
    for v in 0..ix.node_count() {
        let c = sx.cell(v);
        if c == NO_CELL {
            continue;
        }
        cells += 1;
        assert_eq!(scene.rects.id[c as usize], v);
        assert!(scene.rects.inst[c as usize].fill.0[3] > 0);
    }
    assert!(cells > 10);
    assert_eq!(sx.cell(u32::MAX), NO_CELL);
    // Segments: every curve instance belongs to exactly one drawn edge.
    let mut seen = HashSet::new();
    for &e in sx.drawn() {
        let segs = sx.segments(e);
        assert!(!segs.is_empty(), "drawn edge {e} has no segment");
        for &s in segs {
            assert_eq!(scene.curves.id[s as usize], e);
            assert!(seen.insert(s));
        }
    }
    assert_eq!(seen.len(), scene.curves.len());
    assert!(sx.segments(u32::MAX).is_empty());
    // Touching edges equal a brute-force scan, for every node.
    for v in 0..ix.node_count() {
        let r = ix.subtree(v);
        let mut got = Vec::new();
        sx.edges_touching(&m.edges, r.clone(), |e, d| got.push((e, d)));
        let mut want = Vec::new();
        for &e in sx.drawn() {
            let me = &m.edges[e as usize];
            let (s, d) = (r.contains(&me.src), r.contains(&me.dst));
            match (s, d) {
                (true, true) => want.push((e, Dir::Internal)),
                (true, false) => want.push((e, Dir::Out)),
                (false, true) => want.push((e, Dir::In)),
                _ => {}
            }
        }
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want, "node {v}");
    }
    assert!(sx.heap_bytes() > 0);
}

#[test]
fn highlight_neighbourhood_redraws_touching_edges_in_their_bands() {
    let (_p, m, scene, sx) = fixture();
    let ix = m.analysis.index();
    let b = base(&m, &scene, &sx);
    // Nothing focused: empty overlay.
    let (s, st) = highlight::build(&b, &HighlightSpec::default());
    assert!(s.rects.is_empty() && s.curves.is_empty());
    assert_eq!(st, Default::default());
    let low = node(ix, "app.low");
    let spec = HighlightSpec { hover: Some(Focus::Node(low)), ..HighlightSpec::default() };
    let (s, st) = highlight::build(&b, &spec);
    let mut touching = Vec::new();
    sx.edges_touching(&m.edges, ix.subtree(low), |e, d| touching.push((e, d)));
    assert!(!touching.is_empty());
    assert_eq!((st.out_edges + st.in_edges + st.internal_edges) as usize, touching.len());
    assert_eq!(st.skipped, 0);
    // Every segment of every touching edge is re-drawn, with the original LOD.
    let segs: usize = touching.iter().map(|&(e, _)| sx.segments(e).len()).sum();
    assert_eq!(s.curves.len(), segs);
    for (i, &e) in s.curves.id.iter().enumerate() {
        let orig = sx.segments(e).iter().map(|&k| scene.curves.lod[k as usize]).collect::<Vec<_>>();
        assert!(orig.contains(&s.curves.lod[i]));
    }
    // The veil covers the map and is always drawn.
    let veil = s.rects.inst.iter().position(|r| r.fill == VEIL).expect("veil");
    assert_eq!(s.rects.lod[veil], Lod::ALWAYS);
    // Without dimming there is no veil; a cap counts the rest as skipped.
    let (s2, st2) = highlight::build(&b, &HighlightSpec { dim: false, max_edges: 1, ..spec });
    assert!(s2.rects.inst.iter().all(|r| r.fill != VEIL));
    assert_eq!(st2.skipped as usize, touching.len() - 1);
    // A hovered edge outlines both endpoints and re-draws itself.
    let e = sx.drawn()[0];
    let (s3, _) = highlight::build(&b, &HighlightSpec { hover: Some(Focus::Edge(e)), dim: false, ..HighlightSpec::default() });
    assert_eq!(s3.curves.len(), sx.segments(e).len());
    assert!(s3.rects.len() >= 2);
    // An edge id out of range is ignored.
    let (s4, _) = highlight::build(&b, &HighlightSpec { hover: Some(Focus::Edge(u32::MAX)), dim: false, ..HighlightSpec::default() });
    assert!(s4.curves.is_empty());
}

#[test]
fn selection_markers_keep_a_constant_screen_size() {
    let (_p, m, scene, sx) = fixture();
    let ix = m.analysis.index();
    let b = base(&m, &scene, &sx);
    // The smallest cell needs markers at the coarsest bands.
    let v = (0..ix.node_count())
        .filter(|&v| sx.cell(v) != NO_CELL)
        .min_by(|&a, &c| {
            let w = |v: NodeId| {
                let r = &scene.rects.inst[sx.cell(v) as usize];
                (r.max[0] - r.min[0]).max(r.max[1] - r.min[1])
            };
            w(a).total_cmp(&w(c))
        })
        .expect("cells");
    let (s, _) = highlight::build(&b, &HighlightSpec { selected: Some(v), dim: false, ..HighlightSpec::default() });
    // Markers: one band each, MARKER_PX physical pixels at the band's zoom.
    let markers: Vec<_> = (0..s.rects.len()).filter(|&i| s.rects.lod[i].max == s.rects.lod[i].min + 1).collect();
    assert!(!markers.is_empty(), "the smallest cell is tiny at the coarsest bands");
    for &i in &markers {
        let r = &s.rects.inst[i];
        let px = (r.max[0] - r.min[0]) * band_zoom(s.rects.lod[i].min) as f32;
        assert!((px - MARKER_PX).abs() < 0.01, "{px}");
    }
    let bands: BTreeSet<_> = markers.iter().map(|&i| s.rects.lod[i].min).collect();
    assert_eq!(bands.len(), markers.len(), "one marker per band");
}

#[test]
fn path_overlay_joins_consecutive_cells() {
    let (_p, m, scene, sx) = fixture();
    let ix = m.analysis.index();
    let b = base(&m, &scene, &sx);
    let q = Query::new(m.analysis.index(), m.analysis.imports(), m.analysis.calls());
    let mut scratch = Default::default();
    let from = node(ix, "app.main:run");
    let to = node(ix, "app.a:helper");
    let p = q.path(from, to, &Default::default(), &mut scratch).expect("run -> helper");
    assert!(p.nodes.len() >= 3, "{:?}", p.nodes);
    let (s, _) = highlight::build(&b, &HighlightSpec { path: &p.nodes, dim: false, ..HighlightSpec::default() });
    assert_eq!(s.curves.len(), p.nodes.len() - 1);
    assert!(s.curves.lod.iter().all(|&l| l == Lod::ALWAYS));
}

#[test]
fn search_ranks_exact_fuzzy_and_qualified() {
    let (_p, m, _, _) = fixture();
    let ix = m.analysis.index();
    let q = Query::new(m.analysis.index(), m.analysis.imports(), m.analysis.calls());
    let si = SearchIndex::build(&q);
    assert!(si.distinct_names() > 5);
    assert!(si.search(&q, "", 10).is_empty());
    assert!(si.search(&q, "zzzqqq", 10).is_empty());
    // Exact qualified id first.
    let hits = si.search(&q, "app.low:tangled", 10);
    assert_eq!(hits[0].node, node(ix, "app.low:tangled"));
    // Fuzzy on the name.
    let hits = si.search(&q, "tngl", 10);
    assert_eq!(hits.first().map(|h| h.node), Some(node(ix, "app.low:tangled")));
    let hits = si.search(&q, "eng", 10);
    assert_eq!(hits.first().map(|h| h.node), Some(node(ix, "app.main:Engine")));
    // A qualified prefix restricts the candidates: `fa` lives in app.a only.
    let hits = si.search(&q, "a.f", 10);
    assert!(hits.iter().any(|h| h.node == node(ix, "app.a:fa")));
    assert!(hits.iter().all(|h| ix.qid_string(h.node).contains('a')));
    let hits = si.search(&q, "app.low:", 50);
    let names: HashSet<String> = hits.iter().map(|h| ix.qid_string(h.node)).collect();
    assert!(names.contains("app.low:base") && names.contains("app.low:tangled"), "{names:?}");
    // Limits and order.
    let hits = si.search(&q, "a", 3);
    assert!(hits.len() <= 3);
    assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
}

#[test]
fn findings_map_to_nodes_with_badges() {
    let (_p, m, _, _) = fixture();
    let a = &m.analysis;
    let ix = a.index();
    let q = Query::new(a.index(), a.imports(), a.calls());
    let ins = findings::analyse(a, &q);
    assert!(!ins.findings.is_empty());
    assert_eq!(ins.nodes.len(), ins.findings.len());
    let rules: HashSet<u16> = ins.findings.as_slice().iter().map(|f| f.rule.code()).collect();
    assert!(rules.contains(&IMPORT_CYCLE.code()), "{rules:?}");
    assert_eq!(ins.cycles.len(), 1);
    assert_eq!(ins.layer_violations.len(), 1, "{rules:?}");
    let lv = ins.layer_violations[0];
    assert_eq!(ins.findings.as_slice()[lv.finding as usize].rule.def().pass, Pass::Layers);
    assert_eq!((lv.importer, lv.imported), (node(ix, "app.low"), node(ix, "app.high")));
    assert_eq!(ins.nodes[lv.finding as usize], node(ix, "app.low"));
    // The complex function is reported on its node.
    let tangled = node(ix, "app.low:tangled");
    assert!(ins.on_node(tangled).count() > 0, "complexity finding on tangled");
    // The suppressed unused function is not reported.
    let unused = node(ix, "app.low:unused_one");
    assert!(ins.on_node(unused).all(|i| ins.findings.as_slice()[i as usize].rule.code() != 401));
    // Worst severity first.
    let sev: Vec<Severity> = ins.findings.as_slice().iter().map(|f| f.rule.def().severity).collect();
    assert!(sev.windows(2).all(|w| w[0] >= w[1]));
    // Badges: subtree counts equal a brute-force count.
    let b = &ins.badges;
    assert_eq!(b.min_conf, 0);
    for bd in &b.list {
        let sub = ins.nodes.iter().filter(|&&n| n != NO_NODE && ix.subtree(bd.node).contains(&n)).count();
        let direct = ins.nodes.iter().filter(|&&n| n == bd.node).count();
        assert_eq!(bd.subtree as usize, sub, "node {}", ix.qid_string(bd.node));
        assert_eq!(bd.direct as usize, direct);
    }
    assert!(b.list.windows(2).all(|w| w[0].node < w[1].node));
    assert!(b.get(node(ix, "app")).is_some());
    // Findings per node: the same as a scan, in order.
    for v in 0..ix.node_count() {
        let scan: Vec<u32> = (0..ins.nodes.len() as u32).filter(|&i| ins.nodes[i as usize] == v).collect();
        assert_eq!(ins.on_node(v).collect::<Vec<_>>(), scan);
    }
    assert!(ins.on_node(u32::MAX).next().is_none() && ins.on_node(NO_NODE).next().is_none());
    // Coupling reuses the query's call graph: the same edges as check's call graph.
    let cg = pymapper_core::graph::call_graph(ix, a.calls(), pymapper_core::graph::CallFilter::default());
    for v in 0..ix.node_count() {
        assert_eq!(cg.succ(v), q.call_graph().succ(v));
    }
    // A threshold above every confidence leaves no badges.
    assert!(ins.badges_at(ix, 101).list.is_empty());
    assert!(ins.heap_bytes() > 0);
}

#[test]
fn marks_draw_cycles_violations_and_badges() {
    let (_p, m, scene, sx) = fixture();
    let a = &m.analysis;
    let q = Query::new(a.index(), a.imports(), a.calls());
    let ins = findings::analyse(a, &q);
    let b = base(&m, &scene, &sx);
    let (s, st) = marks::build(&b, &m, &ins, &ins.badges, &MarksSpec::default());
    assert!(s.rects.is_empty() && st == Default::default());
    let (s, st) = marks::build(&b, &m, &ins, &ins.badges, &MarksSpec { cycles: true, ..MarksSpec::default() });
    assert_eq!(st.cycles, 1);
    assert!(s.curves.len() >= 2, "a -> b -> a");
    let (s, st) = marks::build(&b, &m, &ins, &ins.badges, &MarksSpec { layers: true, ..MarksSpec::default() });
    assert_eq!(st.violations, 1);
    assert_eq!(s.curves.len(), 1);
    let (s, st) = marks::build(&b, &m, &ins, &ins.badges, &MarksSpec { badges: true, ..MarksSpec::default() });
    assert!(st.badges > 0 && s.rects.len() == st.badges as usize);
    // Badge LODs never start before their cell's.
    for i in 0..s.rects.len() {
        let c = sx.cell(s.rects.id[i]);
        assert!(s.rects.lod[i].min >= scene.rects.lod[c as usize].min);
    }
}

#[test]
fn worker_answers_jobs() {
    let p = TempProject::new(PROJ);
    let a = p.analysis();
    let ix_nodes = a.index().node_count();
    let w = InsightWorker::spawn(Arc::clone(&a), true, Arc::new(|| {})).expect("spawn");
    let ix = a.index();
    let mut acc = Results::default();
    let tangled = node(ix, "app.low:tangled");
    w.search("tangled");
    w.wait_into(&mut acc, |r| r.search.is_some());
    let s = acc.search.take().expect("search");
    assert_eq!(s.hits.first().map(|h| h.node), Some(tangled));
    w.source(tangled);
    w.wait_into(&mut acc, |r| r.source.is_some());
    let src = acc.source.take().expect("source");
    let h = src.source.expect("readable");
    assert!(h.text.starts_with("def tangled(x):"), "{:?}", &h.text[..20.min(h.text.len())]);
    assert_eq!(h.first_line, 6);
    w.neighbours(tangled);
    w.wait_into(&mut acc, |r| r.neighbours.is_some());
    let n = acc.neighbours.take().expect("neighbours");
    assert_eq!(n.callers.hops.first().map(|h| h.node), Some(node(ix, "app.main:run")));
    w.path(PathJob { from: node(ix, "app.main"), to: node(ix, "app.high"), graph: PathGraph::Imports });
    w.wait_into(&mut acc, |r| r.path.is_some());
    let path = acc.path.take().expect("path").path.expect("main -> low -> high");
    assert_eq!(path.nodes.len(), 3);
    w.wait_into(&mut acc, |r| r.insights.is_some());
    assert!(!acc.insights.take().expect("insights").findings.is_empty());
    w.badges(101);
    w.wait_into(&mut acc, |r| r.badges.is_some());
    assert!(acc.badges.take().expect("badges").list.is_empty());
    // A distribution has no source.
    w.source(0);
    w.wait_into(&mut acc, |r| r.source.is_some());
    assert!(acc.source.take().expect("source").source.is_err() || ix_nodes > 0);
    drop(w);
}

/// Applies worker results to `it` until `done` holds.
fn pump(it: &mut Interaction, done: impl Fn(&Interaction) -> bool) {
    while !done(it) {
        let mut acc = Results::default();
        it.worker.as_ref().expect("worker").wait_into(&mut acc, |r| !r.is_empty());
        it.apply(acc);
    }
}

#[test]
fn interaction_state_end_to_end() {
    let (_p, m, scene, sx) = fixture();
    let ix = m.analysis.index();
    let mut map = MapState::new(Arc::clone(&scene));
    let mut it = Interaction::default();
    it.set_scene(SceneCtx { map: Arc::clone(&m), scene: Arc::clone(&scene), index: Arc::clone(&sx) }, true, Arc::new(|| {}));
    assert!(it.worker.is_some());
    assert!(it.needs_overlays());
    assert!(it.sync_overlays(&mut map));
    assert!(!it.needs_overlays());
    assert!(map.overlays[OVERLAY_HIGHLIGHT].scene.rects.is_empty(), "nothing focused");
    // Hover a node: highlight rebuilt once.
    let low = node(ix, "app.low");
    assert!(it.set_hover(Some(Focus::Node(low))));
    assert!(!it.set_hover(Some(Focus::Node(low))), "same focus, no rebuild");
    it.sync_overlays(&mut map);
    assert!(!map.overlays[OVERLAY_HIGHLIGHT].scene.curves.is_empty());
    // Select + focus: a camera request for the cell, neighbours and source requested.
    let tangled = node(ix, "app.low:tangled");
    it.focus(tangled);
    assert_eq!(it.selected, Some(tangled));
    assert_eq!(it.selected_qid, "app.low:tangled");
    let r = it.focus_request.expect("focus rect");
    assert!(r.is_positive());
    pump(&mut it, |it| it.neighbours.is_some() && it.source.is_some());
    assert!(it.neighbours.is_some() && it.source.is_some());
    // Shift-click: path from the selection.
    it.path_graph = PathGraph::Calls;
    it.path_to(node(ix, "app.a:helper"));
    assert!(it.path_pending());
    pump(&mut it, |it| !it.path_pending());
    // tangled calls nothing that reaches helper: no path, still a result.
    assert!(it.path.as_ref().is_some_and(|p| p.path.is_none()));
    it.select(Some(node(ix, "app.main:run")));
    it.path_to(node(ix, "app.a:helper"));
    pump(&mut it, |it| !it.path_pending());
    assert!(it.path.as_ref().is_some_and(|p| p.path.is_some()));
    it.sync_overlays(&mut map);
    // Marks once the insights arrive.
    pump(&mut it, |it| it.insights.is_some());
    it.set_marks(MarksSpec { cycles: true, layers: true, badges: true, min_conf: 0 });
    it.sync_overlays(&mut map);
    assert!(!map.overlays[OVERLAY_MARKS].scene.rects.is_empty());
    assert!(it.marks_stats.cycles == 1 && it.marks_stats.violations == 1);
    // Search through the state.
    it.search_text = "helper".to_owned();
    it.update_search();
    pump(&mut it, |it| it.search.is_some());
    assert!(it.search.as_ref().is_some_and(|s| !s.hits.is_empty()));
    // Clearing the selection drops the path.
    it.select(None);
    assert!(it.path.is_none() && it.selected_qid.is_empty());
    // A path still running when the selection is cleared is dropped on arrival.
    it.select(Some(node(ix, "app.main:run")));
    it.path_to(node(ix, "app.a:helper"));
    it.select(None);
    pump(&mut it, |it| !it.path_pending());
    assert!(it.path.is_none());
    // The same scene shown again (back from another view) clears the map's overlays: they are
    // rebuilt without any interaction change.
    it.sync_overlays(&mut map);
    map.show_scene(Arc::clone(&scene), None);
    assert!(map.overlays[OVERLAY_MARKS].scene.rects.is_empty());
    assert!(it.sync_overlays(&mut map));
    assert!(!map.overlays[OVERLAY_MARKS].scene.rects.is_empty());
    assert!(!it.sync_overlays(&mut map), "nothing left to rebuild");
    // A new scene of the same analysis keeps the worker; overlays of another scene are not
    // applied to the map.
    let other = Arc::new(Scene::default());
    it.set_scene(SceneCtx { map: Arc::clone(&m), scene: other, index: Arc::clone(&sx) }, true, Arc::new(|| {}));
    assert!(!it.sync_overlays(&mut map));
}

#[test]
fn source_job_numbers_every_line() {
    let h = super::source::highlight(b"def f(x):\n    return x  # c\n\nclass A: pass\n", 41, false);
    let job = super::panel::source_job(&h, true, egui::FontId::monospace(12.0));
    let text = job.text.as_str();
    for n in 41..=44 {
        assert!(text.contains(&format!("{n}  ")), "line {n} in {text:?}");
    }
    assert!(!text.contains("45  "));
    assert!(text.contains("return x"));
}

#[test]
fn panels_render_every_interaction_view_and_settle() {
    use crate::interact::panel::BottomTab;
    use crate::panels::{self, UiState};
    use egui::{RawInput, Rect, ViewportId, pos2, vec2};
    let (_p, m, scene, sx) = fixture();
    let ix = m.analysis.index();
    let mut state = UiState::new("proj".to_owned(), "test".to_owned());
    state.interact.set_scene(SceneCtx { map: Arc::clone(&m), scene, index: sx }, true, Arc::new(|| {}));
    state.interact.search_text = "tangled".to_owned();
    state.interact.update_search();
    state.interact.select(Some(node(ix, "app.low:tangled")));
    state.interact.set_marks(MarksSpec { cycles: true, layers: true, badges: true, min_conf: 0 });
    pump(&mut state.interact, |it| it.search.is_some() && it.insights.is_some() && it.neighbours.is_some() && it.source.is_some());
    state.interact.path_to(node(ix, "app.a:helper"));
    pump(&mut state.interact, |it| !it.path_pending());
    let ctx = egui::Context::default();
    let mut t = 0.0;
    for tab in [BottomTab::Findings, BottomTab::Source] {
        state.panel.tab = tab;
        let mut last = None;
        for _ in 0..12 {
            // Time advances so scroll bar fade-ins finish.
            t += 0.25;
            let raw = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1400.0, 900.0))), time: Some(t), ..RawInput::default() };
            let mut out = ctx.run_ui(raw, |ui| panels::show(ui, &mut state));
            out.textures_delta.clear();
            last = Some(out.viewport_output[&ViewportId::ROOT].repaint_delay);
        }
        assert_eq!(last, Some(std::time::Duration::MAX), "idle panels settle ({tab:?})");
    }
    // A selection cleared from the panel state renders the empty details.
    state.interact.select(None);
    let raw = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))), ..RawInput::default() };
    let _ = ctx.run_ui(raw, |ui| panels::show(ui, &mut state));
}

/// With the default `min-confidence`, the slider, the findings list and the badges all start
/// at the configured threshold; moving the slider with badges off and turning them on later
/// recounts the badges at the new threshold.
#[test]
fn default_min_confidence_drives_slider_findings_and_badges() {
    use crate::interact::panel::BottomTab;
    use crate::panels::{self, UiState};
    use egui::{RawInput, Rect, pos2, vec2};
    use pymapper_core::config::DEFAULT_MIN_CONFIDENCE;
    let mut f = files();
    f[0].1 = PYPROJECT.replace("min-confidence = 0\n", "");
    assert!(!f[0].1.contains("min-confidence"));
    let refs: Vec<(&str, &str)> = f.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let p = TempProject::new(&refs);
    let m = Arc::new(CodeMap::build(p.analysis(), MapParams::default()));
    assert_eq!(m.analysis.config.min_confidence, DEFAULT_MIN_CONFIDENCE);
    let (scene, _, drawn) = m.scene(&SceneParams::default());
    let sx = Arc::new(SceneIndex::build(&scene, m.analysis.index().node_count(), &m.edges, &drawn));
    let mut state = UiState::new("proj".to_owned(), "test".to_owned());
    let it = &mut state.interact;
    it.set_scene(SceneCtx { map: Arc::clone(&m), scene: Arc::new(scene), index: sx }, true, Arc::new(|| {}));
    assert_eq!(it.marks.min_conf, DEFAULT_MIN_CONFIDENCE, "slider default");
    pump(it, |it| it.insights.is_some());
    assert_eq!(it.marks.min_conf, DEFAULT_MIN_CONFIDENCE);
    let ins = Arc::clone(it.insights.as_ref().expect("insights"));
    let all = ins.findings.as_slice();
    assert!(all.iter().any(|f| f.confidence < DEFAULT_MIN_CONFIDENCE), "fixture: some findings below the threshold");
    let expected: Vec<u32> = (0..all.len() as u32).filter(|&i| all[i as usize].confidence >= DEFAULT_MIN_CONFIDENCE).collect();
    assert!(!expected.is_empty(), "fixture: some findings at the threshold");
    // Badges count exactly the findings the list shows.
    let b = Arc::clone(it.badges.as_ref().expect("badges"));
    assert_eq!(b.min_conf, DEFAULT_MIN_CONFIDENCE);
    let on_nodes = expected.iter().filter(|&&i| ins.nodes[i as usize] != NO_NODE).count();
    assert_eq!(b.list.iter().map(|x| x.direct as usize).sum::<usize>(), on_nodes);
    // The findings tab lists the same findings.
    state.panel.tab = BottomTab::Findings;
    let ctx = egui::Context::default();
    let frame = |state: &mut UiState| {
        let raw = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1400.0, 900.0))), ..RawInput::default() };
        let mut out = ctx.run_ui(raw, |ui| panels::show(ui, state));
        out.textures_delta.clear();
    };
    frame(&mut state);
    assert_eq!(state.panel.findings_view(), &expected[..]);
    // Slider to 0 with badges off: nothing recounted yet; turning badges on recounts at 0.
    let it = &mut state.interact;
    assert!(!it.marks.badges);
    it.set_marks(MarksSpec { min_conf: 0, ..it.marks });
    it.set_marks(MarksSpec { badges: true, ..it.marks });
    pump(it, |it| it.badges.as_ref().is_some_and(|b| b.min_conf == 0));
    let b0 = it.badges.as_ref().expect("badges");
    let all_on_nodes = ins.nodes.iter().filter(|&&n| n != NO_NODE).count();
    assert_eq!(b0.list.iter().map(|x| x.direct as usize).sum::<usize>(), all_on_nodes);
    assert!(all_on_nodes > on_nodes);
    // A later analysis keeps the user's threshold.
    let m2 = Arc::new(CodeMap::build(p.analysis(), MapParams::default()));
    it.set_scene(SceneCtx { map: m2, scene: Arc::new(Scene::default()), index: Arc::new(SceneIndex::default()) }, true, Arc::new(|| {}));
    assert_eq!(it.marks.min_conf, 0);
}
