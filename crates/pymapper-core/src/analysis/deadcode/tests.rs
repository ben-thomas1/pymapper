//! Dead code analysis tests: small projects loaded from temporary directories.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::config::{Config, Preset};
use crate::extract::facts::sup_kind;
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
        self.run_emit(opts, Emit::ALL)
    }

    /// [`Fx::run_with`] with only the rules of `emit`.
    pub fn run_emit(&self, opts: &DeadCodeOptions<'_>, emit: Emit) -> (DeadCode, Vec<String>) {
        let mut out = Findings::new();
        let dc = run(&self.ix, &self.imp, &self.calls, opts, emit, &mut out);
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

    // Name-based dispatch in a base class's module reaches the subclass's methods.
    let f = fx(&[
        ("printer.py", "class Printer:\n    def doprint(self, x):\n        return getattr(self, '_print_' + type(x).__name__)(x)\n"),
        ("mathml.py", "from printer import Printer\n\nclass MathML(Printer):\n    def _print_Add(self, x):\n        pass\n"),
        ("main.py", "from mathml import MathML\n\nif __name__ == '__main__':\n    MathML().doprint(1)\n"),
    ]);
    let (dc, _) = f.app();
    let v = f.node("mathml:MathML._print_Add");
    assert_eq!((dc.level(v), dc.why(v)), (level::DYNAMIC, Why::Dynamic));
}

const LIB: &[(&str, &str)] = &[
    ("pkg/__init__.py", "from .core import api\n__all__ = ['api']\n"),
    (
        "pkg/core.py",
        "def api():\n    pass\n\ndef public_unused():\n    pass\n\ndef _private_unused():\n    pass\n\n\
         class Pub:\n    def method(self):\n        pass\n    def _hidden(self):\n        pass\n",
    ),
    ("pkg/_impl.py", "def internal():\n    pass\n\ndef _gone():\n    pass\n"),
];

#[test]
fn library_mode_keeps_the_public_api() {
    let f = fx(LIB);
    // No `__main__` guard and no scripts: `auto` is library mode.
    let (dc, lines) = f.run_with(&DeadCodeOptions::default());
    assert!(dc.stats().library);
    // The private module is imported by nothing: one PM404 covers its defs.
    assert_eq!(lines, ["PM404 pkg._impl 100", "PM401 pkg.core:_private_unused 100", "PM403 pkg.core:Pub._hidden 100"]);
    assert_eq!(dc.why(f.node("pkg._impl:internal")), Why::LibraryPublic);
    assert_eq!(dc.confidence(f.node("pkg._impl:internal")), 80);
    assert_eq!(dc.confidence(f.node("pkg._impl:_gone")), 100);
    assert_eq!(dc.level(f.node("pkg.core:Pub.method")), level::LIVE);
    // `PM404`: the private module is not imported by anything.
    assert_eq!(dc.module_level(f.node("pkg._impl")), Some(0));
    assert_eq!(dc.module_level(f.node("pkg.core")), Some(level::LIVE));
}

#[test]
fn library_mode_star_exports_and_api_methods() {
    let f = fx(&[
        ("lib/__init__.py", "from ._impl import *\nfrom ._feat import make\n__all__ = ['Path', 'make']\n"),
        ("lib/_impl.py", "__all__ = ['Path']\n\nclass Path:\n    def parts(self):\n        pass\n\nclass Other:\n    pass\n"),
        (
            "lib/_feat.py",
            "class _Feature:\n    def release(self):\n        pass\n    def _internal(self):\n        pass\n\n\
             def make():\n    return _Feature()\n",
        ),
    ]);
    let (dc, lines) = f.run_with(&DeadCodeOptions { mode: Mode::Library, ..DeadCodeOptions::default() });
    assert_eq!((dc.level(f.node("lib._impl:Path")), dc.why(f.node("lib._impl:Path"))), (level::LIVE, Why::StarExport));
    // Not in the star-imported module's `__all__`.
    assert_eq!(dc.confidence(f.node("lib._impl:Other")), 80);
    // A public method of a private class whose instances the API returns.
    assert_eq!((dc.level(f.node("lib._feat:_Feature.release")), dc.why(f.node("lib._feat:_Feature.release"))), (level::API_METHOD, Why::ApiMethod));
    assert!(lines.contains(&"PM403 lib._feat:_Feature._internal 100".to_owned()), "{lines:?}");
}

