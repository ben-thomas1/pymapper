//! Rollup tests: hand-checked fixtures plus brute-force references on the test corpus.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::graph::CallFilter;
use crate::index::{Index, Inputs, NONE, load};
use crate::resolve::{Attribution, Imports, Reason, ResolvedCalls, resolve_calls, resolve_imports};
use crate::roots::Origin;
use crate::walk::{WalkOptions, walk};

struct Fx {
    ix: Index,
    imp: Imports,
    calls: ResolvedCalls,
}

fn load_dir(dir: &Path, origin: Origin) -> Fx {
    let w = walk(&[dir], &WalkOptions::everything());
    let ix = load(Inputs::from_root_dir(dir, &w.files, origin, b"proj"));
    let imp = resolve_imports(&ix, &Attribution::new());
    let calls = resolve_calls(&ix, &imp);
    Fx { ix, imp, calls }
}

/// Loads `files` (relative path, source) as one first-party root and resolves everything.
fn fx(files: &[(&str, &str)]) -> Fx {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pymapper-rollup-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    for (rel, src) in files {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, src).expect("write");
    }
    let out = load_dir(&dir, Origin::FirstParty);
    let _ = fs::remove_dir_all(&dir);
    out
}

fn corpus(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus").join(sub)
}

impl Fx {
    fn node(&self, qid: &str) -> NodeId {
        (0..self.ix.node_count()).find(|&v| self.ix.qid_string(v) == qid).unwrap_or_else(|| panic!("no node {qid}"))
    }

    fn rollup(&self) -> Rollup {
        Rollup::build(&self.ix, &self.imp, &self.calls, CallFilter::default(), 0)
    }

    /// Edges of a level as sorted `src -> dst` qid strings with their values.
    fn render(&self, le: &LevelEdges) -> Vec<String> {
        let mut out: Vec<String> = le
            .edges()
            .iter()
            .map(|e| {
                let conf = match (e.best(), e.worst()) {
                    (Some(b), Some(w)) => format!(" {b:?}/{w:?}"),
                    _ => String::new(),
                };
                let imp = if e.has_import() { format!(" imports={} flags={:#x}", e.imports, e.import_flags) } else { String::new() };
                format!(
                    "{} -> {} sites={} pairs={}{conf}{imp}",
                    self.ix.qid_string(e.src),
                    self.ix.qid_string(e.dst),
                    e.sites,
                    e.pairs
                )
            })
            .collect();
        out.sort();
        out
    }
}

/// Brute-force level edge: sets instead of counters.
#[derive(Default)]
struct RefEdge {
    sites: HashSet<(NodeId, u32)>,
    pairs: HashSet<(NodeId, NodeId)>,
    best: u8,
    worst: u8,
    imports: u32,
    flags: Option<u8>,
}

/// The edges of `level` computed naively from the raw call and import edges (hash maps,
/// [`Level::apply`] per endpoint), in the shape of [`Rollup::level`].
fn reference(f: &Fx, level: Level, filter: CallFilter, import_exclude: u8) -> Vec<RollupEdge> {
    let ix = &f.ix;
    let mut m: HashMap<(NodeId, NodeId), RefEdge> = HashMap::new();
    for e in f.calls.edges() {
        if !filter.keeps(e.conf, e.reason) {
            continue;
        }
        let (a, b) = (level.apply(ix, e.src), level.apply(ix, e.dst));
        if a == b {
            continue;
        }
        let r = m.entry((a, b)).or_insert_with(|| RefEdge { best: 3, ..RefEdge::default() });
        if e.site != NONE {
            r.sites.insert((e.src, e.site));
        }
        r.pairs.insert((e.src, e.dst));
        r.best = r.best.min(e.conf as u8);
        r.worst = r.worst.max(e.conf as u8);
    }
    for (file, e) in f.imp.all_module_edges() {
        if e.flags & import_exclude != 0 {
            continue;
        }
        let (a, b) = (level.apply(ix, ix.file_node(file)), level.apply(ix, e.dst));
        if a == b {
            continue;
        }
        let r = m.entry((a, b)).or_insert_with(|| RefEdge { best: 3, ..RefEdge::default() });
        r.imports += e.rows;
        r.flags = Some(match r.flags {
            None => merge_import_flags(e.flags, e.flags),
            Some(acc) => merge_import_flags(acc, e.flags),
        });
    }
    let mut out: Vec<RollupEdge> = m
        .into_iter()
        .map(|((src, dst), r)| {
            let mut e = RollupEdge::new_for_tests(src, dst);
            e.sites = r.sites.len() as u32;
            e.pairs = r.pairs.len() as u32;
            e.imports = r.imports;
            e.import_flags = r.flags.unwrap_or(0);
            e.set_conf_for_tests(r.best, r.worst);
            e
        })
        .collect();
    out.sort_unstable_by_key(|e| (e.src, e.dst));
    out
}

