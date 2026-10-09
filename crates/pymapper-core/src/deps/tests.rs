//! Deps tests: scorecards, usage, the scorecard store and the report on a hand-checked
//! fixture of one first-party project and four third-party distributions.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::score::{WORDS, Scratch};
use super::*;
use crate::cache::Cache;
use crate::index::{Inputs, load};
use crate::resolve::{Attribution, Imports, resolve_calls, resolve_imports};
use crate::roots::{DistId, FileStatus};

const APP_MAIN: &str = "\
import liba
from liba.core import Engine, helper
from libb import thing


def run():
    e = Engine()
    e.start()
    helper(1)
    return thing
";

const LIBA_INIT: &str = "\
from .core import Engine

__all__ = [\"Engine\"]
";

const LIBA_CORE: &str = "\
from liba.util import deep


class Engine:
    def __init__(self):
        self.n = 0

    def start(self):
        return self._go()

    def _go(self):
        return deep()


def helper(x):
    return x


def unused_fn():
    a = 1
    return a
";

const LIBA_UTIL: &str = "\
import liba.core


def deep():
    return 1
";

const LIBA_PRIVATE: &str = "\
def hidden():
    pass
";

const LIBB_INIT: &str = "\
import os
import requests
import liba

thing = 1


def complex(a, b, c):
    if a:
        if b:
            if c:
                for x in a:
                    while b:
                        if x:
                            pass
                        elif c:
                            pass
                        elif b:
                            pass
                        elif a:
                            pass
                        else:
                            pass
    return 0
";

/// The fixture: groups `proj` (first-party, 0), `liba` (1, dist 0), `libb` (2, dist 1),
/// `libc` (3, dist 2, unused), `libd` (4, dist 3, stubs only).
struct Fx {
    _dir: TempDir,
    ix: Index,
    imp: Imports,
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-deps-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// (group, path, module, source)
const FILES: &[(u32, &str, &str, &str)] = &[
    (0, "proj/app/__init__.py", "app", ""),
    (0, "proj/app/main.py", "app.main", APP_MAIN),
    (1, "site/liba/__init__.py", "liba", LIBA_INIT),
    (1, "site/liba/core.py", "liba.core", LIBA_CORE),
    (1, "site/liba/util.py", "liba.util", LIBA_UTIL),
    (1, "site/liba/_private.py", "liba._private", LIBA_PRIVATE),
    (1, "site/liba/extra.pyi", "liba.extra", "def typed() -> int: ...\n"),
    (2, "site/libb/__init__.py", "libb", LIBB_INIT),
    (3, "site/libc/__init__.py", "libc", "def f():\n    pass\n"),
    (4, "site/libd/__init__.pyi", "libd", "def g() -> None: ...\n"),
];

fn fixture() -> Fx {
    let dir = TempDir::new("fx");
    let mut inp = Inputs::new();
    inp.add_group(b"proj", Origin::FirstParty, None);
    for (i, name) in ["liba", "libb", "libc", "libd"].iter().enumerate() {
        inp.add_group(name.as_bytes(), Origin::ThirdParty, Some(DistId(i as u32)));
    }
    for &(g, rel, module, src) in FILES {
        let p = dir.0.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, src).expect("write");
        inp.push(p.as_os_str().as_encoded_bytes(), g, module.as_bytes(), FileStatus::Active).expect("push");
    }
    let ix = load(inp);
    let imp = resolve_imports(&ix, &Attribution::new());
    Fx { _dir: dir, ix, imp }
}

impl Fx {
    fn usage_where(&self, in_scope: impl Fn(FileId) -> bool) -> Usage {
        let calls = resolve_calls(&self.ix, &self.imp);
        usage(&self.ix, &self.imp, &calls, in_scope, &UsageOptions::default())
    }

    fn score(&self, g: u32) -> Scorecard {
        let files = GroupFiles::new(&self.ix);
        let cx = ScoreCtx::new(&self.ix, &self.imp, &files);
        compute_scorecard(&cx, g, &mut Scratch::default())
    }
}

/// Lines of `src` as the extractor counts them.
fn lines(src: &str) -> u32 {
    src.lines().count() as u32
}

#[test]
fn group_files_bucket_every_file_once() {
    let fx = fixture();
    let gf = GroupFiles::new(&fx.ix);
    let sizes: Vec<usize> = (0..5).map(|g| gf.of(g).len()).collect();
    assert_eq!(sizes, [2, 5, 1, 1, 1]);
    for g in 0..5 {
        assert!(gf.of(g).windows(2).all(|w| w[0] < w[1]));
        assert!(gf.of(g).iter().all(|&f| fx.ix.file_group(f) == g));
    }
}

#[test]
fn usage_counts_imports_api_and_call_sites() {
    let fx = fixture();
    let u = fx.usage_where(|_| true);
    assert_eq!(u.first_party_files, 2);
    let a = u.group(1);
    // `import liba` (liba) + `from liba.core import Engine, helper` (two rows, liba.core).
    assert_eq!((a.importers, a.import_rows, a.modules_used), (1, 3, 2));
    // Engine and helper (imported), Engine.__init__ (constructor) and Engine.start (hint).
    assert_eq!(a.api_used, 4);
    // Engine() links the class and its __init__, plus e.start() and helper(1).
    assert_eq!(a.call_sites, 4);
    let b = u.group(2);
    // `thing` is a variable: imported, but no class or function of libb is used.
    assert_eq!((b.importers, b.import_rows, b.modules_used, b.api_used, b.call_sites), (1, 1, 1, 0, 0));
    assert!(a.is_used() && b.is_used());
    for g in [3, 4] {
        let c = u.group(g);
        assert!(!c.is_used());
        assert_eq!((c.importers, c.api_used, c.loc_reached), (0, 0, 0));
    }
    // First-party groups are not counted.
    assert_eq!(*u.group(0), DistUsage::default());
    assert!(u.unloaded().is_empty());
}

#[test]
fn loc_reached_follows_calls_and_imports_transitively() {
    let fx = fixture();
    let u = fx.usage_where(|_| true);
    let a = u.group(1);
    let total = lines(LIBA_INIT) + lines(LIBA_CORE) + lines(LIBA_UTIL) + lines(LIBA_PRIVATE) + 1;
    assert_eq!(a.loc, total);
    // Not reached: unused_fn (3 lines), the private module nobody imports, the stub. Reached
    // through two hops: Engine.start -> Engine._go -> liba.util:deep, and liba.util's module
    // code through `from liba.util import deep` in liba.core.
    assert_eq!(a.loc_reached, total - 3 - lines(LIBA_PRIVATE) - 1);
    // libb: only its module-level code runs (the import); `complex` is never called.
    let b = u.group(2);
    assert_eq!(b.loc, lines(LIBB_INIT));
    assert_eq!(b.loc_reached, 7);
    assert_eq!((u.group(3).loc, u.group(4).loc), (2, 1));
}

#[test]
fn usage_counts_only_first_party_files_in_scope() {
    let fx = fixture();
    let main = (0..fx.ix.file_count()).find(|&f| fx.ix.file_path(f).ends_with("app/main.py")).expect("main");
    let none = fx.usage_where(|f| f != main);
    assert_eq!(none.first_party_files, 1);
    for g in 1..5 {
        assert!(!none.group(g).is_used(), "group {g}");
        assert_eq!(none.group(g).loc_reached, 0, "group {g}");
    }
    let only = fx.usage_where(|f| f == main);
    assert_eq!(only.first_party_files, 1);
    assert_eq!(only.group(1), fx.usage_where(|_| true).group(1));
}

#[test]
fn usage_threshold_drops_less_sure_calls() {
    let fx = fixture();
    let calls = resolve_calls(&fx.ix, &fx.imp);
    let certain = usage(&fx.ix, &fx.imp, &calls, |_| true, &UsageOptions { threshold: crate::resolve::Confidence::Certain });
    let likely = usage(&fx.ix, &fx.imp, &calls, |_| true, &UsageOptions::default());
    // `e.start()` is resolved through a local construction hint (likely): dropped, and with it
    // everything only reachable through it (Engine._go, liba.util:deep).
    assert_eq!(certain.group(1).call_sites + 1, likely.group(1).call_sites);
    assert!(certain.group(1).loc_reached < likely.group(1).loc_reached);
}

#[test]
fn scorecard_of_a_library_with_an_import_cycle() {
    let fx = fixture();
    let sc = fx.score(1);
    assert_eq!((sc.files, sc.stub_files, sc.classes, sc.functions), (4, 1, 1, 7));
    assert_eq!(sc.loc, lines(LIBA_INIT) + lines(LIBA_CORE) + lines(LIBA_UTIL) + lines(LIBA_PRIVATE));
    // Engine, Engine.start, helper, unused_fn, deep. Not: __init__ (dunder), _go (private),
    // hidden (private module), typed (stub). Engine re-exported by liba/__init__ counts once.
    assert_eq!(sc.public_api, 5);
    // __init__ -> core, core -> util, util -> core; core and util form a cycle.
    assert_eq!((sc.internal_edges, sc.fan_out_max, sc.cyclic_modules, sc.external_deps), (3, 1, 2, 0));
    assert!((sc.fan_out_mean - 0.75).abs() < 1e-6);
    assert_eq!((sc.cc_max, sc.hotspots), (1, 0));
    assert!((sc.cc_mean - 1.0).abs() < 1e-6);
    assert!(sc.mi_mean > 70.0 && sc.mi_min <= sc.mi_mean);
    assert_eq!(sc.score, sc.compute_score());
    // Flawless but for the cycle: half the files cyclic costs the coupling sub-score half.
    assert!((sc.score - 90.0).abs() < 1e-3, "{}", sc.score);
    assert_eq!(sc.grade(), Some(Grade::A));
}

#[test]
fn scorecard_counts_hotspots_and_external_dependencies() {
    let fx = fixture();
    let sc = fx.score(2);
    assert_eq!((sc.functions, sc.cc_max, sc.cog_max, sc.nesting_max), (1, 10, 25, 6));
    // Cognitive 25 > 15: a hotspot; 1 per 24 code lines.
    assert_eq!(sc.hotspots, 1);
    assert!((sc.hotspot_density - 1000.0 / 24.0).abs() < 1e-3);
    // `requests` (unattributed) and liba (another distribution); `os` is the stdlib.
    assert_eq!(sc.external_deps, 2);
    assert_eq!(sc.grade(), Some(Grade::F));
}

#[test]
fn distributions_without_functions_have_no_grade() {
    let fx = fixture();
    let sc = fx.score(4);
    assert_eq!((sc.files, sc.stub_files, sc.functions, sc.loc), (0, 1, 0, 0));
    assert!(sc.score.is_nan());
    assert_eq!(sc.grade(), None);
    assert_eq!(fx.score(3).grade(), Some(Grade::A));
}

#[test]
fn grade_boundaries() {
    let cases = [(100.0, Grade::A), (85.0, Grade::A), (84.99, Grade::B), (70.0, Grade::B), (69.9, Grade::C)];
    for (s, g) in cases {
        assert_eq!(Grade::of(s), g, "{s}");
    }
    let more = [(55.0, Grade::C), (54.9, Grade::D), (40.0, Grade::D), (39.9, Grade::F), (0.0, Grade::F)];
    for (s, g) in more {
        assert_eq!(Grade::of(s), g, "{s}");
    }
}

#[test]
fn score_formula_extremes() {
    let worst = Scorecard {
        files: 4,
        functions: 10,
        hotspot_density: 50.0,
        mi_mean: 10.0,
        cc_mean: 12.0,
        cyclic_modules: 4,
        fan_out_mean: 9.0,
        ..Scorecard::default()
    };
    assert_eq!(worst.compute_score(), 0.0);
    let best = Scorecard { files: 4, functions: 10, mi_mean: 90.0, cc_mean: 1.0, ..Scorecard::default() };
    assert_eq!(best.compute_score(), 100.0);
    // One sub-score at a time.
    let hot = Scorecard { hotspot_density: 7.5, ..best };
    assert!((hot.compute_score() - 85.0).abs() < 1e-3);
    assert!(Scorecard::default().compute_score().is_nan());
}

#[test]
fn scorecard_words_round_trip() {
    let fx = fixture();
    for g in 1..5 {
        let sc = fx.score(g);
        let back = Scorecard::from_words(&sc.to_words());
        assert_eq!(back.to_words(), sc.to_words());
        assert_eq!(back.grade(), sc.grade());
    }
    assert_eq!(Scorecard::default().to_words().len(), WORDS);
}

/// Cache key of the fixture's groups: distributions 0..2 are cacheable, libd is not.
fn fx_key(g: &Group) -> Option<u64> {
    g.dist.filter(|d| d.0 < 3).map(|d| 1000 + u64::from(d.0))
}

#[test]
fn scorecards_are_computed_once_and_then_read_from_the_store() {
    let fx = fixture();
    let dir = TempDir::new("store");
    let cache = Cache::new(dir.0.join("cache"));
    let files = GroupFiles::new(&fx.ix);
    let cx = ScoreCtx::new(&fx.ix, &fx.imp, &files);

    let mut cold = ScoreStore::open(Some(&cache));
    let a = scorecards(&cx, fx_key, &mut cold);
    assert!(a[0].is_none(), "first-party code has no scorecard");
    for (g, sc) in a.iter().enumerate().skip(1) {
        assert_eq!(sc.map(|s| s.to_words()), Some(fx.score(g as u32).to_words()), "group {g}");
    }
    assert_eq!(cold.stats(), ScoreStats { cached: 0, computed: 4, written: false });
    cold.save(&cache).expect("save");
    assert!(cold.stats().written);
    assert!(cache.dir().join(SCORES_FILE).is_file());

    let mut warm = ScoreStore::open(Some(&cache));
    let b = scorecards(&cx, fx_key, &mut warm);
    let words = |v: &[Option<Scorecard>]| v.iter().map(|s| s.map(|s| s.to_words())).collect::<Vec<_>>();
    assert_eq!(words(&a), words(&b));
    // libd has no key: scored on every run.
    assert_eq!(warm.stats(), ScoreStats { cached: 3, computed: 1, written: false });
    // Nothing new and nothing unused: no rewrite.
    let before = fs::metadata(cache.dir().join(SCORES_FILE)).expect("stat").modified().expect("mtime");
    warm.save(&cache).expect("save");
    assert!(!warm.stats().written);
    assert_eq!(before, fs::metadata(cache.dir().join(SCORES_FILE)).expect("stat").modified().expect("mtime"));
}

#[test]
fn store_drops_unused_entries_and_ignores_corrupt_files() {
    let dir = TempDir::new("store2");
    let cache = Cache::new(dir.0.join("cache"));
    let sc = Scorecard { files: 3, functions: 2, score: 77.5, mi_mean: 50.0, ..Scorecard::default() };
    let mut st = ScoreStore::open(Some(&cache));
    st.put(Some(1), sc);
    st.put(Some(2), Scorecard { files: 9, ..sc });
    st.put(None, sc);
    assert_eq!(st.stats().computed, 3);
    st.save(&cache).expect("save");

    // Only key 1 is used by the next run: key 2 is dropped from the file.
    let mut st = ScoreStore::open(Some(&cache));
    assert_eq!(st.get(1).map(|s| s.to_words()), Some(sc.to_words()));
    st.save(&cache).expect("save");
    assert!(st.stats().written);
    let mut st = ScoreStore::open(Some(&cache));
    assert!(st.get(2).is_none());
    assert!(st.get(1).is_some());

    // get_or_compute: a hit does not call the closure.
    let got = st.get_or_compute(Some(1), || panic!("cached"));
    assert_eq!(got.files, 3);
    let got = st.get_or_compute(Some(5), || Scorecard { files: 5, ..sc });
    assert_eq!(got.files, 5);

    // A corrupt file is ignored.
    let path = cache.dir().join(SCORES_FILE);
    let mut bytes = fs::read(&path).expect("read");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&path, &bytes).expect("write");
    let mut st = ScoreStore::open(Some(&cache));
    assert!(st.get(1).is_none());
    fs::write(&path, b"garbage").expect("write");
    assert!(ScoreStore::open(Some(&cache)).get(1).is_none());
    // No cache at all.
    assert!(ScoreStore::open(None).get(1).is_none());
}