#[test]
fn application_mode_lowers_exported_names() {
    let mut files = LIB.to_vec();
    files.push(("main.py", "import pkg\n\nif __name__ == '__main__':\n    pkg.api()\n"));
    let f = fx(&files);
    let (dc, lines) = f.app();
    // Called through the package's re-export.
    assert_eq!(dc.level(f.node("pkg.core:api")), level::LIVE);
    assert!(lines.contains(&"PM401 pkg.core:public_unused 100".to_owned()), "{lines:?}");
    assert!(lines.contains(&"PM402 pkg.core:Pub 100".to_owned()), "{lines:?}");
    assert!(lines.contains(&"PM404 pkg._impl 100".to_owned()), "{lines:?}");
    // The defs of the unused module are not reported separately.
    assert!(!lines.iter().any(|l| l.contains("pkg._impl:")), "{lines:?}");
    let f = fx(&[("mod.py", "__all__ = ['exported']\n\ndef exported():\n    pass\n")]);
    let (dc, _) = f.app();
    // The `__all__` string is name evidence in the def's own file.
    assert_eq!((dc.level(f.node("mod:exported")), dc.why(f.node("mod:exported"))), (level::LINKED, Why::Linked));
    // A re-export (`import x as x`); the import statement itself is not name evidence.
    let f = fx(&[("pkg/__init__.py", "from .core import thing as thing\n"), ("pkg/core.py", "def thing():\n    pass\n")]);
    let (dc, lines) = f.app();
    assert_eq!((dc.level(f.node("pkg.core:thing")), dc.why(f.node("pkg.core:thing"))), (level::EXPORTED, Why::Exported));
    assert!(lines.contains(&"PM401 pkg.core:thing 30".to_owned()) || lines.iter().any(|l| l.starts_with("PM404 pkg.core")), "{lines:?}");
}

#[test]
fn name_fallback_levels() {
    let f = fx(&[
        (
            "main.py",
            "import lib\nfrom lib import aliased as al\n\n\
             def callback():\n    pass\n\ndef dead():\n    chained()\n\ndef chained():\n    pass\n\n\
             def run():\n    register(callback)\n    register(al)\n    lib.by_attr\n\n\
             if __name__ == '__main__':\n    run()\n",
        ),
        ("lib.py", "def aliased():\n    pass\n\ndef by_attr():\n    pass\n\ndef by_string():\n    pass\n\ndef nothing():\n    pass\n"),
        ("other.py", "X = 'lib.by_string'\n"),
    ]);
    let (dc, lines) = f.app();
    let lvl = |q: &str| (dc.level(f.node(q)), dc.why(f.node(q)));
    assert_eq!(lvl("main:callback"), (level::LINKED, Why::Linked));
    assert_eq!(lvl("lib:aliased"), (level::LINKED, Why::Linked));
    assert_eq!(lvl("lib:by_attr"), (level::LINKED, Why::Linked));
    assert_eq!(lvl("lib:by_string"), (level::EVIDENCE, Why::Evidence));
    // A dead call chain is reported in full: `chained`'s name is explained by the dead call.
    assert_eq!(lvl("main:chained"), (0, Why::Dead));
    assert!(lines.contains(&"PM401 lib:nothing 100".to_owned()), "{lines:?}");
    assert!(lines.contains(&"PM401 main:chained 100".to_owned()), "{lines:?}");
    // Without the fallback every unreached def is reported with full confidence.
    let (dc, _) = f.run_with(&DeadCodeOptions { mode: Mode::Application, name_fallback: false, ..DeadCodeOptions::default() });
    assert_eq!(dc.level(f.node("main:callback")), 0);
    assert_eq!(dc.stats().candidates, 0);
}