/// Every level of `ix`: each depth up to one past the deepest node, and the module level.
fn levels(ix: &Index) -> Vec<Level> {
    let mut out: Vec<Level> = (0..=max_depth(ix) + 1).map(Level::Depth).collect();
    out.push(Level::Module);
    out
}

/// Brute-force aggregates of node `v` (subtree walks, parent-chain checks).
fn reference_agg(ix: &Index, v: NodeId) -> NodeAgg {
    let mx = ix.metrics();
    let own_loc = |w: NodeId| match ix.metrics_row(w) {
        Some(m) if ix.kind(w).is_def() => mx.loc[m],
        _ => ix.file(w).map_or(0, |f| ix.file_facts(f).lines),
    };
    let mut a = NodeAgg { loc: own_loc(v), ..NodeAgg::default() };
    for w in ix.subtree(v) {
        let k = ix.kind(w);
        if w != v && is_file_level(k) {
            a.loc += own_loc(w);
        }
        if k.is_def() {
            a.symbols += 1;
        }
        if k != NodeKind::Function {
            continue;
        }
        let m = ix.metrics_row(w).expect("function metrics");
        a.cc_max = a.cc_max.max(mx.cyclomatic[m]);
        a.cog_max = a.cog_max.max(mx.cognitive[m]);
        // Outermost within the subtree: no function strictly between v and w.
        let mut p = ix.parent(w);
        let mut nested = false;
        while let Some(q) = p {
            if q < v {
                break;
            }
            if ix.kind(q) == NodeKind::Function && q != w {
                nested = true;
                break;
            }
            if q == v {
                break;
            }
            p = ix.parent(q);
        }
        if !nested {
            a.cc_sum += u32::from(mx.cyclomatic[m]);
            a.cog_sum += u32::from(mx.cognitive[m]);
        }
    }
    a
}

const PROJ: &[(&str, &str)] = &[
    ("pkg/__init__.py", "from .a import fa\n"),
    (
        "pkg/a.py",
        "from . import b\nfrom .models import Model\n\ndef fa():\n    m = Model()\n    return b.fb() + b.fb()\n\n\
         def helper():\n    def inner():\n        return fa()\n    return inner()\n",
    ),
    ("pkg/b.py", "from .a import fa\n\ndef fb():\n    return fa()\n"),
    (
        "pkg/models.py",
        "class Base:\n    def __init__(self):\n        pass\n\n    def run(self):\n        return 1\n\n\
         class Model(Base):\n    def __init__(self):\n        super().__init__()\n\n    def run(self):\n        return 2\n",
    ),
    (
        "pkg/typed.py",
        "from typing import TYPE_CHECKING\nif TYPE_CHECKING:\n    from .models import Base\n\ndef ft(x):\n    return x\n",
    ),
];

#[test]
fn module_level_counts_sites_pairs_and_imports() {
    let f = fx(PROJ);
    let r = f.rollup();
    let le = r.level(&f.ix, Level::Module);
    let (a, b, models) = (f.node("pkg.a"), f.node("pkg.b"), f.node("pkg.models"));
    // `Model()` is one site with two targets (the class and its `__init__`).
    let e = le.get(a, models).expect("a -> models");
    assert_eq!((e.sites, e.pairs, e.imports), (1, 2, 1));
    assert_eq!(e.best(), Some(Confidence::Certain));
    // Two `b.fb()` sites, one pair, one import row.
    let e = le.get(a, b).expect("a -> b");
    assert_eq!((e.sites, e.pairs, e.imports), (2, 1, 1));
    assert!(e.has_import() && e.has_calls());
    let e = le.get(b, a).expect("b -> a");
    assert_eq!((e.sites, e.pairs, e.imports), (1, 1, 1));
    // Overrides and `super().__init__()` stay inside `models`: no self edges.
    assert!(le.edges().iter().all(|e| e.src != e.dst));
    assert!(le.from(models).is_empty());
    // The package's `from .a import fa` is an import-only edge.
    let e = le.get(f.node("pkg"), a).expect("pkg -> a");
    assert!(!e.has_calls() && e.best().is_none() && e.worst().is_none());
    assert_eq!((e.sites, e.pairs, e.imports), (0, 0, 1));
    assert_eq!(le.level(), Level::Module);
}

