//! Tests of the secondary views: quadtree against brute force, force layout behaviour, view
//! graphs against references, matrix ordering, N-hop focus, scenes and the worker.

use super::force::{ForceGraph, ForceLayout, ForceParams, extent};
use super::quadtree::QuadTree;

/// Deterministic pseudo-random points (xorshift).
fn points(n: usize, seed: u64, spread: f32) -> Vec<[f32; 2]> {
    let mut s = seed | 1;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 40) as f32 / (1u64 << 24) as f32
    };
    (0..n).map(|_| [(next() - 0.5) * spread, (next() - 0.5) * spread]).collect()
}

fn brute(pos: &[[f32; 2]], mass: &[f32], i: usize, k: f32) -> [f32; 2] {
    let (mut fx, mut fy) = (0.0f64, 0.0f64);
    for j in 0..pos.len() {
        if j == i {
            continue;
        }
        let (dx, dy) = (f64::from(pos[i][0] - pos[j][0]), f64::from(pos[i][1] - pos[j][1]));
        let d2 = dx * dx + dy * dy;
        let f = f64::from(k * mass[i] * mass[j]) / d2;
        fx += f * dx;
        fy += f * dy;
    }
    [fx as f32, fy as f32]
}

#[test]
fn quadtree_exact_with_zero_theta() {
    let pos = points(400, 7, 100.0);
    let mass: Vec<f32> = (0..pos.len()).map(|i| 1.0 + (i % 5) as f32).collect();
    let mut t = QuadTree::default();
    t.build(&pos, &mass);
    let (m, c) = t.root().expect("root");
    let total: f32 = mass.iter().sum();
    assert!((m - total).abs() < 1e-3);
    let cx: f32 = pos.iter().zip(&mass).map(|(p, m)| p[0] * m).sum::<f32>() / total;
    let cy: f32 = pos.iter().zip(&mass).map(|(p, m)| p[1] * m).sum::<f32>() / total;
    assert!((c[0] - cx).abs() < 1e-2 && (c[1] - cy).abs() < 1e-2, "{c:?} vs {cx},{cy}");
    for i in [0usize, 13, 200, 399] {
        let a = t.repulsion(i as u32, 0.0, 1.5);
        let b = brute(&pos, &mass, i, 1.5);
        let err = ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt();
        let norm = (b[0] * b[0] + b[1] * b[1]).sqrt();
        assert!(err <= 1e-3 * norm.max(1.0), "body {i}: {a:?} vs {b:?}");
    }
}

#[test]
fn quadtree_approximation_is_close() {
    let pos = points(3000, 99, 1000.0);
    let mass = vec![1.0f32; pos.len()];
    let mut t = QuadTree::default();
    t.build(&pos, &mass);
    let mut worst = 0.0f32;
    for i in (0..pos.len()).step_by(97) {
        let a = t.repulsion(i as u32, 0.7, 1.0);
        let b = brute(&pos, &mass, i, 1.0);
        let norm = (b[0] * b[0] + b[1] * b[1]).sqrt().max(1e-3);
        let err = ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt() / norm;
        worst = worst.max(err);
    }
    assert!(worst < 0.1, "relative error {worst}");
    // Logarithmic depth: far fewer cells than a pathological chain.
    assert!(t.cells() < 8 * pos.len(), "{} cells", t.cells());
}

#[test]
fn quadtree_coincident_and_degenerate_points() {
    // All on one point: chained in the root, pushed apart deterministically and symmetrically.
    let pos = vec![[5.0f32, 5.0]; 4];
    let mass = vec![1.0f32; 4];
    let mut t = QuadTree::default();
    t.build(&pos, &mass);
    assert_eq!(t.cells(), 1);
    let sum = (0..4).map(|i| t.repulsion(i, 0.9, 1.0)).fold([0.0f32; 2], |a, f| [a[0] + f[0], a[1] + f[1]]);
    assert!(sum[0].abs() < 1e-4 && sum[1].abs() < 1e-4, "{sum:?}");
    assert!(t.repulsion(0, 0.9, 1.0) != [0.0; 2]);
    // Points closer than the finest cell: the depth stays bounded.
    let pos = vec![[0.0f32, 0.0], [1e-9, 0.0], [1000.0, 1000.0]];
    t.build(&pos, &[1.0; 3]);
    assert!(t.cells() <= 1 + 4 * super::quadtree::MAX_DEPTH as usize);
    assert!(t.repulsion(2, 0.0, 1.0)[0] > 0.0);
    // Non-finite positions are skipped, an empty tree has no root.
    t.build(&[[f32::NAN, 0.0], [1.0, 1.0]], &[1.0, 1.0]);
    assert_eq!(t.root().map(|r| r.0), Some(1.0));
    t.build(&[], &[]);
    assert!(t.root().is_none());
    assert_eq!(t.repulsion(0, 0.9, 1.0), [0.0; 2]);
}

