//! Hotspot tests: hand-checked fixtures plus a reference over the node hierarchy on the corpus.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::config::{Config, RuleSelector};
use crate::index::{Inputs, NodeKind, load};
use crate::walk::{WalkOptions, walk};

fn load_dir(dir: &Path, origin: Origin) -> Index {
    let w = walk(&[dir], &WalkOptions::everything());
    load(Inputs::from_root_dir(dir, &w.files, origin, b"proj"))
}

/// Loads `files` (relative path, source) as one root of `origin`.
fn fx_origin(files: &[(&str, &str)], origin: Origin) -> Index {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pymapper-hotspot-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    for (rel, src) in files {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, src).expect("write");
    }
    let ix = load_dir(&dir, origin);
    let _ = fs::remove_dir_all(&dir);
    ix
}

fn fx(files: &[(&str, &str)]) -> Index {
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

/// Findings rendered as `PM101 line:col qid args...`, in emission order.
fn render(ix: &Index, out: &Findings) -> Vec<String> {
    out.iter()
        .map(|(f, args)| {
            let mut s = format!("{} {}:{}", f.rule, f.loc.line, f.loc.col);
            for a in args {
                match *a {
                    Arg::Symbol(v) => s += &format!(" {}", ix.qid_string(v.0)),
                    Arg::U32(n) => s += &format!(" {n}"),
                    Arg::F32(x) => s += &format!(" {x:.1}"),
                    other => s += &format!(" {other:?}"),
                }
            }
            assert_eq!(f.confidence, 100);
            assert_eq!(f.symbol, match args[0] {
                Arg::Symbol(v) => v,
                _ => panic!("first argument is not a symbol"),
            });
            s
        })
        .collect()
}

fn run(ix: &Index, cfg: &Config) -> (HotspotReport, Vec<String>) {
    let engine = RuleEngine::new(cfg);
    let mut out = Findings::new();
    let report = analyse(ix, &engine, &mut out);
    assert_eq!(report.stats.findings, out.len() as u64);
    let lines = render(ix, &out);
    (report, lines)
}

fn file_of(ix: &Index, suffix: &str) -> FileId {
    (0..ix.file_count())
        .find(|&f| ix.file_path(f).to_string_lossy().ends_with(suffix))
        .unwrap_or_else(|| panic!("no file {suffix}"))
}

fn node(ix: &Index, qid: &str) -> NodeId {
    (0..ix.node_count()).find(|&v| ix.qid_string(v) == qid).unwrap_or_else(|| panic!("no node {qid}"))
}

const BRANCHY: &str = "\
def f(a, b):
    if a:
        for x in b:
            if x:
                return 1
    elif b:
        pass
    return 0


def g():
    return 1
";

#[test]
fn function_thresholds_report_strictly_above() {
    let ix = fx(&[("m.py", BRANCHY)]);
    let mut cfg = config();
    // f: McCabe 1 + if + for + if + elif = 5; cognitive 1 + 2 + 3 + 1 = 7; nesting 3; 8 lines.
    cfg.thresholds.max_mccabe = 4;
    cfg.thresholds.max_cognitive = 6;
    cfg.thresholds.max_nesting = 2;
    cfg.thresholds.max_function_loc = 7;
    cfg.thresholds.max_params = 1;
    let (_, got) = run(&ix, &cfg);
    assert_eq!(got, [
        "PM101 1:5 m:f 5 4",
        "PM102 1:5 m:f 7 6",
        "PM103 1:5 m:f 3 2",
        "PM104 1:5 m:f 8 7",
        "PM105 1:5 m:f 2 1",
    ]);
    // At the limit: nothing.
    cfg.thresholds.max_mccabe = 5;
    cfg.thresholds.max_cognitive = 7;
    cfg.thresholds.max_nesting = 3;
    cfg.thresholds.max_function_loc = 8;
    cfg.thresholds.max_params = 2;
    let (report, got) = run(&ix, &cfg);
    assert!(got.is_empty(), "{got:?}");
    assert_eq!(report.stats.functions, 2);
    assert_eq!(report.stats.flagged, 0);
}

#[test]
fn defaults_report_nothing_on_simple_code() {
    let ix = fx(&[("m.py", BRANCHY)]);
    let (report, got) = run(&ix, &config());
    assert!(got.is_empty(), "{got:?}");
    let m = report.modules.get(file_of(&ix, "m.py"));
    assert_eq!(m.functions, 2);
    assert_eq!(m.cc_sum, 6);
    assert_eq!(m.cc_max, 5);
    assert_eq!(m.cog_sum, 7);
    assert_eq!(m.cog_max, 7);
    assert_eq!(m.nesting_max, 3);
    assert_eq!(m.worst, Some(node(&ix, "m:f")));
    assert!(m.mi_min > 0.0 && m.mi_min < 100.0, "{}", m.mi_min);
}

#[test]
fn method_parameters_exclude_self_and_cls() {
    let src = "\
class C:
    def m(self, a, b): pass
    @staticmethod
    def s(a, b): pass
    @classmethod
    def c(cls, a): pass
    def v(*args): pass
    def k(self, *, a, b): pass
    def p(self, /, a): pass

def top(self, a): pass

def outer():
    def inner(self, a): pass
";
    let ix = fx(&[("m.py", src)]);
    let mut cfg = config();
    cfg.thresholds.max_params = 1;
    let (_, got) = run(&ix, &cfg);
    assert_eq!(got, [
        "PM105 2:9 m:C.m 2 1",
        "PM105 4:9 m:C.s 2 1",
        "PM105 8:9 m:C.k 2 1",
        "PM105 11:5 m:top 2 1",
        "PM105 14:9 m:outer.inner 2 1",
    ]);
    assert_eq!(top_functions(&ix, Metric::Params, 3, |_| true).iter().map(|&(v, n)| (ix.qid_string(v), n)).collect::<Vec<_>>(), [
        ("m:C.m".to_owned(), 2),
        ("m:C.s".to_owned(), 2),
        ("m:C.k".to_owned(), 2),
    ]);
}

const COMPLEX: &str = "\
from typing import overload

@overload
def h(a: int, b: int, c: int) -> int: ...
@overload
def h(a: str, b: str, c: str) -> str: ...
def h(a, b, c):
    if a:
        return 1
    if b:
        return 2
    return c
";

#[test]
fn stubs_overloads_and_third_party_are_not_reported() {
    let mut cfg = config();
    cfg.thresholds.max_mccabe = 2;
    cfg.thresholds.max_params = 2;
    cfg.thresholds.max_module_loc = 3;
    let stub = "def h(a, b, c) -> int: ...\n\n\n\n";
    let ix = fx(&[("m.py", COMPLEX), ("s.pyi", stub)]);
    let (report, got) = run(&ix, &cfg);
    // Only the implementation of `h`, which takes the unsuffixed id (the overloads are `h#2`
    // and `h#3`).
    assert_eq!(got, ["PM101 7:5 m:h 3 2", "PM105 7:5 m:h 3 2", "PM107 1:1 m 12 3"]);
    assert_eq!(report.stats.functions, 2, "overloads are not counted");
    // Aggregates still cover the stub.
    assert_eq!(report.modules.get(file_of(&ix, "s.pyi")).functions, 1);
    assert_eq!(report.modules.get(file_of(&ix, "s.pyi")).flagged, 1);

    let ix = fx_origin(&[("m.py", COMPLEX)], Origin::ThirdParty);
    let (report, got) = run(&ix, &cfg);
    assert!(got.is_empty(), "{got:?}");
    assert_eq!(report.modules.get(0).flagged, 1);
    assert_eq!(report.stats.flagged, 1);
}

#[test]
fn rule_selection_controls_findings_not_aggregates() {
    let ix = fx(&[("m.py", BRANCHY)]);
    let mut cfg = config();
    cfg.thresholds.max_mccabe = 1;
    cfg.thresholds.max_cognitive = 1;
    cfg.rules.ignore = vec![sel("PM102")];
    let (with, got) = run(&ix, &cfg);
    assert_eq!(got, ["PM101 1:5 m:f 5 1"]);

    cfg.rules.ignore = vec![sel("PM1")];
    let (without, got) = run(&ix, &cfg);
    assert!(got.is_empty());
    assert_eq!(with.modules, without.modules);
    assert_eq!(without.modules.get(0).flagged, 1);
    assert_eq!(aggregates(&ix, &cfg.thresholds), without);

    // PM106 is off by default and reports below the limit.
    cfg.rules.ignore.clear();
    cfg.thresholds = Thresholds::default();
    cfg.thresholds.min_maintainability = 100.0;
    let (_, got) = run(&ix, &cfg);
    assert!(got.is_empty());
    cfg.rules.extend_select = vec![sel("PM106")];
    let (_, got) = run(&ix, &cfg);
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got[0].starts_with("PM106 1:5 m:f ") && got[0].ends_with(" 100.0"), "{got:?}");
    assert!(got[1].starts_with("PM106 11:5 m:g "), "{got:?}");
}

