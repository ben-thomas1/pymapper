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

/// Real-world determinism and invariants: the stdlib (`PYMAPPER_STDLIB`, else the default path
/// of docs/design.md) loaded on 1 and on many threads must give identical indexes. Slow in
/// debug builds: `cargo test --release -p pymapper-core stdlib_ -- --ignored`.
#[test]
#[ignore]
fn stdlib_deterministic_and_consistent() {
    let root = std::env::var_os("PYMAPPER_STDLIB").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from("/Users/bt386671/.local/share/uv/python/cpython-3.13.5-macos-aarch64-none/lib/python3.13")
    });
    if !root.is_dir() {
        eprintln!("skipping: {} not found", root.display());
        return;
    }
    let many = load_dir_threads(&root, 14);
    check_invariants(&many);
    // Spot checks when the root is a stdlib (the variable may point at any tree).
    if many.module_node(b"json.decoder", 0).is_some() {
        assert!(many.find(b"json.decoder:JSONDecoder.decode").is_some());
        assert!(many.find(b"collections:OrderedDict").is_some());
    }
    assert_eq!(dump(&load_dir_threads(&root, 1)), dump(&many));
}

#[test]
fn module_names_from_root_dir() {
    let t = TempDir::new("names");
    t.file("__init__.py", "")
        .file("a.py", "")
        .file("a.pyi", "")
        .file("my-script.py", "def main():\n    pass\n")
        .file("pkg/__init__.pyi", "")
        .file("pkg/x.y/z.py", "")
        .file("README.txt", "");
    let w = walk(&[t.path()], &WalkOptions::everything());
    let inp = Inputs::from_root_dir(t.path(), &w.files, Origin::FirstParty, b"proj");
    let got: Vec<(String, String, FileStatus)> = (0..inp.len() as u32)
        .map(|f| {
            let rel = inp.path(f).strip_prefix(t.path()).expect("under root").display().to_string();
            (rel, String::from_utf8_lossy(inp.module(f)).into_owned(), inp.status(f))
        })
        .collect();
    let want = [
        ("__init__.py", "__init__", FileStatus::NotImportable),
        ("a.py", "a", FileStatus::StubShadowed),
        ("a.pyi", "a", FileStatus::Active),
        ("my-script.py", "my-script", FileStatus::NotImportable),
        ("pkg/__init__.pyi", "pkg", FileStatus::Active),
        ("pkg/x.y/z.py", "pkg.x.y.z", FileStatus::NotImportable),
    ];
    let want: Vec<(String, String, FileStatus)> = want.iter().map(|&(a, b, c)| (a.to_owned(), b.to_owned(), c)).collect();
    assert_eq!(got, want);

    let ix = load(inp);
    check_invariants(&ix);
    let all: Vec<String> = (0..ix.node_count()).map(|v| qid(&ix, v)).collect();
    // The implementation of `a` comes first, its stub is `a#2`. `x.y` is not an identifier,
    // so `pkg.x.y.z` gets namespace nodes `pkg.x` and `pkg.x.y`.
    assert_eq!(
        all,
        ["@proj", "__init__", "a", "a#2", "my-script", "my-script:main", "pkg", "pkg.x", "pkg.x.y", "pkg.x.y.z"]
    );
    assert_eq!(ix.kind(find(&ix, "pkg")), NodeKind::Package);
    assert_eq!(ix.file_status(ix.file(find(&ix, "a")).expect("file")), FileStatus::StubShadowed);
    assert!(ix.file_path(ix.file(find(&ix, "a#2")).expect("file")).ends_with("a.pyi"));
}

