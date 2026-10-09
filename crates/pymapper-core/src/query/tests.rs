//! Query tests: a hand-checked fixture, origin ranking, and brute-force references on the test
//! corpus.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::graph::CallFilter;
use crate::index::{Inputs, load};
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
fn resolve_with_limit_one_stays_ambiguous() {
    let f = fx(PROJECT);
    let q = f.q();
    let one = LookupOptions { limit: 1, ..LookupOptions::default() };
    // A second suffix match is still looked for; the list is cut to the limit.
    match q.resolve(b"leaf", &one) {
        Resolved::Ambiguous(v) => assert_eq!(f.qids(v.iter().map(|m| m.node)), strs(&["app.main:leaf"])),
        r => panic!("{r:?}"),
    }
    assert_eq!(q.resolve(b"util.leaf", &one), Resolved::Unique(f.node("pkg.util:leaf")));
    assert_eq!(q.resolve(b"pkg.core:Engine.run", &one), Resolved::Unique(f.node("pkg.core:Engine.run")));
    assert!(matches!(q.resolve(b"helpr", &one), Resolved::NotFound(v) if v.len() == 1));
    assert_eq!(q.resolve(b"leaf", &LookupOptions { limit: 0, ..one }), Resolved::NotFound(Vec::new()));
}

#[test]
fn lookup_rejects_empty_segments() {
    let f = fx(PROJECT);
    let q = f.q();
    let o = LookupOptions { fuzzy: false, ..LookupOptions::default() };
    for s in ["pkg.", ".pkg", "pkg..core", "pkg.core.", "core..Engine", ":Engine.run", "Engine.run:", "."] {
        assert!(q.lookup(s.as_bytes(), &o).is_empty(), "{s}");
        assert!(!matches!(q.resolve(s.as_bytes(), &o), Resolved::Unique(_)), "{s}");
    }
    // Fuzzy suggestions still use the last segment.
    let m = q.lookup(b".pkg", &LookupOptions::default());
    assert!(m.iter().all(|m| m.kind == MatchKind::Fuzzy) && m[0].node == f.node("pkg"));
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
    // Leading `./` components are not part of the suffix.
    assert_eq!(q.file_by_path(Path::new("./pkg/core.py")), Some(file));
    assert_eq!(q.file_by_path(Path::new("././core.py")), Some(file));
    assert_eq!(q.file_by_path(Path::new("./")), None);
    assert_eq!(q.file_by_path(Path::new(".")), None);
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

// -- deps -----------------------------------------------------------------------------------

/// `(qid, sites, imports)` of every dependency.
fn deps_of(f: &Fx, d: &Deps) -> Vec<(String, u32, u32)> {
    d.deps.iter().map(|e| (f.ix.qid_string(e.node), e.sites, e.imports)).collect()
}

#[test]
fn module_deps_in_and_out() {
    let f = fx(PROJECT);
    let q = f.q();
    let mut s = Scratch::default();
    let o = DepOptions::default();
    let core = f.node("pkg.core");
    // pkg.core imports pkg.util (one row, `from pkg import util`) and calls util.helper once.
    let out = q.deps(core, Direction::Out, &o, &mut s);
    assert_eq!(deps_of(&f, &out), vec![("pkg.util".into(), 1, 1)]);
    assert_eq!(out.deps[0].best(), Some(Confidence::Certain));
    // Into pkg.core: app.main (import + make() + e.run()), pkg (__init__ imports .core).
    let inn = q.deps(core, Direction::In, &o, &mut s);
    assert_eq!(deps_of(&f, &inn), vec![("app.main".into(), 2, 1), ("pkg".into(), 0, 1)]);
    let main_dep = inn.deps[0];
    assert_eq!((main_dep.best(), main_dep.worst()), (Some(Confidence::Certain), Some(Confidence::Possible)));
    // Calls only / imports only.
    let calls_only = q.deps(core, Direction::In, &DepOptions { imports: false, ..o }, &mut s);
    assert_eq!(deps_of(&f, &calls_only), vec![("app.main".into(), 2, 0)]);
    let sure = q.deps(core, Direction::In, &DepOptions { calls: CallFilter::at_least(Confidence::Likely), imports: false, ..o }, &mut s);
    assert_eq!(deps_of(&f, &sure), vec![("app.main".into(), 1, 0)]);
}

#[test]
fn deps_roll_up_without_swallowing_the_target() {
    let f = fx(PROJECT);
    let q = f.q();
    let mut s = Scratch::default();
    // At depth 1, app.main's dependencies roll up to the package `pkg`.
    let o = DepOptions { level: Level::Depth(1), ..DepOptions::default() };
    let out = q.deps(f.node("app.main"), Direction::Out, &o, &mut s);
    assert_eq!(deps_of(&f, &out), vec![("pkg".into(), 2, 1)]);
    // pkg.core's dependency pkg.util would roll up to pkg, an ancestor of pkg.core: it stays
    // pkg.util. Its dependents roll up to `app` and stay `pkg` (the __init__ module itself).
    let out = q.deps(f.node("pkg.core"), Direction::Out, &o, &mut s);
    assert_eq!(deps_of(&f, &out), vec![("pkg.util".into(), 1, 1)]);
    let inn = q.deps(f.node("pkg.core"), Direction::In, &o, &mut s);
    assert_eq!(deps_of(&f, &inn), vec![("app".into(), 2, 1), ("pkg".into(), 0, 1)]);
    // A package target: everything inside it is internal.
    let out = q.deps(f.node("pkg"), Direction::Out, &DepOptions::default(), &mut s);
    assert!(out.deps.is_empty(), "{:?}", deps_of(&f, &out));
    let inn = q.deps(f.node("pkg"), Direction::In, &DepOptions::default(), &mut s);
    assert_eq!(deps_of(&f, &inn), vec![("app.main".into(), 2, 1)]);
    // A def target: calls of its subtree; incoming calls from its callers.
    let step = f.node("pkg.core:Engine.step");
    let out = q.deps(step, Direction::Out, &DepOptions::default(), &mut s);
    assert_eq!(deps_of(&f, &out), vec![("pkg.util".into(), 1, 0)]);
    let inn = q.deps(step, Direction::In, &DepOptions { level: Level::Depth(u32::MAX), ..DepOptions::default() }, &mut s);
    assert_eq!(deps_of(&f, &inn), vec![("pkg.core:Engine.run".into(), 1, 0)]);
}

#[test]
fn deps_external_imports() {
    let f = fx(&[("m.py", "import os\nimport os.path\nimport requests\nfrom json import dumps\n")]);
    let q = f.q();
    let mut s = Scratch::default();
    let out = q.deps(f.node("m"), Direction::Out, &DepOptions::default(), &mut s);
    let ext: Vec<(String, u32)> =
        out.external.iter().map(|e| (String::from_utf8_lossy(e.name(&f.ix).unwrap_or(b"?")).into_owned(), e.rows)).collect();
    assert_eq!(ext, vec![("os".into(), 2), ("json".into(), 1), ("requests".into(), 1)]);
    assert!(out.external.iter().all(|e| e.dist().is_none()));
}

#[test]
fn function_level_imports_count_for_def_targets() {
    let f = fx(&[("a.py", "def f():\n    import b\n    b.g()\n"), ("b.py", "def g():\n    pass\n")]);
    let q = f.q();
    let mut s = Scratch::default();
    let out = q.deps(f.node("a:f"), Direction::Out, &DepOptions::default(), &mut s);
    assert_eq!(deps_of(&f, &out), vec![("b".into(), 1, 1)]);
    assert_ne!(out.deps[0].import_flags & res_flags::LAZY, 0);
    let runtime = q.deps(f.node("a"), Direction::Out, &DepOptions { import_exclude: res_flags::LAZY, ..DepOptions::default() }, &mut s);
    assert_eq!(deps_of(&f, &runtime), vec![("b".into(), 1, 0)]);
}

// -- cycles ---------------------------------------------------------------------------------

#[test]
fn import_cycles_containing_a_target() {
    let f = fx(PROJECT);
    let q = f.q();
    let mut s = Scratch::default();
    let hits = q.import_cycles(f.node("cyc.a"), &mut s);
    assert_eq!(hits.len(), 1);
    assert_eq!(f.qids(hits[0].members.iter().copied()), strs(&["cyc.a", "cyc.b"]));
    assert_eq!(hits[0].witness.len(), 2);
    // A def stands for its module; a package for its modules.
    assert_eq!(q.import_cycles(f.node("cyc.a:fa"), &mut s), hits);
    assert_eq!(q.import_cycles(f.node("cyc"), &mut s), hits);
    assert!(q.import_cycles(f.node("pkg"), &mut s).is_empty());
}

#[test]
fn call_cycles_through_a_node() {
    let f = fx(PROJECT);
    let q = f.q();
    let mut s = Scratch::default();
    let o = WalkOptions::default();
    let rec = f.node("pkg.util:rec");
    let hit = q.call_cycle(rec, &o, &mut s).expect("self recursion");
    assert_eq!((hit.members.clone(), hit.witness.clone()), (vec![rec], vec![rec]));
    let hit = q.call_cycle(f.node("pkg.util:pong"), &o, &mut s).expect("mutual recursion");
    assert_eq!(f.qids(hit.members.iter().copied()), strs(&["pkg.util:ping", "pkg.util:pong"]));
    assert_eq!(f.qids(hit.witness.iter().copied()), strs(&["pkg.util:pong", "pkg.util:ping"]));
    assert!(q.call_cycle(f.node("pkg.util:helper"), &o, &mut s).is_none());
}

// -- summary --------------------------------------------------------------------------------

#[test]
fn summary_counts_and_rankings() {
    let f = fx(PROJECT);
    let q = f.q();
    let s = q.summary(&SummaryOptions::default());
    let fp = s.origins[Origin::FirstParty as usize];
    assert_eq!((fp.files, fp.packages, fp.modules), (8, 3, 5));
    assert_eq!(fp.classes, 1);
    assert_eq!(fp.functions, 12);
    assert_eq!(fp.lines, PROJECT.iter().map(|(_, src)| src.lines().count() as u64).sum::<u64>());
    assert_eq!(s.origins[Origin::Stdlib as usize], OriginCounts::default());
    assert_eq!((s.import_cycles, s.largest_import_cycle), (1, 2));
    assert_eq!(s.call_sites, f.calls.len() as u64);
    assert_eq!(s.calls_by_conf.iter().sum::<u64>(), s.call_sites);
    assert!(s.cognitive.len() <= 10 && !s.cognitive.is_empty());
    assert!(s.cognitive.windows(2).all(|w| w[0].value >= w[1].value));
    // pkg.core is imported by pkg and app.main; most called: each called symbol once.
    assert_eq!(s.most_imported[0], Ranked { node: f.node("pkg.core"), value: 2 });
    assert!(s.most_called.iter().all(|r| r.value >= 1));
    let top1 = q.summary(&SummaryOptions { top: 1, ..SummaryOptions::default() });
    assert_eq!((top1.cognitive.len(), top1.most_called.len(), top1.most_imported.len()), (1, 1, 1));
}

#[test]
fn summary_where_restricts_rankings_only() {
    let f = fx(PROJECT);
    let q = f.q();
    let opts = SummaryOptions::default();
    let all = q.summary(&opts);
    assert_eq!(q.summary_where(&opts, |_| true), all);
    let none = q.summary_where(&opts, |_| false);
    assert_eq!((none.origins, none.call_sites, none.import_cycles), (all.origins, all.call_sites, all.import_cycles));
    assert!(none.cognitive.is_empty() && none.cyclomatic.is_empty() && none.most_called.is_empty() && none.most_imported.is_empty());
    // One file: every ranked node lies in it.
    let core = f.ix.file(f.node("pkg.core")).expect("file");
    let one = q.summary_where(&opts, |file| file == core);
    for r in one.cognitive.iter().chain(&one.cyclomatic).chain(&one.most_called).chain(&one.most_imported) {
        assert_eq!(f.ix.file(r.node), Some(core));
    }
    assert!(!one.most_called.is_empty() || !one.cognitive.is_empty());
}

#[test]
fn edge_site_agrees_with_call_sites() {
    let f = corpus_fx();
    let q = f.q();
    for e in f.calls.edges().iter().filter(|e| e.conf != Confidence::Unresolved).take(5000) {
        let site = q.edge_site(e);
        assert!(q.call_sites(e.src, e.dst).any(|s| s == site), "{e:?}");
    }
}

// -- brute-force references on the corpus ---------------------------------------------------

/// Adjacency of the kept call edges: node -> neighbours (forward or reverse).
fn adjacency(f: &Fx, min_conf: Confidence, overrides: bool, reverse: bool) -> HashMap<NodeId, Vec<NodeId>> {
    let mut adj: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for e in f.calls.edges() {
        if e.conf == Confidence::Unresolved || e.conf > min_conf || (!overrides && e.reason == Reason::Override) {
            continue;
        }
        let (a, b) = if reverse { (e.dst, e.src) } else { (e.src, e.dst) };
        adj.entry(a).or_default().push(b);
    }
    adj
}

/// (best confidence, only an override edge is that sure) of every (src, dst) pair over the
/// call edges with a target (override edges only when `overrides`).
fn best_edges(f: &Fx, overrides: bool) -> HashMap<(NodeId, NodeId), (Confidence, bool)> {
    let mut best: HashMap<(NodeId, NodeId), (Confidence, bool)> = HashMap::new();
    for e in f.calls.edges() {
        if e.conf == Confidence::Unresolved || (!overrides && e.reason == Reason::Override) {
            continue;
        }
        let new = (e.conf, e.reason == Reason::Override);
        let cur = best.entry((e.src, e.dst)).or_insert(new);
        // Lower confidence first; at equal confidence a real call wins.
        if new < *cur {
            *cur = new;
        }
    }
    best
}

/// Plain BFS distances from `s` (itself at 0).
fn distances(adj: &HashMap<NodeId, Vec<NodeId>>, s: NodeId) -> HashMap<NodeId, u32> {
    let mut dist = HashMap::from([(s, 0)]);
    let mut queue = VecDeque::from([s]);
    while let Some(u) = queue.pop_front() {
        let d = dist[&u];
        for &w in adj.get(&u).map_or(&[][..], |v| &v[..]) {
            if let std::collections::hash_map::Entry::Vacant(e) = dist.entry(w) {
                e.insert(d + 1);
                queue.push_back(w);
            }
        }
    }
    dist
}

/// The test corpus plus a deterministic synthetic project with dense cross-module calls,
/// imports, classes and cycles, loaded as one root.
fn corpus_fx() -> Fx {
    let dir = temp_dir("corpus");
    let src = corpus("");
    let w = walk(&[src.as_path()], &FsWalk::everything());
    for i in 0..w.files.len() {
        let p = w.files.get(i);
        let rel = p.strip_prefix(&src).expect("under corpus");
        let dst = dir.join("corpus").join(rel);
        fs::create_dir_all(dst.parent().expect("parent")).expect("mkdir");
        fs::copy(p, &dst).expect("copy");
    }
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = |n: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % n
    };
    let (pkgs, mods, funcs) = (3u64, 4u64, 6u64);
    let mut files: Vec<(String, String)> = Vec::new();
    for p in 0..pkgs {
        files.push((format!("syn{p}/__init__.py"), String::new()));
        for m in 0..mods {
            let mut imports = String::new();
            let mut body = String::new();
            for fi in 0..funcs {
                body.push_str(&format!("def f{fi}(x):\n"));
                for _ in 0..2 {
                    let (tp, tm, tf) = (next(pkgs), next(mods), next(funcs));
                    if (tp, tm) == (p, m) {
                        body.push_str(&format!("    f{tf}(x)\n"));
                    } else {
                        let alias = format!("s{tp}m{tm}f{tf}");
                        imports.push_str(&format!("from syn{tp}.m{tm} import f{tf} as {alias}\n"));
                        body.push_str(&format!("    {alias}(x)\n"));
                    }
                }
                body.push('\n');
            }
            body.push_str(&format!(
                "class K{m}:\n    def run(self):\n        return self.go()\n\n    def go(self):\n        return f{}(1)\n",
                next(funcs)
            ));
            // A subclass of another module's class: a cross-file override edge.
            let (bp, bm) = (next(pkgs), next(mods));
            if (bp, bm) != (p, m) {
                imports.push_str(&format!("from syn{bp}.m{bm} import K{bm} as Base\n"));
                body.push_str(&format!("\nclass Sub{m}(Base):\n    def go(self):\n        return f{}(2)\n", next(funcs)));
            }
            files.push((format!("syn{p}/m{m}.py"), format!("{imports}\n{body}")));
        }
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    write_files(&dir, &refs);
    let out = load_dir(&dir);
    let _ = fs::remove_dir_all(&dir);
    out
}

#[test]
fn walks_match_plain_bfs() {
    let f = corpus_fx();
    let q = f.q();
    let mut s = Scratch::default();
    let mut checked = 0;
    let mut dispatch = 0;
    for (min_conf, overrides) in [Confidence::Possible, Confidence::Likely, Confidence::Certain].into_iter().flat_map(|c| [(c, true), (c, false)]) {
        let rev = adjacency(&f, min_conf, overrides, true);
        let fwd = adjacency(&f, min_conf, overrides, false);
        let best = best_edges(&f, overrides);
        for v in 0..f.ix.node_count() {
            for (adj, callers) in [(&rev, true), (&fwd, false)] {
                let o = WalkOptions { min_conf, overrides, scope: Scope::Node, ..WalkOptions::transitive() };
                let w = if callers { q.callers(v, &o, &mut s) } else { q.callees(v, &o, &mut s) };
                let mut want = distances(adj, v);
                want.remove(&v);
                let got: HashMap<NodeId, u32> = w.hops.iter().map(|h| (h.node, h.depth)).collect();
                assert_eq!(got.len(), w.hops.len(), "each node once");
                assert_eq!(got, want, "node {} callers={callers} {min_conf:?} overrides={overrides}", f.ix.qid_string(v));
                // Each hop reports the best edge between it and `via` among those followed.
                for h in &w.hops {
                    let pair = if callers { (h.node, h.via) } else { (h.via, h.node) };
                    assert_eq!(Some(&(h.conf, h.dispatch)), best.get(&pair), "{pair:?} overrides={overrides}");
                    assert!(overrides || !h.dispatch);
                    dispatch += usize::from(h.dispatch);
                }
                // Depth-bounded walks are the prefix of the unbounded one.
                let d2 = WalkOptions { depth: 2, ..o };
                let w2 = if callers { q.callers(v, &d2, &mut s) } else { q.callees(v, &d2, &mut s) };
                assert_eq!(w2.hops.len(), want.values().filter(|&&d| d <= 2).count());
                // Every hop's path is made of kept edges and has depth + 1 nodes.
                for i in 0..w.hops.len() {
                    let p = w.path_from(i);
                    assert_eq!(p.len() as u32, w.hops[i].depth + 1);
                    assert_eq!(*p.last().expect("target"), v);
                    for pair in p.windows(2) {
                        assert!(adj[&pair[1]].contains(&pair[0]));
                    }
                }
                checked += w.hops.len();
            }
        }
    }
    assert!(checked > 100, "the corpus has call chains ({checked})");
    assert!(dispatch > 10, "the corpus has override edges ({dispatch})");
}

#[test]
fn paths_match_plain_bfs() {
    let f = corpus_fx();
    let q = f.q();
    let mut s = Scratch::default();
    let mut found = 0;
    let n = f.ix.node_count();
    for overrides in [true, false] {
        let fwd = adjacency(&f, Confidence::Possible, overrides, false);
        let best = best_edges(&f, overrides);
        for a in 0..n {
            let dist = distances(&fwd, a);
            for b in (0..n).step_by(3) {
                let o = PathOptions { overrides, scope: Scope::Node, ..PathOptions::default() };
                let p = q.path(a, b, &o, &mut s);
                match dist.get(&b) {
                    None => assert!(p.is_none(), "{} -> {}", f.ix.qid_string(a), f.ix.qid_string(b)),
                    Some(&d) => {
                        let p = p.expect("reachable");
                        assert_eq!(p.len() as u32, d);
                        assert_eq!((p.nodes[0], *p.nodes.last().expect("end")), (a, b));
                        for (i, pair) in p.nodes.windows(2).enumerate() {
                            assert!(fwd[&pair[0]].contains(&pair[1]));
                            let (conf, dispatch) = best[&(pair[0], pair[1])];
                            assert_eq!((p.conf(i), p.dispatch(i)), (Some(conf), dispatch));
                        }
                        found += usize::from(d > 1);
                    }
                }
            }
        }
    }
    assert!(found > 20, "multi-hop paths checked ({found})");
}

#[test]
fn call_cycles_match_tarjan() {
    let f = corpus_fx();
    let q = f.q();
    let mut s = Scratch::default();
    let n = f.ix.node_count();
    for overrides in [true, false] {
        // Reference graph: plain (src, dst) pairs of the followed edges.
        let g = Graph::build(n, || {
            f.calls
                .edges()
                .iter()
                .filter(move |e| e.conf != Confidence::Unresolved && (overrides || e.reason != Reason::Override))
                .map(|e| (e.src, e.dst))
        });
        let scc = Scc::of(&g);
        let mut cyclic = 0;
        let o = WalkOptions { overrides, ..WalkOptions::default() };
        for v in 0..n {
            let c = scc.comp(v);
            let hit = q.call_cycle(v, &o, &mut s);
            if !scc.is_cyclic(&g, c) {
                assert!(hit.is_none());
                continue;
            }
            cyclic += 1;
            let hit = hit.expect("cyclic");
            let mut want = scc.members(c).to_vec();
            want.sort_unstable();
            assert_eq!(hit.members, want);
            assert_eq!(hit.witness[0], v);
            for i in 0..hit.witness.len() {
                assert!(g.has_edge(hit.witness[i], hit.witness[(i + 1) % hit.witness.len()]));
            }
        }
        assert!(cyclic > 10, "{cyclic}");
    }
}

/// `Base.go` calls `Sub.go` for real at `Possible` (a local hint inside a lambda) and also
/// dispatches to it (override edge, `Likely`); `A.run` only dispatches to `B.run`.
const DISPATCH: &[(&str, &str)] = &[(
    "m.py",
    "class Base:\n\
     \x20   def go(self):\n\
     \x20       x = Sub()\n\
     \x20       (lambda: x.go())()\n\
     \x20       return 0\n\
     \n\
     class Sub(Base):\n\
     \x20   def go(self):\n\
     \x20       return 1\n\
     \n\
     class A:\n\
     \x20   def run(self):\n\
     \x20       return 0\n\
     \n\
     class B(A):\n\
     \x20   def run(self):\n\
     \x20       return 1\n",
)];

#[test]
fn parallel_override_edge_keeps_the_weaker_real_call() {
    let f = fx(DISPATCH);
    let q = f.q();
    let mut s = Scratch::default();
    let (base, sub) = (f.node("m:Base.go"), f.node("m:Sub.go"));
    let kinds: Vec<(Confidence, Reason)> =
        f.calls.edges().iter().filter(|e| (e.src, e.dst) == (base, sub)).map(|e| (e.conf, e.reason)).collect();
    assert_eq!(kinds.len(), 2, "the fixture has a real call and an override edge: {kinds:?}");
    assert!(kinds.contains(&(Confidence::Likely, Reason::Override)));
    assert!(kinds.iter().any(|&(c, r)| c == Confidence::Possible && r != Reason::Override));

    let find = |w: &Walk, v: NodeId| w.hops.iter().find(|h| h.node == v).map(|h| (h.conf, h.dispatch));
    let o = WalkOptions { scope: Scope::Node, ..WalkOptions::default() };
    let real = WalkOptions { overrides: false, ..o };
    // With override edges: the best edge is the dispatch.
    assert_eq!(find(&q.callees(base, &o, &mut s), sub), Some((Confidence::Likely, true)));
    assert_eq!(find(&q.callers(sub, &o, &mut s), base), Some((Confidence::Likely, true)));
    // Without: the weaker real call is still there.
    assert_eq!(find(&q.callees(base, &real, &mut s), sub), Some((Confidence::Possible, false)));
    assert_eq!(find(&q.callers(sub, &real, &mut s), base), Some((Confidence::Possible, false)));
    assert_eq!(find(&q.callees(base, &WalkOptions { min_conf: Confidence::Likely, ..real }, &mut s), sub), None);
    assert_eq!(find(&q.callees(base, &WalkOptions { min_conf: Confidence::Likely, ..o }, &mut s), sub), Some((Confidence::Likely, true)));
    // A pure dispatch is dropped without override edges.
    let (a, b) = (f.node("m:A.run"), f.node("m:B.run"));
    assert_eq!(find(&q.callees(a, &o, &mut s), b), Some((Confidence::Likely, true)));
    assert_eq!(find(&q.callees(a, &real, &mut s), b), None);
    // Direct neighbours report the best edge.
    assert!(q.direct_callees(base).any(|e| e == (sub, Confidence::Likely, true)));

    // Paths report the edge as followed.
    let po = PathOptions { scope: Scope::Node, ..PathOptions::default() };
    let p = q.path(base, sub, &po, &mut s).expect("dispatch path");
    assert_eq!((p.conf(0), p.dispatch(0)), (Some(Confidence::Likely), true));
    let p = q.path(base, sub, &PathOptions { overrides: false, ..po }, &mut s).expect("real path");
    assert_eq!((p.conf(0), p.dispatch(0)), (Some(Confidence::Possible), false));
    assert!(q.path(a, b, &PathOptions { overrides: false, ..po }, &mut s).is_none());

    // Most called counts real callers only.
    let sm = q.summary(&SummaryOptions::default());
    assert!(sm.most_called.contains(&Ranked { node: sub, value: 1 }), "{:?}", sm.most_called);
    assert!(sm.most_called.iter().all(|r| r.node != b), "{:?}", sm.most_called);
}

#[test]
fn call_labels_fold_per_kind() {
    use Confidence::*;
    let all = [Certain, Likely, Possible];
    for &x in &all {
        for &y in &all {
            for (rx, ry) in [(Reason::Override, Reason::Override), (Reason::Override, Reason::UniqueMethod), (Reason::UniqueMethod, Reason::Override), (Reason::UniqueMethod, Reason::UniqueMethod)] {
                let l = merge_labels(label(x, rx), label(y, ry));
                assert_eq!(l, merge_labels(label(y, ry), label(x, rx)), "commutative");
                let best = |want: Reason| [(x, rx), (y, ry)].iter().filter(|e| e.1 == want).map(|e| e.0).min();
                let (real, ovr) = (best(Reason::UniqueMethod), best(Reason::Override));
                assert_eq!(edge(l, false), real.map(|c| (c, false)));
                let want = match (real, ovr) {
                    (Some(r), Some(o)) if o < r => Some((o, true)),
                    (Some(r), _) => Some((r, false)),
                    (None, o) => o.map(|c| (c, true)),
                };
                assert_eq!(edge(l, true), want, "{x:?} {rx:?} {y:?} {ry:?}");
            }
        }
    }
}

#[test]
fn import_cycles_match_the_analysis() {
    let f = fx(&[
        ("p/__init__.py", "from . import a\n"),
        ("p/a.py", "from . import b\n"),
        ("p/b.py", "from . import c\nfrom . import a\n"),
        ("p/c.py", "from . import a\n"),
        ("p/d.py", "import p.e\n"),
        ("p/e.py", "import p.d\n"),
        ("p/f.py", "import p.a\n"),
    ]);
    let q = f.q();
    let mut s = Scratch::default();
    let all = crate::analysis::cycles::import_cycles(&f.ix, &f.imp, &CycleOptions::default());
    assert_eq!(all.len(), 2);
    for i in 0..all.len() {
        for &m in all.members(i) {
            let hits = q.import_cycles(m, &mut s);
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].members, all.members(i));
            assert!(hits[0].witness.contains(&m), "the witness goes through the target");
            let mg = &q.cycle_graph().0;
            let w = &hits[0].witness;
            for k in 0..w.len() {
                let (x, y) = (mg.local(w[k]).expect("x"), mg.local(w[(k + 1) % w.len()]).expect("y"));
                assert!(mg.graph().has_edge(x, y));
            }
        }
    }
    assert!(q.import_cycles(f.node("p.f"), &mut s).is_empty());
    // The package holds both cycles.
    assert_eq!(q.import_cycles(f.node("p"), &mut s).len(), 2);
}