#[test]
fn sums_do_not_count_nested_functions_twice() {
    let src = "\
def outer(a):
    if a:
        pass
    def inner(b):
        if b:
            pass
        class K:
            def deep(self, c):
                while c:
                    pass
    return inner

class C:
    def m(self, x):
        for y in x:
            pass
        return [z for z in x if z]
";
    let ix = fx(&[("m.py", src)]);
    let mx = ix.metrics();
    let cc = |q: &str| mx.cyclomatic[ix.metrics_row(node(&ix, q)).expect("row")];
    let cog = |q: &str| mx.cognitive[ix.metrics_row(node(&ix, q)).expect("row")];
    let (report, _) = run(&ix, &config());
    let m = report.modules.get(0);
    assert_eq!(m.functions, 4);
    assert_eq!(m.classes, 2);
    // `inner` and `K.deep` are already part of `outer`'s score.
    assert!(cc("m:outer") > cc("m:outer.inner"));
    assert_eq!(cc("m:C.m"), 2);
    assert_eq!(m.cc_sum, u32::from(cc("m:outer") + cc("m:C.m")));
    assert_eq!(m.cc_max, cc("m:outer"));
    assert_eq!(m.cog_sum, u32::from(cog("m:outer") + cog("m:C.m")));
    assert_eq!(m.worst, Some(node(&ix, "m:outer")));
}

