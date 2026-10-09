use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::extract::facts::render;
use crate::index::{Inputs, NodeId};
use crate::resolve::{Attribution, ExtKind};
use crate::roots::Origin;
use crate::walk::{WalkOptions, walk};

/// A fresh temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-incr-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, contents).expect("write");
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Copies every `.py` / `.pyi` file below `from` into `to` (same relative paths).
fn copy_tree(from: &Path, to: &Path) -> usize {
    let w = walk(&[from], &WalkOptions::everything());
    let mut n = 0;
    for p in w.files.iter() {
        let rel = p.strip_prefix(from).expect("below the root");
        let dst = to.join(rel);
        fs::create_dir_all(dst.parent().expect("parent")).expect("mkdir");
        fs::copy(p, &dst).expect("copy");
        n += 1;
    }
    n
}

fn inputs(root: &Path) -> Inputs {
    let w = walk(&[root], &WalkOptions::everything());
    Inputs::from_root_dir(root, &w.files, Origin::FirstParty, b"proj")
}

fn full(root: &Path) -> Program {
    Program::load(inputs(root), Attribution::new(), true)
}

fn lossy(b: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(b)
}

/// Everything observable about a program, with strings resolved (never `Sym` values), one
/// fact per line.
fn dump(p: &Program) -> Vec<String> {
    let ix = p.index();
    let mut out = Vec::new();
    let mut q = Vec::new();
    for f in 0..ix.file_count() {
        let ff = ix.file_facts(f);
        let g = &ix.groups()[ix.file_group(f) as usize];
        out.push(format!(
            "file {f} {} group={}:{:?} status={:?} module={} node={} lines={} flags={} doc={} main={}",
            ix.file_path(f).display(),
            lossy(&g.name),
            g.origin,
            ix.file_status(f),
            lossy(ix.str(ix.file_module(f))),
            ix.file_node(f),
            ff.lines,
            ff.flags,
            ff.doc_lines,
            ff.main_line,
        ));
        for line in render(ix.file_view(f), |s| ix.str(s)).lines() {
            out.push(format!("  {line}"));
        }
        out.push(format!("  evidence {:?}", ix.file_view(f).evidence()));
        for d in 1..ff.defs.len {
            let v = ix.def_node(f, d);
            let m = ix.metrics_row(v).expect("defs have metrics");
            out.push(format!("  metrics {d} {:?}", ix.metrics().row(m)));
        }
    }
    for v in 0..ix.node_count() {
        q.clear();
        ix.qid(v, &mut q);
        let anc: Vec<NodeId> = (0..=ix.depth(v)).map(|k| ix.ancestor_at(v, k)).collect();
        out.push(format!(
            "node {v} {} {:?} dup={} parent={:?} sub={:?} file={:?} anc={anc:?}",
            lossy(&q),
            ix.kind(v),
            ix.dup(v),
            ix.parent(v),
            ix.subtree(v),
            ix.file(v),
        ));
        assert_eq!(ix.find(&q), Some(v), "qid {} finds its node", lossy(&q));
    }
    out.push(format!("errors {:?}", ix.read_errors()));
    dump_imports(p, &mut out);
    dump_calls(p, &mut out);
    out
}

fn dump_imports(p: &Program, out: &mut Vec<String>) {
    let ix = p.index();
    let imp = p.imports();
    let mut name = Vec::new();
    for row in 0..imp.len() {
        name.clear();
        if let Some(m) = imp.module(row) {
            imp.modules().dotted(ix, m, &mut name);
        }
        out.push(format!(
            "import {row} {:?} module={} target={:?} via={:?} flags={} mnode={} dist={:?} ext={:?}",
            imp.status(row),
            lossy(&name),
            imp.target(row),
            imp.via(row),
            imp.flags(row),
            imp.module_node(row),
            imp.dist(row),
            imp.ext_name(ix, row).map(lossy),
        ));
    }
    for f in 0..ix.file_count() {
        for e in imp.module_edges(f) {
            out.push(format!("medge {f} {e:?}"));
        }
        for e in imp.ext_edges(f) {
            let key = match e.kind {
                ExtKind::ThirdParty => e.key.to_string(),
                _ => crate::intern::Sym::from_raw(e.key).map_or(String::new(), |s| lossy(ix.str(s)).into_owned()),
            };
            out.push(format!("xedge {f} {:?} {key} rows={} flags={}", e.kind, e.rows, e.flags));
        }
    }
}

