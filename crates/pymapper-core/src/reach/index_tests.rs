//! Root sets, weights and the whole pipeline on loaded projects.

use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::config::{Config, Glob, Mode};
use crate::graph::{CallFilter, Condensed, call_graph};
use crate::index::{Index, Inputs, NodeId, load};
use crate::resolve::{Attribution, Imports, ResolvedCalls, resolve_calls, resolve_imports};
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
    let dir = std::env::temp_dir().join(format!("pymapper-reach-{}-{n}", std::process::id()));
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
        self.ix.find(qid.as_bytes()).unwrap_or_else(|| panic!("no node {qid}"))
    }

    /// `qid kind,kind` for every root, sorted.
    fn roots(&self, set: &RootSet) -> Vec<String> {
        let mut out: Vec<String> = set
            .iter()
            .map(|v| {
                let k = set.kind(v);
                let names: Vec<&str> =
                    (0..8).filter(|b| k & (1 << b) != 0).map(|b| root_kind::NAMES[b as usize]).collect();
                format!("{} {}", self.ix.qid_string(v), names.join(","))
            })
            .collect();
        out.sort();
        out
    }
}

const APP: &[(&str, &str)] = &[
    ("pkg/__init__.py", "from .core import api\nfrom ._impl import hidden\n__all__ = ['api', 'hidden']\n"),
    (
        "pkg/core.py",
        "def api():\n    helper()\n\ndef helper():\n    pass\n\ndef unused():\n    pass\n\n\
         class Pub:\n    def method(self):\n        pass\n    def _priv(self):\n        pass\n\n\
         def _private():\n    pass\n\ndef outer():\n    def inner():\n        pass\n    return inner\n",
    ),
    ("pkg/_impl.py", "def hidden():\n    pass\n\ndef internal():\n    pass\n"),
    ("pkg/limited.py", "__all__ = ['a']\n\ndef a():\n    pass\n\ndef b():\n    pass\n"),
    ("pkg/__main__.py", "print('x')\n"),
    ("app.py", "import pkg.core\n\nif __name__ == '__main__':\n    pkg.core.api()\n"),
    (
        "tests/test_core.py",
        "import pytest\nfrom pkg.core import helper\n\n@pytest.fixture\ndef thing():\n    return 1\n\n\
         def test_x(thing):\n    helper()\n\ndef util():\n    pass\n\nclass TestThing:\n    def setUp(self):\n        \
         pass\n    def test_y(self):\n        pass\n    def other(self):\n        pass\n\n\
         def pytest_configure(config):\n    pass\n",
    ),
    ("conftest.py", "def fixture_a():\n    pass\n"),
];

#[test]
fn application_roots() {
    let f = fx(APP);
    let set = roots(&f.ix, Some(&f.imp), Some(&f.calls), &RootOptions::application());
    assert_eq!(
        f.roots(&set),
        vec![
            "app module,main",
            "conftest module,test",
            "conftest:fixture_a test",
            "pkg module",
            "pkg.__main__ module,main",
            "pkg._impl module",
            "pkg.core module",
            "pkg.limited module",
            "tests.test_core module,test",
            "tests.test_core:TestThing test",
            "tests.test_core:TestThing.setUp test",
            "tests.test_core:TestThing.test_y test",
            "tests.test_core:pytest_configure test",
            "tests.test_core:test_x test",
            "tests.test_core:thing test",
        ]
    );
    assert_eq!(set.len(), 15);
    assert_eq!(set.node_count(), f.ix.node_count());
    // Without module scopes, a test file node is still a root only if it is a test.
    let opts = RootOptions { modules: false, ..RootOptions::application() };
    let set = roots(&f.ix, None, None, &opts);
    assert!(!set.contains(f.node("pkg.core")));
    assert_eq!(set.kind(f.node("app")), root_kind::MAIN);
    // Origins filter: no first-party roots at all.
    let opts = RootOptions { origins: 1 << Origin::ThirdParty as u8, ..RootOptions::application() };
    assert!(roots(&f.ix, None, None, &opts).is_empty());
}

#[test]
fn library_roots_are_the_public_api() {
    let f = fx(APP);
    let opts = RootOptions { modules: false, main: false, tests: false, ..RootOptions::library() };
    let set = roots(&f.ix, Some(&f.imp), Some(&f.calls), &opts);
    assert_eq!(
        f.roots(&set),
        vec![
            "app public",
            "conftest public",
            "conftest:fixture_a public",
            "pkg public",
            "pkg.__main__ public",
            "pkg._impl:hidden public",
            "pkg.core public",
            "pkg.core:Pub public",
            "pkg.core:Pub.method public",
            "pkg.core:api public",
            "pkg.core:helper public",
            "pkg.core:outer public",
            "pkg.core:unused public",
            "pkg.limited public",
            "pkg.limited:a public",
            "tests.test_core public",
            "tests.test_core:TestThing public",
            "tests.test_core:TestThing.other public",
            "tests.test_core:TestThing.setUp public",
            "tests.test_core:TestThing.test_y public",
            "tests.test_core:pytest_configure public",
            "tests.test_core:test_x public",
            "tests.test_core:thing public",
            "tests.test_core:util public",
        ]
    );
    // Without import resolution the re-export of a private module's function is not seen.
    let set = roots(&f.ix, None, None, &opts);
    assert!(!set.contains(f.node("pkg._impl:hidden")));
    assert!(!set.contains(f.node("pkg._impl:internal")));
}

