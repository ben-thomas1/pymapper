//! Code map tests: reveal bands and cuts, cut edges against a brute-force reference, edge
//! bundling geometry, scene LODs and label carriers, the legend, loading and the worker.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use pymapper_core::index::{Index, NodeId};
use pymapper_core::layout::{Layout, LayoutConfig, LayoutTree, Region, SizeMetric, TreeOptions};

use super::bundle::{self, Bundler, Cubic, MAX_POINTS};
use super::edges::{self, DST_OPEN, EdgeParams, MapEdge, SRC_OPEN};
use super::load::{Analysis, LoadParams, load};
use super::metrics::{self, ColorLegend, ColorMetric, Colorizer, EdgeMetric};
use super::scene::{self, SceneParams};
use super::worker::{MapWorker, Request};
use super::zoom::{REVEAL_PX, Reveal, reveal_band};
use super::{CodeMap, MapParams};
use crate::map::scene::{Band, LOD_UNBOUNDED, Lod, Rgba};

/// A temp project: `files` (relative path, source) under a fresh directory with a
/// `pyproject.toml`, removed on drop.
struct TempProject(PathBuf);

impl TempProject {
    fn new(files: &[(&str, &str)]) -> TempProject {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-codemap-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        fs::write(dir.join("pyproject.toml"), "[project]\nname = \"proj\"\nversion = \"0\"\n").expect("pyproject");
        for (rel, src) in files {
            let p = dir.join(rel);
            fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            fs::write(&p, src).expect("write");
        }
        TempProject(fs::canonicalize(&dir).expect("canonical"))
    }

