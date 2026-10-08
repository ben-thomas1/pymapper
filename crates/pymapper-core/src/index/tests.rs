use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

use super::*;
use crate::roots::{FileStatus, Origin};
use crate::walk::{WalkOptions, walk};

/// A fresh temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, AtomicOrdering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-index-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, rel: &str, contents: &str) -> &TempDir {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, contents).expect("write");
        self
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn load_dir(root: &Path) -> Index {
    let w = walk(&[root], &WalkOptions::everything());
    load(Inputs::from_root_dir(root, &w.files, Origin::FirstParty, b"proj"))
}

fn load_dir_threads(root: &Path, threads: usize) -> Index {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().expect("pool");
    pool.install(|| load_dir(root))
}

fn qid(ix: &Index, v: NodeId) -> String {
    ix.qid_string(v)
}

/// Every node as `qid kind depth [v, end)`, then every file's facts with resolved strings.
/// Independent of `Sym` numbering, so equal dumps mean equal indexes.
fn dump(ix: &Index) -> String {
    let mut s = String::new();
    for v in 0..ix.node_count() {
        let r = ix.subtree(v);
        let _ = writeln!(s, "{v} {} {} d{} [{}, {})", qid(ix, v), ix.kind(v).as_str(), ix.depth(v), r.start, r.end);
    }
    let txt = |sym| String::from_utf8_lossy(ix.str(sym)).into_owned();
    let chain = |c: &[crate::intern::Sym]| c.iter().map(|&x| txt(x)).collect::<Vec<_>>().join(".");
    for f in 0..ix.file_count() {
        let fv = ix.file_view(f);
        let ff = ix.file_facts(f);
        let _ = writeln!(s, "file {f} {} {}", ix.file_path(f).display(), txt(ix.file_module(f)));
        for row in ff.imports.range() {
            let scope = ix.facts().imports.scope[row];
            let _ = writeln!(s, "  import {} {}", qid(ix, ix.def_node(f, scope)), chain(fv.import_module(row)));
        }
        for row in ff.calls.range() {
            let scope = ix.facts().calls.scope[row];
            let _ = writeln!(s, "  call {} {}", qid(ix, ix.def_node(f, scope)), chain(fv.call_chain(row)));
        }
        for row in ff.bindings.range() {
            let _ = writeln!(s, "  bind {}", chain(fv.binding_target(row)));
        }
        for row in ff.defs.range().skip(1) {
            let v = ix.def_node(f, (row - ff.defs.start as usize) as u32);
            let m = ix.metrics_row(v).expect("def has metrics");
            let _ = writeln!(s, "  metrics {} cc={} loc={}", qid(ix, v), ix.metrics().cyclomatic[m], ix.metrics().loc[m]);
        }
    }
    s
}

/// Checks the structural invariants of the hierarchy against a naive reference.
fn check_invariants(ix: &Index) {
    let n = ix.node_count();
    for v in 0..n {
        let r = ix.subtree(v);
        assert!(r.start < r.end && r.end <= n);
        match ix.parent(v) {
            None => assert_eq!(ix.depth(v), 0),
            Some(p) => {
                assert!(p < v);
                assert_eq!(ix.depth(v), ix.depth(p) + 1);
                assert!(r.end <= ix.subtree(p).end, "child subtree inside parent");
            }
        }
        // Naive ancestors by walking parents.
        let mut naive = vec![v];
        let mut cur = v;
        while let Some(p) = ix.parent(cur) {
            naive.push(p);
            cur = p;
        }
        naive.reverse();
        for (k, &a) in naive.iter().enumerate() {
            assert_eq!(ix.ancestor_at(v, k as u32), a, "ancestor_at({v}, {k})");
            assert!(ix.is_descendant(a, v));
        }
        // Children are exactly the nodes whose parent is v.
        let kids: Vec<NodeId> = ix.children(v).collect();
        let naive_kids: Vec<NodeId> = (v + 1..r.end).filter(|&c| ix.parent(c) == Some(v)).collect();
        assert_eq!(kids, naive_kids);
        // Qids round-trip.
        let mut q = Vec::new();
        ix.qid(v, &mut q);
        assert_eq!(ix.find(&q), Some(v), "find({})", String::from_utf8_lossy(&q));
    }
    for f in 0..ix.file_count() {
        let m = ix.file_node(f);
        assert_eq!(ix.file(m), Some(f));
        assert_eq!(ix.def_row(m), Some(ix.file_facts(f).defs.start as usize));
        assert_eq!(ix.subtree(m).start, m);
    }
}

fn find(ix: &Index, q: &str) -> NodeId {
    ix.find(q.as_bytes()).unwrap_or_else(|| panic!("no node {q}"))
}

fn sample_tree(tag: &str) -> TempDir {
    let t = TempDir::new(tag);
    t.file("pkg/__init__.py", "def helper():\n    pass\n")
        .file(
            "pkg/mod.py",
            "import os\nclass Cls:\n    def run(self):\n        os.path.join('a')\n    def run(self, x):\n        helper()\n",
        )
        .file("pkg/sub/deep.py", "def f():\n    def inner():\n        pass\n    return inner()\n")
        .file("tests/test_x.py", "from pkg.mod import Cls\ndef test_it():\n    Cls().run()\n")
        .file("top.py", "x = 1\n");
    t
}