fn dump_calls(p: &Program, out: &mut Vec<String>) {
    let ix = p.index();
    let Some(c) = p.calls() else { return };
    for row in 0..c.len() {
        out.push(format!("call {row} {:?}", c.call(row)));
    }
    for f in 0..ix.file_count() {
        for e in c.file_edges(f) {
            out.push(format!("cedge {f} {e:?}"));
        }
    }
    let cl = c.classes();
    for k in 0..cl.len() {
        let mro: Vec<NodeId> = cl.mro(k).iter().map(|&m| cl.node(m)).collect();
        out.push(format!("class {} flags={} mro={mro:?}", cl.node(k), cl.flags(k)));
    }
}

/// Asserts two dumps are equal, showing the first difference.
fn assert_same(inc: &[String], full: &[String], what: &str) {
    if inc == full {
        return;
    }
    let i = inc.iter().zip(full).position(|(a, b)| a != b).unwrap_or(inc.len().min(full.len()));
    let mut msg = String::new();
    let _ = writeln!(msg, "{what}: incremental differs from full rebuild at line {i} ({} vs {} lines)", inc.len(), full.len());
    for k in i.saturating_sub(3)..(i + 3) {
        let _ = writeln!(msg, "  inc : {}", inc.get(k).map_or("<none>", String::as_str));
        let _ = writeln!(msg, "  full: {}", full.get(k).map_or("<none>", String::as_str));
    }
    panic!("{msg}");
}

/// xorshift64: deterministic pseudo-random numbers for the edit generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }

    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
}

/// Random edits of a source tree that keep the incremental program and a full rebuild side
/// by side.
struct Editor {
    root: PathBuf,
    rng: Rng,
    counter: u32,
}

/// Names the generated code uses: common method names (unique-method and subclass fallbacks),
/// builtins, and names the corpora define.
const NAMES: &[&str] = &[
    "run", "process", "validate", "save", "load", "helper", "get", "close", "main", "User", "Base",
    "Model", "Product", "format_name", "len", "__init__", "__call__", "connect", "handle",
];

impl Editor {
    fn new(root: &Path, seed: u64) -> Editor {
        Editor { root: root.to_path_buf(), rng: Rng(seed | 1), counter: 0 }
    }

    fn files(&self) -> Vec<PathBuf> {
        walk(&[&self.root], &WalkOptions::everything()).files.iter().map(Path::to_path_buf).collect()
    }

    /// Dotted module names of the tree (from the paths).
    fn modules(&self, files: &[PathBuf]) -> Vec<String> {
        let mut out = Vec::new();
        for p in files {
            let rel = p.strip_prefix(&self.root).expect("below the root").to_string_lossy().into_owned();
            let rel = rel.trim_end_matches(".pyi").trim_end_matches(".py").trim_end_matches("/__init__").replace('/', ".");
            out.push(rel);
        }
        out
    }

    fn name(&mut self) -> String {
        let n = self.rng.below(NAMES.len() + 4);
        match NAMES.get(n) {
            Some(s) => (*s).to_owned(),
            None => {
                self.counter += 1;
                format!("gen_{}", self.counter)
            }
        }
    }

    /// A snippet of Python to insert.
    fn snippet(&mut self, modules: &[String]) -> String {
        let a = self.name();
        let b = self.name();
        let m = self.rng.pick(modules).clone();
        match self.rng.below(12) {
            0 => format!("def {a}(x, *args):\n    return {b}(x)\n"),
            1 => format!("class {a}({b}):\n    def {b}(self):\n        return self.{a}()\n"),
            2 => format!("import {m}\n"),
            3 => format!("from {m} import {a}\n"),
            4 => format!("from {m} import *\n"),
            5 => format!("from . import {a}\n"),
            6 => format!("__all__ = ['{a}', '{b}']\n"),
            7 => format!("{a} = {b}()\n"),
            8 => format!("{a} = {m}.{b}\n"),
            9 => format!("if TYPE_CHECKING:\n    from {m} import {a} as {b}\n"),
            10 => format!(
                "class {a}:\n    def __init__(self):\n        self.x = {b}()\n    def {b}(self):\n        self.x.run()\n        super().{b}()\n"
            ),
            _ => format!("def {a}(:\n  {b}(\n"),
        }
    }