#[test]
fn same_name_siblings_share_liveness() {
    let f = fx(&[(
        "app.py",
        "import sys\n\nif sys.platform == 'win32':\n    def impl():\n        pass\nelse:\n    def impl():\n        pass\n\n\
         class C:\n    @property\n    def x(self):\n        return 1\n    @x.setter\n    def x(self, v):\n        pass\n\n\
         def run():\n    impl()\n    C().x = 2\n\n\
         if __name__ == '__main__':\n    run()\n",
    )]);
    let (dc, lines) = f.app();
    assert_eq!(dc.level(f.node("app:impl")), level::LIVE);
    assert_eq!(dc.level(f.node("app:impl#2")), level::LIVE);
    // The property is used as an attribute: name evidence in its own file.
    assert_eq!(dc.level(f.node("app:C.x")), level::LINKED);
    assert_eq!(dc.level(f.node("app:C.x#2")), level::LINKED);
    assert_eq!(lines, ["PM403 app:C.x 10", "PM403 app:C.x#2 10"]);
}

#[test]
fn unused_modules() {
    let f = fx(&[
        ("app/__init__.py", ""),
        ("app/main.py", "from app import used\nimport app.sub.deep\n\nif __name__ == '__main__':\n    used.f()\n"),
        ("app/used.py", "def f():\n    pass\n"),
        ("app/orphan.py", "def g():\n    pass\n"),
        ("app/plugins/__init__.py", ""),
        ("app/plugins/loaded.py", "def h():\n    pass\n"),
        ("app/loader.py", "import importlib\nimportlib.import_module('app.plugins.loaded')\n"),
        ("app/sub/__init__.py", ""),
        ("app/sub/deep.py", ""),
        ("app/typed.py", "from typing import TYPE_CHECKING\nif TYPE_CHECKING:\n    from app import hints\n"),
        ("app/hints.py", "class H:\n    pass\n"),
        ("setup.py", "from setuptools import setup\nsetup()\n"),
        ("script.py", "print('hi')\n"),
        ("tests/test_x.py", "import app.typed\n\ndef test_x():\n    pass\n"),
    ]);
    let (dc, lines) = f.app();
    let modules: Vec<&String> = lines.iter().filter(|l| l.starts_with("PM404")).collect();
    assert_eq!(modules, [
        "PM404 app.loader 100",
        "PM404 app.orphan 100",
        "PM404 app.plugins.loaded 50",
        "PM404 script 40",
    ]);
    let ml = |q: &str| (dc.module_level(f.node(q)).unwrap(), dc.module_why(f.node(q)).unwrap());
    assert_eq!(ml("app.main"), (level::LIVE, Why::Root));
    assert_eq!(ml("app.used"), (level::LIVE, Why::Import));
    assert_eq!(ml("app.sub").0, level::LIVE);
    assert_eq!(ml("app.sub.deep"), (level::LIVE, Why::Import));
    // `import app.used` runs `app/__init__`: a live submodule keeps its package live.
    assert_eq!(ml("app").0, level::LIVE);
    // Type-only imports count (tests import `app.typed`).
    assert_eq!(ml("app.hints"), (level::LIVE, Why::Import));
    assert_eq!(ml("setup"), (level::LIVE, Why::Root));
    assert_eq!(ml("script"), (level::SCRIPT_LIKE, Why::ScriptLike));
    assert_eq!(ml("app.plugins.loaded"), (level::EVIDENCE, Why::Evidence));
    // The defs of an unused module are covered by its finding.
    assert!(!lines.iter().any(|l| l.contains("app.orphan:")), "{lines:?}");
}