#[test]
fn duplicates_close_after_the_primary_subtree() {
    // Hand-built inputs: two files claim `pkg` (a package and a shadowed one), one claims
    // `pkg.m`; the duplicate must not end up inside the primary's subtree.
    let t = TempDir::new("dups");
    t.file("one/pkg/__init__.py", "def f():\n    pass\n")
        .file("one/pkg/m.py", "")
        .file("two/pkg/__init__.py", "def g():\n    pass\n");
    let mut inp = Inputs::new();
    let g = inp.add_group(b"proj", Origin::FirstParty, None);
    let p = |rel: &str| t.path().join(rel).into_os_string().into_encoded_bytes();
    inp.push(&p("two/pkg/__init__.py"), g, b"pkg", FileStatus::Shadowed).expect("push");
    inp.push(&p("one/pkg/m.py"), g, b"pkg.m", FileStatus::Active).expect("push");
    inp.push(&p("one/pkg/__init__.py"), g, b"pkg", FileStatus::Active).expect("push");
    let ix = load(inp);
    check_invariants(&ix);
    let all: Vec<String> = (0..ix.node_count()).map(|v| qid(&ix, v)).collect();
    assert_eq!(all, ["@proj", "pkg", "pkg:f", "pkg.m", "pkg#2", "pkg#2:g"]);
    assert_eq!(ix.file(find(&ix, "pkg")), Some(2));
    assert_eq!(ix.file(find(&ix, "pkg#2")), Some(0));
    assert_eq!(ix.parent(find(&ix, "pkg#2")), Some(0));
    assert!(!ix.is_descendant(find(&ix, "pkg"), find(&ix, "pkg#2")));
}

#[test]
fn several_duplicates_keep_rank_order() {
    // Four files named `m`: implementation, stub, shadowed, not importable (pushed in reverse).
    let t = TempDir::new("ranks");
    t.file("d/m.py", "def d():\n    pass\n")
        .file("c/m.py", "def c():\n    pass\n")
        .file("a/m.pyi", "def b():\n    pass\n")
        .file("a/m.py", "def a():\n    pass\n")
        .file("a/m/sub.py", "");
    let mut inp = Inputs::new();
    let g = inp.add_group(b"proj", Origin::FirstParty, None);
    let p = |rel: &str| t.path().join(rel).into_os_string().into_encoded_bytes();
    inp.push(&p("d/m.py"), g, b"m", FileStatus::NotImportable).expect("push");
    inp.push(&p("c/m.py"), g, b"m", FileStatus::Shadowed).expect("push");
    inp.push(&p("a/m.pyi"), g, b"m", FileStatus::Active).expect("push");
    inp.push(&p("a/m/sub.py"), g, b"m.sub", FileStatus::Shadowed).expect("push");
    inp.push(&p("a/m.py"), g, b"m", FileStatus::StubShadowed).expect("push");
    let ix = load(inp);
    check_invariants(&ix);
    let all: Vec<String> = (0..ix.node_count()).map(|v| qid(&ix, v)).collect();
    assert_eq!(all, ["@proj", "m", "m:a", "m.sub", "m#2", "m#2:b", "m#3", "m#3:c", "m#4", "m#4:d"]);
}

#[test]
fn groups_sorted_and_namespaces_deduplicated() {
    let t = TempDir::new("groups");
    t.file("a/google/protobuf/__init__.py", "class Message:\n    pass\n")
        .file("b/google/api/x.py", "")
        .file("s/os.py", "def getcwd():\n    pass\n")
        .file("p/app.py", "import os\n");
    let mut inp = Inputs::new();
    // Added out of order: groups sort by (origin, name).
    let protobuf = inp.add_group(b"protobuf", Origin::ThirdParty, None);
    let stdlib = inp.add_group(b"stdlib", Origin::Stdlib, None);
    let api = inp.add_group(b"googleapis", Origin::ThirdParty, None);
    let proj = inp.add_group(b"proj", Origin::FirstParty, None);
    let p = |rel: &str| t.path().join(rel).into_os_string().into_encoded_bytes();
    inp.push(&p("a/google/protobuf/__init__.py"), protobuf, b"google.protobuf", FileStatus::Active).expect("push");
    inp.push(&p("b/google/api/x.py"), api, b"google.api.x", FileStatus::Active).expect("push");
    inp.push(&p("s/os.py"), stdlib, b"os", FileStatus::Active).expect("push");
    inp.push(&p("p/app.py"), proj, b"app", FileStatus::Active).expect("push");
    let ix = load(inp);
    check_invariants(&ix);
    let all: Vec<String> = (0..ix.node_count()).map(|v| qid(&ix, v)).collect();
    assert_eq!(
        all,
        [
            "@proj",
            "app",
            "@googleapis",
            "google",
            "google.api",
            "google.api.x",
            "@protobuf",
            "google#2",
            "google.protobuf",
            "google.protobuf:Message",
            "@stdlib",
            "os",
            "os:getcwd",
        ]
    );
    assert_eq!(ix.group_node(stdlib), find(&ix, "@stdlib"));
    assert_eq!(ix.file_origin(2), Origin::Stdlib);
    assert_eq!(ix.groups()[ix.file_group(0) as usize].name.as_ref(), b"protobuf");
}