#[test]
fn deeper_levels_split_multi_target_sites() {
    let f = fx(PROJ);
    let r = f.rollup();
    assert_eq!(r.stats().multi_sites, 1);
    let (fa, model, init) = (f.node("pkg.a:fa"), f.node("pkg.models:Model"), f.node("pkg.models:Model.__init__"));
    // Depth 3 (classes and functions): both targets are inside `Model`.
    let le = r.level(&f.ix, Level::Depth(3));
    let e = le.get(fa, model).expect("fa -> Model");
    assert_eq!((e.sites, e.pairs), (1, 2));
    // Depth 4 (methods): one edge per target, the site counted in each.
    let le = r.level(&f.ix, Level::Depth(4));
    assert_eq!(le.get(fa, model).map(|e| (e.sites, e.pairs)), Some((1, 1)));
    assert_eq!(le.get(fa, init).map(|e| (e.sites, e.pairs)), Some((1, 1)));
    // Override edges: no site, one pair, `Likely`.
    let e = le.get(f.node("pkg.models:Base.run"), f.node("pkg.models:Model.run")).expect("override");
    assert_eq!((e.sites, e.pairs, e.best(), e.worst()), (0, 1, Some(Confidence::Likely), Some(Confidence::Likely)));
    // The nested function calls `fa` from inside `helper` until its own depth.
    let (helper, inner) = (f.node("pkg.a:helper"), f.node("pkg.a:helper.inner"));
    assert!(le.get(inner, fa).is_some());
    assert!(le.get(helper, inner).is_some());
    let le3 = r.level(&f.ix, Level::Depth(3));
    assert_eq!(le3.get(helper, fa).map(|e| e.sites), Some(1));
}

#[test]
fn coarse_levels_merge_everything() {
    let f = fx(PROJ);
    let r = f.rollup();
    // One distribution, one top-level package: nothing crosses depth 0 or 1.
    assert!(r.level(&f.ix, Level::Depth(0)).is_empty());
    assert!(r.level(&f.ix, Level::Depth(1)).is_empty());
    // Depth 2 is the module level here.
    assert_eq!(r.level(&f.ix, Level::Depth(2)).edges(), r.level(&f.ix, Level::Module).edges());
}

#[test]
fn import_flags_are_shared_by_all_rows() {
    let f = fx(PROJ);
    let r = f.rollup();
    let le = r.level(&f.ix, Level::Module);
    let e = le.get(f.node("pkg.typed"), f.node("pkg.models")).expect("typed -> models");
    assert_eq!((e.imports, e.pairs), (1, 0));
    assert_ne!(e.import_flags & res_flags::TYPE_ONLY, 0);
    // Excluding type-only imports drops the edge.
    let strict = Rollup::build(&f.ix, &f.imp, &f.calls, CallFilter::default(), res_flags::TYPE_ONLY);
    assert!(strict.level(&f.ix, Level::Module).get(f.node("pkg.typed"), f.node("pkg.models")).is_none());
    assert_eq!(strict.stats().import_edges + 1, r.stats().import_edges);
}

#[test]
fn call_filter_applies() {
    let f = fx(PROJ);
    let filter = CallFilter::at_least(Confidence::Certain).without(Reason::Override);
    let r = Rollup::build(&f.ix, &f.imp, &f.calls, filter, 0);
    let le = r.level(&f.ix, Level::Depth(4));
    assert!(le.get(f.node("pkg.models:Base.run"), f.node("pkg.models:Model.run")).is_none());
    assert!(le.edges().iter().all(|e| e.worst().is_none_or(|w| w == Confidence::Certain)));
}

#[test]
fn rendered_module_level() {
    let f = fx(PROJ);
    let r = f.rollup();
    let got = f.render(&r.level(&f.ix, Level::Module));
    let want = [
        "pkg -> pkg.a sites=0 pairs=0 imports=1 flags=0x0",
        "pkg.a -> pkg.b sites=2 pairs=1 Certain/Certain imports=1 flags=0x0",
        "pkg.a -> pkg.models sites=1 pairs=2 Certain/Certain imports=1 flags=0x0",
        "pkg.b -> pkg.a sites=1 pairs=1 Certain/Certain imports=1 flags=0x0",
        // TYPE_ONLY | CONDITIONAL (inside `if TYPE_CHECKING:`).
        "pkg.typed -> pkg.models sites=0 pairs=0 imports=1 flags=0x3",
    ];
    assert_eq!(got, want);
}