#[test]
fn disabled_containers_do_not_hide_their_contents() {
    let f = fx(&[
        ("app/__init__.py", ""),
        ("app/main.py", "from app.cls import Live\n\nif __name__ == '__main__':\n    x = Live()\n    x.run()\n"),
        ("app/orphan.py", "import os\n\ndef g():\n    pass\n"),
        (
            "app/cls.py",
            "class Live:\n    def run(self):\n        pass\n\n\
             class Dead:\n    def meth(self):\n        pass\n",
        ),
    ]);
    let opts = DeadCodeOptions { mode: Mode::Application, ..DeadCodeOptions::default() };
    let (_, all) = f.run_with(&opts);
    assert_eq!(all, ["PM402 app.cls:Dead 100", "PM404 app.orphan 100"]);
    // Without PM404 / PM402 their contents are reported.
    let (_, lines) = f.run_emit(&opts, Emit { module: false, class: false, ..Emit::ALL });
    assert_eq!(lines, [
        "PM403 app.cls:Dead.meth 100",
        "PM401 app.orphan:g 100",
        "PM405 app.orphan:os@1 100",
    ]);
    // Only PM404 off: the class finding still contains its method.
    let (_, lines) = f.run_emit(&opts, Emit { module: false, ..Emit::ALL });
    assert_eq!(lines, ["PM402 app.cls:Dead 100", "PM401 app.orphan:g 100", "PM405 app.orphan:os@1 100"]);
    // Only PM402 off.
    let (_, lines) = f.run_emit(&opts, Emit { class: false, ..Emit::ALL });
    assert_eq!(lines, ["PM403 app.cls:Dead.meth 100", "PM404 app.orphan 100"]);
}

#[test]
fn unused_imports_across_modules() {
    let f = fx(&[
        (
            "app/__init__.py",
            "from app.core import reexported\nfrom app import core as core\nfrom . import side\n",
        ),
        (
            "app/main.py",
            "import os\nimport sys as system\nimport json\nfrom typing import TYPE_CHECKING\n\
             from __future__ import annotations\nfrom app.core import *\n\
             from app.core import passthrough, via_attr\n\
             try:\n    import ujson\nexcept ImportError:\n    ujson = None\n\
             if TYPE_CHECKING:\n    from app.core import Hint, Unused\n\n\
             def run(x: 'Hint'):\n    os.getcwd()\n    return json\n\n\
             if __name__ == '__main__':\n    run(None)\n",
        ),
        ("app/user.py", "from app.main import passthrough\nimport app.main\n\ndef u():\n    passthrough()\n    app.main.via_attr()\n"),
        ("app/core.py", "def reexported():\n    pass\ndef passthrough():\n    pass\ndef via_attr():\n    pass\nclass Hint:\n    pass\nclass Unused:\n    pass\n"),
        ("app/side.py", ""),
        ("app/dyn.py", "import collections\n\ndef f():\n    return globals()\n"),
        ("app/patched.py", "import shutil\n"),
        ("tests/test_p.py", "import app.dyn\n\ndef test_p(mocker):\n    mocker.patch('app.patched.shutil')\n"),
    ]);
    let (_, lines) = f.app();
    let imports: Vec<&String> = lines.iter().filter(|l| l.starts_with("PM405")).collect();
    assert_eq!(imports, [
        "PM405 app:reexported@1 50",
        // An application-mode `__init__` re-export of a submodule (often for side effects).
        "PM405 app:side@3 30",
        "PM405 app.dyn:collections@1 70",
        "PM405 app.main:system@2 100",
        // Type-only: quoted annotations such as `"Dict[str, Unused]"` are not evidence.
        "PM405 app.main:Unused@13 50",
        // Named in a string of the tests (`mocker.patch('app.patched.shutil')`).
        "PM405 app.patched:shutil@1 60",
    ]);
}