#[test]
fn deps_match_brute_force() {
    let f = corpus_fx();
    let q = f.q();
    let mut s = Scratch::default();
    let module_of = |v: NodeId| Level::Module.apply(&f.ix, v);
    let mut checked = 0;
    let mut overrides = 0;
    let mut targets = crate::graph::module_nodes(&f.ix);
    // Classes too (override edges cross class boundaries).
    targets.extend((0..f.ix.node_count()).filter(|&v| f.ix.kind(v) == NodeKind::Class));
    let with_overrides = DepOptions { calls: CallFilter::default(), imports: false, ..DepOptions::default() };
    for (x, o) in targets.iter().flat_map(|&x| [(x, DepOptions::default()), (x, with_overrides)]) {
        let inside = |v: NodeId| f.ix.is_descendant(x, v);
        for dir in [Direction::Out, Direction::In] {
            // Call edges and import rows per representative.
            let mut want: HashMap<NodeId, (u32, u32)> = HashMap::new();
            for e in f.calls.edges() {
                if !o.calls.keeps(e.conf, e.reason) {
                    continue;
                }
                let (near, far) = if dir == Direction::Out { (e.src, e.dst) } else { (e.dst, e.src) };
                if inside(near) && !inside(far) {
                    want.entry(representative_ref(&f.ix, x, module_of(far), far)).or_default().0 += 1;
                }
            }
            if o.imports && dir == Direction::Out && f.ix.kind(x).is_def() {
                let file = f.ix.file(x).expect("def file");
                for row in f.ix.file_facts(file).imports.range() {
                    let near = f.ix.def_node(file, f.ix.facts().imports.scope[row]);
                    let far = f.imp.module_node(row);
                    if inside(near) && far != crate::index::NONE && !inside(far) {
                        want.entry(representative_ref(&f.ix, x, module_of(far), far)).or_default().1 += 1;
                    }
                }
            }
            for file in 0..f.ix.file_count() {
                if !o.imports || f.ix.kind(x).is_def() {
                    break;
                }
                let src = f.ix.file_node(file);
                for me in f.imp.module_edges(file) {
                    let (near, far) = if dir == Direction::Out { (src, me.dst) } else { (me.dst, src) };
                    if inside(near) && !inside(far) {
                        want.entry(representative_ref(&f.ix, x, module_of(far), far)).or_default().1 += me.rows;
                    }
                }
            }
            let got: HashMap<NodeId, (u32, u32)> = q.deps(x, dir, &o, &mut s).deps.iter().map(|d| (d.node, (d.calls, d.imports))).collect();
            assert_eq!(got, want, "{} {dir:?}", f.ix.qid_string(x));
            checked += got.len();
            overrides += f.calls.edges().iter().filter(|e| e.reason == Reason::Override && o.calls.keeps(e.conf, e.reason) && f.ix.is_descendant(x, e.src) && !f.ix.is_descendant(x, e.dst) && f.ix.file(e.dst) != f.ix.file(x)).count();
        }
    }
    assert!(checked > 10, "{checked}");
    assert!(overrides > 0, "cross-file override edges checked");
}