/// Every level, three filters, against the brute-force reference; one reused scratch.
fn check_against_reference(f: &Fx) {
    let filters = [
        (CallFilter::default(), 0),
        (CallFilter::at_least(Confidence::Likely).without(Reason::Override), res_flags::TYPE_ONLY),
        (CallFilter::at_least(Confidence::Certain), res_flags::TYPE_ONLY | res_flags::LAZY),
    ];
    let mut scratch = LevelScratch::default();
    let mut out = Vec::new();
    for (filter, exclude) in filters {
        let r = Rollup::build(&f.ix, &f.imp, &f.calls, filter, exclude);
        // Deep to coarse and back: stale slots from earlier levels must not leak.
        let mut ls = levels(&f.ix);
        let back: Vec<Level> = ls.iter().rev().copied().collect();
        ls.extend(back);
        for level in ls {
            r.level_with(&f.ix, level, &mut scratch, &mut out);
            let want = reference(f, level, filter, exclude);
            assert_eq!(out.len(), want.len(), "{level:?} {filter:?}");
            for (got, want) in out.iter().zip(&want) {
                assert_eq!(got, want, "{level:?} {filter:?}: {} -> {}", f.ix.qid_string(want.src), f.ix.qid_string(want.dst));
            }
            assert_eq!(r.level(&f.ix, level).edges(), &out[..]);
        }
    }
}

#[test]
fn fixture_matches_reference() {
    check_against_reference(&fx(PROJ));
}

#[test]
fn corpus_matches_reference() {
    for sub in ["", "sample_python"] {
        let f = load_dir(&corpus(sub), Origin::FirstParty);
        assert!(f.calls.edges().len() > 20, "{sub}: corpus too small ({} edges)", f.calls.edges().len());
        eprintln!("{sub}: {} edges, {} module edges", f.calls.edges().len(), f.imp.stats().module_edges);
        check_against_reference(&f);
    }
}

/// A generated project: nested packages, cross-module calls and imports (lazy, type-only,
/// star), constructors with inherited `__init__`, `self.m()` resolved to several subclass
/// methods, decorators, nested functions and classes in functions.
fn generated() -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mods: Vec<String> = (0..3)
        .flat_map(|p| (0..2).flat_map(move |s| (0..3).map(move |m| format!("p{p}.s{s}.m{m}"))))
        .collect();
    for p in 0..3 {
        files.push((format!("p{p}/__init__.py"), "from .s0.m0 import f0\n\ndef pinit():\n    return f0()\n".to_owned()));
        for s in 0..2 {
            files.push((format!("p{p}/s{s}/__init__.py"), String::new()));
        }
    }
    for (i, name) in mods.iter().enumerate() {
        let other = &mods[(i * 7 + 3) % mods.len()];
        let third = &mods[(i * 5 + 1) % mods.len()];
        let mut src = String::new();
        src += &format!("import {other}\nfrom {third} import Base, f0\nfrom typing import TYPE_CHECKING\n");
        src += &format!("if TYPE_CHECKING:\n    from {other} import Kid\nfrom {other} import *\n\n");
        src += "def deco(fn):\n    return fn\n\n";
        src += &format!(
            "@deco\ndef f0():\n    def inner():\n        class Local:\n            def go(self):\n                return f1()\n        return Local().go()\n    return {other}.f1() + inner()\n\n"
        );
        src += &format!("def f1():\n    import {third}\n    k = Kid()\n    return {third}.f0() + k.hook() + Base().run()\n\n");
        src += "class Base:\n    def __init__(self):\n        self.x = 1\n\n    def run(self):\n        return self.hook()\n\n";
        src += &format!("class Kid(Base):\n    def hook(self):\n        return f0()\n\nclass Kid2(Base):\n    def hook(self):\n        return {other}.f0()\n\n");
        src += "def f2():\n    return Kid() and Kid2() and f1() and f1()\n";
        let path = format!("{}.py", name.replace('.', "/"));
        files.push((path, src));
    }
    files
}

