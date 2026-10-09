//! Query tests: a hand-checked fixture, origin ranking, and brute-force references on the test
//! corpus.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::graph::CallFilter;
use crate::index::{Inputs, NONE, load};
use crate::resolve::{Attribution, Imports, ResolvedCalls, res_flags, resolve_calls, resolve_imports};
use crate::rollup::Level;
use crate::roots::{FileStatus, Origin};
use crate::walk::{WalkOptions as FsWalk, walk};

struct Fx {
    ix: Index,
    imp: Imports,
    calls: ResolvedCalls,
}

fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pymapper-query-{tag}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for (rel, src) in files {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, src).expect("write");
    }
}

fn resolve(ix: Index) -> Fx {
    let imp = resolve_imports(&ix, &Attribution::new());
    let calls = resolve_calls(&ix, &imp);
    Fx { ix, imp, calls }
}

fn load_dir(dir: &Path) -> Fx {
    let w = walk(&[dir], &FsWalk::everything());
    resolve(load(Inputs::from_root_dir(dir, &w.files, Origin::FirstParty, b"proj")))
}

/// Loads `files` (relative path, source) as one first-party root.
fn fx(files: &[(&str, &str)]) -> Fx {
    let dir = temp_dir("fx");
    write_files(&dir, files);
    let out = load_dir(&dir);
    let _ = fs::remove_dir_all(&dir);
    out
}

fn corpus(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus").join(sub)
}

const PROJECT: &[(&str, &str)] = &[
    ("pkg/__init__.py", "from .core import Engine\n"),
    (
        "pkg/core.py",
        "from pkg import util\n\
         \n\
         class Engine:\n\
         \x20   def __init__(self):\n\
         \x20       self.n = 0\n\
         \n\
         \x20   def run(self):\n\
         \x20       return self.step()\n\
         \n\
         \x20   def step(self):\n\
         \x20       return util.helper()\n\
         \n\
         def make():\n\
         \x20   return Engine()\n",
    ),
    (
        "pkg/util.py",
        "def helper():\n\
         \x20   return leaf()\n\
         \n\
         def leaf():\n\
         \x20   return 1\n\
         \n\
         def rec(n):\n\
         \x20   return rec(n - 1) if n else 0\n\
         \n\
         def ping(n):\n\
         \x20   return pong(n)\n\
         \n\
         def pong(n):\n\
         \x20   return ping(n)\n",
    ),
    ("app/__init__.py", ""),
    (
        "app/main.py",
        "from pkg.core import make\n\
         \n\
         def main():\n\
         \x20   e = make()\n\
         \x20   e.go()\n\
         \x20   e.run()\n\
         \n\
         def leaf():\n\
         \x20   return 2\n\
         \n\
         if __name__ == \"__main__\":\n\
         \x20   main()\n",
    ),
    ("cyc/__init__.py", ""),
    ("cyc/a.py", "import cyc.b\n\ndef fa():\n    pass\n"),
    ("cyc/b.py", "import cyc.a\n"),
];

impl Fx {
    fn node(&self, qid: &str) -> NodeId {
        self.ix.find(qid.as_bytes()).unwrap_or_else(|| panic!("no node {qid}"))
    }

    fn q(&self) -> Query<'_> {
        Query::new(&self.ix, &self.imp, &self.calls)
    }

    fn qids(&self, nodes: impl IntoIterator<Item = NodeId>) -> Vec<String> {
        nodes.into_iter().map(|v| self.ix.qid_string(v)).collect()
    }

    fn sorted_qids(&self, nodes: impl IntoIterator<Item = NodeId>) -> Vec<String> {
        let mut v = self.qids(nodes);
        v.sort();
        v
    }
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

// -- lookup ---------------------------------------------------------------------------------