#[test]
fn unreadable_files_load_as_empty_modules() {
    let t = TempDir::new("unreadable");
    t.file("ok.py", "def f():\n    pass\n");
    let mut inp = Inputs::new();
    let g = inp.add_group(b"proj", Origin::FirstParty, None);
    let p = |rel: &str| t.path().join(rel).into_os_string().into_encoded_bytes();
    inp.push(&p("missing.py"), g, b"missing", FileStatus::Active).expect("push");
    inp.push(&p("ok.py"), g, b"ok", FileStatus::Active).expect("push");
    let ix = load(inp);
    check_invariants(&ix);
    assert_eq!(ix.read_errors(), &[ReadError { file: 0, kind: std::io::ErrorKind::NotFound }]);
    assert_eq!(ix.file_facts(0).defs.len, 1);
    let all: Vec<String> = (0..ix.node_count()).map(|v| qid(&ix, v)).collect();
    assert_eq!(all, ["@proj", "missing", "ok", "ok:f"]);
}

#[test]
fn empty_inputs() {
    let ix = load(Inputs::new());
    assert_eq!(ix.node_count(), 0);
    assert_eq!(ix.file_count(), 0);
    let mut inp = Inputs::new();
    inp.add_group(b"proj", Origin::FirstParty, None);
    let ix = load(inp);
    assert_eq!(ix.node_count(), 1);
    assert_eq!(ix.find(b"@proj"), Some(0));
    check_invariants(&ix);
}

#[test]
fn deep_nesting_and_many_duplicates() {
    let t = TempDir::new("deep");
    let mut src = String::new();
    let mut want = String::from("deep:");
    for d in 0..60 {
        src.push_str(&" ".repeat(d * 4));
        src.push_str(&format!("def f{d}():\n"));
        if d > 0 {
            want.push('.');
        }
        want.push_str(&format!("f{d}"));
    }
    src.push_str(&" ".repeat(60 * 4));
    src.push_str("pass\n");
    for _ in 0..12 {
        src.push_str("def dup():\n    pass\n");
    }
    t.file("deep.py", &src);
    let ix = load_dir(t.path());
    check_invariants(&ix);
    let last = find(&ix, &want);
    assert_eq!(ix.depth(last), 2 + 59);
    assert_eq!(ix.ancestor_at(last, 1), find(&ix, "deep"));
    assert_eq!(qid(&ix, find(&ix, "deep:dup#12")), "deep:dup#12");
}

/// A distribution in `site` owning `files` (written empty unless `path=contents`).
fn dist(t: &TempDir, site: &str, name: &str, files: &[&str]) {
    let info = format!("{site}/{}-1.0.dist-info", name.replace('-', "_"));
    t.file(&format!("{info}/METADATA"), &format!("Metadata-Version: 2.1\nName: {name}\nVersion: 1.0\n"));
    let mut record = String::new();
    for f in files {
        let (path, contents) = f.split_once('=').unwrap_or((f, ""));
        t.file(&format!("{site}/{path}"), contents);
        record.push_str(&format!("{path},sha256=x,1\n"));
    }
    t.file(&format!("{info}/RECORD"), &record);
}