#[test]
fn declarative_entrypoints_presets_and_scripts() {
    let f = fx(&[
        (
            "proj/views.py",
            "from django.db import models\n\n\
             class Article(models.Model):\n    def save(self):\n        pass\n\n\
             @app.route('/')\ndef index():\n    helper()\n\ndef helper():\n    pass\n\n\
             def keep_me():\n    pass\n\ndef gone():\n    pass\n",
        ),
        ("proj/cli.py", "def main():\n    pass\n\ndef unused():\n    pass\n"),
    ]);
    let mut cfg = Config::new(Path::new("/"));
    cfg.presets.extend([Preset::Flask, Preset::Django]);
    cfg.entrypoints.names.push(crate::config::Glob::new("keep_*").expect("glob"));
    let m = cfg.entrypoint_matcher();
    let scripts: &[&[u8]] = &[b"proj.cli:main"];
    let opts = DeadCodeOptions::from_config(&cfg, &m, None, scripts);
    let (dc, lines) = f.run_with(&DeadCodeOptions { mode: Mode::Auto, ..opts });
    // Scripts make `auto` application mode.
    assert!(!dc.stats().library);
    assert_eq!(lines, ["PM401 proj.cli:unused 100", "PM401 proj.views:gone 100"]);
    for live in ["proj.views:Article", "proj.views:Article.save", "proj.views:index", "proj.views:helper", "proj.cli:main"] {
        assert_eq!(dc.level(f.node(live)), level::LIVE, "{live}");
    }
    assert_eq!(dc.module_level(f.node("proj.views")), Some(level::LIVE));
    // `project-scripts = false` drops the script targets.
    cfg.entrypoints.project_scripts = false;
    let opts = DeadCodeOptions::from_config(&cfg, &m, None, scripts);
    assert!(opts.scripts.is_empty());
}

#[test]
fn flask_routes_are_never_reported_with_high_confidence() {
    let f = corpus();
    let handlers = ["app:list_users", "app:health_check"];
    // Without the preset the route decorators may register the handlers.
    let (dc, _) = f.app();
    for h in handlers {
        assert_eq!((dc.confidence(f.node(h)), dc.why(f.node(h))), (40, Why::Decorated), "{h}");
    }
    let mut cfg = Config::new(Path::new("/"));
    cfg.presets.push(Preset::Flask);
    let m = cfg.entrypoint_matcher();
    let (dc, lines) = f.run_with(&DeadCodeOptions { mode: Mode::Application, entrypoints: Some(&m), ..DeadCodeOptions::default() });
    for h in handlers {
        assert_eq!(dc.level(f.node(h)), level::LIVE, "{h}");
    }
    // At the default gate only genuinely dead code is reported (checked by hand: each is
    // never called, or only from dead code).
    let high: Vec<&String> = lines.iter().filter(|l| l.rsplit(' ').next().is_some_and(|c| c == "100")).collect();
    assert_eq!(high, [
        "PM401 app:dead_endpoint 100",
        "PM401 middleware.pipeline:format_receipt 100",
        "PM403 models.base:BaseModel.save 100",
        "PM403 models.product:Product.display_price 100",
        "PM403 models.user:User.display_name 100",
        "PM404 repo.product_repo 100",
        "PM401 repo.user_repo:delete_user 100",
        "PM401 services.auth:make_validator 100",
        "PM401 services.auth:classify_token 100",
        "PM401 services.auth:create_authenticator 100",
        "PM401 services.user_service:get_user_display 100",
        "PM401 utils.formatting:format_currency 100",
        "PM401 utils.formatting:format_percentage 100",
        "PM401 utils.validation:validate_range 100",
        "PM405 middleware.pipeline:User@6 100",
    ]);
    assert!(!lines.iter().any(|l| l.rsplit(' ').next().is_some_and(|c| c.parse::<u8>().is_ok_and(|c| (80..100).contains(&c)))));
    // Methods reached through the unique-method fallback (`user.validate()` on a parameter).
    assert_eq!(dc.confidence(f.node("models.base:BaseModel.validate")), 50);
    // `cycles.ping` is called from `main`, which carries an unknown first-party decorator (40),
    // and `ping` / `pong` call each other: that recursion does not explain the names in
    // `cycles.py`, so they count as references there (10).
    assert_eq!(dc.confidence(f.node("app:main")), 40);
    assert_eq!(dc.confidence(f.node("cycles:ping")), 10);

    // Explanations are per def: `threading`-like code where `self._restore()` resolves to the
    // class's own method must not hide the attribute reference to another class's method.
    let f = fx(&[(
        "app.py",
        "class Lock:\n    def _restore(self):\n        pass\n\n\
         class Cond:\n    def __init__(self, lock):\n        self._restore = lock._restore\n\
         \x20   def _restore(self):\n        pass\n    def wait(self):\n        self._restore()\n\n\
         def make():\n    def walk(n):\n        return walk(n - 1) if n else 0\n    return walk\n\n\
         def fact(n):\n    return n * fact(n - 1)\n\n\
         def run():\n    Lock()\n    Cond(None).wait()\n    make()\n\n\
         if __name__ == '__main__':\n    run()\n",
    )]);
    let (dc, _) = f.app();
    assert_eq!(dc.level(f.node("app:Lock._restore")), level::LINKED);
    // A closure that calls itself and is returned: the recursion does not explain `return walk`.
    assert_eq!(dc.level(f.node("app:make.walk")), level::LINKED);
    // The price: a dead recursive function also looks referenced.
    assert_eq!(dc.level(f.node("app:fact")), level::LINKED);
}