    /// Edits the text of one file.
    fn edit_text(&mut self, text: &[u8], modules: &[String]) -> Vec<u8> {
        let mut lines: Vec<Vec<u8>> = text.split_inclusive(|&c| c == b'\n').map(<[u8]>::to_vec).collect();
        let at = self.rng.below(lines.len() + 1);
        match self.rng.below(6) {
            0 | 1 => {
                let s = self.snippet(modules);
                lines.insert(at, s.into_bytes());
            }
            2 => {
                let at = at.min(lines.len());
                let n = (1 + self.rng.below(6)).min(lines.len() - at);
                lines.drain(at..at + n);
            }
            3 if at < lines.len() => {
                let n = (1 + self.rng.below(8)).min(lines.len() - at);
                let copy: Vec<Vec<u8>> = lines[at..at + n].to_vec();
                let to = self.rng.below(lines.len() + 1);
                for (k, l) in copy.into_iter().enumerate() {
                    lines.insert(to + k, l);
                }
            }
            4 => {
                // Indented code inside whatever block precedes it.
                let s = self.snippet(modules);
                let mut ind = String::new();
                for l in s.lines() {
                    ind.push_str("    ");
                    ind.push_str(l);
                    ind.push('\n');
                }
                lines.insert(at, ind.into_bytes());
            }
            _ => {
                // Rename one `def` / `class`.
                let defs: Vec<usize> = (0..lines.len())
                    .filter(|&i| {
                        let t = lines[i].trim_ascii_start();
                        t.starts_with(b"def ") || t.starts_with(b"class ")
                    })
                    .collect();
                if !defs.is_empty() {
                    let i = defs[self.rng.below(defs.len())];
                    let l = &lines[i];
                    let pos = l.windows(4).position(|w| w == b"def " || w == b"ass ").map_or(0, |p| p + 4);
                    let mut s = l[..pos].to_vec();
                    s.extend_from_slice(b"renamed_");
                    s.extend_from_slice(&l[pos..]);
                    lines[i] = s;
                }
            }
        }
        lines.concat()
    }

    /// One random step: 1-4 edits. Returns the paths written (changed or added).
    fn step(&mut self) -> Vec<PathBuf> {
        let mut changed = Vec::new();
        for _ in 0..1 + self.rng.below(4) {
            let files = self.files();
            let modules = self.modules(&files);
            let roll = self.rng.below(16);
            if files.is_empty() || roll == 0 {
                // A new module, package or stub.
                self.counter += 1;
                let dir = match files.get(self.rng.below(files.len())) {
                    Some(p) => p.parent().expect("parent").to_path_buf(),
                    None => self.root.clone(),
                };
                let name = match self.rng.below(3) {
                    0 => format!("new_{}.py", self.counter),
                    1 => format!("pkg_{}/__init__.py", self.counter),
                    _ => format!("new_{}.pyi", self.counter),
                };
                let p = dir.join(name);
                fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
                let text = self.snippet(&modules) + &self.snippet(&modules);
                fs::write(&p, text).expect("write");
                changed.push(p);
            } else if roll == 1 && files.len() > 1 {
                let p = self.rng.pick(&files).clone();
                fs::remove_file(&p).expect("remove");
                changed.retain(|c| c != &p);
            } else if roll == 2 {
                // A stub next to a module (stub shadowing changes statuses), or a rename.
                let p = self.rng.pick(&files).clone();
                let text = fs::read(&p).expect("read");
                let to = if p.extension().is_some_and(|e| e == "py") && self.rng.below(2) == 0 {
                    p.with_extension("pyi")
                } else {
                    self.counter += 1;
                    p.with_file_name(format!("moved_{}.py", self.counter))
                };
                if to.exists() {
                    continue;
                }
                fs::write(&to, &text).expect("write");
                if to.extension().is_some_and(|e| e == "py") {
                    fs::remove_file(&p).expect("remove");
                    changed.retain(|c| c != &p);
                }
                changed.push(to);
            } else {
                let p = self.rng.pick(&files).clone();
                let text = fs::read(&p).expect("read");
                let new = self.edit_text(&text, &modules);
                fs::write(&p, new).expect("write");
                changed.push(p);
            }
        }
        changed.sort();
        changed.dedup();
        changed
    }
}