#[test]
fn force_pair_reaches_equilibrium() {
    // Masses 2 and 2, weight 1, no gravity: k m_a m_b / d = d  =>  d = 2.
    let g = ForceGraph { n: 2, edges: vec![(0, 1, 1.0)] };
    let p = ForceParams { gravity: 0.0, max_iters: 2000, tolerance: 1e-6, min_temp: 1e-7, ..ForceParams::default() };
    let mut l = ForceLayout::new(&g, &[[0.0, 0.0], [10.0, 3.0]], p);
    l.run(&g);
    let d = ((l.pos[0][0] - l.pos[1][0]).powi(2) + (l.pos[0][1] - l.pos[1][1]).powi(2)).sqrt();
    assert!((d - 2.0).abs() < 0.05, "distance {d}");
}

/// Two 12-cliques joined by one edge.
fn two_clusters() -> ForceGraph {
    let mut edges = Vec::new();
    for c in 0..2u32 {
        for a in 0..12 {
            for b in a + 1..12 {
                edges.push((c * 12 + a, c * 12 + b, 1.0));
            }
        }
    }
    edges.push((0, 12, 1.0));
    ForceGraph { n: 24, edges }
}

#[test]
fn force_separates_clusters_and_terminates() {
    let g = two_clusters();
    // Interleaved start: both clusters mixed on a line.
    let init: Vec<[f32; 2]> = (0..24).map(|i| [((i * 7) % 24) as f32, 0.0]).collect();
    let mut l = ForceLayout::new(&g, &init, ForceParams::default());
    l.run(&g);
    assert!(l.done() && l.iter <= ForceParams::default().max_iters);
    let centre = |c: usize| {
        let s = l.pos[c * 12..c * 12 + 12].iter().fold([0.0f32; 2], |a, p| [a[0] + p[0], a[1] + p[1]]);
        [s[0] / 12.0, s[1] / 12.0]
    };
    let (a, b) = (centre(0), centre(1));
    let between = ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt();
    let spread = |c: usize, m: [f32; 2]| l.pos[c * 12..c * 12 + 12].iter().map(|p| ((p[0] - m[0]).powi(2) + (p[1] - m[1]).powi(2)).sqrt()).fold(0.0f32, f32::max);
    assert!(between > spread(0, a) && between > spread(1, b), "between {between}, spreads {} {}", spread(0, a), spread(1, b));
    assert!(l.pos.iter().all(|p| p[0].is_finite() && p[1].is_finite()));
}

#[test]
fn force_is_deterministic_in_parallel() {
    // Above the parallel threshold: a ring with chords, laid out twice.
    let n = 1500u32;
    let edges: Vec<(u32, u32, f32)> = (0..n).map(|i| (i, (i + 1) % n, 1.0)).chain((0..n).step_by(7).map(|i| (i, (i * 31 + 5) % n, 0.5))).collect();
    let g = ForceGraph { n, edges };
    let init = points(n as usize, 3, 50.0);
    let p = ForceParams { max_iters: 30, ..ForceParams::default() };
    let mut a = ForceLayout::new(&g, &init, p);
    let mut b = ForceLayout::new(&g, &init, p);
    a.run(&g);
    b.run(&g);
    assert_eq!(a.pos, b.pos);
    assert!(extent(&a.pos) > 1.0);
}