#[test]
fn top_functions_rank_worst_first_with_stable_ties() {
    let src = "\
def a(x):
    if x: pass

def b(x):
    if x: pass
    if x: pass

def c(x):
    if x: pass

def d(): pass
";
    let ix = fx(&[("m.py", src)]);
    let q = |metric, k| {
        top_functions(&ix, metric, k, |_| true).iter().map(|&(v, n)| (ix.qid_string(v), n)).collect::<Vec<_>>()
    };
    let s = |x: &str, n: u32| (x.to_owned(), n);
    assert_eq!(q(Metric::Mccabe, 3), [s("m:b", 3), s("m:a", 2), s("m:c", 2)]);
    assert_eq!(q(Metric::Mccabe, 10).len(), 4);
    assert!(q(Metric::Mccabe, 0).is_empty());
    assert_eq!(q(Metric::Loc, 1), [s("m:b", 3)]);
    // Lowest maintainability first; values are MI * 100.
    let mi = q(Metric::Maintainability, 4);
    assert_eq!(mi[0].0, "m:b");
    assert_eq!(mi[3].0, "m:d");
    assert!(mi.windows(2).all(|w| w[0].1 <= w[1].1), "{mi:?}");
    let row = ix.metrics_row(node(&ix, "m:d")).expect("row");
    assert_eq!(mi[3].1, (ix.metrics().mi(row) * 100.0) as u32);
    // The filter applies per file.
    assert!(top_functions(&ix, Metric::Mccabe, 3, |_| false).is_empty());
}

