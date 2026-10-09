//! Dead code analysis tests: small projects loaded from temporary directories.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::config::{Config, Preset};
use crate::index::{Inputs, load};
use crate::intern::Sym;
use crate::resolve::{Attribution, resolve_calls, resolve_imports};
use crate::roots::Origin;
use crate::walk::{WalkOptions, walk};

pub(super) struct Fx {
    pub ix: Index,
    pub imp: Imports,
    pub calls: ResolvedCalls,
}

/// Loads `files` (relative path, source) as one first-party root and resolves everything.
pub(super) fn fx(files: &[(&str, &str)]) -> Fx {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pymapper-deadcode-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    for (rel, src) in files {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, src).expect("write");
    }
    let f = load_dir(&dir);
    let _ = fs::remove_dir_all(&dir);
    f
}

/// Loads every file under `dir` as one first-party root.
pub(super) fn load_dir(dir: &Path) -> Fx {
    let w = walk(&[dir], &WalkOptions::everything());
    let ix = load(Inputs::from_root_dir(dir, &w.files, Origin::FirstParty, b"proj"));
    let imp = resolve_imports(&ix, &Attribution::new());
    let calls = resolve_calls(&ix, &imp);
    Fx { ix, imp, calls }
}

impl Fx {
    pub fn node(&self, qid: &str) -> NodeId {
        self.ix.find(qid.as_bytes()).unwrap_or_else(|| panic!("no node {qid}"))
    }

    /// Runs with `opts` and every rule; returns the result and `CODE qid conf` per finding
    /// (`CODE qid:name conf` for PM405), in emission order.
    pub fn run_with(&self, opts: &DeadCodeOptions<'_>) -> (DeadCode, Vec<String>) {
        let mut out = Findings::new();
        let dc = run(&self.ix, &self.imp, &self.calls, opts, Emit::ALL, &mut out);
        let lines = out
            .iter()
            .map(|(f, args)| {
                let qid = self.ix.qid_string(f.symbol.0);
                match args.first() {
                    Some(Arg::Name(raw)) => {
                        let name = String::from_utf8_lossy(self.ix.str(Sym::from_raw(*raw).expect("sym"))).into_owned();
                        format!("{} {qid}:{name}@{} {}", f.rule, f.loc.line, f.confidence)
                    }
                    _ => format!("{} {qid} {}", f.rule, f.confidence),
                }
            })
            .collect();
        (dc, lines)
    }

    /// Application mode, no entrypoints.
    pub fn app(&self) -> (DeadCode, Vec<String>) {
        self.run_with(&DeadCodeOptions { mode: Mode::Application, ..DeadCodeOptions::default() })
    }

    /// Findings of [`Fx::app`] at or above `min` confidence.
    pub fn app_at(&self, min: u8) -> Vec<String> {
        let (_, lines) = self.app();
        lines.into_iter().filter(|l| l.rsplit(' ').next().and_then(|c| c.parse::<u8>().ok()).is_some_and(|c| c >= min)).collect()
    }
}

#[test]
fn unreached_defs_are_reported_with_full_confidence() {
    let f = fx(&[
        ("main.py", "from lib import used\n\nif __name__ == '__main__':\n    used()\n"),
        (
            "lib.py",
            "def used():\n    helper()\n\ndef helper():\n    pass\n\ndef unused():\n    chain()\n\ndef chain():\n    pass\n\n\
             class Dead:\n    def m(self):\n        pass\n",
        ),
    ]);
    let (dc, lines) = f.app();
    assert_eq!(lines, [
        "PM401 lib:unused 100",
        "PM401 lib:chain 100",
        "PM402 lib:Dead 100",
    ]);
    assert_eq!(dc.level(f.node("lib:helper")), level::LIVE);
    assert_eq!(dc.why(f.node("lib:helper")), Why::Call);
    assert_eq!(dc.why(f.node("lib:unused")), Why::Dead);
    assert!(!dc.stats().library);
}

fn corpus() -> Fx {
    load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/sample_python"))
}

#[test]
#[ignore]
fn dump_corpus() {
    let f = corpus();
    let (_, lines) = f.app();
    println!("APP:\n{}", lines.join("\n"));
    let mut cfg = Config::new(Path::new("/"));
    cfg.presets.push(Preset::Flask);
    let m = cfg.entrypoint_matcher();
    let (_, lines) = f.run_with(&DeadCodeOptions { mode: Mode::Application, entrypoints: Some(&m), ..DeadCodeOptions::default() });
    println!("FLASK:\n{}", lines.join("\n"));
    let (_, lines) = f.run_with(&DeadCodeOptions::default());
    println!("AUTO:\n{}", lines.join("\n"));
}

#[test]
fn possible_calls_lower_confidence() {
    let f = fx(&[(
        "app.py",
        "class Store:\n    def frobnicate(self):\n        pass\n\n\
         def run(x=None):\n    Store()\n    x.frobnicate()\n\n\
         class Other:\n    def lonely(self):\n        pass\n\n\
         def build(y):\n    return y.lonely()\n\n\
         if __name__ == '__main__':\n    run()\n",
    )]);
    let (dc, lines) = f.app();
    // `Other.lonely` is called (possibly) only from dead `build`: the occurrence of its name is
    // explained by that call, so the class is dead and the method is not reported separately.
    assert_eq!(lines, ["PM403 app:Store.frobnicate 50", "PM402 app:Other 100", "PM401 app:build 100"]);
    assert_eq!(dc.level(f.node("app:Store.frobnicate")), level::POSSIBLE);
    assert_eq!(dc.why(f.node("app:Store.frobnicate")), Why::PossibleCall);
    assert_eq!(dc.level(f.node("app:Other.lonely")), 0);
}