#[test]
fn lookup_exact_qualified_and_suffix() {
    let f = fx(PROJECT);
    let q = f.q();
    let o = LookupOptions::default();
    let run = f.node("pkg.core:Engine.run");

    let m = q.lookup(b"pkg.core:Engine.run", &o);
    assert_eq!(m, vec![Match { node: run, kind: MatchKind::Exact, score: 0 }]);
    let m = q.lookup(b"pkg.core.Engine.run", &o);
    assert_eq!(m, vec![Match { node: run, kind: MatchKind::Qualified, score: 0 }]);
    for s in ["Engine.run", "core.Engine.run", "core:Engine.run"] {
        let m = q.lookup(s.as_bytes(), &o);
        assert_eq!(m, vec![Match { node: run, kind: MatchKind::Suffix, score: 0 }], "{s}");
    }
    // A module by dotted name, a package by its name, a distribution by `@name`.
    assert_eq!(q.lookup(b"pkg.util", &o)[0].node, f.node("pkg.util"));
    assert_eq!(q.lookup(b"pkg", &o)[0].kind, MatchKind::Exact);
    assert_eq!(q.lookup(b"@proj", &o).len(), 1);
    assert_eq!(f.ix.kind(q.lookup(b"@proj", &o)[0].node), NodeKind::Distribution);
    // Suffix of a module path.
    assert_eq!(f.qids(q.lookup(b"util.helper", &o).iter().map(|m| m.node)), strs(&["pkg.util:helper"]));
}

#[test]
fn lookup_ambiguous_and_resolve() {
    let f = fx(PROJECT);
    let q = f.q();
    let o = LookupOptions::default();
    let m = q.lookup(b"leaf", &o);
    assert_eq!(f.qids(m.iter().map(|m| m.node)), strs(&["app.main:leaf", "pkg.util:leaf"]));
    assert!(m.iter().all(|m| m.kind == MatchKind::Suffix));
    assert!(matches!(q.resolve(b"leaf", &o), Resolved::Ambiguous(v) if v.len() == 2));
    assert_eq!(q.resolve(b"util.leaf", &o), Resolved::Unique(f.node("pkg.util:leaf")));
    assert_eq!(q.resolve(b"pkg.util.leaf", &o), Resolved::Unique(f.node("pkg.util:leaf")));
    // The limit keeps the best matches.
    let one = q.lookup(b"leaf", &LookupOptions { limit: 1, ..o });
    assert_eq!(f.qids(one.iter().map(|m| m.node)), strs(&["app.main:leaf"]));
    assert!(q.lookup(b"", &o).is_empty());
    assert!(q.lookup(b"leaf", &LookupOptions { limit: 0, ..o }).is_empty());
}

#[test]
fn lookup_fuzzy() {
    let f = fx(PROJECT);
    let q = f.q();
    let o = LookupOptions::default();
    let helper = f.node("pkg.util:helper");
    // Case-insensitive equality ranks first, then prefix, substring, edit distance.
    let m = q.lookup(b"ENGINE", &o);
    assert_eq!(m[0], Match { node: f.node("pkg.core:Engine"), kind: MatchKind::Fuzzy, score: 0 });
    let m = q.lookup(b"help", &o);
    assert_eq!((m[0].node, m[0].score >> 16), (helper, 1));
    let m = q.lookup(b"elpe", &o);
    assert_eq!((m[0].node, m[0].score >> 16), (helper, 2));
    let m = q.lookup(b"helpr", &o);
    assert_eq!((m[0].node, m[0].score >> 16, m[0].score & 0xffff), (helper, 3, 1));
    assert!(matches!(q.resolve(b"helpr", &o), Resolved::NotFound(v) if v[0].node == helper));
    assert!(q.lookup(b"helpr", &LookupOptions { fuzzy: false, ..o }).is_empty());
    assert!(q.lookup(b"zzzzzzzz", &o).is_empty());
    // Fuzzy matching uses the last segment.
    assert_eq!(q.lookup(b"nowhere.Helper", &o)[0].node, helper);
}

#[test]
fn name_index_finds_every_node_by_name() {
    let f = load_dir(&corpus("sample_python"));
    let q = f.q();
    let names = q.names();
    let mut total = 0;
    let mut last = None;
    for (sym, run) in names.runs(&f.ix) {
        assert!(last < Some(sym.index()), "runs ascending by Sym");
        last = Some(sym.index());
        assert!(run.windows(2).all(|w| w[0] < w[1]), "nodes ascending within a run");
        assert!(run.iter().all(|&v| f.ix.name_sym(v) == sym));
        total += run.len();
    }
    let non_dist = (0..f.ix.node_count()).filter(|&v| f.ix.kind(v) != NodeKind::Distribution).count();
    assert_eq!(total, non_dist);
    assert_eq!(names.len(), non_dist);
    for v in 0..f.ix.node_count() {
        if f.ix.kind(v) != NodeKind::Distribution {
            assert!(names.of(&f.ix, f.ix.name_sym(v)).contains(&v));
            // Every qid is an exact match of itself.
            let qid = f.ix.qid_string(v);
            assert_eq!(q.lookup(qid.as_bytes(), &LookupOptions::default())[0].node, v, "{qid}");
        }
    }
}