fn row<'a>(name: &'a str, version: &'a str, usage: DistUsage, score: Option<Scorecard>, loaded: bool) -> DepRow<'a> {
    DepRow { name: name.as_bytes(), key: name.as_bytes(), version: version.as_bytes(), loaded, usage, score }
}

fn render(report: &DepsReport<'_>, opts: ReportOptions) -> String {
    let mut out = Vec::new();
    assert_eq!(report.write(&opts, &mut out).expect("write"), crate::output::Outcome::Written);
    String::from_utf8(out).expect("utf-8")
}

fn sample_report() -> DepsReport<'static> {
    let used = DistUsage { importers: 3, import_rows: 5, modules_used: 2, api_used: 14, call_sites: 20, loc_reached: 1203, loc: 5301 };
    let sc = Scorecard {
        files: 17,
        loc: 5301,
        code_lines: 4000,
        functions: 300,
        public_api: 152,
        cc_mean: 3.12,
        hotspot_density: 1.25,
        mi_mean: 62.06,
        external_deps: 4,
        score: 78.25,
        ..Scorecard::default()
    };
    let idle = DistUsage { loc: 10, ..DistUsage::default() };
    let rows = vec![
        row("zeta", "1.0", idle, Some(Scorecard { functions: 0, score: f32::NAN, ..sc }), true),
        row("requests", "2.32.3", used, Some(sc), true),
        row("fastlib", "", DistUsage { importers: 1, import_rows: 1, ..DistUsage::default() }, None, false),
    ];
    DepsReport::new(rows, 128, Some(ScoreStats { cached: 2, computed: 1, written: true }))
}