#[test]
fn override_liveness_follows_live_classes() {
    let f = fx(&[(
        "app.py",
        "class Base:\n    def handle(self):\n        raise NotImplementedError\n    def go(self):\n        self.handle()\n\
         \x20   def interface(self):\n        pass\n\n\
         class Live(Base):\n    def handle(self):\n        pass\n    def interface(self):\n        pass\n\n\
         class Never(Base):\n    def handle(self):\n        pass\n\n\
         def run():\n    x = Live()\n    x.go()\n    x.interface()\n\n\
         if __name__ == '__main__':\n    run()\n",
    )]);
    let (dc, lines) = f.app();
    // `Never` is not instantiated: overriding a live method does not make it live.
    assert_eq!(lines, ["PM402 app:Never 100"]);
    assert_eq!(dc.why(f.node("app:Live.handle")), Why::Override);
    // The base method an override of which is live is live too (interface method).
    assert_eq!(dc.level(f.node("app:Base.interface")), level::LIVE);
    assert_eq!(dc.level(f.node("app:Base")), level::LIVE);
    assert_eq!(dc.level(f.node("app:Never.handle")), 0);
}

#[test]
fn members_live_with_their_class() {
    let f = fx(&[(
        "app.py",
        "from typing import Protocol\nfrom abc import ABC, abstractmethod\n\n\
         class Point:\n    def __init__(self):\n        pass\n    def __repr__(self):\n        return 'p'\n\
         \x20   class Meta:\n        pass\n    def unused(self):\n        pass\n\n\
         class Proto(Protocol):\n    def meth(self) -> int: ...\n\n\
         class Shape(ABC):\n    @abstractmethod\n    def area(self): ...\n\n\
         class Square(Shape):\n    def area(self):\n        return 1\n\n\
         class Gone:\n    def __len__(self):\n        return 0\n\n\
         def run(p: Proto):\n    Point()\n    Square()\n\n\
         if __name__ == '__main__':\n    run(None)\n",
    )]);
    let (dc, lines) = f.app();
    // `Proto` is referenced in an annotation of live code (evidence in its own file).
    assert_eq!(lines, ["PM403 app:Point.unused 100", "PM402 app:Proto 10", "PM402 app:Gone 100"]);
    for live in ["app:Point.__init__", "app:Point.__repr__", "app:Point.Meta", "app:Square.area", "app:Shape.area"] {
        assert_eq!(dc.level(f.node(live)), level::LIVE, "{live}");
    }
    assert_eq!(dc.why(f.node("app:Point.__repr__")), Why::Member);
    // Protocol members are live as far as the protocol is.
    assert_eq!(dc.level(f.node("app:Proto.meth")), level::LINKED);
}

#[test]
fn decorators_that_may_register_lower_confidence() {
    let f = fx(&[(
        "app.py",
        "import functools\nfrom typing import override\n\n\
         @functools.lru_cache\ndef cached():\n    pass\n\n\
         @registry.register\ndef plugin():\n    helper()\n\n\
         def helper():\n    pass\n\n\
         class Base:\n    pass\n\n\
         class Impl(Base):\n    @override\n    def hook(self):\n        pass\n    @signal.connect\n    def on(self):\n        pass\n\n\
         class DeadImpl(Base):\n    @override\n    def hook(self):\n        pass\n\n\
         def run():\n    Impl()\n\n\
         if __name__ == '__main__':\n    run()\n",
    )]);
    let (dc, lines) = f.app();
    assert_eq!(lines, [
        "PM401 app:cached 100",
        "PM401 app:plugin 40",
        "PM401 app:helper 40",
        "PM403 app:Impl.on 40",
        "PM402 app:DeadImpl 100",
    ]);
    assert_eq!(dc.why(f.node("app:plugin")), Why::Decorated);
    assert_eq!(dc.why(f.node("app:helper")), Why::Call);
    // `@override` (PEP 698): overrides a method of some base, live with its class.
    assert_eq!(dc.level(f.node("app:Impl.hook")), level::LIVE);
    assert_eq!(dc.level(f.node("app:DeadImpl.hook")), 0);
}

#[test]
fn external_bases_and_dynamic_modules() {
    let f = fx(&[
        (
            "app.py",
            "import threading\nimport dyn\n\n\
             class Worker(threading.Thread):\n    def run(self):\n        pass\n    def _helper(self):\n        pass\n\n\
             class Idle(threading.Thread):\n    def run(self):\n        pass\n\n\
             def main():\n    Worker().start()\n    dyn.entry()\n\n\
             if __name__ == '__main__':\n    main()\n",
        ),
        ("dyn.py", "def entry():\n    return getattr(dyn_self, 'x')()\n\ndef handler_a():\n    pass\n"),
    ]);
    let (dc, lines) = f.app();
    assert_eq!(lines, [
        "PM403 app:Worker.run 50",
        "PM403 app:Worker._helper 50",
        "PM402 app:Idle 100",
        "PM401 dyn:handler_a 70",
    ]);
    assert_eq!(dc.why(f.node("app:Worker.run")), Why::ExternalBase);
    assert_eq!(dc.why(f.node("dyn:handler_a")), Why::Dynamic);
}
