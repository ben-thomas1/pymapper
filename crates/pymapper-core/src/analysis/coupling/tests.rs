//! Coupling tests: hand-checked fixtures plus a reference over the import graph on the corpus.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::config::{Config, RuleSelector};
use crate::graph::{CallFilter, call_graph, import_graph};
use crate::index::{Inputs, load};
use crate::resolve::{Attribution, resolve_calls, resolve_imports};
use crate::walk::{WalkOptions, walk};

struct Fx {
    ix: Index,
    imp: Imports,
    cg: Graph,
}

fn load_dir(dir: &Path, origin: Origin) -> Fx {
    let w = walk(&[dir], &WalkOptions::everything());
    let ix = load(Inputs::from_root_dir(dir, &w.files, origin, b"proj"));
    let imp = resolve_imports(&ix, &Attribution::new());
    let calls = resolve_calls(&ix, &imp);
    let cg = call_graph(&ix, &calls, CallFilter::default());
    Fx { ix, imp, cg }
}

/// Loads `files` (relative path, source) as one root of `origin` and resolves everything.
fn fx_origin(files: &[(&str, &str)], origin: Origin) -> Fx {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pymapper-coupling-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    for (rel, src) in files {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, src).expect("write");
    }
    let out = load_dir(&dir, origin);
    let _ = fs::remove_dir_all(&dir);
    out
}

fn fx(files: &[(&str, &str)]) -> Fx {
    fx_origin(files, Origin::FirstParty)
}

fn corpus(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus").join(sub)
}

fn config() -> Config {
    Config::new(Path::new("/p"))
}

fn sel(s: &str) -> RuleSelector {
    s.parse().expect("selector")
}

impl Fx {
    fn file(&self, suffix: &str) -> FileId {
        (0..self.ix.file_count())
            .find(|&f| self.ix.file_path(f).to_string_lossy().ends_with(suffix))
            .unwrap_or_else(|| panic!("no file {suffix}"))
    }

    fn node(&self, qid: &str) -> NodeId {
        (0..self.ix.node_count()).find(|&v| self.ix.qid_string(v) == qid).unwrap_or_else(|| panic!("no node {qid}"))
    }

    /// Runs the pass; findings rendered as `PM201 line:col qid args...`.
    fn run(&self, cfg: &Config, with_calls: bool) -> (CouplingReport, Vec<String>) {
        let engine = RuleEngine::new(cfg);
        let mut out = Findings::new();
        let report = analyse(&self.ix, &self.imp, with_calls.then_some(&self.cg), &engine, &mut out);
        assert_eq!(report.stats.findings, out.len() as u64);
        let lines = out
            .iter()
            .map(|(f, args)| {
                assert_eq!(f.confidence, 100);
                let mut s = format!("{} {}:{}", f.rule, f.loc.line, f.loc.col);
                for a in args {
                    match *a {
                        Arg::Symbol(v) => s += &format!(" {}", self.ix.qid_string(v.0)),
                        Arg::U32(n) => s += &format!(" {n}"),
                        Arg::F32(x) => s += &format!(" {x:.2}"),
                        other => s += &format!(" {other:?}"),
                    }
                }
                s
            })
            .collect();
        (report, lines)
    }

    /// `(Ca, Ce)` of the file ending in `suffix`.
    fn cc(&self, r: &CouplingReport, suffix: &str) -> (u32, u32) {
        let f = self.file(suffix);
        (r.modules.ca(f), r.modules.ce(f))
    }
}

const LAYERED: &[(&str, &str)] = &[
    ("pkg/__init__.py", ""),
    (
        "pkg/a.py",
        "\
import os
import requests
from pkg import b
from pkg.b import thing
from . import c
from .c import other
from typing import TYPE_CHECKING
if TYPE_CHECKING:
    from pkg import d
",
    ),
    ("pkg/b.py", "from pkg.c import other\nthing = 1\n"),
    ("pkg/c.py", "other = 2\n"),
    ("pkg/d.py", "def f():\n    from pkg import c\n"),
];