#[test]
fn concise_report_lists_used_distributions_sorted() {
    let r = sample_report();
    assert_eq!(r.used(), 2);
    let text = render(&r, ReportOptions { score: false, all: false, json: false });
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("distribution  version  importers  api used  loc reached"), "{text}");
    assert!(lines[1].starts_with("fastlib") && lines[1].ends_with("- (code not loaded)"), "{text}");
    assert!(lines[2].starts_with("requests") && lines[2].ends_with("14  1203/5301 (23%)"), "{text}");
    assert_eq!(lines[3], "");
    assert_eq!(lines[4], "2 of 3 distributions used by first-party code (128 first-party files); --all lists every one.");
    assert_eq!(lines[5], "Scorecards: 1 computed, 2 cached.");
    assert_eq!(lines.len(), 6, "{text}");
}

#[test]
fn concise_report_with_scores_and_all() {
    let text = render(&sample_report(), ReportOptions { score: true, all: true, json: false });
    let lines: Vec<&str> = text.lines().collect();
    let header: Vec<&str> = lines[0].split_whitespace().collect();
    assert_eq!(header.len(), 14, "{text}");
    assert!(lines[0].starts_with("distribution  version  grade  score   loc  hot/kloc    mi   cc  deps  importers"), "{text}");
    let req: Vec<&str> = lines[2].split_whitespace().collect();
    assert_eq!(req, ["requests", "2.32.3", "B", "78.2", "5301", "1.2", "62.1", "3.1", "4", "3", "14/152", "1203/5301", "(23%)"], "{text}");
    let zeta: Vec<&str> = lines[3].split_whitespace().collect();
    assert_eq!(zeta, ["zeta", "1.0", "-", "-", "5301", "-", "-", "-", "4", "0", "0/152", "0/10", "(0%)"], "{text}");
    let fast: Vec<&str> = lines[1].split_whitespace().collect();
    assert_eq!(fast[..10], ["fastlib", "-", "-", "-", "-", "-", "-", "-", "-", "1"], "{text}");
    assert_eq!(lines[5], "2 of 3 distributions used by first-party code (128 first-party files).");
}

