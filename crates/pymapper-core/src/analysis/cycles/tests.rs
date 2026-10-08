//! Import cycles: witnesses, exclusions, findings. Also the fixture shared with the layers
//! tests.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::config::Contract;
use crate::graph::Graph;
use crate::index::{Inputs, load};
use crate::output::{Context, IndexContext};
use crate::resolve::{Attribution, resolve_calls, resolve_imports};
use crate::walk::{WalkOptions, walk};

pub(crate) struct Fx {
    pub ix: Index,
    pub imp: Imports,
    pub root: PathBuf,
}

/// Loads `files` (relative path, source) as one first-party root and resolves imports.
pub(crate) fn fx(files: &[(&str, &str)]) -> Fx {
    fx_with(files, Origin::FirstParty)
}

pub(crate) fn fx_with(files: &[(&str, &str)], origin: Origin) -> Fx {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pymapper-analysis-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    for (rel, src) in files {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, src).expect("write");
    }
    let w = walk(&[dir.as_path()], &WalkOptions::everything());
    let ix = load(Inputs::from_root_dir(&dir, &w.files, origin, b"proj"));
    let _ = fs::remove_dir_all(&dir);
    let imp = resolve_imports(&ix, &Attribution::new());
    Fx { ix, imp, root: dir }
}

impl Fx {
    pub fn qid(&self, v: NodeId) -> String {
        self.ix.qid_string(v)
    }

    /// Findings as `path:line:col: PMxxx message`, in buffer order.
    pub fn render(&self, out: &Findings, contracts: &[Contract]) -> Vec<String> {
        let cx = IndexContext::new(&self.ix, &self.root, contracts);
        let mut lines = Vec::new();
        for (f, _) in out.iter() {
            let mut path = Vec::new();
            cx.write_path(f.loc.file, &mut path);
            let mut msg = String::new();
            out.write_message(f, &cx, &mut msg).expect("format");
            lines.push(format!("{}:{}:{}: {} {msg}", String::from_utf8_lossy(&path), f.loc.line, f.loc.col, f.rule));
        }
        lines
    }

    fn cycles(&self, opts: &CycleOptions) -> (Cycles, Vec<String>) {
        let mut out = Findings::new();
        let c = analyse(&self.ix, &self.imp, opts, &mut out);
        let lines = self.render(&out, &[]);
        (c, lines)
    }
}

/// Every edge `u -> next` of a local witness exists and the witness stays in one component.
fn check_witness(g: &Graph, lc: &LocalCycles) {
    let scc = Scc::of(g);
    for i in 0..lc.member_start.len() - 1 {
        let w = &lc.witness[lc.witness_start[i] as usize..lc.witness_start[i + 1] as usize];
        assert!(!w.is_empty());
        let c = scc.comp(w[0]);
        for k in 0..w.len() {
            let (a, b) = (w[k], w[(k + 1) % w.len()]);
            assert!(g.has_edge(a, b), "missing witness edge {a} -> {b}");
            assert_eq!(scc.comp(a), c);
        }
        // A witness visits each module once.
        let mut sorted = w.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), w.len());
    }
}

fn witnesses(lc: &LocalCycles) -> Vec<Vec<u32>> {
    (0..lc.member_start.len() - 1)
        .map(|i| lc.witness[lc.witness_start[i] as usize..lc.witness_start[i + 1] as usize].to_vec())
        .collect()
}

#[test]
fn acyclic_graph_has_no_cycles() {
    let g = Graph::from_edges(4, &[(0, 1), (1, 2), (0, 2), (2, 3)]);
    let lc = find_cycles(&g);
    assert!(lc.members.is_empty());
    assert_eq!(lc.member_start, vec![0]);
    assert_eq!(lc.witness_start, vec![0]);
}

#[test]
fn two_cycle_and_self_loop() {
    let g = Graph::from_edges(4, &[(0, 1), (1, 0), (2, 2), (3, 0)]);
    let lc = find_cycles(&g);
    check_witness(&g, &lc);
    let mut w = witnesses(&lc);
    w.sort();
    // Rotated to start at the node closing the cycle back to the lowest member.
    assert_eq!(w, vec![vec![1, 0], vec![2]]);
}