#[test]
fn lookup_ranks_first_party_first() {
    let dir = temp_dir("origins");
    write_files(&dir, &[("site/lib/tool.py", "def run():\n    pass\n"), ("proj/app/tool.py", "def run():\n    pass\n")]);
    let mut inputs = Inputs::new();
    let site = inputs.add_group(b"lib", Origin::ThirdParty, None);
    let proj = inputs.add_group(b"proj", Origin::FirstParty, None);
    let p1 = dir.join("site/lib/tool.py");
    let p2 = dir.join("proj/app/tool.py");
    use std::os::unix::ffi::OsStrExt;
    inputs.push(p1.as_os_str().as_bytes(), site, b"lib.tool", FileStatus::Active).expect("push");
    inputs.push(p2.as_os_str().as_bytes(), proj, b"app.tool", FileStatus::Active).expect("push");
    let f = resolve(load(inputs));
    let _ = fs::remove_dir_all(&dir);
    let q = f.q();
    let m = q.lookup(b"tool.run", &LookupOptions::default());
    assert_eq!(f.qids(m.iter().map(|m| m.node)), strs(&["app.tool:run", "lib.tool:run"]));
    assert_eq!(q.symbol(m[0].node).origin(), Origin::FirstParty);
    assert_eq!(q.symbol(m[1].node).origin(), Origin::ThirdParty);
    let d = q.lookup(b"@lib", &LookupOptions::default())[0].node;
    assert_eq!(q.symbol(d).origin(), Origin::ThirdParty);
}

#[test]
fn symbol_view_and_locations() {
    let f = fx(PROJECT);
    let q = f.q();
    let run = q.symbol(f.node("pkg.core:Engine.run"));
    assert_eq!(run.name(), b"run");
    assert_eq!(run.kind(), NodeKind::Function);
    assert_eq!((run.line(), run.col()), (7, 9));
    assert!(run.path().is_some_and(|p| p.ends_with("pkg/core.py")));
    let mut buf = Vec::new();
    run.qid(&mut buf);
    assert_eq!(buf, b"pkg.core:Engine.run");
    let file = run.file().expect("file");
    assert_eq!(q.symbol_at(file, 8), run.node());
    assert_eq!(q.symbol_at(file, 4), f.node("pkg.core:Engine.__init__"));
    assert_eq!(q.symbol_at(file, 3), f.node("pkg.core:Engine"));
    assert_eq!(q.symbol_at(file, 1), f.node("pkg.core"));
    assert_eq!(q.file_by_path(Path::new("pkg/core.py")), Some(file));
    assert_eq!(q.file_by_path(Path::new("core.py")), Some(file));
    assert_eq!(q.file_by_path(Path::new("__init__.py")), None, "ambiguous suffix");
    assert_eq!(q.file_by_path(Path::new("nope.py")), None);
}

// -- callers / callees / impact -------------------------------------------------------------

/// `(qid, depth, via qid, conf)` of every hop.
fn hops(f: &Fx, w: &Walk) -> Vec<(String, u32, String, Confidence)> {
    w.hops.iter().map(|h| (f.ix.qid_string(h.node), h.depth, f.ix.qid_string(h.via), h.conf)).collect()
}