#[test]
fn json_report_follows_the_check_conventions() {
    let text = render(&sample_report(), ReportOptions { score: true, all: false, json: true });
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        format!("{{\"version\":2,\"tool\":{{\"name\":\"pymapper\",\"version\":\"{}\"}},\"distributions\":[", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        lines[1],
        "{\"name\":\"fastlib\",\"version\":\"\",\"loaded\":false,\"used\":true,\"importers\":1,\"import_rows\":1,\
         \"modules_used\":0,\"api_used\":0,\"call_sites\":0,\"loc_reached\":0,\"loc\":0,\"scorecard\":null},"
    );
    assert!(lines[2].starts_with(
        "{\"name\":\"requests\",\"version\":\"2.32.3\",\"loaded\":true,\"used\":true,\"importers\":3,\"import_rows\":5,\
         \"modules_used\":2,\"api_used\":14,\"call_sites\":20,\"loc_reached\":1203,\"loc\":5301,\
         \"scorecard\":{\"grade\":\"B\",\"score\":78.25,\"files\":17,"
    ), "{text}");
    assert!(lines[2].contains("\"cc_mean\":3.12,") && lines[2].contains("\"mi_mean\":62.06,"), "{text}");
    assert!(lines[2].ends_with("\"external_deps\":4}}"), "{text}");
    assert_eq!(
        lines[3],
        "],\"summary\":{\"distributions\":3,\"used\":2,\"listed\":2,\"first_party_files\":128,\"scorecards_computed\":1,\"scorecards_cached\":2}}"
    );
    assert_eq!(lines.len(), 4);
    // Without --score: no scorecard objects and no scorecard counters; a NaN score is null.
    let r = DepsReport::new(sample_report().rows().to_vec(), 128, None);
    let text = render(&r, ReportOptions { score: false, all: true, json: true });
    assert!(!text.contains("scorecard"), "{text}");
    let text = render(&sample_report(), ReportOptions { score: true, all: true, json: true });
    assert!(text.contains("{\"name\":\"zeta\"") && text.contains("\"scorecard\":{\"grade\":null,\"score\":null,"), "{text}");
}

#[test]
fn empty_report() {
    let r = DepsReport::new(Vec::new(), 0, None);
    let text = render(&r, ReportOptions::default());
    assert_eq!(text, "0 of 0 distributions used by first-party code (0 first-party files).\n");
    let json = render(&r, ReportOptions { json: true, ..ReportOptions::default() });
    assert!(json.ends_with("\"distributions\":[],\"summary\":{\"distributions\":0,\"used\":0,\"listed\":0,\"first_party_files\":0}}\n"), "{json}");
}

#[test]
fn report_stops_quietly_on_a_closed_pipe() {
    struct Closed;
    impl std::io::Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let outcome = sample_report().write(&ReportOptions::default(), &mut Closed).expect("no error");
    assert_eq!(outcome, crate::output::Outcome::Closed);
}