fn globs(patterns: &[&str]) -> Vec<Glob> {
    patterns.iter().map(|p| Glob::new(p).expect("glob")).collect()
}

const FRAMEWORK: &[(&str, &str)] = &[
    (
        "web.py",
        "from flask import Flask\napp = Flask(__name__)\n\n@app.route('/')\ndef index():\n    pass\n\n\
         @app.errorhandler\ndef oops():\n    pass\n\ndef handle_get():\n    pass\n\ndef plain():\n    pass\n",
    ),
    (
        "models.py",
        "from django.db import models\nimport django.forms as forms\n\n\
         class M(models.Model):\n    def save2(self):\n        pass\n\n\
         class Sub(M):\n    def go(self):\n        pass\n\n\
         class F(forms.Form):\n    pass\n\nclass Other:\n    def x(self):\n        pass\n",
    ),
    (
        "pkg/base.py",
        "class Plugin:\n    def run(self):\n        pass\n\nclass P1(Plugin):\n    def run(self):\n        pass\n",
    ),
    ("pkg/ext.py", "from pkg.base import Plugin as P\n\nclass P2(P):\n    pass\n"),
    ("pkg/__init__.py", ""),
];

#[test]
fn entrypoint_rule_roots() {
    let f = fx(FRAMEWORK);
    let mut cfg = Config::new(std::path::Path::new("/proj"));
    cfg.entrypoints.decorators = globs(&["app.route", "*.errorhandler"]);
    cfg.entrypoints.names = globs(&["handle_*", "models:Other.x"]);
    cfg.entrypoints.bases = globs(&["django.db.models.Model", "django.forms.Form", "pkg.base.Plugin"]);
    let mt = cfg.entrypoint_matcher();
    let opts = RootOptions { modules: false, entrypoints: Some(&mt), ..RootOptions::application() };
    let set = roots(&f.ix, Some(&f.imp), Some(&f.calls), &opts);
    assert_eq!(
        f.roots(&set),
        vec![
            "models:F base",
            "models:M base",
            "models:M.save2 base",
            "models:Other.x name",
            "models:Sub base",
            "models:Sub.go base",
            "pkg.base:P1 base",
            "pkg.base:P1.run base",
            "pkg.ext:P2 base",
            "web:handle_get name",
            "web:index decorator",
            "web:oops decorator",
        ]
    );
    // Base rules need the class table; without it only decorators and names apply.
    let set = roots(&f.ix, Some(&f.imp), None, &opts);
    assert!(!set.contains(f.node("models:M")));
    assert!(set.contains(f.node("web:index")));
    // An empty matcher is the same as none.
    let empty = Config::new(std::path::Path::new("/proj")).entrypoint_matcher();
    let opts = RootOptions { modules: false, entrypoints: Some(&empty), ..RootOptions::application() };
    assert!(roots(&f.ix, Some(&f.imp), Some(&f.calls), &opts).is_empty());
}

#[test]
fn script_targets() {
    let f = fx(APP);
    let scripts: &[&[u8]] = &[b"pkg.core:api", b"app", b"missing.mod:x", b" pkg.core : Pub.method [extra] ", b"pkg.core:nope"];
    let opts = RootOptions { modules: false, main: false, tests: false, scripts, ..RootOptions::application() };
    let set = roots(&f.ix, None, None, &opts);
    assert_eq!(
        f.roots(&set),
        vec!["app script", "pkg.core script", "pkg.core:Pub.method script", "pkg.core:api script"]
    );
    assert_eq!(set.unmatched_scripts(), &[2, 4]);
}