/// Runs `steps` random steps on a copy of `src`, checking the incremental program against a
/// full rebuild after every step.
fn property(src: &Path, steps: usize, seed: u64, tag: &str) {
    let tmp = TempDir::new(tag);
    assert!(copy_tree(src, tmp.path()) > 0, "corpus {} is empty", src.display());
    let mut prog = full(tmp.path());
    assert_same(&dump(&prog), &dump(&full(tmp.path())), "initial load");
    let mut ed = Editor::new(tmp.path(), seed);
    for step in 0..steps {
        let changed = ed.step();
        let before = Oracle::take(&prog);
        let (diff, st) = prog.update(inputs(tmp.path()), &changed);
        if std::env::var_os("INCR_DEBUG").is_some() {
            eprintln!(
                "{tag} {step}: files={} re={} add={} rm={} nodes+{} -{} ~{} iedges={} cedges={} oedges={}",
                st.files, st.reextracted, st.added, st.removed, diff.added.len(), diff.removed.len(), diff.changed.len(),
                diff.import_edges.len(), diff.call_edges.len(), diff.override_edges.len()
            );
        }
        let reference = full(tmp.path());
        assert_same(&dump(&prog), &dump(&reference), &format!("{tag} step {step} (seed {seed}, changed {changed:?})"));
        check_diff(&diff, &prog);
        before.check(&diff, &prog);
    }
}

/// The program before an update, by qualified id: what the diff must report.
struct Oracle {
    qids: Vec<Vec<u8>>,
    keys: Vec<NodeKey>,
    edges: EdgeMap,
}

type EdgeMap = std::collections::BTreeMap<(u8, NodeId, NodeId), EdgeLabel>;

/// Every edge of `p` aggregated per (kind, src, dst): 0 = import, 1 = call, 2 = override.
fn edges_by_node(p: &Program) -> EdgeMap {
    let ix = p.index();
    let mut out = std::collections::BTreeMap::new();
    for f in 0..ix.file_count() {
        for e in p.imports().module_edges(f) {
            out.insert((0, ix.file_node(f), e.dst), EdgeLabel { count: e.rows, label: e.flags });
        }
    }
    if let Some(c) = p.calls() {
        for e in c.edges() {
            if e.conf == crate::resolve::Confidence::Unresolved {
                continue;
            }
            let k = if e.reason == crate::resolve::Reason::Override { 2 } else { 1 };
            let l = out.entry((k, e.src, e.dst)).or_insert(EdgeLabel { count: 0, label: u8::MAX });
            l.count += 1;
            l.label = l.label.min(e.conf as u8);
        }
    }
    out
}

/// (path, def part of the qid) of a node with a file.
type NodeKey = Option<(PathBuf, Vec<u8>)>;

/// Qualified ids and (path, def part) keys of every node.
fn node_keys(ix: &Index) -> (Vec<Vec<u8>>, Vec<NodeKey>) {
    let mut qids = Vec::new();
    let mut keys = Vec::new();
    for v in 0..ix.node_count() {
        let mut q = Vec::new();
        ix.qid(v, &mut q);
        let key = ix.file(v).map(|f| {
            let def = q.iter().position(|&c| c == b':').map_or(Vec::new(), |i| q[i + 1..].to_vec());
            (ix.file_path(f).to_path_buf(), def)
        });
        qids.push(q);
        keys.push(key);
    }
    (qids, keys)
}

impl Oracle {
    fn take(p: &Program) -> Oracle {
        let (qids, keys) = node_keys(p.index());
        let edges = edges_by_node(p);
        Oracle { qids, keys, edges }
    }

