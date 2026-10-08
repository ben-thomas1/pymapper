//! Graphs of an index: call graph, import graph, module call graph.

use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::index::{Index, Inputs, NodeId, load};
use crate::resolve::{Attribution, Confidence, Imports, Reason, ResolvedCalls, res_flags, resolve_calls, resolve_imports};
use crate::roots::Origin;
use crate::walk::{WalkOptions, walk};

struct Fx {
    ix: Index,
    imp: Imports,
    calls: ResolvedCalls,
}

/// Loads `files` (relative path, source) as one first-party root and resolves everything.
fn fx(files: &[(&str, &str)]) -> Fx {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pymapper-graph-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    for (rel, src) in files {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, src).expect("write");
    }
    let w = walk(&[dir.as_path()], &WalkOptions::everything());
    let ix = load(Inputs::from_root_dir(&dir, &w.files, Origin::FirstParty, b"proj"));
    let _ = fs::remove_dir_all(&dir);
    let imp = resolve_imports(&ix, &Attribution::new());
    let calls = resolve_calls(&ix, &imp);
    Fx { ix, imp, calls }
}

impl Fx {
    fn node(&self, qid: &str) -> NodeId {
        (0..self.ix.node_count()).find(|&v| self.ix.qid_string(v) == qid).unwrap_or_else(|| panic!("no node {qid}"))
    }

    /// Edges of a module graph as sorted `src -> dst` qids.
    fn module_edges(&self, mg: &ModuleGraph) -> Vec<String> {
        let g = mg.graph();
        let mut out = Vec::new();
        for m in 0..mg.len() {
            for &d in g.succ(m) {
                out.push(format!("{} -> {}", self.ix.qid_string(mg.node(m)), self.ix.qid_string(mg.node(d))));
            }
        }
        out.sort();
        out
    }

    /// Cyclic components of a module graph as sorted member qid lists.
    fn module_cycles(&self, mg: &ModuleGraph) -> Vec<Vec<String>> {
        let scc = Scc::of(mg.graph());
        let mut out: Vec<Vec<String>> = scc
            .nontrivial()
            .map(|c| scc.members(c).iter().map(|&m| self.ix.qid_string(mg.node(m))).collect())
            .collect();
        out.sort();
        out
    }
}

const CYCLE: &[(&str, &str)] = &[
    ("pkg/__init__.py", "from .a import fa\n"),
    ("pkg/a.py", "from . import b\n\ndef fa():\n    return b.fb()\n"),
    ("pkg/b.py", "from .a import fa\n\ndef fb():\n    return fa()\n"),
    (
        "pkg/c.py",
        "from typing import TYPE_CHECKING\nif TYPE_CHECKING:\n    from .d import D\n\ndef fc():\n    import pkg.a\n    return pkg.a.fa()\n",
    ),
    ("pkg/d.py", "from .c import fc\n\nclass D:\n    def run(self):\n        return fc()\n"),
    ("pkg/ns/e.py", "def fe(x):\n    return x.only_here()\n\nclass E:\n    def only_here(self):\n        return fe(self)\n"),
];

#[test]
fn import_graph_edges_and_cycles() {
    let f = fx(CYCLE);
    let all = import_graph(&f.ix, &f.imp, 0);
    // Every package/module node is a local id, namespace package `pkg.ns` included.
    let names: Vec<String> = all.nodes().iter().map(|&v| f.ix.qid_string(v)).collect();
    assert_eq!(names, ["pkg", "pkg.a", "pkg.b", "pkg.c", "pkg.d", "pkg.ns", "pkg.ns.e"]);
    assert_eq!(all.local(f.node("pkg.ns")).map(|m| all.node(m)), Some(f.node("pkg.ns")));
    assert_eq!(all.local(f.node("pkg.a:fa")), None);
    assert_eq!(
        f.module_edges(&all),
        ["pkg -> pkg.a", "pkg.a -> pkg.b", "pkg.b -> pkg.a", "pkg.c -> pkg.a", "pkg.c -> pkg.d", "pkg.d -> pkg.c"]
    );
    assert_eq!(f.module_cycles(&all), [vec!["pkg.a", "pkg.b"], vec!["pkg.c", "pkg.d"]]);
    // Labels are the edge flags.
    let local = |q: &str| all.local(f.node(q)).expect("module");
    let (a, c, d) = (local("pkg.a"), local("pkg.c"), local("pkg.d"));
    assert_eq!(all.graph().label(c, d).map(|l| l & res_flags::TYPE_ONLY), Some(res_flags::TYPE_ONLY));
    assert_eq!(all.graph().label(c, a).map(|l| l & res_flags::LAZY), Some(res_flags::LAZY));

    // What runs at import time: type-only and lazy edges go, and with them the c/d cycle.
    let runtime = import_graph(&f.ix, &f.imp, res_flags::TYPE_ONLY | res_flags::LAZY);
    assert_eq!(f.module_edges(&runtime), ["pkg -> pkg.a", "pkg.a -> pkg.b", "pkg.b -> pkg.a", "pkg.d -> pkg.c"]);
    assert_eq!(f.module_cycles(&runtime), [vec!["pkg.a", "pkg.b"]]);
    let cd = Condensed::of(runtime.graph());
    assert_eq!(cd.count(), runtime.len() - 1);
}