#[test]
fn afferent_and_efferent_coupling_count_distinct_modules() {
    let fx = fx(LAYERED);
    let (r, got) = fx.run(&config(), true);
    assert!(got.is_empty(), "{got:?}");
    // a -> b, c (os/requests are not loaded, d is type-only, repeats count once).
    assert_eq!(fx.cc(&r, "pkg/a.py"), (0, 2));
    assert_eq!(fx.cc(&r, "pkg/b.py"), (1, 1));
    // c <- a, b, d (a function-level import still couples).
    assert_eq!(fx.cc(&r, "pkg/c.py"), (3, 0));
    assert_eq!(fx.cc(&r, "pkg/d.py"), (0, 1));
    assert_eq!(fx.cc(&r, "pkg/__init__.py"), (0, 0));
    assert_eq!(r.stats.edges, 4);
    let m = &r.modules;
    assert_eq!(m.instability(fx.file("pkg/a.py")), Some(1.0));
    assert_eq!(m.instability(fx.file("pkg/b.py")), Some(0.5));
    assert_eq!(m.instability(fx.file("pkg/c.py")), Some(0.0));
    assert_eq!(m.instability(fx.file("pkg/__init__.py")), None);
    assert_eq!(m.distance(fx.file("pkg/__init__.py")), None);
    // No classes: A = 0, so D = |I - 1|.
    assert_eq!(m.distance(fx.file("pkg/c.py")), Some(1.0));
    assert_eq!(m.distance(fx.file("pkg/a.py")), Some(0.0));
    assert_eq!(m.symbols(fx.file("pkg/d.py")), 1);
}

#[test]
fn high_fan_out_reports_strictly_above() {
    let fx = fx(LAYERED);
    let mut cfg = config();
    cfg.thresholds.max_fan_out = 1;
    let (_, got) = fx.run(&cfg, false);
    assert_eq!(got, ["PM201 1:1 pkg.a 2 1"]);
    cfg.thresholds.max_fan_out = 2;
    assert!(fx.run(&cfg, false).1.is_empty());
    cfg.thresholds.max_fan_out = 0;
    cfg.rules.ignore = vec![sel("PM201")];
    let (r, got) = fx.run(&cfg, false);
    assert!(got.is_empty());
    assert_eq!(fx.cc(&r, "pkg/c.py"), (3, 0), "metrics do not depend on rule selection");
}

const SHAPES: &str = "\
import abc
from abc import ABC, ABCMeta, abstractmethod
from typing import Protocol, TypeVar
T = TypeVar('T')

class A(ABC): pass
class B(abc.ABC): pass
class P(Protocol[T]): pass
class M(metaclass=ABCMeta): pass
class W(metaclass=abc.ABCMeta): pass
class Abs:
    @abstractmethod
    def run(self): ...
class Concrete(A): pass
class Plain:
    def run(self):
        def helper():
            pass
class Keyword(meta=ABCMeta): pass
";

#[test]
fn abstractness_counts_abstract_classes() {
    let fx = fx(&[("shapes.py", SHAPES)]);
    let (r, _) = fx.run(&config(), true);
    let f = fx.file("shapes.py");
    // Abstract: A, B, P, M, W, Abs. Concrete: Concrete, Plain, Keyword.
    assert_eq!(r.modules.classes(f), 9);
    assert_eq!(r.modules.abstract_classes(f), 6);
    assert_eq!(r.modules.abstractness(f), 6.0 / 9.0);
    // Uncoupled: no instability, no distance.
    assert_eq!(r.modules.instability(f), None);
    // 9 classes + Abs.run + Plain.run + helper.
    assert_eq!(r.modules.symbols(f), 12);
}