#[test]
fn witness_is_shortest_through_lowest_member() {
    // 0 -> 1 -> 2 -> 3 -> 0 (long way) and 0 -> 4 -> 0 (short way), all one component.
    let g = Graph::from_edges(5, &[(0, 1), (1, 2), (2, 3), (3, 0), (0, 4), (4, 0), (2, 0)]);
    let lc = find_cycles(&g);
    check_witness(&g, &lc);
    assert_eq!(lc.members, vec![0, 1, 2, 3, 4]);
    assert_eq!(witnesses(&lc), vec![vec![4, 0]]);
}

#[test]
fn several_components_and_reset_scratch() {
    // Components {0,1,2}, {3,4}, {5,6,7}; 2 -> 3 and 4 -> 5 join them as a DAG.
    let e = [(0, 1), (1, 2), (2, 0), (2, 3), (3, 4), (4, 3), (4, 5), (5, 6), (6, 7), (7, 5)];
    let g = Graph::from_edges(8, &e);
    let lc = find_cycles(&g);
    check_witness(&g, &lc);
    assert_eq!(lc.member_start.len(), 4);
    // Topological order: importers first.
    assert_eq!(witnesses(&lc), vec![vec![2, 0, 1], vec![4, 3], vec![7, 5, 6]]);
}

#[test]
fn long_chain_cycle_is_iterative() {
    let n = 200_000u32;
    let g = Graph::build(n, || (0..n).map(move |v| (v, (v + 1) % n)));
    let lc = find_cycles(&g);
    assert_eq!(lc.members.len(), n as usize);
    let w = witnesses(&lc);
    assert_eq!(w[0].len(), n as usize);
    assert_eq!(w[0][0], n - 1);
    assert_eq!(w[0][1], 0);
}

#[test]
fn import_cycle_finding_and_type_checking() {
    let f = fx(&[
        ("pkg/__init__.py", ""),
        ("pkg/a.py", "import os\nfrom pkg import b\n"),
        ("pkg/b.py", "from .c import thing\n"),
        ("pkg/c.py", "thing = 1\nfrom pkg.a import x\n"),
        ("pkg/d.py", "from typing import TYPE_CHECKING\nif TYPE_CHECKING:\n    from pkg.e import E\n"),
        ("pkg/e.py", "from pkg.d import D\nclass E: pass\n"),
    ]);
    let (c, lines) = f.cycles(&CycleOptions::default());
    assert_eq!(c.len(), 1);
    let members: Vec<String> = c.members(0).iter().map(|&v| f.qid(v)).collect();
    assert_eq!(members, ["pkg.a", "pkg.b", "pkg.c"]);
    assert_eq!(lines, ["pkg/c.py:2:19: PM301 import cycle: pkg.c -> pkg.a -> pkg.b -> pkg.c"]);

    // With type-only edges, d <-> e is a cycle too.
    let opts = CycleOptions { exclude: 0, ..CycleOptions::default() };
    let (c, lines) = f.cycles(&opts);
    assert_eq!(c.len(), 2);
    assert!(lines.contains(&"pkg/e.py:1:19: PM301 import cycle: pkg.e -> pkg.d -> pkg.e".to_owned()), "{lines:?}");
}

#[test]
fn config_toggle_keeps_type_only_edges() {
    let root = std::env::temp_dir();
    let mut cfg = Config::new(&root);
    assert_eq!(CycleOptions::from_config(&cfg).exclude, res_flags::TYPE_ONLY);
    cfg.analysis.type_checking_cycles = true;
    assert_eq!(CycleOptions::from_config(&cfg).exclude, 0);
}