#[test]
fn call_graph_threshold_and_reasons() {
    let f = fx(CYCLE);
    let all = call_graph(&f.ix, &f.calls, CallFilter::default());
    assert_eq!(all.node_count(), f.ix.node_count());
    let (fa, fb, fe) = (f.node("pkg.a:fa"), f.node("pkg.b:fb"), f.node("pkg.ns.e:fe"));
    let only = f.node("pkg.ns.e:E.only_here");
    assert!(all.has_edge(fa, fb) && all.has_edge(fb, fa));
    assert_eq!(all.label(fa, fb), Some(Confidence::Certain as u8));
    // `x.only_here()` on an unknown receiver: the unique method of that name (Possible).
    assert_eq!(all.label(fe, only), Some(Confidence::Possible as u8));
    assert!(all.pred(only).contains(&fe));
    let scc = Scc::of(&all);
    assert_eq!(scc.comp(fa), scc.comp(fb));
    assert_eq!(scc.comp(fe), scc.comp(only));

    let certain = call_graph(&f.ix, &f.calls, CallFilter::at_least(Confidence::Certain));
    assert!(certain.has_edge(fa, fb));
    assert!(!certain.has_edge(fe, only));
    assert!(certain.edge_count() < all.edge_count());
    assert!(certain.forward().labels().iter().all(|&l| l == Confidence::Certain as u8));
    let scc = Scc::of(&certain);
    assert_ne!(scc.comp(fe), scc.comp(only));

    let no_unique = call_graph(&f.ix, &f.calls, CallFilter::default().without(Reason::UniqueMethod));
    assert!(!no_unique.has_edge(fe, only));
    assert!(no_unique.has_edge(fa, fb));
    // Labels are the best confidence of the call edges behind each graph edge.
    for e in f.calls.edges() {
        let l = all.label(e.src, e.dst).expect("edge kept");
        assert!(l <= e.conf as u8);
    }
}

#[test]
fn module_call_graph_lifts_calls() {
    let f = fx(CYCLE);
    let mg = module_call_graph(&f.ix, &f.calls, CallFilter::default());
    assert_eq!(f.module_edges(&mg), ["pkg.a -> pkg.b", "pkg.b -> pkg.a", "pkg.c -> pkg.a", "pkg.d -> pkg.c"]);
    assert_eq!(f.module_cycles(&mg), [vec!["pkg.a", "pkg.b"]]);
    let (c, a) = (mg.local(f.node("pkg.c")).expect("c"), mg.local(f.node("pkg.a")).expect("a"));
    assert_eq!(mg.graph().label(c, a), Some(Confidence::Certain as u8));
    assert!(mg.heap_bytes() > 0);
}

#[test]
fn empty_index() {
    let f = fx(&[]);
    let mg = import_graph(&f.ix, &f.imp, 0);
    assert!(mg.is_empty());
    assert_eq!(mg.graph().edge_count(), 0);
    let g = call_graph(&f.ix, &f.calls, CallFilter::default());
    assert_eq!(g.edge_count(), 0);
    assert_eq!(Condensed::of(&g).count(), f.ix.node_count());
}