/// Suppressions of the analysed files, from the facts.
fn suppressions(ix: &Index) -> crate::rules::Suppressions {
    let mut b = crate::rules::SuppressionsBuilder::new();
    let st = &ix.facts().suppressions;
    let mut entries = Vec::new();
    for f in 0..ix.file_count() {
        entries.clear();
        let view = ix.file_view(f);
        for r in ix.file_facts(f).suppressions.range() {
            if st.kind[r] == sup_kind::NOQA_F401 {
                entries.push(crate::rules::noqa_f401(st.line[r]));
                continue;
            }
            if st.kind[r] == sup_kind::ALL {
                crate::rules::parse_codes(st.line[r], None, &mut entries, |_| {});
            }
            for &s in view.chain(st.codes[r]) {
                crate::rules::parse_codes(st.line[r], Some(ix.str(s)), &mut entries, |_| {});
            }
        }
        b.push_file(rules::FileId(f), &entries);
    }
    b.build()
}

#[test]
fn engine_selection_and_suppressions() {
    let f = fx(&[(
        "app.py",
        "import os  # pymapper: ignore[PM405]\nimport sys\n\n\
         def a():  # pymapper: ignore[PM401]\n    pass\n\n\
         def b():  # pymapper: ignore[PM4]\n    pass\n\n\
         def c():\n    pass\n\n\
         class D:  # pymapper: ignore\n    pass\n\n\
         if __name__ == '__main__':\n    pass\n",
    )]);
    let mut cfg = Config::new(Path::new("/"));
    cfg.mode = Mode::Application;
    let m = cfg.entrypoint_matcher();
    let opts = DeadCodeOptions::from_config(&cfg, &m, None, &[]);
    let engine = RuleEngine::new(&cfg);
    let mut out = Findings::new();
    analyse(&f.ix, &f.imp, &f.calls, &opts, &engine, &mut out);
    assert_eq!(out.len(), 6);
    let stats = engine.filter(&mut out, &suppressions(&f.ix), false, |_, _| false);
    assert_eq!((stats.reported, stats.suppressed), (2, 4));
    let left: Vec<(crate::rules::Rule, u32)> = out.iter().map(|(x, _)| (x.rule, x.loc.line)).collect();
    assert_eq!(left, [(UNUSED_FUNCTION, 10), (UNUSED_IMPORT, 2)]);
    // Ignoring the whole category skips the pass.
    cfg.rules.ignore.push("PM4".parse().expect("selector"));
    let engine = RuleEngine::new(&cfg);
    let mut out = Findings::new();
    let dc = analyse(&f.ix, &f.imp, &f.calls, &opts, &engine, &mut out);
    assert!(out.is_empty());
    assert_eq!(dc.heap_bytes(), 0);
    // Only PM405.
    cfg.rules.ignore.clear();
    cfg.rules.select = Some(vec!["PM405".parse().expect("selector")]);
    let engine = RuleEngine::new(&cfg);
    let mut out = Findings::new();
    analyse(&f.ix, &f.imp, &f.calls, &opts, &engine, &mut out);
    assert!(out.iter().all(|(x, _)| x.rule == UNUSED_IMPORT) && out.len() == 2);
}