    /// Node correspondence (file identity, then qualified id) and edge deltas by qualified ids.
    fn check(&self, d: &IndexDiff, p: &Program) {
        let ix = p.index();
        assert_eq!(d.node_map.len(), self.qids.len());
        let (_, new_keys) = node_keys(ix);
        let paths: std::collections::HashSet<PathBuf> = (0..ix.file_count()).map(|f| ix.file_path(f).to_path_buf()).collect();
        let by_key: std::collections::HashMap<&(PathBuf, Vec<u8>), NodeId> =
            new_keys.iter().enumerate().filter_map(|(w, k)| k.as_ref().map(|k| (k, w as NodeId))).collect();
        let mut want = vec![NONE; self.qids.len()];
        let mut claimed = vec![false; ix.node_count() as usize];
        let mut by_id = Vec::new();
        for (v, key) in self.keys.iter().enumerate() {
            match key {
                Some(k) if paths.contains(&k.0) => {
                    if let Some(&w) = by_key.get(k) {
                        want[v] = w;
                        claimed[w as usize] = true;
                    }
                }
                _ => by_id.push(v),
            }
        }
        for v in by_id {
            if let Some(w) = ix.find(&self.qids[v])
                && !claimed[w as usize]
            {
                want[v] = w;
                claimed[w as usize] = true;
            }
        }
        for (v, q) in self.qids.iter().enumerate() {
            assert_eq!(d.node_map[v], want[v], "node map of old {v} {}", lossy(q));
        }
        for (i, &v) in d.removed.iter().enumerate() {
            assert_eq!(d.removed_qid(i), &self.qids[v as usize][..]);
        }
        let after = edges_by_node(p);
        let mut old = EdgeMap::new();
        for (&(k, a, b), &l) in &self.edges {
            let (a, b) = (want[a as usize], want[b as usize]);
            if a != NONE && b != NONE {
                old.insert((k, a, b), l);
            }
        }
        let mut want = Vec::new();
        for (key, l) in &old {
            match after.get(key) {
                Some(n) if n == l => {}
                n => want.push((*key, Some(*l), n.copied())),
            }
        }
        for (key, n) in &after {
            if !old.contains_key(key) {
                want.push((*key, None, Some(*n)));
            }
        }
        want.sort();
        let mut got = Vec::new();
        for (kind, list) in [(0u8, &d.import_edges), (1, &d.call_edges), (2, &d.override_edges)] {
            for e in list {
                got.push(((kind, e.src, e.dst), e.old, e.new));
            }
        }
        got.sort();
        assert_eq!(got, want, "edge deltas");
    }
}

/// Internal consistency of a diff against the updated program.
fn check_diff(d: &IndexDiff, p: &Program) {
    let ix = p.index();
    let mut seen = vec![false; ix.node_count() as usize];
    for &w in &d.node_map {
        if w != NONE {
            assert!(!seen[w as usize], "node map is injective");
            seen[w as usize] = true;
        }
    }
    for &w in &d.added {
        assert!(!seen[w as usize], "added nodes have no old node");
        seen[w as usize] = true;
    }
    assert!(seen.iter().all(|&s| s), "every new node is mapped or added");
    assert_eq!(d.removed.len(), d.node_map.iter().filter(|&&w| w == NONE).count());
    for (i, &v) in d.removed.iter().enumerate() {
        assert_eq!(d.node_map[v as usize], NONE);
        assert!(!d.removed_qid(i).is_empty(), "removed node {v} has its qid");
    }
    for e in d.import_edges.iter().chain(&d.call_edges).chain(&d.override_edges) {
        assert!(e.src < ix.node_count() && e.dst < ix.node_count());
        assert!(e.old.is_some() || e.new.is_some());
        assert_ne!(e.old, e.new);
    }
}

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

#[test]
fn random_edits_on_corpus_match_full_rebuild() {
    // `INCR_SEEDS=N` runs N more seeds (soak testing).
    let extra: u64 = std::env::var("INCR_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    for seed in [1u64, 7, 42, 1234].into_iter().chain(10_000..10_000 + extra) {
        property(&corpus(), 25, seed, "corpus");
    }
}

/// The stdlib of the benchmark interpreter (`PYMAPPER_STDLIB` overrides it).
fn stdlib() -> Option<PathBuf> {
    let p = match std::env::var_os("PYMAPPER_STDLIB") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/share/uv/python/cpython-3.13.5-macos-aarch64-none/lib/python3.13"),
    };
    p.is_dir().then_some(p)
}

/// Slow (a full stdlib rebuild per step): `cargo test --release -p pymapper-core --lib
/// incremental -- --ignored`.
#[test]
#[ignore]
fn random_edits_on_stdlib_match_full_rebuild() {
    let Some(dir) = stdlib() else {
        eprintln!("stdlib not found; skipped");
        return;
    };
    for seed in [3u64, 99] {
        property(&dir, 12, seed, "stdlib");
    }
}

fn node(p: &Program, qid: &str) -> NodeId {
    p.index().find(qid.as_bytes()).unwrap_or_else(|| panic!("no node {qid}"))
}