/// Reference: the aggregates from the node hierarchy (kinds, parents, metrics rows) instead of
/// the def rows, on every file of the corpus.
#[test]
fn corpus_aggregates_match_node_reference() {
    for sub in ["fixtures", "sample_python"] {
        let ix = load_dir(&corpus(sub), Origin::FirstParty);
        let mut cfg = config();
        cfg.thresholds.max_mccabe = 3;
        cfg.thresholds.max_cognitive = 3;
        cfg.thresholds.max_params = 3;
        let (report, got) = run(&ix, &cfg);
        assert_eq!(report.modules.len(), ix.file_count() as usize);
        let mx = ix.metrics();
        let flags = &ix.facts().defs.flags;
        let mut total_flagged = 0u64;
        for f in 0..ix.file_count() {
            let v0 = ix.file_node(f);
            if v0 == NONE {
                continue;
            }
            let (mut functions, mut classes, mut cc_sum, mut cc_max, mut flagged) = (0, 0, 0u32, 0u16, 0);
            for v in v0 + 1..v0 + ix.file_facts(f).defs.len {
                let m = ix.metrics_row(v).expect("metrics row");
                let row = ix.def_row(v).expect("def row");
                match ix.kind(v) {
                    NodeKind::Class => classes += 1,
                    NodeKind::Function if flags[row] & def_flags::OVERLOAD == 0 => {
                        functions += 1;
                        let mut nested = false;
                        let mut p = ix.parent(v).expect("parent");
                        while p != v0 {
                            nested |= ix.kind(p) == NodeKind::Function;
                            p = ix.parent(p).expect("parent");
                        }
                        if !nested {
                            cc_sum += u32::from(mx.cyclomatic[m]);
                        }
                        cc_max = cc_max.max(mx.cyclomatic[m]);
                        let np = effective_params(&ix, f, row, flags[row]);
                        let over = mx.cyclomatic[m] > 3
                            || mx.cognitive[m] > 3
                            || np > 3
                            || mx.max_nesting[m] > 4
                            || mx.loc[m] > 100;
                        flagged += u32::from(over);
                    }
                    _ => {}
                }
            }
            let r = report.modules.get(f);
            let got_row = (r.functions, r.classes, r.cc_sum, r.cc_max, r.flagged);
            assert_eq!(got_row, (functions, classes, cc_sum, cc_max, flagged), "{}", ix.file_path(f).display());
            total_flagged += u64::from(flagged);
        }
        assert_eq!(report.stats.flagged, total_flagged);
        assert!(!got.is_empty(), "{sub}: low thresholds find something");
    }
}

// ----- docstrings (PM104 / PM107 lengths) -----

/// `doc_lines` per definition row of `src` (def_enter order) and the file's total.
fn doc_lines(src: &str) -> (Vec<u32>, u32) {
    use crate::extract::{MetricsScratch, MetricsTable, MetricsVisitor, parse};
    let mut scratch = MetricsScratch::new();
    let mut table = MetricsTable::new();
    let v = MetricsVisitor::new(src.as_bytes(), &mut scratch, &mut table);
    let total = parse(src.as_bytes(), v).file_doc_lines();
    (table.doc_lines.clone(), total)
}