#[test]
fn noqa_f401_suppresses_unused_imports_only() {
    let f = fx(&[(
        "app.py",
        "import a1  # noqa\n\
         import a2  # noqa: F401  registers converters\n\
         import a3  # noqa:F401,E501\n\
         import a4  # NOQA: f401\n\
         import a5  # noqa: E501\n\
         import a6; s = '# noqa'\n\
         from m import (\n    b1,  # noqa: F401\n    b2,\n)\n\
         from m import (  # noqa: F401\n    c1,\n    c2,\n)\n\
         import a7  # noqa: E501  # pymapper: ignore[PM405]\n\
         import a8  # pymapper: ignore[PM401]  # noqa\n\
         def dead():  # noqa\n    pass\n\n\
         if __name__ == '__main__':\n    pass\n",
    )]);
    let mut cfg = Config::new(Path::new("/"));
    cfg.mode = Mode::Application;
    let m = cfg.entrypoint_matcher();
    let opts = DeadCodeOptions::from_config(&cfg, &m, None, &[]);
    let engine = RuleEngine::new(&cfg);
    let mut out = Findings::new();
    analyse(&f.ix, &f.imp, &f.calls, &opts, &engine, &mut out);
    engine.filter(&mut out, &suppressions(&f.ix), false, |_, _| false);
    let left: Vec<(crate::rules::Rule, u32)> = out.iter().map(|(x, _)| (x.rule, x.loc.line)).collect();
    // `a5` (E501 only), `a6` (noqa in a string) and `b2` (no noqa on its line or the
    // statement's first line) are reported; a bare noqa does not hide PM401 on `dead`.
    assert_eq!(left, [(UNUSED_FUNCTION, 17), (UNUSED_IMPORT, 5), (UNUSED_IMPORT, 6), (UNUSED_IMPORT, 9)]);
}

#[test]
fn name_groups_link_through_module_imports() {
    let handler = "def handler():\n    pass\n";
    let f = fx(&[
        ("m1.py", handler),
        ("m2.py", handler),
        ("m3.py", handler),
        ("m4.py", handler),
        // One module edge against a group of four: the module-edge side is iterated.
        ("user.py", "import m2\n\nif __name__ == '__main__':\n    print(m2.handler)\n"),
        // Four module edges: the member side is iterated.
        ("user2.py", "import m3, x1, x2, x3\n\nif __name__ == '__main__':\n    print(m3.handler, x1, x2, x3)\n"),
        ("x1.py", ""),
        ("x2.py", ""),
        ("x3.py", ""),
    ]);
    let (dc, lines) = f.app();
    let lvl = |q: &str| (dc.level(f.node(q)), dc.why(f.node(q)));
    assert_eq!(lvl("m2:handler"), (level::LINKED, Why::Linked));
    assert_eq!(lvl("m3:handler"), (level::LINKED, Why::Linked));
    // Mentioned only by files that do not import them.
    assert_eq!(lvl("m1:handler"), (level::EVIDENCE, Why::Evidence));
    assert_eq!(lvl("m4:handler"), (level::EVIDENCE, Why::Evidence));
    assert_eq!(lines.iter().filter(|l| l.starts_with("PM404")).count(), 2, "{lines:?}");
}