#[test]
fn callers_by_depth_and_confidence() {
    let f = fx(PROJECT);
    let q = f.q();
    let mut s = Scratch::default();
    let helper = f.node("pkg.util:helper");
    let w = q.callers(helper, &WalkOptions::default(), &mut s);
    assert_eq!(f.qids(w.hops.iter().map(|h| h.node)), strs(&["pkg.core:Engine.step"]));
    assert!(!w.truncated);
    let w = q.callers(helper, &WalkOptions { depth: 2, ..WalkOptions::default() }, &mut s);
    assert_eq!(f.qids(w.hops.iter().map(|h| h.node)), strs(&["pkg.core:Engine.step", "pkg.core:Engine.run"]));
    assert_eq!(w.hops[1].via, f.node("pkg.core:Engine.step"));
    assert_eq!(w.hops[1].conf, Confidence::Certain, "self.step() resolves through the MRO");
    // `e.run()` on an unhinted variable: a unique-method edge (Possible).
    let all = q.callers(helper, &WalkOptions::transitive(), &mut s);
    let h = hops(&f, &all);
    assert!(h.contains(&("app.main:main".into(), 3, "pkg.core:Engine.run".into(), Confidence::Possible)), "{h:?}");
    let main_ix = all.hops.iter().position(|h| h.node == f.node("app.main:main")).expect("main");
    assert_eq!(all.hops[main_ix].worst, Confidence::Possible);
    assert_eq!(f.qids(all.path_from(main_ix)), strs(&["app.main:main", "pkg.core:Engine.run", "pkg.core:Engine.step", "pkg.util:helper"]));
    assert_eq!(all.by_depth()[..3], [1, 1, 1]);
    // Likely and better: the walk stops at Engine.run.
    let sure = q.callers(helper, &WalkOptions { min_conf: Confidence::Likely, ..WalkOptions::transitive() }, &mut s);
    assert!(sure.hops.iter().all(|h| h.conf <= Confidence::Likely && h.worst <= Confidence::Likely));
    assert!(!sure.hops.iter().any(|h| h.node == f.node("app.main:main")));
    // Depth 0 and limits.
    assert!(q.callers(helper, &WalkOptions { depth: 0, ..WalkOptions::default() }, &mut s).hops.is_empty());
    let lim = q.callers(helper, &WalkOptions { limit: 1, ..WalkOptions::transitive() }, &mut s);
    assert_eq!(lim.hops.len(), 1);
    assert!(lim.truncated);
}

#[test]
fn callees_and_subtree_targets() {
    let f = fx(PROJECT);
    let q = f.q();
    let mut s = Scratch::default();
    let main = f.node("app.main:main");
    let w = q.callees(main, &WalkOptions::default(), &mut s);
    let got = f.sorted_qids(w.hops.iter().map(|h| h.node));
    assert!(got.contains(&"pkg.core:make".to_string()) && got.contains(&"pkg.core:Engine.run".to_string()), "{got:?}");
    // Direct callees through the zero-copy iterator agree with depth 1.
    let mut direct: Vec<NodeId> = q.direct_callees(main).map(|(v, _, _)| v).collect();
    direct.sort_unstable();
    let mut walked: Vec<NodeId> = w.hops.iter().map(|h| h.node).collect();
    walked.sort_unstable();
    assert_eq!(direct, walked);
    // A class target covers its methods: callers of Engine include make (constructor) and
    // Engine.run's caller main; calls inside the class (run -> step) are not reported.
    let engine = f.node("pkg.core:Engine");
    let w = q.callers(engine, &WalkOptions::default(), &mut s);
    let got = f.sorted_qids(w.hops.iter().map(|h| h.node));
    assert_eq!(got, strs(&["app.main:main", "pkg.core:make"]));
    let only = q.callers(engine, &WalkOptions { scope: Scope::Node, ..WalkOptions::default() }, &mut s);
    assert_eq!(f.qids(only.hops.iter().map(|h| h.node)), strs(&["pkg.core:make"]));
    // Call sites behind an edge.
    let sites: Vec<Site> = q.call_sites(f.node("pkg.core:make"), engine).collect();
    assert_eq!(sites.len(), 1);
    assert_eq!((sites[0].line, sites[0].col), (14, 12));
    assert_eq!(sites[0].reason, Reason::Constructor);
}

#[test]
fn override_edges_are_dispatch() {
    let f = fx(&[(
        "m.py",
        "class Base:\n    def run(self):\n        pass\n\nclass Child(Base):\n    def run(self):\n        pass\n\ndef use():\n    b = Base()\n    b.run()\n",
    )]);
    let q = f.q();
    let mut s = Scratch::default();
    let child = f.node("m:Child.run");
    let w = q.callers(child, &WalkOptions::transitive(), &mut s);
    let base = w.hops.iter().find(|h| h.node == f.node("m:Base.run")).expect("override edge");
    assert!(base.dispatch);
    assert!(w.hops.iter().any(|h| h.node == f.node("m:use")), "callers of the base method reach the override");
    let no = q.callers(child, &WalkOptions { overrides: false, ..WalkOptions::transitive() }, &mut s);
    assert!(no.hops.is_empty());
    let sites: Vec<Site> = q.call_sites(f.node("m:Base.run"), child).collect();
    assert_eq!(sites.len(), 1);
    assert_eq!((sites[0].line, sites[0].reason), (6, Reason::Override));
}