#[test]
fn script_targets_through_reexports() {
    let f = fx(&[
        ("mypkg/__init__.py", "from .cli import main\nfrom . import tools as t\n"),
        ("mypkg/cli.py", "def main():\n    run()\n\ndef run():\n    pass\n\nclass App:\n    def go(self):\n        pass\n"),
        ("mypkg/tools.py", "from .cli import App as Alias\n"),
        ("mypkg/sub/__init__.py", ""),
        ("mypkg/sub/deep.py", "def entry():\n    pass\n"),
    ]);
    let scripts: &[&[u8]] = &[
        b"mypkg:main",
        b"mypkg:t.Alias.go",
        b"mypkg.sub:deep.entry",
        b"mypkg:missing",
        b"mypkg:main.nope",
        b"@proj",
        b"mypkg:main:x",
        b"",
    ];
    let opts = RootOptions { modules: false, main: false, tests: false, scripts, ..RootOptions::application() };
    let set = roots(&f.ix, Some(&f.imp), None, &opts);
    assert_eq!(
        f.roots(&set),
        vec![
            "mypkg script",
            "mypkg.cli script",
            "mypkg.cli:App.go script",
            "mypkg.cli:main script",
            "mypkg.sub script",
            "mypkg.sub.deep script",
            "mypkg.sub.deep:entry script",
        ]
    );
    assert_eq!(set.unmatched_scripts(), &[3, 4, 5, 6, 7]);
    // The call tree of the entry point is reachable from the script root.
    let g = call_graph(&f.ix, &f.calls, CallFilter::default());
    let live = forward(&g, set.iter());
    assert!(live.contains(f.node("mypkg.cli:run")));
    // Without import resolution only qualified ids match.
    let set = roots(&f.ix, None, None, &opts);
    assert_eq!(set.unmatched_scripts(), &[0, 1, 3, 4, 5, 6, 7]);
}

#[test]
fn library_mode_resolution() {
    let with_main = fx(APP);
    assert!(!is_library(&with_main.ix, Mode::Auto, false));
    assert!(is_library(&with_main.ix, Mode::Library, false));
    let lib = fx(&[("lib/__init__.py", "def f():\n    pass\n")]);
    assert!(is_library(&lib.ix, Mode::Auto, false));
    assert!(!is_library(&lib.ix, Mode::Auto, true), "declared scripts make an application");
    assert!(!is_library(&lib.ix, Mode::Application, false));
}

#[test]
fn index_weights_are_exclusive_lines() {
    let f = fx(&[(
        "a.py",
        "x = 1\n\ndef f():\n    return 1\n\nclass C:\n    y = 2\n    def m(self):\n        pass\n",
    )]);
    let w = Weights::of_index(&f.ix);
    let lines = |q: &str| w.lines[f.node(q) as usize];
    assert_eq!(lines("a"), 3, "module lines outside defs: 1, 2, 5");
    assert_eq!(lines("a:f"), 2);
    assert_eq!(lines("a:C"), 2, "class lines outside methods: 6, 7");
    assert_eq!(lines("a:C.m"), 2);
    let total: u32 = w.lines.iter().sum();
    assert_eq!(total, f.ix.file_facts(0).lines, "exclusive lines partition the file");
    assert!(!w.symbol.contains(f.node("a")));
    assert!(w.symbol.contains(f.node("a:f")) && w.symbol.contains(f.node("a:C")) && w.symbol.contains(f.node("a:C.m")));
    assert!(!w.symbol.contains(f.ix.group_node(0)));
}

#[test]
fn pipeline_finds_unreachable_code_and_sizes() {
    let f = fx(APP);
    let g = call_graph(&f.ix, &f.calls, CallFilter::default());
    let set = roots(&f.ix, Some(&f.imp), Some(&f.calls), &RootOptions::application());
    let live = forward(&g, set.iter());
    let dead: Vec<String> = (0..f.ix.node_count())
        .filter(|&v| f.ix.kind(v).is_def() && !live.contains(v))
        .map(|v| f.ix.qid_string(v))
        .collect();
    assert_eq!(
        dead,
        vec![
            "pkg._impl:hidden",
            "pkg._impl:internal",
            "pkg.core:unused",
            "pkg.core:Pub",
            "pkg.core:Pub.method",
            "pkg.core:Pub._priv",
            "pkg.core:_private",
            "pkg.core:outer",
            "pkg.core:outer.inner",
            "pkg.limited:a",
            "pkg.limited:b",
            "tests.test_core:util",
            "tests.test_core:TestThing.other",
        ]
    );
    // `helper` is live through `api`, called from the `__main__` block of `app`.
    let path: Vec<String> = live.path_to(f.node("pkg.core:helper")).iter().map(|&v| f.ix.qid_string(v)).collect();
    assert!(path.last().is_some_and(|q| q == "pkg.core:helper"));
    assert!(set.contains(f.node(&path[0])));
    // Sizes: `api` reaches itself and `helper`; approximate = exact on small sets.
    let cd = Condensed::of(&g);
    let w = Weights::of_index(&f.ix);
    let exact = reach_sizes(&cd, &w, SizeMode::Exact);
    let approx = reach_sizes(&cd, &w, SizeMode::default());
    let api = f.node("pkg.core:api");
    assert_eq!(exact.symbols(api), 2);
    assert_eq!(exact.lines(api), 4);
    for v in 0..f.ix.node_count() {
        assert_eq!((approx.symbols(v), approx.lines(v)), (exact.symbols(v), exact.lines(v)));
    }
    let app = f.node("app");
    assert_eq!(exact.symbols(app), 2, "app's module scope reaches api and helper");
}