/// Reference of the representative rule: `m` unless it is an ancestor of `x`, then the
/// shallowest ancestor of `w` that is not.
fn representative_ref(ix: &Index, x: NodeId, m: NodeId, w: NodeId) -> NodeId {
    if !ix.is_descendant(m, x) {
        return m;
    }
    let mut chain = vec![w];
    let mut v = w;
    while let Some(p) = ix.parent(v) {
        chain.push(p);
        v = p;
    }
    chain.into_iter().rev().find(|&a| !ix.is_descendant(a, x)).unwrap_or(w)
}

#[test]
fn query_is_shareable_between_threads() {
    fn sync<T: Sync>(_: &T) {}
    let f = fx(PROJECT);
    let q = f.q();
    sync(&q);
    // Lazy tables initialised from several threads at once give the same answers.
    let leaf = f.node("pkg.util:leaf");
    let results: Vec<(usize, usize)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..4)
            .map(|_| {
                sc.spawn(|| {
                    let mut s = Scratch::default();
                    (q.lookup(b"leaf", &LookupOptions::default()).len(), q.impact(leaf, &WalkOptions::transitive(), &mut s).importers.len())
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().expect("thread")).collect()
    });
    assert!(results.windows(2).all(|w| w[0] == w[1]));
    assert_eq!(results[0], (2, 3));
}

#[test]
fn scratch_reuse_and_generation_wrap() {
    let f = fx(PROJECT);
    let q = f.q();
    let mut s = Scratch::default();
    let helper = f.node("pkg.util:helper");
    let want = q.callers(helper, &WalkOptions::transitive(), &mut s);
    // Force the stamp to wrap: marks are cleared, results unchanged.
    s.a.stamp = u32::MAX - 1;
    for _ in 0..4 {
        assert_eq!(q.callers(helper, &WalkOptions::transitive(), &mut s), want);
    }
    assert!(s.heap_bytes() > 0);
    // Callers and callees of every node with one scratch never see stale marks.
    let mut fresh = Scratch::default();
    for v in 0..f.ix.node_count() {
        let o = WalkOptions::transitive();
        assert_eq!(q.callers(v, &o, &mut s), q.callers(v, &o, &mut fresh));
        fresh = Scratch::default();
    }
}