#[test]
fn force_handles_trivial_graphs() {
    let g = ForceGraph { n: 0, edges: Vec::new() };
    let mut l = ForceLayout::new(&g, &[], ForceParams::default());
    assert!(l.done());
    assert_eq!(l.step(&g), 0.0);
    let g = ForceGraph { n: 1, edges: Vec::new() };
    let l = ForceLayout::new(&g, &[[3.0, 4.0]], ForceParams::default());
    assert!(l.done());
    // Identical start positions are spread and never produce NaN.
    let g = ForceGraph { n: 5, edges: vec![(0, 1, 1.0)] };
    let mut l = ForceLayout::new(&g, &[[1.0, 1.0]; 5], ForceParams::default());
    l.run(&g);
    assert!(l.pos.iter().all(|p| p[0].is_finite() && p[1].is_finite()));
    for i in 0..5 {
        for j in i + 1..5 {
            assert_ne!(l.pos[i], l.pos[j]);
        }
    }
}

// ---------------------------------------------------------------------------------------
// Fixtures: a temp project analysed in process.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use pymapper_core::graph::Graph;
use pymapper_core::index::{Index, NodeId};

use super::dsm::{self, DsmParams};
use super::graph::{Adjacency, Direction, FocusParams, Weight, focus_graph, level_graph, radial_layout};
use crate::codemap::load::{LoadParams, load};
use crate::codemap::{CodeMap, MapParams};
use crate::map::scene::WorldRect;

struct TempProject(PathBuf);