    fn analysis(&self) -> Arc<Analysis> {
        Arc::new(load(&params(&self.0), &|_| {}).expect("load"))
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn params(path: &Path) -> LoadParams {
    LoadParams { path: path.to_path_buf(), isolated: true, deps: Some(false), stdlib: Some(false), cache_dir: None, no_cache: true, watch: false }
}

/// Nested packages, cross-module calls (functions, constructors, methods), module-level
/// calls, imports and a class hierarchy: enough for every cut state and edge metric.
const PROJ: &[(&str, &str)] = &[
    ("app/__init__.py", ""),
    (
        "app/main.py",
        "from app.core import engine\nfrom app.core.models import User\nfrom app import util\n\n\
         def run():\n    e = engine.Engine()\n    e.start()\n    u = User('x')\n    util.log(u.name)\n    return helper()\n\n\
         def helper():\n    if True:\n        return util.log('h')\n    return None\n\n\
         run()\n",
    ),
    ("app/util.py", "def log(msg):\n    for c in msg:\n        if c:\n            print(c)\n    return msg\n\ndef unused():\n    return 1\n"),
    ("app/core/__init__.py", ""),
    (
        "app/core/engine.py",
        "from app import util\n\nclass Engine:\n    def __init__(self):\n        self.n = 0\n\n    def start(self):\n        util.log('start')\n        self.step()\n\n    def step(self):\n        if self.n:\n            return 1\n        elif self.n > 2:\n            return 2\n        return 0\n",
    ),
    (
        "app/core/models.py",
        "from app.util import log\n\nclass Base:\n    def __init__(self, name):\n        self.name = name\n\nclass User(Base):\n    def greet(self):\n        return log(self.name)\n",
    ),
    ("tools/__init__.py", ""),
    ("tools/cli.py", "from app.main import run\nfrom app.util import log\n\ndef main():\n    log('cli')\n    run()\n\nif __name__ == '__main__':\n    main()\n"),
];

/// A code map of `PROJ` laid out in a small world, so the reveal bands spread over several
/// bands near the fit zoom.
fn fixture() -> (TempProject, CodeMap) {
    let p = TempProject::new(PROJ);
    let a = p.analysis();
    let m = CodeMap::build(a, MapParams { edges: EdgeParams { per_source: 0, workers: 2 }, ..MapParams::default() });
    (p, m)
}

fn ix(m: &CodeMap) -> &Index {
    m.analysis.index()
}

fn node(m: &CodeMap, qid: &str) -> NodeId {
    let ix = ix(m);
    (0..ix.node_count()).find(|&v| ix.qid_string(v) == qid).unwrap_or_else(|| panic!("no node {qid}"))
}

#[test]
fn reveal_bands_follow_the_hierarchy() {
    let (_p, m) = fixture();
    let ix = ix(&m);
    let r = &m.reveal;
    assert_eq!(r.len(), ix.node_count() as usize);
    for v in 0..ix.node_count() {
        let (rev, open) = (r.rev[v as usize], r.open[v as usize]);
        match ix.parent(v) {
            None => assert_eq!(rev, 0, "distributions are always shown"),
            Some(p) => assert_eq!(rev, r.open[p as usize], "children appear when their parent opens"),
        }
        assert!(open >= rev, "a node opens after it is shown");
        let has_children = ix.subtree(v).len() > 1;
        if !has_children {
            assert_eq!(open, LOD_UNBOUNDED, "leaves never open");
        } else {
            let rect = m.layout.rect(v).expect("laid out");
            assert_eq!(open, reveal_band(&rect).max(rev));
        }
    }
    let bands = r.change_bands();
    assert_eq!(bands[0], 0);
    assert!(bands.windows(2).all(|w| w[0] < w[1]));
    assert!(bands.len() >= 3, "{bands:?}: the fixture should open over several bands");
    assert!(!bands.contains(&LOD_UNBOUNDED));
}

#[test]
fn reveal_band_is_the_first_band_with_enough_pixels() {
    let r = pymapper_core::layout::Rect::new(0.0, 0.0, 400.0, 100.0);
    let b = reveal_band(&r);
    let side = 200.0f64; // sqrt(400 * 100)
    let zoom = |b: Band| crate::map::scene::band_zoom(b);
    assert!(side * zoom(b) >= f64::from(REVEAL_PX));
    assert!(side * zoom(b - 1) < f64::from(REVEAL_PX));
    assert_eq!(reveal_band(&pymapper_core::layout::Rect::new(0.0, 0.0, 0.0, 10.0)), LOD_UNBOUNDED);
}

/// The cut at `b` by definition: the deepest ancestor-or-self shown at `b`.
fn cut_ref(ix: &Index, r: &Reveal, b: Band, v: NodeId) -> NodeId {
    let mut path = vec![v];
    let mut x = v;
    while let Some(p) = ix.parent(x) {
        path.push(p);
        x = p;
    }
    // Shown: every ancestor-or-self from the root down has rev <= b.
    let mut best = *path.last().expect("root");
    for &u in path.iter().rev() {
        if r.rev[u as usize] > b {
            break;
        }
        best = u;
    }
    best
}

#[test]
fn cuts_are_monotone_and_whole_sibling_sets() {
    let (_p, m) = fixture();
    let ix = ix(&m);
    let mut map = Vec::new();
    let mut bands = m.reveal.change_bands();
    bands.push(254);
    for &b in &bands {
        m.reveal.fill_cut(ix, b, &mut map);
        assert!(map.windows(2).all(|w| w[0] <= w[1]), "band {b}: not monotone");
        let shown: HashSet<NodeId> = map.iter().copied().collect();
        for v in 0..ix.node_count() {
            assert_eq!(map[v as usize], cut_ref(ix, &m.reveal, b, v), "band {b} node {v}");
            // Siblings are shown together.
            if let Some(p) = ix.parent(v) {
                for c in ix.children(p) {
                    assert_eq!(m.reveal.rev[c as usize] <= b, m.reveal.rev[v as usize] <= b);
                }
            }
        }
        // Every shown node maps to itself.
        for &s in &shown {
            assert_eq!(map[s as usize], s);
        }
    }
}

#[test]
fn endpoint_states_partition_the_bands_after_reveal() {
    let (_p, m) = fixture();
    let ix = ix(&m);
    for v in 0..ix.node_count() {
        let (rev, open) = (m.reveal.rev[v as usize], m.reveal.open[v as usize]);
        if rev == LOD_UNBOUNDED {
            continue;
        }
        for b in rev..=254 {
            let e = m.reveal.endpoint(v, b);
            assert!(e.lod.contains(b));
            assert_eq!(e.open, b >= open);
            if e.open {
                assert_eq!(e.lod, Lod { min: open, max: LOD_UNBOUNDED });
            } else {
                assert_eq!(e.lod, Lod { min: rev, max: open });
            }
        }
    }
}

#[test]
fn clamping_stops_external_nodes_at_a_depth() {
    let (_p, m) = fixture();
    let ix = ix(&m);
    // Treat everything under `tools` as external.
    let tools = node(&m, "tools");
    let ext = |v: NodeId| ix.subtree(tools).contains(&v) || ix.is_descendant(tools, v);
    let c = m.reveal.clamped(ix, ix.depth(tools), ext);
    for v in 0..ix.node_count() {
        let i = v as usize;
        if !ext(v) {
            assert_eq!((c.rev[i], c.open[i]), (m.reveal.rev[i], m.reveal.open[i]));
        } else if v == tools {
            assert_eq!((c.rev[i], c.open[i]), (m.reveal.rev[i], LOD_UNBOUNDED));
        } else {
            assert_eq!((c.rev[i], c.open[i]), (LOD_UNBOUNDED, LOD_UNBOUNDED));
        }
    }
}

fn close(a: [f32; 2], b: [f32; 2]) -> bool {
    (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3
}

#[test]
fn spline_segments_are_clamped_and_smooth() {
    let mut out: Vec<Cubic> = Vec::new();
    assert_eq!(bundle::spline(&[], &mut out), 0);
    assert_eq!(bundle::spline(&[[1.0, 1.0]], &mut out), 0);
    // Two points: a straight segment.
    assert_eq!(bundle::spline(&[[0.0, 0.0], [3.0, 0.0]], &mut out), 1);
    assert_eq!(out[0], [[0.0, 0.0], [1.0, 0.0], [2.0, 0.0], [3.0, 0.0]]);
    // Three points: one cubic pulled toward the middle point.
    out.clear();
    assert_eq!(bundle::spline(&[[0.0, 0.0], [3.0, 3.0], [6.0, 0.0]], &mut out), 1);
    assert!(close(out[0][1], [2.0, 2.0]) && close(out[0][2], [4.0, 2.0]), "{:?}", out[0]);
    // Many points: n - 1 segments, exact endpoints, C0 joints, C1 at inner joints, inside
    // the control polygon's bbox.
    let pts = [[0.0, 0.0], [10.0, 40.0], [30.0, 50.0], [60.0, 20.0], [80.0, 60.0], [100.0, 0.0]];
    out.clear();
    assert_eq!(bundle::spline(&pts, &mut out), pts.len() - 1);
    assert_eq!(out[0][0], pts[0]);
    assert_eq!(out[out.len() - 1][3], pts[pts.len() - 1]);
    for w in out.windows(2) {
        assert_eq!(w[0][3], w[1][0], "joint");
    }
    for i in 1..out.len() - 2 {
        let (a, b) = (&out[i], &out[i + 1]);
        let t0 = [a[3][0] - a[2][0], a[3][1] - a[2][1]];
        let t1 = [b[1][0] - b[0][0], b[1][1] - b[0][1]];
        assert!(close(t0, t1), "tangent at joint {i}: {t0:?} vs {t1:?}");
    }
    for c in &out {
        for p in c {
            assert!((0.0..=100.0).contains(&p[0]) && (0.0..=60.0).contains(&p[1]), "{p:?}");
        }
    }
}

#[test]
fn straighten_and_thin() {
    let pts = [[0.0, 0.0], [5.0, 10.0], [10.0, -10.0], [15.0, 0.0]];
    let mut p = pts;
    bundle::straighten(&mut p, 1.0);
    assert_eq!(p, pts);
    bundle::straighten(&mut p, 0.0);
    for (i, q) in p.iter().enumerate() {
        assert!(close(*q, [5.0 * i as f32, 0.0]), "{q:?}");
    }
    let mut p = pts;
    bundle::straighten(&mut p, f32::NAN); // falls back to the default strength
    assert!(p[1][1] > 0.0 && p[1][1] < 10.0);
    let long: Vec<[f32; 2]> = (0..20).map(|i| [i as f32, 0.0]).collect();
    let mut out = Vec::new();
    bundle::thin(&long, MAX_POINTS, &mut out);
    assert_eq!(out.len(), MAX_POINTS);
    assert_eq!((out[0], out[MAX_POINTS - 1]), (long[0], long[19]));
    assert!(out.windows(2).all(|w| w[0][0] < w[1][0]), "order kept, no repeats");
    bundle::thin(&long[..5], MAX_POINTS, &mut out);
    assert_eq!(out, &long[..5]);
    bundle::thin(&long, 0, &mut out);
    assert_eq!(out, vec![long[0], long[19]]);
}

fn center(r: pymapper_core::layout::Rect) -> [f32; 2] {
    [(r.x0 + r.x1) * 0.5, (r.y0 + r.y1) * 0.5]
}

#[test]
fn routes_follow_the_hierarchy() {
    let (_p, m) = fixture();
    let ix = ix(&m);
    let mut b = Bundler::default();
    let mut out = Vec::new();
    // Siblings: one cubic between the two rect centres, bowed toward the parent.
    let (run, helper) = (node(&m, "app.main:run"), node(&m, "app.main:helper"));
    assert_eq!(b.route(ix, &m.layout, run, false, helper, false, 1.0, &mut out), 1);
    let (cr, ch, cp) = (center(m.layout.rect(run).unwrap()), center(m.layout.rect(helper).unwrap()), center(m.layout.rect(node(&m, "app.main")).unwrap()));
    assert!(close(out[0][0], cr) && close(out[0][3], ch));
    assert!(close(out[0][1], [cr[0] + (cp[0] - cr[0]) * 2.0 / 3.0, cr[1] + (cp[1] - cr[1]) * 2.0 / 3.0]));
    // Across packages: several segments, from centre to centre.
    out.clear();
    let (cli, step) = (node(&m, "tools.cli:main"), node(&m, "app.core.engine:Engine.step"));
    let n = b.route(ix, &m.layout, cli, false, step, false, 0.85, &mut out);
    assert!((3..MAX_POINTS).contains(&n), "{n}");
    assert_eq!(out.len(), n);
    assert!(close(out[0][0], center(m.layout.rect(cli).unwrap())));
    assert!(close(out[n - 1][3], center(m.layout.rect(step).unwrap())));
    // An open endpoint is anchored at its header band; a parent-to-child edge ends inside.
    out.clear();
    let main = node(&m, "app.main");
    assert!(b.route(ix, &m.layout, main, true, run, false, 0.85, &mut out) >= 1);
    let head = m.layout.config().header_band(&m.layout.rect(main).unwrap());
    assert!(close(out[0][0], center(head)));
    // Beta 0 straightens everything onto the chord.
    out.clear();
    b.route(ix, &m.layout, cli, false, step, false, 0.0, &mut out);
    let (a, z) = (out[0][0], out[out.len() - 1][3]);
    for c in &out {
        for p in c {
            let cross = (z[0] - a[0]) * (p[1] - a[1]) - (z[1] - a[1]) * (p[0] - a[0]);
            assert!(cross.abs() < 1e-1 * (z[0] - a[0]).hypot(z[1] - a[1]), "{p:?} off the chord");
        }
    }
}

#[test]
fn routes_between_distributions_pass_the_region_centre() {
    // Three distributions: two first-party, one third-party. They have no common ancestor,
    // so edges bundle through the centres of their regions.
    let p = TempProject::new(&[("a/x.py", "def f():\n    return 1\n"), ("b/y.py", "def g():\n    return 1\n"), ("c/z.py", "def h():\n    return 1\n")]);
    let mut inputs = pymapper_core::index::Inputs::new();
    let groups = [
        inputs.add_group(b"one", pymapper_core::roots::Origin::FirstParty, None),
        inputs.add_group(b"two", pymapper_core::roots::Origin::FirstParty, None),
        inputs.add_group(b"ext", pymapper_core::roots::Origin::ThirdParty, None),
    ];
    for (g, (rel, module)) in groups.iter().zip([("a/x.py", b"x"), ("b/y.py", b"y"), ("c/z.py", b"z")]) {
        let path = p.0.join(rel);
        inputs.push(path.as_os_str().as_encoded_bytes(), *g, module, pymapper_core::roots::FileStatus::Active).expect("push");
    }
    let ix = pymapper_core::index::load(inputs);
    let agg = pymapper_core::rollup::Aggregates::build(&ix);
    let tree = Arc::new(LayoutTree::from_index(&ix, &agg, TreeOptions::default()));
    let mut layout = Layout::new(tree, LayoutConfig::default());
    layout.ensure(8);
    let roots: Vec<NodeId> = (0..ix.node_count()).filter(|&v| ix.parent(v).is_none()).collect();
    assert_eq!(roots.len(), 3);
    let find = |q: &str| (0..ix.node_count()).find(|&v| ix.qid_string(v) == q).expect(q);
    let (f, g, h) = (find("x:f"), find("y:g"), find("z:h"));
    let main = center(layout.region(Region::Main).expect("main"));
    let third = center(layout.region(Region::ThirdParty).expect("third-party"));
    let c = |v: NodeId| center(layout.rect(v).expect("rect"));
    let up = |v: NodeId| ix.parent(v).expect("parent");
    let mut b = Bundler::default();
    // f -> g: f, x, one, [main region], two, y, g.
    assert!(b.polygon(&ix, &layout, f, false, g, false));
    let want = [c(f), c(up(f)), c(up(up(f))), main, c(up(up(g))), c(up(g)), c(g)];
    assert_eq!(b.points(), &want);
    // f -> h crosses regions: both region centres, in order.
    assert!(b.polygon(&ix, &layout, f, false, h, false));
    let want = [c(f), c(up(f)), c(up(up(f))), main, third, c(up(up(h))), c(up(h)), c(h)];
    assert_eq!(b.points(), &want);
    let mut out = Vec::new();
    assert_eq!(b.route(&ix, &layout, f, false, h, false, 1.0, &mut out), MAX_POINTS - 1);
}

#[test]
fn scene_rects_carry_reveal_lods_and_moving_labels() {
    let (_p, m) = fixture();
    let ix = ix(&m);
    let (s, legend, drawn) = m.scene(&SceneParams::default());
    assert_eq!(legend.rects as u32, ix.node_count(), "every node of the fixture has a cell");
    // Per node: its cell and label carriers.
    let mut by_node: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, &id) in s.rects.id.iter().enumerate() {
        by_node.entry(id).or_default().push(i);
    }
    let mut carriers = 0;
    for v in 0..ix.node_count() {
        let items = &by_node[&v];
        let (cells, carry): (Vec<usize>, Vec<usize>) = items.iter().partition(|&&i| s.rects.inst[i].fill.0[3] > 0);
        assert_eq!(cells.len(), 1, "one visible cell per node");
        let cell = cells[0];
        let lod = s.rects.lod[cell];
        assert!(lod.min >= m.reveal.rev[v as usize], "cell shown before its parent opens");
        let open = m.reveal.open[v as usize];
        if open == LOD_UNBOUNDED {
            assert!(carry.is_empty());
            assert_eq!(s.label(cell), ix.name(v));
        } else {
            assert!(s.label(cell).is_empty(), "an opening cell moves its label to carriers");
            carriers += carry.len();
            for &c in &carry {
                assert_eq!(s.label(c), ix.name(v));
                let cl = s.rects.lod[c];
                assert!(cl.max == open || cl.min == open.max(lod.min), "{cl:?} open {open}");
                if cl.max == open {
                    // Collapsed: covers the cell.
                    assert_eq!((s.rects.inst[c].min, s.rects.inst[c].max), (s.rects.inst[cell].min, s.rects.inst[cell].max));
                } else {
                    // Open: the header band at the top of the cell.
                    assert_eq!(s.rects.inst[c].min, s.rects.inst[cell].min);
                    assert!(s.rects.inst[c].max[1] < s.rects.inst[cell].max[1]);
                }
            }
        }
        // Layer = depth.
        let layer = s.rects.layers.iter().find(|l| l.items.contains(&(cell as u32))).expect("layer");
        assert_eq!(u32::from(layer.layer), ix.depth(v));
    }
    assert!(carriers > 0);
    assert_eq!(s.rects.len(), ix.node_count() as usize + carriers);
    // Curves: segments of the drawn edges with their LODs.
    assert_eq!(legend.edges, drawn.len());
    assert_eq!(s.curves.len(), legend.segments);
    for (k, &id) in s.curves.id.iter().enumerate() {
        assert!(drawn.contains(&id));
        assert_eq!(s.curves.lod[k], m.edges[id as usize].lod);
    }
    assert_eq!(legend.edge_max, drawn.iter().map(|&i| m.edges[i as usize].weight(EdgeMetric::Sites)).max().unwrap_or(0));
    assert!(!legend.has_muted);
    // No edges when hidden.
    let (s, legend, drawn) = m.scene(&SceneParams { show_edges: false, ..SceneParams::default() });
    assert!(s.curves.is_empty() && drawn.is_empty() && legend.edges == 0);
}

#[test]
fn scenes_follow_the_selected_metrics() {
    let (_p, m) = fixture();
    let (_, l, _) = m.scene(&SceneParams { color: ColorMetric::CcMax, edge: EdgeMetric::Imports, ..SceneParams::default() });
    assert_eq!(l.color, ColorMetric::CcMax);
    assert!(matches!(l.color_legend, ColorLegend::Ramp { .. }));
    assert_eq!(l.edge, EdgeMetric::Imports);
    let imports_max = m.edges.iter().map(|e| e.weight(EdgeMetric::Imports)).max().unwrap_or(0);
    assert_eq!(l.edge_max, imports_max);
    let (_, l, drawn) = m.scene(&SceneParams { edge: EdgeMetric::LocReached, ..SceneParams::default() });
    assert!(drawn.iter().all(|&i| m.edges[i as usize].weight(EdgeMetric::LocReached) > 0));
    assert_eq!(l.color_legend, ColorLegend::Kinds);
}

#[test]
fn edge_selection_caps_each_band_by_weight() {
    let (_p, m) = fixture();
    let mut out = Vec::new();
    scene::select_edges(&m.edges, EdgeMetric::Pairs, 1, |_| true, &mut out);
    let mut per_band: HashMap<Band, Vec<u32>> = HashMap::new();
    for &i in &out {
        per_band.entry(m.edges[i as usize].lod.min).or_default().push(i);
    }
    for (b, ids) in &per_band {
        assert_eq!(ids.len(), 1, "band {b}");
        let best = m.edges.iter().filter(|e| e.lod.min == *b).map(|e| e.weight(EdgeMetric::Pairs)).max().unwrap_or(0);
        assert_eq!(m.edges[ids[0] as usize].weight(EdgeMetric::Pairs), best);
    }
    // The filter applies before the cap; no cap keeps every positive edge.
    let banned = m.edges[0].src;
    out.clear();
    scene::select_edges(&m.edges, EdgeMetric::Pairs, 0, |e| e.src != banned, &mut out);
    let want = m.edges.iter().filter(|e| e.src != banned && e.weight(EdgeMetric::Pairs) > 0).count();
    assert_eq!(out.len(), want);
    assert!(out.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn colours_and_ramps() {
    assert_eq!(metrics::ramp(0.0), metrics::ramp(-1.0));
    assert_eq!(metrics::ramp(1.0), metrics::ramp(7.0));
    assert_eq!(metrics::ramp(f32::NAN), metrics::ramp(0.0));
    assert_ne!(metrics::ramp(0.0), metrics::ramp(1.0));
    let (_p, m) = fixture();
    let (ix, agg) = (ix(&m), &m.analysis.agg);
    for metric in ColorMetric::ALL {
        let c = Colorizer::new(metric, ix, agg);
        for v in 0..ix.node_count() {
            match c.value(ix, agg, v) {
                None => {
                    assert_eq!(metric, ColorMetric::Kind);
                    assert_eq!(c.color(ix, agg, v), metrics::KIND_COLORS[ix.kind(v) as usize]);
                }
                Some(t) => assert!((0.0..=1.0).contains(&t) || metric == ColorMetric::CcMax || metric == ColorMetric::CogMax || metric == ColorMetric::CcDensity, "{metric:?} {t}"),
            }
        }
    }
    // Relative scales reach 1 at the largest node of each depth.
    let c = Colorizer::new(ColorMetric::Loc, ix, agg);
    let top = (0..ix.node_count()).filter(|&v| ix.depth(v) == 2).max_by_key(|&v| agg.loc()[v as usize]).expect("depth 2");
    assert!((c.value(ix, agg, top).unwrap() - 1.0).abs() < 1e-6);
    let grey = metrics::muted(Rgba::rgb(200, 40, 40));
    assert!(grey.0[0] < 200 && grey.0[1] > 40);
    assert_eq!(metrics::mix(Rgba::rgb(0, 0, 0), Rgba::rgb(200, 100, 50), 0.5), Rgba::rgb(100, 50, 25));
    assert_eq!(metrics::darken(Rgba::rgb(200, 100, 50), 0.5), Rgba::rgb(100, 50, 25));
}

#[test]
fn load_reports_phases_and_tables() {
    let p = TempProject::new(PROJ);
    let a = p.analysis();
    assert!(a.index().node_count() > 10);
    assert!(a.rollup.stats().call_pairs > 0);
    assert_eq!(a.agg.len(), a.index().node_count() as usize);
    let names: Vec<&str> = a.phases.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, ["config", "walk", "index", "resolve imports", "resolve calls", "rollup + aggregates"]);
    assert!(a.warnings.is_empty(), "{:?}", a.warnings);
    assert!(a.cache_dir.is_none(), "--no-cache");
    assert_eq!(a.root, p.0);
}

#[test]
fn size_metric_changes_the_layout_not_the_hierarchy() {
    let p = TempProject::new(PROJ);
    let a = p.analysis();
    let loc = CodeMap::build(Arc::clone(&a), MapParams::default());
    let cc = CodeMap::build(a, MapParams { size: SizeMetric::CcSum, ..MapParams::default() });
    assert_eq!(loc.reveal.len(), cc.reveal.len());
    assert_ne!(loc.layout.levels(), cc.layout.levels());
    assert!(loc.heap_bytes() > 0);
    let mut s = String::new();
    loc.describe_node(node(&loc, "app.util:log"), &mut s);
    assert!(s.starts_with("app.util:log\nfunction"), "{s}");
    loc.describe_edge(0, &mut s);
    assert!(s.contains("call sites:") && s.contains("imports:"), "{s}");
    loc.describe_edge(u32::MAX, &mut s);
    assert!(s.is_empty());
}

#[test]
fn worker_builds_scenes_and_reuses_the_map() {
    let p = TempProject::new(PROJ);
    let notified = Arc::new(AtomicU32::new(0));
    let n = Arc::clone(&notified);
    let w = MapWorker::spawn(params(&p.0), MapParams::default(), Request::default(), Arc::new(move || {
        n.fetch_add(1, Ordering::Relaxed);
    }))
    .expect("spawn");
    let first = w.wait().expect("first scene");
    assert!(first.scene.rects.len() > 10);
    assert!(first.map_build.is_some());
    assert!(notified.load(Ordering::Relaxed) > 0);
    // A colour change reuses the code map; a size change rebuilds it.
    w.request(Request { scene: SceneParams { color: ColorMetric::CogMax, ..SceneParams::default() }, ..Request::default() });
    let second = w.wait().expect("second scene");
    assert!(Arc::ptr_eq(&first.map, &second.map));
    assert!(second.map_build.is_none());
    assert_eq!(second.legend.color, ColorMetric::CogMax);
    w.request(Request { size: SizeMetric::Symbols, ..Request::default() });
    let third = w.wait().expect("third scene");
    assert!(!Arc::ptr_eq(&first.map, &third.map));
    assert_eq!(third.map.params.size, SizeMetric::Symbols);
    assert!(w.take().is_none());
}

#[test]
fn empty_and_tiny_projects_build_empty_or_small_scenes() {
    let p = TempProject::new(&[]);
    let m = CodeMap::build(p.analysis(), MapParams::default());
    assert!(m.edges.is_empty());
    let (s, l, drawn) = m.scene(&SceneParams::default());
    assert!(drawn.is_empty() && l.edges == 0 && l.edge_max == 0);
    assert_eq!(s.curves.len(), 0);
    let p = TempProject::new(&[("one.py", "x = 1\n")]);
    let m = CodeMap::build(p.analysis(), MapParams::default());
    let (s, l, _) = m.scene(&SceneParams::default());
    assert!(l.rects >= 1 && s.bounds.is_positive());
    assert!(m.edges.is_empty());
}

#[test]
fn worker_reports_configuration_errors() {
    let p = TempProject::new(&[("m.py", "x = 1\n"), ("pymapper.toml", "no-such-option = 1\n")]);
    let w = MapWorker::spawn(LoadParams { isolated: false, ..params(&p.0) }, MapParams::default(), Request::default(), Arc::new(|| {})).expect("spawn");
    let err = w.wait().err().expect("a configuration error");
    assert!(err.contains("no-such-option"), "{err}");
    assert!(matches!(w.status(), super::worker::Status::Failed(_)));
}

/// Reference rows of band `b`: (A, B) -> weights, from the cut by definition, the core
/// rollup (sites, pairs, imports: tested against raw edges in core) and hash sets for the
/// distinct-callee metrics.
fn reference_band(m: &CodeMap, reveal: &Reveal, b: Band) -> HashMap<(NodeId, NodeId), [u32; 6]> {
    let ix = ix(m);
    let a = &m.analysis;
    let map: Vec<NodeId> = (0..ix.node_count()).map(|v| cut_ref(ix, reveal, b, v)).collect();
    let mut rows = Vec::new();
    a.rollup.map_with(&map, &mut Default::default(), &mut rows);
    let mut out = HashMap::new();
    let mut callees: HashMap<(NodeId, NodeId), HashSet<NodeId>> = HashMap::new();
    let mut cx: HashMap<(NodeId, NodeId), u64> = HashMap::new();
    for (u, w, sites) in a.rollup.call_pairs() {
        let (s, d) = (map[u as usize], map[w as usize]);
        if s == d {
            continue;
        }
        callees.entry((s, d)).or_default().insert(w);
        *cx.entry((s, d)).or_default() += u64::from(sites) * u64::from(a.agg.cc_sum()[w as usize].max(1));
    }
    for r in rows {
        let key = (r.src, r.dst);
        let set = callees.remove(&key).unwrap_or_default();
        let loc: u64 = set.iter().map(|&w| u64::from(a.agg.loc()[w as usize])).sum();
        let mut w = [0u32; 6];
        w[EdgeMetric::Sites.column()] = r.sites;
        w[EdgeMetric::Pairs.column()] = r.pairs;
        w[EdgeMetric::ApiSurface.column()] = set.len() as u32;
        w[EdgeMetric::LocReached.column()] = loc as u32;
        w[EdgeMetric::Complexity.column()] = cx.get(&key).copied().unwrap_or(0) as u32;
        w[EdgeMetric::Imports.column()] = r.imports;
        if w.iter().any(|&x| x > 0) {
            out.insert(key, w);
        }
    }
    assert!(callees.is_empty(), "call pairs without a rollup row: {callees:?}");
    out
}

fn check_edges_against_reference(m: &CodeMap, reveal: &Reveal, table: &[MapEdge]) {
    let mut bands = reveal.change_bands();
    bands.push(254);
    // Emitted once per (src, dst, state) at the first band of its interval.
    let mut seen = HashSet::new();
    for e in table {
        assert!(seen.insert((e.src, e.dst, e.flags, e.lod.min)), "duplicate edge {e:?}");
        assert!(e.lod.min < e.lod.max, "empty LOD {e:?}");
        let (ea, eb) = (reveal.endpoint(e.src, e.lod.min), reveal.endpoint(e.dst, e.lod.min));
        assert_eq!(e.lod.min, ea.lod.min.max(eb.lod.min), "{e:?} not emitted at its first band");
        assert_eq!(e.lod.max, ea.lod.max.min(eb.lod.max));
    }
    let mut total = 0;
    for &b in &bands {
        let want = reference_band(m, reveal, b);
        let got: Vec<&MapEdge> = table.iter().filter(|e| e.lod.contains(b)).collect();
        total += want.len();
        assert_eq!(got.len(), want.len(), "band {b}: edge count");
        for e in got {
            let w = want.get(&(e.src, e.dst)).unwrap_or_else(|| panic!("band {b}: unexpected edge {e:?}"));
            assert_eq!(&e.w, w, "band {b}: weights of {} -> {}", ix(m).qid_string(e.src), ix(m).qid_string(e.dst));
            assert_eq!(e.flags & SRC_OPEN != 0, reveal.endpoint(e.src, b).open);
            assert_eq!(e.flags & DST_OPEN != 0, reveal.endpoint(e.dst, b).open);
        }
    }
    assert!(total > 0, "the fixture should have edges");
}

#[test]
fn cut_edges_match_the_reference_in_every_band() {
    let (_p, m) = fixture();
    let r = m.reveal.clamped(ix(&m), m.params.external_edge_depth, |v| m.layout.muted(v));
    check_edges_against_reference(&m, &r, &m.edges);
    // One worker and many give the same table.
    let one = edges::compute(ix(&m), &m.analysis.agg, &m.analysis.rollup, &r, EdgeParams { per_source: 0, workers: 1 });
    assert_eq!(one, m.edges);
    // Every metric and endpoint state is exercised somewhere.
    for metric in EdgeMetric::ALL {
        assert!(m.edges.iter().any(|e| e.weight(metric) > 0), "{metric:?} never positive");
    }
    assert!(m.edges.iter().any(|e| e.flags & SRC_OPEN != 0), "no edge from an open node's own code");
    assert!(m.edges.iter().any(|e| e.flags == 0), "no edge between collapsed nodes");
}

#[test]
fn cut_edges_match_the_reference_on_the_corpus() {
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/sample_python");
    let a = Arc::new(load(&params(&fs::canonicalize(corpus).expect("corpus")), &|_| {}).expect("load"));
    let m = CodeMap::build(a, MapParams { edges: EdgeParams { per_source: 0, workers: 3 }, ..MapParams::default() });
    check_edges_against_reference(&m, &m.reveal, &m.edges);
}

#[test]
fn pruning_keeps_the_heaviest_per_source_under_every_metric() {
    let (_p, m) = fixture();
    let ix = ix(&m);
    let r = &m.reveal;
    let pruned = edges::compute(ix, &m.analysis.agg, &m.analysis.rollup, r, EdgeParams { per_source: 1, workers: 2 });
    assert!(pruned.len() < m.edges.len(), "{} vs {}", pruned.len(), m.edges.len());
    // Pruned edges are a subset with identical values.
    let full: HashSet<_> = m.edges.iter().map(|e| (e.src, e.dst, e.lod.min, e.flags, e.w)).collect();
    for e in &pruned {
        assert!(full.contains(&(e.src, e.dst, e.lod.min, e.flags, e.w)), "{e:?}");
    }
    // At its first band, the heaviest edge of every source under every metric survives.
    for &b in &r.change_bands() {
        let want = reference_band(&m, r, b);
        let mut by_src: HashMap<NodeId, Vec<(NodeId, [u32; 6])>> = HashMap::new();
        for (&(s, d), &w) in &want {
            by_src.entry(s).or_default().push((d, w));
        }
        for (s, rows) in by_src {
            for metric in EdgeMetric::ALL {
                let c = metric.column();
                let top = rows.iter().map(|(_, w)| w[c]).max().unwrap_or(0);
                if top == 0 {
                    continue;
                }
                for (d, w) in &rows {
                    let first = r.endpoint(s, b).lod.min.max(r.endpoint(*d, b).lod.min);
                    if w[c] == top && first == b {
                        assert!(
                            pruned.iter().any(|e| e.src == s && e.dst == *d && e.lod.min == b),
                            "band {b}: {} lost its heaviest {metric:?} edge",
                            ix.qid_string(s)
                        );
                    }
                }
            }
        }
    }
}