#[test]
fn results_do_not_depend_on_the_thread_count() {
    let f = corpus();
    let run_on = |threads: usize| {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().expect("pool");
        pool.install(|| {
            let (dc, lines) = f.app();
            let levels: Vec<(u8, Why)> = (0..f.ix.node_count()).map(|v| (dc.level(v), dc.why(v))).collect();
            (levels, lines)
        })
    };
    let one = run_on(1);
    assert!(!one.1.is_empty());
    for threads in [2, 8] {
        assert_eq!(run_on(threads), one, "{threads} threads");
    }
}

#[test]
fn imports_used_anywhere_in_expressions() {
    let f = fx(&[(
        "app.py",
        "from m import A, B, C, D, E, F, G, H, I, J, K, Unused\n\n\
         @A\ndef f(x: B, y=C) -> D:\n    pass\n\n\
         class Z(E, metaclass=F):\n    attr: G = None\n\n\
         def g():\n    try:\n        pass\n    except H:\n        pass\n    with I():\n        pass\n    \
         return f'{J}' + 'K'\n\n\
         if __name__ == '__main__':\n    g()\n",
    )]);
    let (_, lines) = f.app();
    let imports: Vec<&String> = lines.iter().filter(|l| l.starts_with("PM405")).collect();
    assert_eq!(imports, ["PM405 app:Unused@1 100"]);
}

#[test]
fn one_unit_resolves_like_the_whole_index() {
    let units = crate::roots::ProjectUnits::single(Path::new("/nowhere"), None);
    let app = fx(&[("main.py", "def dead():\n    pass\n\nif __name__ == '__main__':\n    pass\n"), ("lib.py", "def api():\n    pass\n")]);
    for f in [corpus(), app] {
        for mode in [Mode::Auto, Mode::Application, Mode::Library] {
            let (a, la) = f.run_with(&DeadCodeOptions { mode, ..DeadCodeOptions::default() });
            let (b, lb) = f.run_with(&DeadCodeOptions { mode, units: Some(&units), ..DeadCodeOptions::default() });
            assert_eq!(la, lb, "{mode:?}");
            assert_eq!(a.stats().library, b.stats().library, "{mode:?}");
            assert_eq!(b.stats().units, 1);
            assert_eq!(a.roots(), b.roots());
        }
    }
}

#[test]
fn units_without_entry_points() {
    let units = crate::roots::ProjectUnits::single(Path::new("/nowhere"), None);
    let opts = |mode| DeadCodeOptions { mode, units: Some(&units), ..DeadCodeOptions::default() };
    let none = fx(&[("a.py", "import b\n\ndef f():\n    b.g()\n"), ("b.py", "def g():\n    pass\n")]);
    let (dc, _) = none.run_with(&opts(Mode::Application));
    assert_eq!(dc.units_without_entry_points().collect::<Vec<_>>(), [0]);
    assert!(dc.modes().forced());
    // Library mode, or any entry point, silences it.
    let (dc, _) = none.run_with(&opts(Mode::Library));
    assert_eq!(dc.units_without_entry_points().count(), 0);
    let guard = fx(&[("a.py", "def f():\n    pass\n\nif __name__ == '__main__':\n    f()\n")]);
    let (dc, _) = guard.run_with(&opts(Mode::Application));
    assert_eq!(dc.units_without_entry_points().count(), 0);
    let test = fx(&[("a.py", "def f():\n    pass\n"), ("tests/test_a.py", "def test_f():\n    pass\n")]);
    let (dc, _) = test.run_with(&opts(Mode::Application));
    assert_eq!(dc.units_without_entry_points().count(), 0);
}