#[test]
fn hierarchy_and_qids() {
    let t = sample_tree("basic");
    let ix = load_dir(t.path());
    check_invariants(&ix);
    let all: Vec<String> = (0..ix.node_count()).map(|v| qid(&ix, v)).collect();
    assert_eq!(
        all,
        [
            "@proj",
            "pkg",
            "pkg:helper",
            "pkg.mod",
            "pkg.mod:Cls",
            "pkg.mod:Cls.run",
            "pkg.mod:Cls.run#2",
            "pkg.sub",
            "pkg.sub.deep",
            "pkg.sub.deep:f",
            "pkg.sub.deep:f.inner",
            "tests",
            "tests.test_x",
            "tests.test_x:test_it",
            "top",
        ]
    );
    let kinds: Vec<&str> = (0..ix.node_count()).map(|v| ix.kind(v).as_str()).collect();
    assert_eq!(
        kinds,
        [
            "distribution", "package", "function", "module", "class", "function", "function", "package", "module",
            "function", "function", "package", "module", "function", "module",
        ]
    );
    // Namespace packages have no file; the rest do.
    assert_eq!(ix.file(find(&ix, "pkg.sub")), None);
    assert_eq!(ix.file(find(&ix, "tests")), None);
    assert!(ix.file(find(&ix, "pkg")).is_some());
    let pkg = find(&ix, "pkg");
    assert_eq!(ix.subtree(pkg), 1..11);
    let kids: Vec<String> = ix.children(pkg).map(|c| qid(&ix, c)).collect();
    assert_eq!(kids, ["pkg:helper", "pkg.mod", "pkg.sub"]);
    let inner = find(&ix, "pkg.sub.deep:f.inner");
    assert_eq!(ix.depth(inner), 5);
    assert_eq!(ix.ancestor_at(inner, 1), pkg);
    assert_eq!(ix.ancestor_at(inner, 3), find(&ix, "pkg.sub.deep"));
    assert!(ix.is_descendant(pkg, inner));
    assert!(!ix.is_descendant(find(&ix, "tests"), inner));
    assert_eq!(ix.name(inner), b"inner");
    assert_eq!(ix.dup(find(&ix, "pkg.mod:Cls.run#2")), 1);
    assert_eq!(ix.module_node(b"pkg.mod", 0), Some(find(&ix, "pkg.mod")));
}

#[test]
fn scopes_map_to_nodes() {
    let t = sample_tree("scopes");
    let ix = load_dir(t.path());
    let d = dump(&ix);
    assert!(d.contains("  call pkg.mod:Cls.run os.path.join\n"), "{d}");
    assert!(d.contains("  call pkg.mod:Cls.run#2 helper\n"), "{d}");
    assert!(d.contains("  import tests.test_x pkg.mod\n"), "{d}");
    assert!(d.contains("  call tests.test_x:test_it Cls\n"), "{d}");
    assert!(d.contains("  metrics pkg.sub.deep:f cc=2 loc=4\n"), "{d}");
    // Def rows line up with the defs table.
    let run = find(&ix, "pkg.mod:Cls.run#2");
    let row = ix.def_row(run).expect("row");
    assert_eq!(ix.facts().defs.name[row].map(|s| ix.str(s).to_vec()), Some(b"run".to_vec()));
}

#[test]
fn find_rejects_unknown_and_malformed() {
    let t = sample_tree("find");
    let ix = load_dir(t.path());
    for q in ["", "nope", "pkg.mod:Nope", "pkg.mod:Cls.run#3", "pkg.mod:Cls.run#1", "pkg#2", "pkg.mod:", "@nope", ":x", "pkg.mod:Cls..run"] {
        assert_eq!(ix.find(q.as_bytes()), None, "{q}");
    }
    // Def segments never match packages/modules.
    assert_eq!(ix.find(b"pkg:mod"), None);
}

#[test]
fn deterministic_across_thread_counts() {
    let t = sample_tree("det");
    for i in 0..40 {
        t.file(&format!("gen/m{i:02}.py"), &format!("import os\ndef f{i}():\n    os.f{i}()\nclass C{i}:\n    def m(self):\n        self.x{i}()\n"));
    }
    let base = dump(&load_dir_threads(t.path(), 1));
    for threads in [2, 3, 8] {
        for _ in 0..3 {
            assert_eq!(dump(&load_dir_threads(t.path(), threads)), base, "{threads} threads");
        }
    }
}

#[test]
fn corpus_invariants_and_determinism() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
    let ix = load_dir_threads(&root, 4);
    assert!(ix.file_count() > 10);
    assert!(ix.read_errors().is_empty());
    check_invariants(&ix);
    assert_eq!(dump(&load_dir_threads(&root, 1)), dump(&ix));
    let s = ix.stats();
    assert_eq!(s.files, u64::from(ix.file_count()));
    assert!(s.bytes > 0 && s.lines > 0);
    assert!(ix.heap_bytes() > 0);
}