#[test]
fn main_sequence_distance_is_opt_in_and_needs_coupling() {
    // `core` is concrete and imported by four modules: I = 0, A = 0, D = 1 (zone of pain).
    // `api` is abstract and imported by four modules: I = 0, A = 1, D = 0.
    let user = "from pkg import core, api\n";
    let files = [
        ("pkg/__init__.py", ""),
        ("pkg/core.py", "class Impl: pass\n"),
        ("pkg/api.py", "from abc import ABC\nclass Port(ABC): pass\n"),
        ("pkg/u1.py", user),
        ("pkg/u2.py", user),
        ("pkg/u3.py", user),
        ("pkg/u4.py", user),
    ];
    let fx = fx(&files);
    let mut cfg = config();
    cfg.thresholds.distance_min_coupling = 4;
    let (r, got) = fx.run(&cfg, true);
    assert!(got.is_empty(), "off by default: {got:?}");
    assert_eq!(r.modules.distance(fx.file("pkg/core.py")), Some(1.0));
    assert_eq!(r.modules.distance(fx.file("pkg/api.py")), Some(0.0));
    cfg.rules.extend_select = vec![sel("PM202")];
    let (_, got) = fx.run(&cfg, true);
    // The users have Ce = 2 < 4: not judged.
    assert_eq!(got, ["PM202 1:1 pkg.core 1.00 0.70 0.00 0.00"]);
    cfg.thresholds.distance_min_coupling = 5;
    assert!(fx.run(&cfg, true).1.is_empty());
    cfg.thresholds.distance_min_coupling = 1;
    cfg.thresholds.max_distance = 0.4;
    let (_, got) = fx.run(&cfg, true);
    // Users: I = 1, A = 0, D = 0.
    assert_eq!(got, ["PM202 1:1 pkg.core 1.00 0.40 0.00 0.00"]);
}

#[test]
fn god_module_needs_size_and_coupling() {
    let fx = fx(LAYERED);
    let mut cfg = config();
    cfg.thresholds.max_module_symbols = 0;
    cfg.thresholds.max_module_coupling = 2;
    let (_, got) = fx.run(&cfg, true);
    // d has one symbol but coupling 1; c has coupling 3 but no symbols.
    assert!(got.is_empty(), "{got:?}");
    // The same with a function in c.
    let files: Vec<(&str, &str)> = LAYERED
        .iter()
        .map(|&(p, s)| if p == "pkg/c.py" { (p, "other = 2\ndef h(): pass\n") } else { (p, s) })
        .collect();
    let fx = self::fx(&files);
    let (_, got) = fx.run(&cfg, true);
    assert_eq!(got, ["PM203 1:1 pkg.c 1 0 3 2"]);
    cfg.thresholds.max_module_coupling = 3;
    assert!(fx.run(&cfg, true).1.is_empty());
    cfg.thresholds.max_module_coupling = 2;
    cfg.thresholds.max_module_symbols = 1;
    assert!(fx.run(&cfg, true).1.is_empty());
}

const GOD: &[(&str, &str)] = &[
    (
        "lib.py",
        "\
class A:
    def go(self): pass
class B:
    def go(self): pass
class C:
    pass
def plain(): pass
",
    ),
    (
        "god.py",
        "\
from lib import A, B, C, plain

class God:
    def one(self, x):
        if x:
            A()
        elif x > 1:
            B().go()
        plain()
        self.two(x)

    def two(self, x):
        for y in x:
            C()
            A().go()
        return Inner()

    class Inner:
        def make(self):
            return Inner2()

class Inner2: pass

class Small:
    def go(self):
        A()
        B()
        C()
",
    ),
];

#[test]
fn god_class_needs_wmc_and_class_fan_out() {
    let fx = fx(GOD);
    let god = fx.node("god:God");
    let mut scratch = Vec::new();
    // A, B, C and Inner2 (from the nested class); not God, God.Inner, `plain`.
    assert_eq!(class_fan_out(&fx.ix, &fx.cg, god, &mut scratch), 4);
    assert_eq!(class_fan_out(&fx.ix, &fx.cg, fx.node("god:Small"), &mut scratch), 3);
    assert_eq!(enclosing_class(&fx.ix, fx.node("god:God.Inner.make")), Some(fx.node("god:God.Inner")));
    assert_eq!(enclosing_class(&fx.ix, fx.node("lib:plain")), None);
    assert_eq!(enclosing_class(&fx.ix, fx.node("god")), None);

    let wmc = u32::from(fx.ix.metrics().cyclomatic[fx.ix.metrics_row(god).expect("row")]);
    let small = u32::from(fx.ix.metrics().cyclomatic[fx.ix.metrics_row(fx.node("god:Small")).expect("row")]);
    assert!(wmc > small, "{wmc} {small}");
    let mut cfg = config();
    cfg.thresholds.max_class_wmc = small;
    cfg.thresholds.max_class_fan_out = 3;
    let (r, got) = fx.run(&cfg, true);
    assert_eq!(got, [format!("PM204 3:7 god:God {wmc} {small} 4 3")]);
    // Only classes above the WMC limit look at the call graph.
    assert_eq!(r.stats.class_candidates, 1);
    // Without a call graph the rule is skipped.
    assert!(fx.run(&cfg, false).1.is_empty());
    cfg.thresholds.max_class_fan_out = 4;
    assert!(fx.run(&cfg, true).1.is_empty());
    cfg.thresholds.max_class_fan_out = 3;
    cfg.thresholds.max_class_wmc = wmc;
    assert!(fx.run(&cfg, true).1.is_empty());
}