#[test]
fn diff_reports_nodes_and_edges() {
    let t = TempDir::new("diff");
    t.write("a.py", "def foo():\n    pass\n\ndef bar():\n    foo()\n\nclass C:\n    def m(self):\n        pass\n");
    let b = t.write("b.py", "from a import foo\nfoo()\n");
    let mut p = full(t.path());
    let (foo, bar, m) = (node(&p, "a:foo"), node(&p, "a:bar"), node(&p, "a:C.m"));
    let a = t.write("a.py", "def new():\n    pass\n\ndef foo():\n    pass\n\nclass C:\n    def m(self):\n        x = 1\n        return x\n");
    let (d, st) = p.update(inputs(t.path()), &[&a]);
    assert_eq!((st.reextracted, st.added, st.removed), (1, 0, 0));
    assert_eq!(d.node_map[foo as usize], node(&p, "a:foo"));
    assert_eq!(d.node_map[bar as usize], NONE);
    assert_eq!(d.removed, vec![bar]);
    assert_eq!(d.removed_qid(0), b"a:bar");
    assert_eq!(d.added, vec![node(&p, "a:new")]);
    assert!(d.changed.contains(&node(&p, "a:C.m")), "m's body changed");
    assert!(d.changed.contains(&node(&p, "a")), "the module node of a re-extracted file");
    assert!(!d.changed.contains(&node(&p, "a:foo")), "foo moved down but did not change: {:?}", d.changed);
    assert_eq!(d.moved, vec![node(&p, "a:foo")]);
    assert!(d.changed.contains(&node(&p, "a:C")), "C's span and LOC grew");
    let _ = m;
    assert_eq!(d.reextracted, vec![p.index().file(node(&p, "a")).expect("file")]);
    assert!(d.call_edges.is_empty() && d.import_edges.is_empty(), "{d:?}");

    // A new import and calls in b.
    t.write("b.py", "from a import foo, C\nfoo()\nC().m()\n");
    let (d, _) = p.update(inputs(t.path()), &[&b]);
    let (bm, c) = (node(&p, "b"), node(&p, "a:C"));
    assert!(d.call_edges.iter().any(|e| e.src == bm && e.dst == c && e.old.is_none() && e.new.is_some()), "{d:?}");
    assert!(d.import_edges.iter().all(|e| e.old.is_some() && e.new.is_some()), "rows of the a edge changed: {d:?}");
    assert_eq!(d.import_edges.len(), 1);
    assert_eq!(d.import_edges[0].new.map(|l| l.count), Some(2));

    // Nothing changed: an empty diff with the identity map.
    let (d, st) = p.update(inputs(t.path()), &[] as &[&Path]);
    assert!(d.is_empty());
    assert_eq!(st.reextracted, 0);
    assert!(d.node_map.iter().enumerate().all(|(v, &w)| w == v as NodeId));
}

#[test]
fn package_without_init_lives_on_as_namespace_package() {
    let t = TempDir::new("ns");
    let init = t.write("pkg/__init__.py", "X = 1\n");
    t.write("pkg/mod.py", "def f():\n    pass\n");
    let mut p = full(t.path());
    let (pkg, f) = (node(&p, "pkg"), node(&p, "pkg.mod:f"));
    fs::remove_file(&init).expect("remove");
    let (d, st) = p.update(inputs(t.path()), &[] as &[&Path]);
    assert_eq!(st.removed, 1);
    let ns = node(&p, "pkg");
    assert_eq!(p.index().file(ns), None, "namespace package");
    assert_eq!(d.node_map[pkg as usize], ns);
    assert!(d.changed.contains(&ns));
    assert_eq!(d.node_map[f as usize], node(&p, "pkg.mod:f"));
    assert_eq!(d.removed_files.len(), 1);
    assert_same(&dump(&p), &dump(&full(t.path())), "namespace package");
}

#[test]
fn unlisted_changes_keep_old_facts() {
    let t = TempDir::new("unlisted");
    t.write("a.py", "def f():\n    pass\n");
    let mut p = full(t.path());
    t.write("a.py", "def g():\n    pass\n");
    let (d, _) = p.update(inputs(t.path()), &[] as &[&Path]);
    assert!(d.is_empty());
    assert!(p.index().find(b"a:f").is_some());
}