#[test]
fn impact_counts() {
    let f = fx(PROJECT);
    let q = f.q();
    let mut s = Scratch::default();
    let leaf = f.node("pkg.util:leaf");
    let imp = q.impact(leaf, &WalkOptions::transitive(), &mut s);
    let affected = f.sorted_qids(imp.walk.hops.iter().map(|h| h.node));
    // helper, Engine.step, Engine.run, main, and app.main's module scope (the __main__ guard).
    assert_eq!(affected, strs(&["app.main", "app.main:main", "pkg.core:Engine.run", "pkg.core:Engine.step", "pkg.util:helper"]));
    assert_eq!((imp.symbols, imp.module_scopes, imp.files, imp.tests), (4, 1, 3, 0));
    let mods: Vec<(String, u32)> = imp.modules.iter().map(|&(m, c)| (f.ix.qid_string(m), c)).collect();
    assert_eq!(mods, vec![("app.main".into(), 2), ("pkg.core".into(), 2), ("pkg.util".into(), 1)]);
    // pkg.util is imported by pkg.core (`from pkg import util`), which pkg and app.main import.
    assert_eq!(f.sorted_qids(imp.importers.iter().copied()), strs(&["app.main", "pkg", "pkg.core"]));
}

// -- path -----------------------------------------------------------------------------------

#[test]
fn shortest_paths() {
    let f = fx(PROJECT);
    let q = f.q();
    let mut s = Scratch::default();
    let (main, leaf) = (f.node("app.main:main"), f.node("pkg.util:leaf"));
    let p = q.path(main, leaf, &PathOptions::default(), &mut s).expect("path");
    assert_eq!(f.qids(p.nodes.iter().copied()), strs(&["app.main:main", "pkg.core:Engine.run", "pkg.core:Engine.step", "pkg.util:helper", "pkg.util:leaf"]));
    assert_eq!(p.len(), 4);
    assert_eq!(p.conf(0), Some(Confidence::Possible));
    assert_eq!(p.worst(), Some(Confidence::Possible));
    assert!(!p.dispatch(0));
    assert!(q.path(main, leaf, &PathOptions { min_conf: Confidence::Likely, ..PathOptions::default() }, &mut s).is_none());
    assert!(q.path(main, leaf, &PathOptions { max_len: 3, ..PathOptions::default() }, &mut s).is_none());
    assert!(q.path(main, leaf, &PathOptions { max_len: 4, ..PathOptions::default() }, &mut s).is_some());
    assert!(q.path(leaf, main, &PathOptions::default(), &mut s).is_none(), "no edge back");
    // Overlapping targets: a single node.
    let p = q.path(f.node("pkg.core"), f.node("pkg.core:make"), &PathOptions::default(), &mut s).expect("overlap");
    assert!(p.is_empty());
    // A class target reaches through any method.
    let p = q.path(f.node("pkg.core:Engine"), leaf, &PathOptions::default(), &mut s).expect("class path");
    assert_eq!(f.qids(p.nodes.iter().copied()), strs(&["pkg.core:Engine.step", "pkg.util:helper", "pkg.util:leaf"]));
    // Import paths between modules; defs stand for their module.
    let o = PathOptions { graph: PathGraph::Imports, ..PathOptions::default() };
    let p = q.path(f.node("app.main"), f.node("pkg.util:leaf"), &o, &mut s).expect("import path");
    assert_eq!(f.qids(p.nodes.iter().copied()), strs(&["app.main", "pkg.core", "pkg.util"]));
    assert_eq!(p.conf(0), None);
    assert_eq!(p.import_flags(0) & res_flags::TYPE_ONLY, 0);
    assert!(q.path(f.node("pkg.util"), f.node("app.main"), &o, &mut s).is_none());
}