#[test]
fn generated_project_matches_reference() {
    let owned = generated();
    let files: Vec<(&str, &str)> = owned.iter().map(|(p, s)| (p.as_str(), s.as_str())).collect();
    let f = fx(&files);
    let r = f.rollup();
    let st = r.stats();
    assert!(st.multi_sites > 20 && st.call_pairs > 200 && st.import_edges > 25, "{st:?}");
    assert!(max_depth(&f.ix) >= 6);
    check_against_reference(&f);
    // Aggregates against subtree walks.
    let a = Aggregates::build(&f.ix);
    for v in 0..f.ix.node_count() {
        assert_eq!(a.get(v), reference_agg(&f.ix, v), "{}", f.ix.qid_string(v));
    }
}

/// The brute-force comparisons on the stdlib (PYMAPPER_STDLIB or the pinned interpreter):
/// `cargo test --release -p pymapper-core stdlib_rollup -- --ignored --nocapture`.
#[test]
#[ignore = "needs a CPython stdlib; slow in debug"]
fn stdlib_rollup_matches_reference() {
    let root = std::env::var_os("PYMAPPER_STDLIB").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from("/Users/bt386671/.local/share/uv/python/cpython-3.13.5-macos-aarch64-none/lib/python3.13")
    });
    if !root.is_dir() {
        eprintln!("skipping: {} not found", root.display());
        return;
    }
    let f = load_dir(&root, Origin::Stdlib);
    let r = f.rollup();
    eprintln!("{:?}", r.stats());
    check_against_reference(&f);
    let a = Aggregates::build(&f.ix);
    for v in 0..f.ix.node_count() {
        assert_eq!(a.get(v), reference_agg(&f.ix, v), "{}", f.ix.qid_string(v));
    }
}

#[test]
fn level_maps_are_monotone_and_match_apply() {
    let f = load_dir(&corpus("sample_python"), Origin::FirstParty);
    let ix = &f.ix;
    let mut map = Vec::new();
    for level in levels(ix) {
        level.fill_map(ix, &mut map);
        assert_eq!(map.len(), ix.node_count() as usize);
        for v in 0..ix.node_count() {
            let r = map[v as usize];
            assert_eq!(r, level.apply(ix, v), "{level:?} {}", ix.qid_string(v));
            assert!(ix.is_descendant(r, v), "representative is an ancestor-or-self");
            assert_eq!(map[r as usize], r, "representatives map to themselves");
            if v > 0 {
                assert!(map[v as usize - 1] <= r, "monotone in pre-order");
            }
        }
    }
    // Past the deepest node, depth levels are the identity.
    Level::Depth(max_depth(ix)).fill_map(ix, &mut map);
    assert!(map.iter().enumerate().all(|(v, &r)| r == v as NodeId));
}

#[test]
fn empty_index() {
    let f = fx(&[]);
    let r = f.rollup();
    assert_eq!(*r.stats(), RollupStats::default());
    for level in levels(&f.ix) {
        assert!(r.level(&f.ix, level).is_empty());
    }
    let a = Aggregates::build(&f.ix);
    assert_eq!(a.len(), f.ix.node_count() as usize);
}

#[test]
#[should_panic(expected = "rollup used with another index")]
fn level_rejects_another_index() {
    let f = fx(PROJ);
    let g = fx(&[("m.py", "def f():\n    pass\n")]);
    let r = f.rollup();
    let _ = r.level(&g.ix, Level::Module);
}

#[test]
fn edge_lookups() {
    let f = fx(PROJ);
    let r = f.rollup();
    let le = r.level(&f.ix, Level::Module);
    let a = f.node("pkg.a");
    let from: Vec<NodeId> = le.from(a).iter().map(|e| e.dst).collect();
    assert_eq!(from, [f.node("pkg.b"), f.node("pkg.models")]);
    assert!(le.from(f.node("pkg.models")).is_empty());
    assert!(le.from(u32::MAX - 1).is_empty());
    assert!(le.get(f.node("pkg.models"), a).is_none());
    assert!(le.heap_bytes() >= le.len() * size_of::<RollupEdge>());
    assert!(r.heap_bytes() > 0);
    assert_eq!(le.clone().into_vec().len(), le.len());
}

const AGG: &[(&str, &str)] = &[
    ("agg/__init__.py", "X = 1\n"),
    (
        "agg/m.py",
        "def outer(a):\n    if a:\n        return 1\n    def inner(b):\n        for i in b:\n            pass\n        return 0\n    return inner(a)\n\n\
         class C:\n    def meth(self, x):\n        if x:\n            return 1\n        elif x is None:\n            return 2\n        return 3\n",
    ),
    ("agg/sub/n.py", "def g():\n    return 1\n"),
];