#[test]
fn closing_import_prefers_runtime_row() {
    // b imports a twice: under TYPE_CHECKING first, then for real; the finding points at the
    // runtime import.
    let f = fx(&[
        ("a.py", "import b\n"),
        ("b.py", "from typing import TYPE_CHECKING\nif TYPE_CHECKING:\n    import a\nimport a\n"),
    ]);
    let (_, lines) = f.cycles(&CycleOptions::default());
    assert_eq!(lines, ["b.py:4:8: PM301 import cycle: b -> a -> b"]);
}

#[test]
fn lazy_imports_count_and_stubs_do_not() {
    let f = fx(&[
        ("a.py", "def f():\n    import b\n"),
        ("b.py", "import a\n"),
        // A stub cycle never runs.
        ("s.pyi", "import t\n"),
        ("t.py", "import s\n"),
    ]);
    let (c, lines) = f.cycles(&CycleOptions::default());
    assert_eq!(c.len(), 1, "{lines:?}");
    assert_eq!(lines, ["b.py:1:8: PM301 import cycle: b -> a -> b"]);
}

#[test]
fn third_party_files_are_skipped_by_default() {
    let files = [("x.py", "import y\n"), ("y.py", "import x\n")];
    let f = fx_with(&files, Origin::ThirdParty);
    assert!(f.cycles(&CycleOptions::default()).0.is_empty());
    let all = CycleOptions { first_party_only: false, ..CycleOptions::default() };
    assert_eq!(f.cycles(&all).0.len(), 1);
}

#[test]
fn package_init_cycles() {
    // `from . import sub` in an `__init__` and `from pkg import thing` back.
    let f = fx(&[
        ("pkg/__init__.py", "from . import sub\nthing = 1\n"),
        ("pkg/sub.py", "from pkg import thing\n"),
    ]);
    let (_, lines) = f.cycles(&CycleOptions::default());
    assert_eq!(lines, ["pkg/sub.py:1:17: PM301 import cycle: pkg.sub -> pkg -> pkg.sub"]);
}

#[test]
fn row_module_matches_graph_edges() {
    let f = fx(&[
        ("pkg/__init__.py", "from .m import f\n"),
        ("pkg/m.py", "def f(): pass\n"),
        ("pkg/n.py", "from pkg import m\nfrom pkg import f\nimport pkg.missing\nimport json\n"),
    ]);
    let ix = &f.ix;
    let n = (0..ix.file_count()).find(|&i| ix.file_path(i).ends_with("pkg/n.py")).expect("n.py");
    let got: Vec<String> = ix
        .file_facts(n)
        .imports
        .range()
        .map(|r| match row_module(&f.imp, r) {
            NONE => "-".to_owned(),
            v => f.qid(v),
        })
        .collect();
    // Submodule, the named package for a symbol, the deepest prefix, nothing for stdlib.
    assert_eq!(got, ["pkg.m", "pkg", "pkg", "-"]);
}

#[test]
fn call_cycles_between_modules() {
    let files = [
        ("a.py", "import b\ndef fa():\n    return b.fb()\n"),
        ("b.py", "def fb():\n    import a\n    return a.fa()\n"),
        ("c.py", "import a\ndef fc():\n    return a.fa()\n"),
    ];
    let f = fx(&files);
    let calls = resolve_calls(&f.ix, &f.imp);
    let c = call_cycles(&f.ix, &calls, CallFilter::default());
    assert_eq!(c.len(), 1);
    let members: Vec<String> = c.members(0).iter().map(|&v| f.qid(v)).collect();
    assert_eq!(members, ["a", "b"]);
    assert_eq!(c.witness(0).len(), 2);
}

#[test]
fn deterministic_across_runs() {
    let files = [
        ("p/__init__.py", ""),
        ("p/a.py", "from p import b, c\n"),
        ("p/b.py", "from p import c\n"),
        ("p/c.py", "from p import a\n"),
        ("q.py", "import r\n"),
        ("r.py", "import q\n"),
    ];
    let a = fx(&files).cycles(&CycleOptions::default()).1;
    let b = fx(&files).cycles(&CycleOptions::default()).1;
    assert_eq!(a, b);
    assert_eq!(a.len(), 2);
}