#[test]
fn docstrings_are_detected() {
    let cases: &[(&str, &[u32], u32)] = &[
        // Multi-line triple-quoted.
        ("def f():\n    \"\"\"Doc.\n\n    More.\n    \"\"\"\n    return 1\n", &[4], 4),
        // Single line, single quotes.
        ("def f():\n    'doc'\n    return 1\n", &[1], 1),
        // Raw and unicode prefixes.
        ("def f():\n    r\"\"\"a\\d\n    b\"\"\"\n    return 1\n", &[2], 2),
        ("def f():\n    u'doc'\n    return 1\n", &[1], 1),
        // Implicit concatenation on one line.
        ("def f():\n    'a' 'b'\n    return 1\n", &[1], 1),
        // `;` ends the docstring statement.
        ("def f():\n    'doc'; x = 1\n    return x\n", &[1], 1),
        // A body that is only a docstring.
        ("def f():\n    \"\"\"Only\n    a docstring.\"\"\"\n", &[2], 2),
        // No docstring.
        ("def f():\n    return 1\n", &[0], 0),
        // Bytes and f-strings are not docstrings.
        ("def f():\n    b'doc'\n    return 1\n", &[0], 0),
        ("def f():\n    f'doc {x}'\n    return 1\n", &[0], 0),
        // A string expression that is not a statement on its own.
        ("def f():\n    '-'.join(x)\n    return 1\n", &[0], 0),
        ("def f():\n    'a %s' % x\n", &[0], 0),
        // A string statement after the first statement is not a docstring.
        ("def f():\n    x = 1\n    \"\"\"Not a\n    docstring.\"\"\"\n    return x\n", &[0], 0),
        // On the header's line: not counted (the line holds code).
        ("def f(): 'doc'\n", &[0], 0),
        // Classes, nested definitions (counted into their parents) and the module docstring.
        (
            "\"\"\"Module\ndoc.\"\"\"\nclass C:\n    'Class doc.'\n    def m(self):\n        \"\"\"M\n        doc.\"\"\"\n        def inner():\n            'i'\n        return inner\n",
            &[4, 3, 1],
            6,
        ),
        // A comment before the docstring does not matter; decorators neither.
        ("@dec\ndef f(a,\n      b):\n    # comment\n    'doc'\n    return a\n", &[1], 1),
        // A module whose first statement is code has no docstring.
        ("import os\n'not a docstring'\n", &[], 0),
    ];
    for (src, rows, total) in cases {
        let (got, t) = doc_lines(src);
        assert_eq!((got.as_slice(), t), (*rows, *total), "{src:?}");
    }
}

/// A function of 1 (def) + 12 (docstring) + 3 (code) lines.
const DOCUMENTED: &str = "\
\"\"\"Module docstring
on two lines.\"\"\"


def f(a):
    \"\"\"Summary.

    Parameters
    ----------
    a : int
        The input.

    Returns
    -------
    int
    \"\"\"
    b = a + 1
    c = b * 2
    return c
";

#[test]
fn function_and_module_length_exclude_docstrings() {
    let ix = fx(&[("m.py", DOCUMENTED)]);
    let mut cfg = config();
    // f spans 15 lines, 11 of them docstring: 4 lines of code.
    cfg.thresholds.max_function_loc = 3;
    cfg.thresholds.max_module_loc = 5;
    let (_, got) = run(&ix, &cfg);
    // The file has 19 lines, 13 of them docstrings: 6 lines.
    assert_eq!(got, ["PM104 5:5 m:f 4 3", "PM107 1:1 m 6 5"]);
    cfg.thresholds.max_module_loc = 6;
    cfg.thresholds.max_function_loc = 4;
    let (_, got) = run(&ix, &cfg);
    assert!(got.is_empty(), "{got:?}");
    assert_eq!(top_functions(&ix, Metric::Loc, 1, |_| true)[0].1, 4);
}

// ----- test code -----

const LONG_PARAMS: &str = "def f(a, b, c):\n    return a\n";

#[test]
fn test_code_is_skipped_unless_included() {
    let files = [
        ("app/core.py", LONG_PARAMS),
        ("app/latest.py", LONG_PARAMS),
        ("tests/test_core.py", LONG_PARAMS),
        ("tests/helpers.py", LONG_PARAMS),
        ("app/sub/tests/deep/util.py", LONG_PARAMS),
        ("app/testing/fixtures.py", LONG_PARAMS),
        ("app/core_test.py", LONG_PARAMS),
        ("conftest.py", LONG_PARAMS),
        ("app/conftest.py", LONG_PARAMS),
    ];
    let ix = fx(&files);
    let mut cfg = config();
    cfg.thresholds.max_params = 2;
    let (report, got) = run(&ix, &cfg);
    assert_eq!(got, ["PM105 1:5 app.core:f 3 2", "PM105 1:5 app.latest:f 3 2"]);
    // Aggregates still cover test files.
    assert_eq!(report.stats.functions, files.len() as u64);
    cfg.analysis.include_tests_in_metrics = true;
    let (_, got) = run(&ix, &cfg);
    assert_eq!(got.len(), files.len(), "{got:?}");
}