impl TempProject {
    fn new(files: &[(&str, &str)]) -> TempProject {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-views-{}-{n}", std::process::id()));
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

    fn map(&self) -> CodeMap {
        let lp = LoadParams { path: self.0.clone(), isolated: true, deps: Some(false), stdlib: Some(false), cache_dir: None, no_cache: true };
        CodeMap::build(Arc::new(load(&lp, &|_| {}).expect("load")), MapParams::default())
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Layered packages with a module-level import cycle (`app.a` <-> `app.b`) and calls.
const PROJ: &[(&str, &str)] = &[
    ("app/__init__.py", ""),
    ("app/base.py", "def log(m):\n    return m\n\nclass Store:\n    def get(self):\n        return log('g')\n"),
    ("app/a.py", "from app import b\nfrom app.base import log\n\ndef fa():\n    log('a')\n    return b.fb()\n"),
    ("app/b.py", "from app import a\nfrom app.base import Store\n\ndef fb():\n    s = Store()\n    return s.get()\n\ndef back():\n    return a.fa()\n"),
    ("app/top.py", "from app.a import fa\nfrom app.b import back\n\ndef main():\n    fa()\n    back()\n"),
    ("lib/__init__.py", ""),
    ("lib/util.py", "from app.base import log\n\ndef helper():\n    return log('u')\n"),
];

fn ix(m: &CodeMap) -> &Index {
    &m.analysis.index
}

fn node(m: &CodeMap, qid: &str) -> NodeId {
    let ix = ix(m);
    (0..ix.node_count()).find(|&v| ix.qid_string(v) == qid).unwrap_or_else(|| panic!("no node {qid}"))
}

fn world(m: &CodeMap) -> WorldRect {
    let mut w = WorldRect::EMPTY;
    for v in 0..ix(m).node_count() {
        if let Some(r) = m.layout.rect(v) {
            w = w.union(&WorldRect::new(r.x0, r.y0, r.x1, r.y1));
        }
    }
    w
}

// ---------------------------------------------------------------------------------------
// Matrix ordering.

/// Checks that `order` is a permutation with SCC members contiguous; returns the feedback
/// edges (target placed after the source, i.e. above the diagonal).
fn check_order(g: &Graph, order: &[u32]) -> usize {
    let n = g.node_count() as usize;
    assert_eq!(order.len(), n);
    let mut pos = vec![u32::MAX; n];
    for (r, &v) in order.iter().enumerate() {
        assert_eq!(pos[v as usize], u32::MAX, "node {v} twice");
        pos[v as usize] = r as u32;
    }
    let scc = pymapper_core::graph::Scc::of(g);
    for c in 0..scc.count() {
        let rows: Vec<u32> = scc.members(c).iter().map(|&v| pos[v as usize]).collect();
        let (lo, hi) = (rows.iter().min().unwrap(), rows.iter().max().unwrap());
        assert_eq!((hi - lo + 1) as usize, rows.len(), "component {c} not contiguous");
    }
    let mut feedback = 0;
    for v in 0..n as u32 {
        for &w in g.succ(v) {
            if w != v && pos[w as usize] > pos[v as usize] {
                assert_eq!(scc.comp(v), scc.comp(w), "feedback edge {v}->{w} outside a cycle");
                feedback += 1;
            }
        }
    }
    feedback
}

#[test]
fn dsm_order_of_a_dag_is_lower_triangular() {
    // 0 -> 1 -> 3, 0 -> 2 -> 3, 4 isolated, 5 -> 3.
    let g = Graph::from_edges(6, &[(0, 1), (1, 3), (0, 2), (2, 3), (5, 3)]);
    let order = dsm::order(&g);
    assert_eq!(check_order(&g, &order), 0);
    // Sinks first, ties by the smallest id: 3; then 1, 2 (freed by 3) and the sink 4; 0 and
    // 5 after their dependencies.
    assert_eq!(order, vec![3, 1, 2, 0, 4, 5]);
}

#[test]
fn dsm_order_puts_cycles_in_blocks_with_few_feedback_edges() {
    // A 3-cycle (1 -> 2 -> 3 -> 1) between a source 0 and a sink 4: one feedback edge.
    let g = Graph::from_edges(5, &[(0, 1), (1, 2), (2, 3), (3, 1), (3, 4)]);
    let order = dsm::order(&g);
    assert_eq!(check_order(&g, &order), 1);
    assert_eq!(order[0], 4);
    assert_eq!(order[4], 0);
    // Two 2-cycles sharing a node plus a chord and a self loop: valid and contiguous.
    let g = Graph::from_edges(4, &[(0, 1), (1, 0), (1, 2), (2, 1), (0, 2), (3, 3)]);
    let order = dsm::order(&g);
    let fb = check_order(&g, &order);
    assert!(fb <= 2, "feedback {fb}");
}

#[test]
fn dsm_order_on_random_graphs_is_valid_and_near_optimal() {
    let mut s = 0x1234_5678u64;
    let mut rnd = move |m: u64| {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s % m
    };
    for round in 0..200 {
        let n = 2 + rnd(6) as u32;
        let m = rnd(u64::from(n * n));
        let edges: Vec<(u32, u32)> = (0..m).map(|_| (rnd(u64::from(n)) as u32, rnd(u64::from(n)) as u32)).collect();
        let g = Graph::from_edges(n, &edges);
        let order = dsm::order(&g);
        let fb = check_order(&g, &order);
        let best = best_feedback(&g);
        assert!(fb >= best, "round {round}: {fb} below the optimum {best}");
        // ELS is a heuristic; on graphs this small it stays within a couple of edges.
        assert!(fb <= best + 2, "round {round}: {fb} vs optimum {best} for {edges:?}");
    }
}

/// The fewest feedback edges over all orders (brute force).
fn best_feedback(g: &Graph) -> usize {
    let n = g.node_count() as usize;
    let mut perm: Vec<u32> = (0..n as u32).collect();
    let mut best = usize::MAX;
    loop {
        let mut pos = vec![0usize; n];
        for (r, &v) in perm.iter().enumerate() {
            pos[v as usize] = r;
        }
        let mut fb = 0;
        for v in 0..n as u32 {
            for &w in g.succ(v) {
                if w != v && pos[w as usize] > pos[v as usize] {
                    fb += 1;
                }
            }
        }
        best = best.min(fb);
        let Some(i) = (0..n.saturating_sub(1)).rev().find(|&i| perm[i] < perm[i + 1]) else { break };
        let j = (i + 1..n).rev().find(|&j| perm[j] > perm[i]).expect("successor");
        perm.swap(i, j);
        perm[i + 1..].reverse();
    }
    best
}

#[test]
fn dsm_of_a_project_exposes_the_cycle() {
    let p = TempProject::new(PROJ);
    let m = p.map();
    let d = dsm::build(&m, DsmParams { weight: Weight::Imports, ..DsmParams::default() });
    let ix = ix(&m);
    let names: Vec<String> = d.nodes.iter().map(|&v| ix.qid_string(v)).collect();
    for q in ["app", "app.base", "app.a", "app.b", "app.top", "lib", "lib.util"] {
        assert!(names.iter().any(|n| n == q), "{q} missing from {names:?}");
    }
    let row = |q: &str| d.row_of(node(&m, q)).expect("row");
    // app.base is depended upon: it comes before everything that imports it.
    for q in ["app.a", "app.b", "lib.util", "app.top"] {
        assert!(row("app.base") < row(q), "{names:?}");
    }
    // The a <-> b cycle: one block of two adjacent rows with one feedback cell.
    let (ra, rb) = (row("app.a"), row("app.b"));
    assert_eq!(ra.abs_diff(rb), 1, "{names:?}");
    assert_eq!(d.blocks, vec![(ra.min(rb), ra.max(rb) + 1)]);
    assert_eq!(d.feedback, 1);
    let fb: Vec<_> = d.cells.iter().filter(|c| c.is_feedback()).collect();
    assert_eq!(fb.len(), 1);
    assert!(fb[0].row == ra.min(rb) && fb[0].col == ra.max(rb));
    // Cells: sorted, no diagonal, every import edge present.
    assert!(d.cells.windows(2).all(|w| (w[0].row, w[0].col) < (w[1].row, w[1].col)));
    assert!(d.cells.iter().all(|c| c.row != c.col && c.imports > 0));
    let has = |a: &str, b: &str| d.cells.iter().any(|c| c.row == row(a) && c.col == row(b));
    assert!(has("app.top", "app.a") && has("app.top", "app.b") && has("lib.util", "app.base") && has("app.a", "app.b") && has("app.b", "app.a"));
    assert!(!has("app.base", "app.a"));
    for (v, r) in d.row_index() {
        assert_eq!(d.row_of(v), Some(r));
    }
    // Scope: only lib's subtree.
    let lib = node(&m, "lib");
    let s = dsm::build(&m, DsmParams { scope: Some(lib), ..DsmParams::default() });
    assert!(!s.nodes.is_empty() && s.nodes.iter().all(|&v| ix.is_descendant(lib, v)));
    assert!(s.cells.is_empty() && s.blocks.is_empty());
    // Calls only: fa calls b.fb and b.back calls a.fa, so the cycle stays.
    let c = dsm::build(&m, DsmParams { weight: Weight::Sites, ..DsmParams::default() });
    assert!(c.cells.iter().all(|x| x.sites > 0));
    assert_eq!(c.blocks.len(), 1);
}

// ---------------------------------------------------------------------------------------
// Focus graphs.

/// Brute-force BFS distances from `f` (unlimited budget) over `rows`.
fn bfs(n: u32, rows: &[(u32, u32, u32)], f: u32, dir: Direction, hops: u8) -> Vec<Option<u8>> {
    let mut dist = vec![None; n as usize];
    dist[f as usize] = Some(0);
    for h in 1..=hops {
        let prev: Vec<u32> = (0..n).filter(|&v| dist[v as usize] == Some(h - 1)).collect();
        for &(a, b, _) in rows {
            if a == b {
                continue;
            }
            if dir != Direction::In && prev.contains(&a) && dist[b as usize].is_none() {
                dist[b as usize] = Some(h);
            }
            if dir != Direction::Out && prev.contains(&b) && dist[a as usize].is_none() {
                dist[a as usize] = Some(h);
            }
        }
    }
    dist
}

#[test]
fn focus_graph_matches_bfs_without_a_budget() {
    let mut s = 99u64;
    let mut rnd = move |m: u64| {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s % m
    };
    for round in 0..100 {
        let n = 3 + rnd(30) as u32;
        let mut set = BTreeSet::new();
        for _ in 0..rnd(u64::from(3 * n)) {
            set.insert((rnd(u64::from(n)) as u32, rnd(u64::from(n)) as u32));
        }
        let rows: Vec<(u32, u32, u32)> = set.iter().map(|&(a, b)| (a, b, 1 + rnd(9) as u32)).collect();
        let adj = Adjacency::build(n, || rows.iter().copied());
        let f = rnd(u64::from(n)) as u32;
        for dir in Direction::ALL {
            let hops = 1 + rnd(3) as u8;
            let g = focus_graph(&adj, f, FocusParams { hops, direction: dir, max_nodes: usize::MAX });
            let want = bfs(n, &rows, f, dir, hops);
            let got: HashSet<u32> = g.nodes.iter().copied().collect();
            let expect: HashSet<u32> = (0..n).filter(|&v| want[v as usize].is_some()).collect();
            assert_eq!(got, expect, "round {round} {dir:?}");
            assert_eq!(g.dropped, 0);
            for (i, &v) in g.nodes.iter().enumerate() {
                assert_eq!(Some(g.hops[i]), want[v as usize], "hop of {v}");
                let par = g.parent[i] as usize;
                if v == f {
                    assert_eq!(par, i);
                } else {
                    assert_eq!(g.hops[par] + 1, g.hops[i], "parent is one ring in");
                }
            }
            // Edges: exactly the rows between kept nodes.
            let mut edges: Vec<(u32, u32, u32)> = g.edges.iter().map(|e| (g.nodes[e.src as usize], g.nodes[e.dst as usize], e.sites)).collect();
            edges.sort_unstable();
            let mut ref_edges: Vec<(u32, u32, u32)> = rows.iter().copied().filter(|&(a, b, _)| a != b && got.contains(&a) && got.contains(&b)).collect();
            ref_edges.sort_unstable();
            assert_eq!(edges, ref_edges);
            for &(a, b, w) in &rows {
                if a != b {
                    assert_eq!(adj.weight(a, b), w);
                }
            }
        }
    }
}

#[test]
fn focus_graph_budget_keeps_the_heaviest() {
    // Focus 0 calls 1..=5 with weights 1..=5 and 5 calls 6.
    let rows = vec![(0, 1, 1), (0, 2, 2), (0, 3, 3), (0, 4, 4), (0, 5, 5), (5, 6, 7)];
    let adj = Adjacency::build(7, || rows.iter().copied());
    let g = focus_graph(&adj, 0, FocusParams { hops: 2, direction: Direction::Out, max_nodes: 3 });
    assert_eq!(g.nodes, vec![0, 4, 5]);
    assert_eq!(g.dropped, 3, "1, 2, 3 at hop 1; the full budget stops the walk");
    let g = focus_graph(&adj, 0, FocusParams { hops: 2, direction: Direction::Out, max_nodes: 4 });
    assert_eq!(g.nodes, vec![0, 3, 4, 5]);
    // Direction: nothing calls 0.
    let g = focus_graph(&adj, 0, FocusParams { hops: 3, direction: Direction::In, max_nodes: 10 });
    assert_eq!(g.nodes, vec![0]);
    let g = focus_graph(&adj, 6, FocusParams { hops: 2, direction: Direction::In, max_nodes: 10 });
    assert_eq!(g.nodes, vec![0, 5, 6]);
    // Radial layout: focus at the origin, rings in order, finite.
    let g = focus_graph(&adj, 0, FocusParams { hops: 2, direction: Direction::Both, max_nodes: 10 });
    let pos = radial_layout(&g);
    let r = |i: usize| (pos[i][0].powi(2) + pos[i][1].powi(2)).sqrt();
    let f = g.local(0).unwrap() as usize;
    assert_eq!(pos[f], [0.0, 0.0]);
    let six = g.local(6).unwrap() as usize;
    for i in 0..g.nodes.len() {
        if g.hops[i] == 1 {
            assert!(r(i) > 0.5 && r(i) < r(six), "{pos:?}");
        }
    }
    assert!(pos.iter().all(|p| p[0].is_finite() && p[1].is_finite()));
}

#[test]
fn focus_on_a_project_walks_calls_or_modules() {
    let p = TempProject::new(PROJ);
    let m = p.map();
    let ix = ix(&m);
    let rollup = &m.analysis.rollup;
    let calls = Adjacency::calls(ix.node_count(), rollup);
    let fa = node(&m, "app.a:fa");
    assert!(super::graph::focus_uses_calls(ix, fa));
    let g = focus_graph(&calls, fa, FocusParams { hops: 1, ..FocusParams::default() });
    let names: HashSet<String> = g.nodes.iter().map(|&v| ix.qid_string(v)).collect();
    for q in ["app.a:fa", "app.base:log", "app.b:fb", "app.b:back", "app.top:main"] {
        assert!(names.contains(q), "{q} missing from {names:?}");
    }
    let mods = Adjacency::modules(ix, rollup);
    let base = node(&m, "app.base");
    assert!(!super::graph::focus_uses_calls(ix, base));
    let g = focus_graph(&mods, base, FocusParams { hops: 1, direction: Direction::In, max_nodes: 100 });
    let names: HashSet<String> = g.nodes.iter().map(|&v| ix.qid_string(v)).collect();
    for q in ["app.base", "app.a", "app.b", "lib.util"] {
        assert!(names.contains(q), "{q} missing from {names:?}");
    }
    assert!(!names.contains("app.top"), "top does not use base directly");
    // Representatives: a method maps to its module in a module-level graph.
    let get = node(&m, "app.base:Store.get");
    assert_eq!(g.representative(ix, get), g.local(base));
}

// ---------------------------------------------------------------------------------------
// The current level of the code map.

#[test]
fn level_graph_is_the_visible_cut() {
    let p = TempProject::new(PROJ);
    let m = p.map();
    let ix = ix(&m);
    let all = world(&m);
    let mut cut = Vec::new();
    for &b in &m.reveal.change_bands() {
        let g = level_graph(&m, b, &all, usize::MAX);
        m.reveal.fill_cut(ix, b, &mut cut);
        assert!(g.nodes.windows(2).all(|w| w[0] < w[1]));
        // Every collapsed shown cell is a node; open ones only with edges.
        for v in 0..ix.node_count() {
            let shown = m.reveal.rev[v as usize] <= b && m.layout.rect(v).is_some_and(|r| r.w() > 0.0 && r.h() > 0.0);
            let collapsed = m.reveal.open[v as usize] > b;
            if shown && collapsed {
                assert!(g.local(v).is_some(), "band {b}: {} missing", ix.qid_string(v));
            }
            if !shown {
                assert!(g.local(v).is_none());
            }
        }
        // Edges: the rolled-up dependencies between nodes, by brute force over call pairs.
        let mut want: BTreeSet<(NodeId, NodeId)> = BTreeSet::new();
        for (u, w, _) in m.analysis.rollup.call_pairs() {
            let (a, c) = (cut[u as usize], cut[w as usize]);
            if a != c && g.local(a).is_some() && g.local(c).is_some() {
                want.insert((a, c));
            }
        }
        let got: BTreeSet<(NodeId, NodeId)> = g.edges.iter().filter(|e| e.sites > 0).map(|e| (g.nodes[e.src as usize], g.nodes[e.dst as usize])).collect();
        assert_eq!(got, want, "band {b}");
        assert!(g.edges.iter().all(|e| e.src != e.dst && e.weight(Weight::All) > 0));
    }
    // The deepest band: functions are nodes, with the call fa -> fb.
    let deep = *m.reveal.change_bands().last().unwrap();
    let g = level_graph(&m, deep, &all, usize::MAX);
    let (fa, fb) = (node(&m, "app.a:fa"), node(&m, "app.b:fb"));
    let (la, lb) = (g.local(fa).expect("fa"), g.local(fb).expect("fb"));
    assert!(g.edges.iter().any(|e| e.src == la && e.dst == lb));
    // View restriction: only cells intersecting the view.
    let r = m.layout.rect(node(&m, "lib.util")).unwrap();
    let view = WorldRect::new(r.x0 + 0.25 * r.w(), r.y0 + 0.25 * r.h(), r.x1 - 0.25 * r.w(), r.y1 - 0.25 * r.h());
    let g = level_graph(&m, deep, &view, usize::MAX);
    assert!(!g.nodes.is_empty());
    for &v in &g.nodes {
        let c = m.layout.rect(v).unwrap();
        assert!(WorldRect::new(c.x0, c.y0, c.x1, c.y1).intersects(&view), "{}", ix.qid_string(v));
    }
    // Budget: the largest cells win.
    let full = level_graph(&m, deep, &all, usize::MAX);
    let leaves = full.nodes.iter().filter(|&&v| m.reveal.open[v as usize] > deep).count();
    let g = level_graph(&m, deep, &all, 2);
    assert_eq!(g.dropped, leaves - 2);
    let loc = m.analysis.agg.loc();
    let kept_min = g.nodes.iter().filter(|&&v| m.reveal.open[v as usize] > deep).map(|&v| loc[v as usize]).min().unwrap();
    let dropped_max = full.nodes.iter().filter(|&&v| m.reveal.open[v as usize] > deep && g.local(v).is_none()).map(|&v| loc[v as usize]).max().unwrap();
    assert!(kept_min >= dropped_max);
}