#[test]
fn function_fan_in_and_fan_out_ignore_recursion() {
    let src = "\
def leaf(): pass
def rec(n):
    if n:
        rec(n - 1)
    leaf()
    leaf()
def top():
    rec(3)
    leaf()
";
    let fx = fx(&[("m.py", src)]);
    let (leaf, rec, top) = (fx.node("m:leaf"), fx.node("m:rec"), fx.node("m:top"));
    assert_eq!((fan_in(&fx.cg, leaf), fan_out(&fx.cg, leaf)), (2, 0));
    assert_eq!((fan_in(&fx.cg, rec), fan_out(&fx.cg, rec)), (1, 1));
    assert_eq!((fan_in(&fx.cg, top), fan_out(&fx.cg, top)), (0, 2));
}

#[test]
fn scope_excludes_stub_sources_and_other_origins_from_findings() {
    let files = [
        ("pkg/__init__.py", ""),
        ("pkg/a.py", "from pkg import b, c\n"),
        ("pkg/a.pyi", "from pkg import b, c\n"),
        ("pkg/b.py", ""),
        ("pkg/c.py", ""),
    ];
    let mut cfg = config();
    cfg.thresholds.max_fan_out = 1;
    let fx = fx(&files);
    let (r, got) = fx.run(&cfg, true);
    assert_eq!(got, ["PM201 1:1 pkg.a 2 1"]);
    assert_eq!(fx.cc(&r, "pkg/a.pyi"), (0, 0));
    assert_eq!(fx.cc(&r, "pkg/b.py"), (1, 0));

    let fx = fx_origin(&files, Origin::ThirdParty);
    let (r, got) = fx.run(&cfg, true);
    assert!(got.is_empty(), "{got:?}");
    // Metrics within one distribution still count.
    assert_eq!(fx.cc(&r, "pkg/a.py"), (0, 2));
}

/// Reference: Ca / Ce from the module import graph without type-only edges (graph degrees),
/// on every file of the corpus; findings are deterministic.
#[test]
fn corpus_coupling_matches_import_graph() {
    let mut found = 0;
    let mut total_edges = 0;
    for sub in ["fixtures", "sample_python"] {
        let fx = load_dir(&corpus(sub), Origin::FirstParty);
        let ix = &fx.ix;
        let mg = import_graph(ix, &fx.imp, res_flags::TYPE_ONLY);
        let mut cfg = config();
        cfg.thresholds.max_fan_out = 1;
        cfg.thresholds.max_class_wmc = 5;
        cfg.thresholds.max_class_fan_out = 0;
        cfg.rules.extend_select = vec![sel("PM202")];
        cfg.thresholds.distance_min_coupling = 1;
        let (r, got) = fx.run(&cfg, true);
        let (r2, got2) = fx.run(&cfg, true);
        assert_eq!((&r, &got), (&r2, &got2));
        let is_file = |m: u32| ix.file(mg.node(m)).is_some_and(|g| !is_stub(ix, g));
        let mut edges = 0u64;
        for f in 0..ix.file_count() {
            let v = ix.file_node(f);
            let Some(m) = mg.local(v) else { continue };
            let path = ix.file_path(f).display().to_string();
            let ce = if is_stub(ix, f) {
                0
            } else {
                mg.graph().succ(m).iter().filter(|&&w| ix.file(mg.node(w)).is_some()).count() as u32
            };
            let ca = mg.graph().pred(m).iter().filter(|&&w| is_file(w)).count() as u32;
            assert_eq!((r.modules.ca(f), r.modules.ce(f)), (ca, ce), "{path}");
            edges += u64::from(ce);
        }
        assert_eq!(r.stats.edges, edges);
        found += got.len();
        total_edges += edges;
    }
    assert!(found > 0 && total_edges > 0, "low thresholds find something");
}