#[test]
fn aggregates_hand_checked() {
    let f = fx(AGG);
    let ix = &f.ix;
    let a = Aggregates::build(ix);
    assert_eq!(a.len(), ix.node_count() as usize);
    let (outer, inner, c, meth) = (f.node("agg.m:outer"), f.node("agg.m:outer.inner"), f.node("agg.m:C"), f.node("agg.m:C.meth"));
    let (m, pkg, sub, n) = (f.node("agg.m"), f.node("agg"), f.node("agg.sub"), f.node("agg.sub.n"));
    let cog = |v: NodeId| u32::from(ix.metrics().cognitive[ix.metrics_row(v).expect("row")]);
    // C901: inner 1 + for; outer 1 + if + inner's full score; meth 1 + if + elif.
    assert_eq!((a.get(inner).cc_sum, a.get(outer).cc_sum, a.get(meth).cc_sum), (2, 4, 3));
    // The nested function is inside outer's score: not added again.
    assert_eq!((a.get(m).cc_sum, a.get(m).cc_max), (7, 4));
    assert_eq!(a.get(m).cog_sum, cog(outer) + cog(meth));
    assert_eq!(a.get(m).cog_max, cog(outer).max(cog(meth)) as u16);
    // A class adds its methods, not its own (WMC-like) row.
    assert_eq!((a.get(c).cc_sum, a.get(c).cc_max, a.get(c).symbols), (3, 3, 2));
    // Lines: defs their span, modules their file, packages the sum of their modules.
    assert_eq!(a.get(outer).loc, 8);
    assert_eq!(a.get(meth).loc, 6);
    assert_eq!(a.get(m).loc, 16);
    assert_eq!(a.get(n).loc, 2);
    assert_eq!(a.get(sub).loc, 2, "namespace package: sum of its modules");
    assert_eq!(a.get(pkg).loc, 1 + 16 + 2);
    let dist = ix.group_node(0);
    assert_eq!(a.get(dist).loc, 19);
    assert_eq!((a.get(m).symbols, a.get(pkg).symbols, a.get(dist).symbols), (4, 5, 5));
    assert_eq!((a.get(dist).cc_sum, a.get(dist).cc_max), (8, 4));
    assert_eq!(a.loc()[m as usize], 16);
    assert_eq!(a.symbols()[pkg as usize], 5);
    assert_eq!(a.cc_sum()[m as usize], 7);
    assert_eq!(a.cc_max()[m as usize], 4);
    assert_eq!(a.cog_sum()[m as usize], a.get(m).cog_sum);
    assert_eq!(a.cog_max()[m as usize], a.get(m).cog_max);
    for v in 0..ix.node_count() {
        assert_eq!(a.get(v), reference_agg(ix, v), "{}", ix.qid_string(v));
    }
}

#[test]
fn findings_and_scores_hooks() {
    let f = fx(AGG);
    let ix = &f.ix;
    let mut a = Aggregates::build(ix);
    let (outer, inner, c, meth, m) =
        (f.node("agg.m:outer"), f.node("agg.m:outer.inner"), f.node("agg.m:C"), f.node("agg.m:C.meth"), f.node("agg.m"));
    let dist = ix.group_node(0);
    assert_eq!((a.findings(m), a.score(m), a.get(m).score), (0, None, None));
    let base = a.heap_bytes();
    a.set_findings(ix, [meth, meth, outer]);
    assert_eq!((a.findings(meth), a.findings(c), a.findings(outer), a.findings(inner)), (2, 2, 1, 0));
    assert_eq!((a.findings(m), a.findings(dist), a.get(m).findings), (3, 3, 3));
    // Replaces, not adds.
    a.set_findings(ix, [inner]);
    assert_eq!((a.findings(meth), a.findings(outer), a.findings(dist)), (0, 1, 1));
    a.set_scores(vec![0.5; ix.node_count() as usize]);
    assert_eq!(a.score(m), Some(0.5));
    assert!(a.heap_bytes() >= base + 8 * ix.node_count() as usize);
}

#[test]
#[should_panic(expected = "one score per node")]
fn scores_need_one_value_per_node() {
    let f = fx(AGG);
    let mut a = Aggregates::build(&f.ix);
    a.set_scores(vec![1.0]);
}