#[test]
fn inputs_from_discovered_roots() {
    use crate::roots::{RootOptions, discover};
    const SITE: &str = "proj/.venv/lib/python3.12/site-packages";
    let t = TempDir::new("roots");
    let root = t.path().display().to_string();
    t.file("proj/pyproject.toml", "[project]\nname = \"Proj\"\n")
        .file("proj/src/proj/__init__.py", "")
        .file("proj/src/proj/core.py", "def run():\n    pass\n")
        .file("proj/tests/test_core.py", "")
        .file("proj/.venv/pyvenv.cfg", &format!("home = {root}/python/bin\nversion_info = 3.12.3\n"))
        .file(&format!("{SITE}/stray.py"), "");
    fs::create_dir_all(t.path().join("python/bin")).expect("mkdir");
    dist(&t, SITE, "protobuf", &["google/protobuf/__init__.py"]);
    dist(&t, SITE, "googleapis-common-protos", &["google/api/x.py", "google/api/x.pyi"]);
    let opts = RootOptions { include_deps: true, path_env: Some(Default::default()), ..RootOptions::default() };
    let roots = discover(&t.path().join("proj"), &opts);
    let w = walk(&[t.path().join("proj")], &WalkOptions::everything());
    let ix = load(Inputs::from_roots(&roots, &w.files));
    check_invariants(&ix);
    let all: Vec<String> = (0..ix.node_count()).map(|v| qid(&ix, v)).collect();
    assert_eq!(
        all,
        [
            "@Proj",
            "proj",
            "proj.core",
            "proj.core:run",
            "tests",
            "tests.test_core",
            "@googleapis-common-protos",
            "google",
            "google.api",
            "google.api.x",
            "google.api.x#2",
            "@protobuf",
            "google#2",
            "google.protobuf",
            "@unknown",
            "stray",
        ]
    );
    let x = ix.file(find(&ix, "google.api.x")).expect("file");
    assert_eq!(ix.file_status(x), FileStatus::StubShadowed);
    assert_eq!(ix.file_origin(x), Origin::ThirdParty);
    assert!(ix.groups()[ix.file_group(x) as usize].dist.is_some());
}

#[test]
fn overload_implementations_have_no_suffix() {
    let t = TempDir::new("overloads");
    t.file(
        "m.py",
        "from typing import overload\n\
         @overload\ndef f(a: int) -> int: ...\n\
         @overload\ndef f(a: str) -> str: ...\n\
         def f(a):\n    return a\n\
         class C:\n    @property\n    def x(self): ...\n    @x.setter\n    def x(self, v): ...\n\
         \x20   @overload\n    def g(self) -> int: ...\n    def g(self, a=0): ...\n    def g(self): ...\n",
    )
    .file("s.pyi", "from typing import overload\n@overload\ndef h(a: int) -> int: ...\n@overload\ndef h(a: str) -> str: ...\n");
    let ix = load_dir(t.path());
    let all: Vec<String> = (0..ix.node_count()).map(|v| qid(&ix, v)).collect();
    // `f`: the implementation is `f`, the overloads `f#2`, `f#3` (source order). Property
    // pairs keep source order. In `C.g` the implementation is the last non-overload def.
    // A stub with only overloads keeps source order.
    for q in ["m:f", "m:f#2", "m:f#3", "m:C.x", "m:C.x#2", "m:C.g", "m:C.g#2", "m:C.g#3", "s:h", "s:h#2"] {
        assert!(all.iter().any(|a| a == q), "{q} not in {all:?}");
    }
    let line = |q: &str| ix.facts().defs.name_line[ix.def_row(find(&ix, q)).expect("row")];
    assert_eq!([line("m:f#2"), line("m:f#3"), line("m:f")], [3, 5, 6]);
    assert_eq!([line("m:C.g#2"), line("m:C.g#3"), line("m:C.g")], [14, 15, 16]);
    assert_eq!([line("m:C.x"), line("m:C.x#2")], [10, 12]);
    check_invariants(&ix);
}
